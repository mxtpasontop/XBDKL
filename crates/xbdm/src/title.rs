//! Reading title metadata from XEX2 executables, STFS packages (e.g. `nxeart`), and
//! the running title's in-memory XEX header. XEX and STFS fields are big-endian.

use crate::client::{Client, Module};
use crate::path;
use crate::error::{Error, Result};

/// Fields from a XEX2 header's execution-info optional header (key 0x00040006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionInfo {
    pub media_id: u32,
    pub version: Version,
    pub base_version: Version,
    pub title_id: u32,
    pub disc_number: u8,
    pub disc_count: u8,
}

/// Packed `major.minor.build.qfe` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version(pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v = self.0;
        write!(f, "{}.{}.{}.{}", v >> 28, (v >> 24) & 0xF, (v >> 8) & 0xFFFF, v & 0xFF)
    }
}

const XEX_EXECUTION_INFO: u32 = 0x0004_0006;

fn be32(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .ok_or_else(|| Error::Protocol(format!("header truncated at offset {offset:#x}")))
}

/// Parses the execution info out of the start of a XEX2 file.
pub fn xex_execution_info(xex: &[u8]) -> Result<ExecutionInfo> {
    let at = execution_info_offset(xex)? as usize;
    let body = xex
        .get(at..at + EXECUTION_INFO_LEN)
        .ok_or_else(|| Error::Protocol("execution info out of bounds".into()))?;
    parse_execution_info(body)
}

const EXECUTION_INFO_LEN: usize = 0x18;
/// Kernel export ordinal of `XexExecutableModuleHandle`, a pointer to the running title's loader entry.
const XEX_EXECUTABLE_MODULE_HANDLE: u32 = 0x193;
/// Offset of `XexHeaderBase` in `LDR_DATA_TABLE_ENTRY`.
const LDR_XEX_HEADER_BASE: u32 = 0x58;

/// Finds the execution info in a XEX2 header (`header` needs the fixed part and the
/// optional-header table) and returns its offset from the start of the header.
fn execution_info_offset(header: &[u8]) -> Result<u32> {
    if header.get(..4) != Some(b"XEX2") {
        return Err(Error::Protocol("not a XEX2 header".into()));
    }
    let count = be32(header, 0x14)? as usize;
    for i in 0..count.min(256) {
        let entry = 0x18 + i * 8;
        if be32(header, entry)? == XEX_EXECUTION_INFO {
            return be32(header, entry + 4);
        }
    }
    Err(Error::Protocol("XEX has no execution info header".into()))
}

fn parse_execution_info(body: &[u8]) -> Result<ExecutionInfo> {
    Ok(ExecutionInfo {
        media_id: be32(body, 0)?,
        version: Version(be32(body, 4)?),
        base_version: Version(be32(body, 8)?),
        title_id: be32(body, 12)?,
        disc_number: *body.get(18).unwrap_or(&0),
        disc_count: *body.get(19).unwrap_or(&0),
    })
}

/// Reads the running title's execution info straight from console memory.
///
/// Follows the kernel export `XexExecutableModuleHandle` to the title's loader entry and
/// from there to the XEX header the loader keeps in memory. Works for any title, including
/// ones that are not on a GAME: drive.
pub fn running_execution_info(client: &mut Client) -> Result<ExecutionInfo> {
    let kernel = client
        .modules()?
        .into_iter()
        .find(|m| m.name.eq_ignore_ascii_case("xboxkrnl.exe"))
        .ok_or_else(|| Error::Protocol("kernel module not listed".into()))?
        .base;

    // The kernel image is a PE file; its headers are little-endian.
    let pe = le32(&client.read_memory(kernel + 0x3C, 4)?, 0)?;
    let export_rva = le32(&client.read_memory(kernel + pe + 0x78, 4)?, 0)?;
    let exports = client.read_memory(kernel + export_rva, 0x28)?;
    let (ordinal_base, count, functions) = (le32(&exports, 0x10)?, le32(&exports, 0x14)?, le32(&exports, 0x1C)?);
    let index = XEX_EXECUTABLE_MODULE_HANDLE
        .checked_sub(ordinal_base)
        .filter(|&i| i < count)
        .ok_or_else(|| Error::Protocol("kernel does not export XexExecutableModuleHandle".into()))?;
    let variable = kernel + le32(&client.read_memory(kernel + functions + index * 4, 4)?, 0)?;

    let ldr = client.read_u32_be(variable)?;
    let header_base = client.read_u32_be(ldr + LDR_XEX_HEADER_BASE)?;
    let fixed = client.read_memory(header_base, 0x18)?;
    if fixed.get(..4) != Some(b"XEX2") {
        return Err(Error::Protocol(format!("no XEX2 header at {header_base:#010x}")));
    }
    let table_len = 0x18 + 8 * be32(&fixed, 0x14)?.min(256);
    let header = client.read_memory(header_base, table_len)?;
    let at = execution_info_offset(&header)?;
    parse_execution_info(&client.read_memory(header_base + at, EXECUTION_INFO_LEN as u32)?)
}

