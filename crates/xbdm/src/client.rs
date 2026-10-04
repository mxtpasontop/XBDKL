//! A blocking XBDM connection over TCP port 730.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::path::{self, quote};
use crate::response::{code, Params, Status};
use crate::screenshot::{self, Screenshot};

const TIMEOUT: Duration = Duration::from_secs(15);
/// Longest status or text line we accept; protects against a misbehaving peer.
const MAX_LINE: u64 = 64 * 1024;

/// One entry from `dirlist`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
}

impl DirEntry {
    fn from_params(p: &Params) -> Result<DirEntry> {
        let name = p
            .get("name")
            .ok_or_else(|| Error::Protocol("dirlist entry without a name".into()))?;
        Ok(DirEntry {
            name: name.to_string(),
            size: p.u64_pair("size").unwrap_or(0),
            is_dir: p.has("directory"),
            created: p.u64_pair("create").and_then(filetime_to_system),
            modified: p.u64_pair("change").and_then(filetime_to_system),
        })
    }
}

/// One entry from `modules`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub base: u32,
    pub size: u32,
    /// Link time, seconds since 1970.
    pub timestamp: u32,
}

/// Result of `drivefreespace`, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeSpace {
    pub total: u64,
    pub free: u64,
    /// Free space usable by the caller (may be less than `free`).
    pub available: u64,
}

/// Converts a Windows FILETIME (100 ns ticks since 1601-01-01) to `SystemTime`.
fn filetime_to_system(ft: u64) -> Option<SystemTime> {
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let secs = (ft / 10_000_000).checked_sub(EPOCH_DIFF_SECS)?;
    let nanos = (ft % 10_000_000) * 100;
    Some(UNIX_EPOCH + Duration::new(secs, nanos as u32))
}

