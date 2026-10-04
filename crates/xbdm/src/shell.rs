//! A small command interpreter over an XBDM connection, used by `xbdk shell` and the
//! app's Terminal tab. Each command returns its output as text.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::client::Client;
use crate::display::{format_time, hexdump, human_size};
use crate::error::{Error, Result};
use crate::memory::Pattern;
use crate::path::{self, quote};
use crate::transfer;

pub const HELP: &str = "\
Console files              Local side
  ls [-l] [PATH]             lpwd
  cd PATH | pwd              lcd DIR
  cat FILE                 Transfers
  mkdir DIR                  get REMOTE [LOCAL]      (folders too)
  rm FILE | rmdir DIR        put LOCAL [REMOTE_DIR]  (folders too)
  mv FROM TO                 sync-get REMOTE_DIR [LOCAL_DIR]
  drives | df                sync-put LOCAL_DIR [REMOTE_DIR]
Console                    Memory
  info | title               mem ADDR [LEN]          ADDR: 82000000, default.xex+0x10
  modules | threads          find HEX [MODULE]       default: the running title
  launch XEX | reboot      Other
  raw COMMAND                help | exit
Paths without a drive are relative to the current folder; `..` goes up.";

/// Output of one command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    /// The user asked to leave the shell.
    pub exit: bool,
}

impl Reply {
    fn text(text: impl Into<String>) -> Reply {
        Reply { text: text.into(), exit: false }
    }
}

pub struct Shell {
    /// Current console folder, e.g. `HDD:\Content`.
    pub cwd: String,
    /// Local folder that relative local paths are resolved against.
    pub local_dir: PathBuf,
}

impl Shell {
    pub fn new(local_dir: PathBuf) -> Shell {
        Shell { cwd: "HDD:\\".into(), local_dir }
    }

    pub fn prompt(&self) -> String {
        format!("{}> ", self.cwd)
    }

