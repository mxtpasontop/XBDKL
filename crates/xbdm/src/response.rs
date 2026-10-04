//! Parsing of XBDM status lines and `key=value` parameter lines.

use crate::error::{Error, Result};

/// Status codes that matter to the client.
pub mod code {
    pub const OK: u16 = 200;
    pub const CONNECTED: u16 = 201;
    pub const MULTILINE: u16 = 202;
    pub const BINARY: u16 = 203;
    pub const SEND_BINARY: u16 = 204;
    pub const DEDICATED: u16 = 205;
}

/// A status line such as `200- OK`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub code: u16,
    pub message: String,
}

impl Status {
    pub fn parse(line: &str) -> Result<Status> {
        let bad = || Error::Protocol(format!("malformed status line: {line:?}"));
        let code = line.get(..3).ok_or_else(bad)?;
        if !code.bytes().all(|b| b.is_ascii_digit()) || line.as_bytes().get(3) != Some(&b'-') {
            return Err(bad());
        }
        Ok(Status {
            code: code.parse().map_err(|_| bad())?,
            message: line[4..].trim_start().to_string(),
        })
    }

    pub fn is_error(&self) -> bool {
        self.code >= 400
    }

    /// Converts a 4xx status into an `Err`.
    pub fn check(self) -> Result<Status> {
        if self.is_error() {
            Err(Error::Console { code: self.code, message: self.message })
        } else {
            Ok(self)
        }
    }

    /// Fails unless the status code is exactly `expected`.
    pub fn expect(self, expected: u16) -> Result<Status> {
        let status = self.check()?;
        if status.code == expected {
            Ok(status)
        } else {
            Err(Error::Protocol(format!(
                "expected status {expected}, got {} ({})",
                status.code, status.message
            )))
        }
    }
}

/// A parsed parameter line, e.g. `name="foo" sizelo=0x10 directory`.
///
/// Keys are matched case-insensitively. Bare words are stored as flags with no value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params(Vec<(String, Option<String>)>);

impl Params {
    pub fn parse(line: &str) -> Params {
        let mut out = Vec::new();
        let mut rest = line.trim_start();
        while !rest.is_empty() {
            let key_end = rest.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(rest.len());
            let key = rest[..key_end].to_string();
            rest = &rest[key_end..];
            let value = if let Some(after_eq) = rest.strip_prefix('=') {
                let (value, remaining) = if let Some(quoted) = after_eq.strip_prefix('"') {
                    match quoted.find('"') {
                        Some(end) => (&quoted[..end], &quoted[end + 1..]),
                        None => (quoted, ""),
                    }
                } else {
                    let end = after_eq.find(char::is_whitespace).unwrap_or(after_eq.len());
                    (&after_eq[..end], &after_eq[end..])
                };
                rest = remaining;
                Some(value.to_string())
            } else {
                None
            };
            if !key.is_empty() {
                out.push((key, value));
            }
            rest = rest.trim_start();
        }
        Params(out)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .and_then(|(_, v)| v.as_deref())
    }

    /// True if `key` appears at all (as a flag or with a value).
    pub fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k.eq_ignore_ascii_case(key))
    }

    /// Numeric value, accepting `0x`-prefixed hex or decimal.
    pub fn u32(&self, key: &str) -> Option<u32> {
        parse_u32(self.get(key)?)
    }

    /// Combines a `<prefix>hi` / `<prefix>lo` pair into one 64-bit value.
    pub fn u64_pair(&self, prefix: &str) -> Option<u64> {
        let hi = self.u32(&format!("{prefix}hi"))?;
        let lo = self.u32(&format!("{prefix}lo"))?;
        Some(((hi as u64) << 32) | lo as u64)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_deref()))
    }
}

fn parse_u32(s: &str) -> Option<u32> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_line() {
        let s = Status::parse("200- OK").unwrap();
        assert_eq!(s, Status { code: 200, message: "OK".into() });
        assert_eq!(Status::parse("202- multiline response follows").unwrap().code, 202);
        assert!(Status::parse("402- file not found").unwrap().check().unwrap_err().is_not_found());
        assert!(Status::parse("garbage").is_err());
        assert!(Status::parse("20").is_err());
    }

    #[test]
    fn dirlist_line() {
        // Captured from a real console.
        let p = Params::parse(
            r#"name="Cache" sizehi=0x0 sizelo=0x0 createhi=0x01c5ef5c createlo=0x81954400 changehi=0x01c5ef5c changelo=0x81954400 directory"#,
        );
        assert_eq!(p.get("name"), Some("Cache"));
        assert_eq!(p.u64_pair("size"), Some(0));
        assert_eq!(p.u64_pair("create"), Some(0x01c5ef5c_81954400));
        assert!(p.has("directory"));
        assert!(p.has("DIRECTORY"));
        assert!(!p.has("readonly"));
    }

    #[test]
    fn spaces_in_quotes_decimal_and_unterminated_quote() {
        let p = Params::parse(r#"name="Content Folder" count=12 other="unterminated"#);
        assert_eq!(p.get("name"), Some("Content Folder"));
        assert_eq!(p.u32("count"), Some(12));
        assert_eq!(p.get("other"), Some("unterminated"));
    }
}