pub struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    /// Connects and consumes the `201- connected` greeting.
    pub fn connect(addr: impl ToSocketAddrs) -> Result<Client> {
        let mut last_err = None;
        for addr in addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&addr, TIMEOUT) {
                Ok(stream) => return Client::from_stream(stream),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address to connect to"))
            .into())
    }

    /// Wraps an already-connected stream and consumes the greeting.
    pub fn from_stream(stream: TcpStream) -> Result<Client> {
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        stream.set_nodelay(true)?;
        let writer = stream.try_clone()?;
        let mut client = Client { reader: BufReader::new(stream), writer };
        client.read_status()?.expect(code::CONNECTED)?;
        Ok(client)
    }

    /// Drops the current connection and opens a new one to the same console.
    fn reconnect(&mut self) -> Result<()> {
        let addr = self.writer.peer_addr()?;
        let _ = self.writer.shutdown(std::net::Shutdown::Both);
        let fresh = Client::from_stream(TcpStream::connect_timeout(&addr, TIMEOUT)?)?;
        // The old value's Drop sends "bye" into the closed socket, which is harmless.
        *self = fresh;
        Ok(())
    }

    // ---- low level ---------------------------------------------------------

    fn read_line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        let n = (&mut self.reader).take(MAX_LINE).read_until(b'\n', &mut buf)?;
        if n == 0 {
            return Err(Error::Protocol("connection closed by console".into()));
        }
        if buf.last() != Some(&b'\n') {
            return Err(Error::Protocol("line too long or truncated".into()));
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        // XBDM is ASCII; keep any stray bytes visible instead of failing.
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn read_status(&mut self) -> Result<Status> {
        let line = self.read_line()?;
        Status::parse(&line)
    }

    /// Waits up to `timeout` for data, then reads one line. `None` if nothing arrived.
    /// Waiting happens before any byte is consumed, so a timeout never loses part of a line.
    pub(crate) fn read_line_timeout(&mut self, timeout: Duration) -> Result<Option<String>> {
        if self.reader.buffer().is_empty() {
            self.reader.get_ref().set_read_timeout(Some(timeout.max(Duration::from_millis(1))))?;
            let waited = self.reader.fill_buf().map(|b| b.len());
            self.reader.get_ref().set_read_timeout(Some(TIMEOUT))?;
            match waited {
                Ok(0) => return Err(Error::Protocol("connection closed by console".into())),
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.read_line().map(Some)
    }

    /// Reads the body of a 202 response up to the terminating `.` line.
    pub(crate) fn read_body(&mut self) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        loop {
            let line = self.read_line()?;
            if line == "." {
                return Ok(lines);
            }
            lines.push(line);
        }
    }

    /// Sends one command line and returns its status, with 4xx turned into `Err`.
    pub fn send_command(&mut self, command: &str) -> Result<Status> {
        if command.contains(['\r', '\n']) {
            return Err(Error::InvalidArgument("command contains a line break".into()));
        }
        self.writer.write_all(command.as_bytes())?;
        self.writer.write_all(b"\r\n")?;
        self.writer.flush()?;
        self.read_status()?.check()
    }

    /// Runs a command that answers with a single `200-` line and returns its text.
    pub fn command_text(&mut self, command: &str) -> Result<String> {
        Ok(self.send_command(command)?.expect(code::OK)?.message)
    }

    /// Runs a command that answers with a 202 multiline body.
    pub fn command_lines(&mut self, command: &str) -> Result<Vec<String>> {
        self.send_command(command)?.expect(code::MULTILINE)?;
        self.read_body()
    }

    // ---- console info ------------------------------------------------------

    /// The console's debug name (what Neighborhood shows).
    pub fn debug_name(&mut self) -> Result<String> {
        self.command_text("dbgname")
    }

    /// e.g. `devkit`, `testkit`, `reviewerkit`.
    pub fn console_type(&mut self) -> Result<String> {
        self.command_text("consoletype")
    }

    /// Key/value pairs from `systeminfo` (HDD, Type, Platform, Krnl, XDK, ...).
    pub fn system_info(&mut self) -> Result<Vec<(String, String)>> {
        let lines = self.command_lines("systeminfo")?;
        Ok(lines
            .iter()
            .flat_map(|l| {
                Params::parse(l)
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.unwrap_or_default().to_string()))
                    .collect::<Vec<_>>()
            })
            .collect())
    }

    // ---- file system -------------------------------------------------------

    /// Drive names without the colon, e.g. `HDD`, `USB0`, `GAME`.
    pub fn drives(&mut self) -> Result<Vec<String>> {
        let lines = self.command_lines("drivelist")?;
        Ok(lines
            .iter()
            .filter_map(|l| Params::parse(l).get("drivename").map(str::to_string))
            .collect())
    }

    pub fn list_dir(&mut self, dir: &str) -> Result<Vec<DirEntry>> {
        let dir = path::normalize(dir)?;
        let lines = self.command_lines(&format!("dirlist name={}", quote(&dir)?))?;
        lines.iter().map(|l| DirEntry::from_params(&Params::parse(l))).collect()
    }

    /// Attributes of a single file or directory.
    pub fn stat(&mut self, file: &str) -> Result<DirEntry> {
        let file = path::normalize(file)?;
        let lines = self.command_lines(&format!("getfileattributes name={}", quote(&file)?))?;
        let line = lines
            .first()
            .ok_or_else(|| Error::Protocol("empty getfileattributes response".into()))?;
        let mut params = Params::parse(line);
        let (_, name) = path::split(&file);
        if params.get("name").is_none() {
            params = Params::parse(&format!("name={} {line}", quote(name)?));
        }
        DirEntry::from_params(&params)
    }

    /// Downloads a file into `out`, returning the number of bytes written.
    pub fn get_file(&mut self, file: &str, out: &mut impl Write) -> Result<u64> {
        let file = path::normalize(file)?;
        self.send_command(&format!("getfile name={}", quote(&file)?))?.expect(code::BINARY)?;
        let mut len = [0u8; 4];
        self.reader.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as u64;
        let copied = io::copy(&mut (&mut self.reader).take(len), out)?;
        if copied != len {
            return Err(Error::Protocol(format!("file truncated: got {copied} of {len} bytes")));
        }
        Ok(len)
    }

    /// Uploads exactly `len` bytes from `data` to `file`, overwriting it if it exists.
    pub fn send_file(&mut self, file: &str, data: &mut impl Read, len: u64) -> Result<()> {
        let file = path::normalize(file)?;
        if len > u32::MAX as u64 {
            return Err(Error::InvalidArgument("files over 4 GiB cannot be sent".into()));
        }
        self.send_command(&format!("sendfile name={} length=0x{len:x}", quote(&file)?))?
            .expect(code::SEND_BINARY)?;
        if len == 0 {
            // xbdm has already created the empty file but keeps waiting for data and never
            // answers, so this connection is stuck; replace it.
            return self.reconnect();
        }
        let copied = io::copy(&mut data.take(len), &mut self.writer)?;
        self.writer.flush()?;
        if copied != len {
            // The console is still waiting for bytes; the connection is unusable now.
            return Err(Error::Protocol(format!(
                "local file shrank during upload ({copied} of {len} bytes)"
            )));
        }
        self.read_status()?.expect(code::OK)?;
        Ok(())
    }

    pub fn delete(&mut self, target: &str, is_dir: bool) -> Result<()> {
        let target = path::normalize(target)?;
        let suffix = if is_dir { " dir" } else { "" };
        self.send_command(&format!("delete name={}{suffix}", quote(&target)?))?;
        Ok(())
    }

    pub fn mkdir(&mut self, dir: &str) -> Result<()> {
        let dir = path::normalize(dir)?;
        self.send_command(&format!("mkdir name={}", quote(&dir)?))?;
        Ok(())
    }

    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let from = path::normalize(from)?;
        let to = path::normalize(to)?;
        self.send_command(&format!("rename name={} newname={}", quote(&from)?, quote(&to)?))?;
        Ok(())
    }

    // ---- processes and memory ----------------------------------------------

    /// Loaded modules (kernel, system DLLs, plugins and the running title).
    pub fn modules(&mut self) -> Result<Vec<Module>> {
        let lines = self.command_lines("modules")?;
        lines
            .iter()
            .map(|l| {
                let p = Params::parse(l);
                let field = |key: &str| {
                    p.u32(key).ok_or_else(|| Error::Protocol(format!("module line without {key}: {l:?}")))
                };
                Ok(Module {
                    name: p.get("name").unwrap_or_default().to_string(),
                    base: field("base")?,
                    size: field("size")?,
                    timestamp: p.u32("timestamp").unwrap_or(0),
                })
            })
            .collect()
    }

    /// Device path of the executable currently running, e.g. `\Device\Harddisk0\Partition1\x\default.xex`.
    pub fn running_executable(&mut self) -> Result<String> {
        let lines = self.command_lines("xbeinfo running")?;
        lines
            .iter()
            .find_map(|l| Params::parse(l).get("name").map(str::to_string))
            .ok_or_else(|| Error::Protocol("xbeinfo returned no name".into()))
    }

    /// Reads console memory. Unmapped bytes (reported as `??`) come back as `None`.
    pub fn get_memory(&mut self, addr: u32, len: u32) -> Result<Vec<Option<u8>>> {
        let lines = self.command_lines(&format!("getmem addr=0x{addr:08x} length=0x{len:x}"))?;
        let mut out = Vec::with_capacity(len as usize);
        for line in &lines {
            for pair in line.as_bytes().chunks(2) {
                let byte = std::str::from_utf8(pair).ok().and_then(|s| u8::from_str_radix(s, 16).ok());
                if byte.is_none() && pair != b"??" {
                    return Err(Error::Protocol(format!("bad hex in getmem reply: {line:?}")));
                }
                out.push(byte);
            }
        }
        Ok(out)
    }

    /// Reads console memory, failing if any byte in the range is unmapped.
    pub fn read_memory(&mut self, addr: u32, len: u32) -> Result<Vec<u8>> {
        self.get_memory(addr, len)?
            .into_iter()
            .collect::<Option<Vec<u8>>>()
            .ok_or_else(|| Error::Protocol(format!("memory at {addr:#010x} (+{len:#x}) is not mapped")))
    }

    /// Reads a big-endian u32 (the console's native byte order).
    pub fn read_u32_be(&mut self, addr: u32) -> Result<u32> {
        let b = self.read_memory(addr, 4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    // ---- screen ------------------------------------------------------------

    /// Captures the current front buffer.
    pub fn screenshot(&mut self) -> Result<Screenshot> {
        const MAX_FRAMEBUFFER: u32 = 64 * 1024 * 1024;
        self.send_command("screenshot")?.expect(code::BINARY)?;
        // e.g. "pitch=0x1400 width=0x500 height=0x2d0 format=0x18280186 offsetx=0 offsety=0, framebuffersize=..."
        let header = Params::parse(&self.read_line()?.replace(',', " "));
        let field = |key: &str| {
            header.u32(key).ok_or_else(|| Error::Protocol(format!("screenshot header without {key}")))
        };
        let (pitch, width, height, format) = (field("pitch")?, field("width")?, field("height")?, field("format")?);
        let size = field("framebuffersize")?;
        if size > MAX_FRAMEBUFFER {
            return Err(Error::Protocol(format!("framebuffer of {size} bytes is implausibly large")));
        }
        let mut fb = vec![0u8; size as usize];
        self.reader.read_exact(&mut fb)?;
        screenshot::decode(&fb, width, height, pitch, format)
    }

    // ---- drives ------------------------------------------------------------

    /// Total and free bytes on a drive (`HDD`, `HDD:` or `HDD:\` all work).
    pub fn drive_free_space(&mut self, drive: &str) -> Result<FreeSpace> {
        let drive = format!("{}:\\", drive.trim_end_matches(['\\', '/']).trim_end_matches(':'));
        let lines = self.command_lines(&format!("drivefreespace name={}", quote(&drive)?))?;
        let p = Params::parse(&lines.join(" "));
        Ok(FreeSpace {
            total: p.u64_pair("totalbytes").unwrap_or(0),
            free: p.u64_pair("totalfreebytes").unwrap_or(0),
            available: p.u64_pair("freetocaller").unwrap_or(0),
        })
    }

    // ---- power / titles ----------------------------------------------------

    /// Launches an executable, using its folder as the title's working directory.
    /// The console reboots into the title, so this connection is closed afterwards.
    pub fn launch(mut self, xex: &str) -> Result<()> {
        let xex = path::normalize(xex)?;
        let (dir, _) = path::split(&xex);
        let cmd = format!("magicboot title={} directory={}", quote(&xex)?, quote(dir)?);
        self.send_command(&cmd)?;
        Ok(())
    }

    /// Reboots to the dashboard. A warm reboot keeps XBDM loaded; a cold one
    /// restarts the console completely.
    pub fn reboot(mut self, cold: bool) -> Result<()> {
        let cmd = if cold { "magicboot cold" } else { "magicboot" };
        match self.send_command(cmd) {
            // The console may drop the connection before or after answering.
            Ok(_) => Ok(()),
            Err(Error::Protocol(_)) => Ok(()),
            Err(Error::Io(e)) if e.kind() == io::ErrorKind::ConnectionReset => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Polite goodbye so the console frees the connection slot; errors don't matter here.
        let _ = self.writer.write_all(b"bye\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_conversion() {
        // 0x01c5ef5c81954400 is 2005-11-22 12:01:44 UTC (dirlist on a real console).
        let t = filetime_to_system(0x01c5ef5c_81954400).unwrap();
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1_132_660_904);
        assert_eq!(filetime_to_system(0), None);
    }
}