fn le32(data: &[u8], offset: usize) -> Result<u32> {
    be32(data, offset).map(u32::swap_bytes)
}

/// Everything known about the title that is running.
#[derive(Debug, Clone)]
pub struct RunningTitle {
    /// Device path, e.g. `\Device\Harddisk0\Partition1\Games\x\default.xex`.
    pub executable: String,
    pub module: Option<Module>,
    pub info: Option<ExecutionInfo>,
    /// Display name, from the title's `nxeart` package when it has one.
    pub name: Option<String>,
}

/// Identifies the running title: executable, module, execution info and name.
///
/// The execution info comes from memory (which reflects installed title updates) and falls
/// back to the copy of the executable on `GAME:`.
pub fn running_title(client: &mut Client) -> Result<RunningTitle> {
    let executable = client.running_executable()?;
    let (_, exe_name) = path::split(&executable);
    let exe_name = exe_name.to_string();
    let module = client
        .modules()?
        .into_iter()
        .rev()
        .find(|m| m.name.eq_ignore_ascii_case(&exe_name));

    let info = running_execution_info(client)
        .or_else(|_| {
            let mut xex = Vec::new();
            client.get_file(&path::join("GAME:\\", &exe_name), &mut xex)?;
            xex_execution_info(&xex)
        })
        .ok();

    let mut art = Vec::new();
    let name = client
        .get_file("GAME:\\nxeart", &mut art)
        .ok()
        .and_then(|_| stfs_package_info(&art).ok())
        .map(|pkg| if pkg.title_name.is_empty() { pkg.display_name } else { pkg.title_name })
        .filter(|n| !n.is_empty());

    Ok(RunningTitle { executable, module, info, name })
}

/// Metadata from an STFS package header (CON/LIVE/PIRS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub title_id: u32,
    /// English display name of the package.
    pub display_name: String,
    /// English name of the title the package belongs to.
    pub title_name: String,
}

pub fn stfs_package_info(pkg: &[u8]) -> Result<PackageInfo> {
    if !matches!(pkg.get(..4), Some(b"CON " | b"LIVE" | b"PIRS")) {
        return Err(Error::Protocol("not an STFS package".into()));
    }
    Ok(PackageInfo {
        title_id: be32(pkg, 0x360)?,
        display_name: utf16be_field(pkg, 0x411, 0x80)?,
        title_name: utf16be_field(pkg, 0x1691, 0x80)?,
    })
}

fn utf16be_field(data: &[u8], offset: usize, len: usize) -> Result<String> {
    let raw = data
        .get(offset..offset + len)
        .ok_or_else(|| Error::Protocol(format!("header truncated at offset {offset:#x}")))?;
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_be_bytes(c))
        .take_while(|&u| u != 0)
        .collect();
    Ok(String::from_utf16_lossy(&units).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_display() {
        assert_eq!(Version(0x2000_5208).to_string(), "2.0.82.8");
    }

    #[test]
    fn minimal_xex() {
        let mut x = vec![0u8; 0x60];
        x[..4].copy_from_slice(b"XEX2");
        x[0x14..0x18].copy_from_slice(&1u32.to_be_bytes());
        x[0x18..0x1C].copy_from_slice(&XEX_EXECUTION_INFO.to_be_bytes());
        x[0x1C..0x20].copy_from_slice(&0x40u32.to_be_bytes());
        x[0x40..0x44].copy_from_slice(&0xAABBCCDDu32.to_be_bytes());
        x[0x4C..0x50].copy_from_slice(&0x4D5307E6u32.to_be_bytes());
        let info = xex_execution_info(&x).unwrap();
        assert_eq!((info.media_id, info.title_id), (0xAABBCCDD, 0x4D5307E6));
        assert!(xex_execution_info(b"MZ\x90\0").is_err());
        x[0x1C..0x20].copy_from_slice(&0xFFFFu32.to_be_bytes());
        assert!(xex_execution_info(&x).is_err());
    }

    #[test]
    fn minimal_stfs() {
        let mut p = vec![0u8; 0x1800];
        p[..4].copy_from_slice(b"CON ");
        p[0x360..0x364].copy_from_slice(&0x4D5307E6u32.to_be_bytes());
        for (i, c) in "Halo".encode_utf16().enumerate() {
            p[0x1691 + i * 2..0x1693 + i * 2].copy_from_slice(&c.to_be_bytes());
        }
        let info = stfs_package_info(&p).unwrap();
        assert_eq!((info.title_id, info.title_name.as_str(), info.display_name.as_str()), (0x4D5307E6, "Halo", ""));
    }
}