    /// Resolves a console path against the current folder, handling `.` and `..`.
    pub fn remote_path(&self, arg: &str) -> Result<String> {
        let arg = arg.replace('/', "\\");
        let joined = if arg.contains(':') { arg } else { path::join(&self.cwd, &arg) };
        let (drive, rest) = joined.split_once(":\\").unwrap_or((joined.trim_end_matches(':'), ""));
        let mut parts: Vec<&str> = Vec::new();
        for part in rest.split('\\') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                p => parts.push(p),
            }
        }
        path::normalize(&format!("{drive}:\\{}", parts.join("\\")))
    }

    fn local_path(&self, arg: &str) -> PathBuf {
        let expanded = match arg.strip_prefix("~/") {
            Some(rest) => std::env::var("HOME").map(|h| Path::new(&h).join(rest)).unwrap_or_else(|_| arg.into()),
            None => PathBuf::from(arg),
        };
        if expanded.is_absolute() { expanded } else { self.local_dir.join(expanded) }
    }

    /// Runs one command line.
    pub fn run(&mut self, xbox: &mut Client, line: &str) -> Result<Reply> {
        let args = split_args(line)?;
        let Some((cmd, args)) = args.split_first() else { return Ok(Reply::default()) };
        let arg = |i: usize| args.get(i).map(String::as_str);
        let need = |i: usize, what: &str| arg(i).ok_or_else(|| Error::InvalidArgument(format!("{cmd}: missing {what}")));

        let text = match cmd.to_ascii_lowercase().as_str() {
            "help" | "?" => HELP.to_string(),
            "exit" | "quit" => return Ok(Reply { text: String::new(), exit: true }),
            "pwd" => self.cwd.clone(),
            "lpwd" => self.local_dir.display().to_string(),
            "lcd" => {
                let dir = self.local_path(need(0, "directory")?);
                if !dir.is_dir() {
                    return Err(Error::InvalidArgument(format!("{} is not a directory", dir.display())));
                }
                self.local_dir = dir.canonicalize().unwrap_or(dir);
                self.local_dir.display().to_string()
            }
            "cd" => {
                let dir = self.remote_path(arg(0).unwrap_or("HDD:\\"))?;
                xbox.list_dir(&dir)?; // fails if it is not a directory
                self.cwd = dir;
                String::new()
            }
            "ls" | "dir" => {
                let long = args.iter().any(|a| a == "-l");
                let target = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or(".");
                let mut entries = xbox.list_dir(&self.remote_path(target)?)?;
                entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
                let mut out = String::new();
                for e in entries {
                    let name = if e.is_dir { format!("{}\\", e.name) } else { e.name };
                    if long {
                        let size = if e.is_dir { "<DIR>".to_string() } else { human_size(e.size) };
                        let _ = writeln!(out, "{size:>10}  {}  {name}", format_time(e.modified));
                    } else {
                        let _ = writeln!(out, "{name}");
                    }
                }
                out
            }
            "cat" => {
                let file = self.remote_path(need(0, "file")?)?;
                let size = xbox.stat(&file)?.size;
                if size > 1024 * 1024 {
                    return Err(Error::InvalidArgument(format!("{file} is {}; cat shows files up to 1 MiB", human_size(size))));
                }
                let mut buf = Vec::new();
                xbox.get_file(&file, &mut buf)?;
                decode_text(&buf)
            }
            "mkdir" => {
                xbox.mkdir(&self.remote_path(need(0, "directory")?)?)?;
                String::new()
            }
            "rm" | "del" => {
                xbox.delete(&self.remote_path(need(0, "file")?)?, false)?;
                String::new()
            }
            "rmdir" => {
                xbox.delete(&self.remote_path(need(0, "directory")?)?, true)?;
                String::new()
            }
            "mv" | "ren" => {
                let from = self.remote_path(need(0, "source")?)?;
                let to = self.remote_path(need(1, "destination")?)?;
                xbox.rename(&from, &to)?;
                String::new()
            }
            "drives" => xbox.drives()?.iter().map(|d| format!("{d}:\n")).collect(),
            "df" => {
                let mut out = format!("{:<16} {:>10} {:>10}\n", "Drive", "Size", "Free");
                for d in xbox.drives()? {
                    if let Ok(s) = xbox.drive_free_space(&d)
                        && s.total > 0
                    {
                        let _ = writeln!(out, "{:<16} {:>10} {:>10}", format!("{d}:"), human_size(s.total), human_size(s.available));
                    }
                }
                out
            }
            "get" => {
                let remote = self.remote_path(need(0, "console path")?)?;
                let local = arg(1).map(|a| self.local_path(a)).unwrap_or_else(|| self.local_dir.clone());
                let mut out = String::new();
                let mut log = |r: &str, l: &Path, n: u64| {
                    let _ = writeln!(out, "{r} -> {} ({})", l.display(), human_size(n));
                };
                if xbox.stat(&remote)?.is_dir {
                    transfer::get_tree(xbox, &remote, &local, &mut log)?;
                } else {
                    transfer::get_file(xbox, &remote, &local, &mut log)?;
                }
                out
            }
            "put" => {
                let local = self.local_path(need(0, "local path")?);
                let dir = self.remote_path(arg(1).unwrap_or("."))?;
                let mut out = String::new();
                transfer::put_any(xbox, &local, &dir, &mut |r, l, n| {
                    let _ = writeln!(out, "{} -> {r} ({})", l.display(), human_size(n));
                })?;
                out
            }
            "sync-get" => {
                let remote = self.remote_path(need(0, "console folder")?)?;
                let local = arg(1).map(|a| self.local_path(a)).unwrap_or_else(|| self.local_dir.clone());
                let report = transfer::sync_pull(xbox, &remote, &local, false, &mut |_, _, _| {})?;
                sync_summary(&report)
            }
            "sync-put" => {
                let local = self.local_path(need(0, "local folder")?);
                let remote = self.remote_path(arg(1).unwrap_or("."))?;
                let report = transfer::sync_push(xbox, &local, &remote, false, &mut |_, _, _| {})?;
                sync_summary(&report)
            }
            "info" => {
                let mut out = format!("Name: {}\nKit:  {}\n", xbox.debug_name()?, xbox.console_type()?);
                for (k, v) in xbox.system_info()? {
                    let _ = writeln!(out, "{k}: {v}");
                }
                out
            }
            "title" => {
                let t = crate::title::running_title(xbox)?;
                let mut out = format!("Executable: {}\n", t.executable);
                if let Some(name) = &t.name {
                    let _ = writeln!(out, "Name:       {name}");
                }
                if let Some(i) = &t.info {
                    let _ = writeln!(out, "Title ID:   {:08X}\nVersion:    {}", i.title_id, i.version);
                }
                out
            }
            "modules" => xbox
                .modules()?
                .iter()
                .map(|m| format!("{:<24} {:08X}  {:#x}\n", m.name, m.base, m.size))
                .collect(),
            "threads" => {
                let mut out = String::new();
                for t in xbox.threads()? {
                    let i = xbox.thread_info(t)?;
                    let _ = writeln!(out, "{t:08X}  pri {:>3}  cpu {}  start {:08X}", i.priority, i.processor, i.start);
                }
                out
            }
            "mem" => {
                let addr = xbox.resolve_address(need(0, "address")?)?;
                let len = arg(1).map(|l| u32::from_str_radix(l.trim_start_matches("0x"), 16)).transpose()
                    .map_err(|_| Error::InvalidArgument("length must be hex".into()))?
                    .unwrap_or(0x100)
                    .min(0x10000);
                hexdump(addr, &xbox.get_memory(addr, len)?)
            }
            "find" => {
                let pattern = Pattern::from_hex(need(0, "hex pattern")?)?;
                let (module, base, size) = match arg(1) {
                    Some(m) => {
                        let (base, size) = xbox.module_range(m)?;
                        (m.to_string(), base, size)
                    }
                    None => xbox.title_module()?,
                };
                let hits = xbox.find_memory(base, size, &pattern, 100, &mut |_, _| {})?;
                if hits.is_empty() {
                    format!("not found in {module}")
                } else {
                    hits.iter().map(|h| format!("{h:08X}  {module}+{:#x}\n", h - base)).collect()
                }
            }
            "launch" => {
                let xex = self.remote_path(need(0, "executable")?)?;
                let (dir, _) = path::split(&xex);
                xbox.send_command(&format!("magicboot title={} directory={}", quote(&xex)?, quote(dir)?))?;
                format!("launching {xex}")
            }
            "reboot" => {
                // The console drops the connection; the caller reconnects on the next command.
                let _ = xbox.send_command("magicboot");
                "rebooting".to_string()
            }
            "raw" => {
                let command = line.trim_start()[cmd.len()..].trim();
                let (status, body) = xbox.raw_command(command)?;
                let mut out = format!("{}- {}\n", status.code, status.message);
                for l in body {
                    let _ = writeln!(out, "{l}");
                }
                out
            }
            other => return Err(Error::InvalidArgument(format!("unknown command {other:?}; try `help`"))),
        };
        Ok(Reply::text(text))
    }
}

