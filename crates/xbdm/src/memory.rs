//! Memory tools: address expressions, dumps and pattern search.

use std::io::Write;

use crate::client::Client;
use crate::error::{Error, Result};

/// Bytes read per `getmem` request (about 570 KiB/s on real hardware).
const CHUNK: u32 = 0x10000;

/// A byte pattern where `None` matches any byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern(pub Vec<Option<u8>>);

impl Pattern {
    /// Parses hex bytes with optional spaces and `??` wildcards, e.g. `"7C 08 ?? A6"`.
    pub fn from_hex(s: &str) -> Result<Pattern> {
        let digits: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.is_empty() || !digits.len().is_multiple_of(2) {
            return Err(Error::InvalidArgument(format!("pattern needs pairs of hex digits: {s:?}")));
        }
        let bytes = (0..digits.len())
            .step_by(2)
            .map(|i| match &digits[i..i + 2] {
                "??" => Ok(None),
                pair => u8::from_str_radix(pair, 16)
                    .map(Some)
                    .map_err(|_| Error::InvalidArgument(format!("not hex: {pair:?}"))),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Pattern(bytes))
    }

    pub fn from_bytes(bytes: &[u8]) -> Pattern {
        Pattern(bytes.iter().copied().map(Some).collect())
    }

    /// A big-endian 32-bit value, the console's native byte order.
    pub fn from_u32(value: u32) -> Pattern {
        Pattern::from_bytes(&value.to_be_bytes())
    }

    /// UTF-16 big-endian text, as the console stores wide strings.
    pub fn from_utf16(text: &str) -> Pattern {
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        Pattern::from_bytes(&bytes)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn matches(&self, window: &[Option<u8>]) -> bool {
        self.0.iter().zip(window).all(|(want, have)| match (want, have) {
            (None, Some(_)) => true,
            (Some(w), Some(h)) => w == h,
            (_, None) => false,
        })
    }
}

impl Client {
    /// Start and size of a loaded module, matched case-insensitively.
    pub fn module_range(&mut self, name: &str) -> Result<(u32, u32)> {
        self.modules()?
            .into_iter()
            .rev()
            .find(|m| m.name.eq_ignore_ascii_case(name))
            .map(|m| (m.base, m.size))
            .ok_or_else(|| Error::InvalidArgument(format!("no module named {name:?} is loaded")))
    }

    /// Name, start and size of the running title's main module (`default.xex`, `dash.xex`, ...).
    pub fn title_module(&mut self) -> Result<(String, u32, u32)> {
        let exe = self.running_executable()?;
        let (_, name) = crate::path::split(&exe);
        let name = name.to_string();
        let (base, size) = self.module_range(&name)?;
        Ok((name, base, size))
    }

    /// Resolves an address expression: hex (`82000000`, `0x82000000`), a module name
    /// (`default.xex`), or a module plus a hex offset (`default.xex+0x1234`).
    pub fn resolve_address(&mut self, expr: &str) -> Result<u32> {
        let expr = expr.trim();
        if let Some(addr) = parse_hex(expr) {
            return Ok(addr);
        }
        let (module, offset) = match expr.rsplit_once('+') {
            Some((m, off)) => {
                let off = parse_hex(off).ok_or_else(|| Error::InvalidArgument(format!("bad offset in {expr:?}")))?;
                (m.trim(), off)
            }
            None => (expr, 0),
        };
        let (base, size) = self.module_range(module)?;
        if offset >= size {
            return Err(Error::InvalidArgument(format!("offset {offset:#x} is past the end of {module} ({size:#x} bytes)")));
        }
        Ok(base + offset)
    }

    /// Copies `len` bytes of memory at `addr` into `out`, writing zeros for unmapped bytes.
    /// `progress` gets (bytes done, total). Returns how many bytes were unmapped.
    pub fn dump_memory(
        &mut self,
        addr: u32,
        len: u32,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<u64> {
        let mut unmapped = 0u64;
        let mut done = 0u32;
        while done < len {
            let n = CHUNK.min(len - done);
            let bytes = self.get_memory(addr.wrapping_add(done), n)?;
            let mut chunk = Vec::with_capacity(bytes.len());
            for b in bytes {
                chunk.push(b.unwrap_or_else(|| {
                    unmapped += 1;
                    0
                }));
            }
            out.write_all(&chunk)?;
            done += n;
            progress(done.into(), len.into());
        }
        Ok(unmapped)
    }

    /// Finds up to `max_hits` occurrences of `pattern` in `[start, start + len)`.
    /// `progress` gets (bytes searched, total).
    pub fn find_memory(
        &mut self,
        start: u32,
        len: u32,
        pattern: &Pattern,
        max_hits: usize,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<Vec<u32>> {
        if pattern.is_empty() {
            return Err(Error::InvalidArgument("empty pattern".into()));
        }
        let mut hits = Vec::new();
        // Bytes kept from the previous chunk so matches spanning chunks are found.
        let mut carry: Vec<Option<u8>> = Vec::new();
        let mut done = 0u32;
        while done < len && hits.len() < max_hits {
            let n = CHUNK.min(len - done);
            let chunk_start = start.wrapping_add(done);
            let mut window = std::mem::take(&mut carry);
            let window_start = chunk_start.wrapping_sub(window.len() as u32);
            window.extend(self.get_memory(chunk_start, n)?);
            if window.len() >= pattern.len() {
                for i in 0..=window.len() - pattern.len() {
                    if pattern.matches(&window[i..i + pattern.len()]) {
                        hits.push(window_start.wrapping_add(i as u32));
                        if hits.len() == max_hits {
                            break;
                        }
                    }
                }
                carry = window[window.len() - (pattern.len() - 1)..].to_vec();
            } else {
                carry = window;
            }
            done += n;
            progress(done.into(), len.into());
        }
        Ok(hits)
    }
}

fn parse_hex(s: &str) -> Option<u32> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        let p = Pattern::from_hex("7C 08 ?? a6").unwrap();
        assert_eq!(p.0, [Some(0x7C), Some(0x08), None, Some(0xA6)]);
        assert!(p.matches(&[Some(0x7C), Some(0x08), Some(0x02), Some(0xA6)]));
        assert!(!p.matches(&[Some(0x7C), Some(0x08), None, Some(0xA6)]));
        assert!(Pattern::from_hex("7C0").is_err());
        assert!(Pattern::from_hex("zz").is_err());
        assert_eq!(Pattern::from_u32(0x454108E6).0[0], Some(0x45));
        assert_eq!(Pattern::from_utf16("Hi").0, [Some(0), Some(b'H'), Some(0), Some(b'i')]);
    }

    #[test]
    fn hex_addresses() {
        assert_eq!(parse_hex("82000000"), Some(0x82000000));
        assert_eq!(parse_hex("0x10"), Some(0x10));
        // Module names that happen to be hex-like letters are not addresses.
        assert_eq!(parse_hex("default.xex"), None);
        assert_eq!(parse_hex(""), None);
    }
}