fn sync_summary(r: &transfer::SyncReport) -> String {
    format!("{} copied ({}), {} unchanged", r.copied.files, human_size(r.copied.bytes), r.unchanged)
}

/// Shows text files as text: UTF-16 (with BOM) or UTF-8/Latin-1.
fn decode_text(buf: &[u8]) -> String {
    match buf {
        [0xFE, 0xFF, rest @ ..] => {
            let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|&c| u16::from_be_bytes(c)).collect();
            String::from_utf16_lossy(&units)
        }
        [0xFF, 0xFE, rest @ ..] => {
            let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(buf).into_owned(),
    }
}

/// Splits a command line on whitespace, keeping "quoted strings" together.
pub fn split_args(line: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if in_quotes {
        return Err(Error::InvalidArgument("unterminated quote".into()));
    }
    if has_token {
        args.push(current);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_splitting() {
        assert_eq!(split_args(r#"get "HDD:\My Games\a.sav" ~/x"#).unwrap(), ["get", r"HDD:\My Games\a.sav", "~/x"]);
        assert_eq!(split_args("  ls   -l ").unwrap(), ["ls", "-l"]);
        assert_eq!(split_args(r#"find "7C 08""#).unwrap(), ["find", "7C 08"]);
        assert!(split_args(r#"cd "oops"#).is_err());
    }

    #[test]
    fn path_resolution() {
        let mut sh = Shell::new(PathBuf::from("/tmp"));
        sh.cwd = "HDD:\\Content\\0000000000000000".into();
        assert_eq!(sh.remote_path("454108E6").unwrap(), "HDD:\\Content\\0000000000000000\\454108E6");
        assert_eq!(sh.remote_path("..").unwrap(), "HDD:\\Content");
        assert_eq!(sh.remote_path("../../..").unwrap(), "HDD:\\");
        assert_eq!(sh.remote_path("USB0:/x/./y").unwrap(), "USB0:\\x\\y");
        assert_eq!(sh.remote_path("GAME:").unwrap(), "GAME:\\");
        assert_eq!(sh.local_path("save.bin"), PathBuf::from("/tmp/save.bin"));
    }

    #[test]
    fn text_decoding() {
        assert_eq!(decode_text(b"\xFE\xFF\x00D\x00i"), "Di");
        assert_eq!(decode_text(b"plain"), "plain");
    }
}
