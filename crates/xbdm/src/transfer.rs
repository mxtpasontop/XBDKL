//! Copying files and directory trees between the local machine and the console.
//!
//! Every function takes an `on_file` callback that is told about each file once it has
//! been copied: (console path, local path, bytes).

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::path;

/// Console status code for "file already exists".
const ALREADY_EXISTS: u16 = 410;

/// Files and bytes copied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub bytes: u64,
}

pub type Progress<'a> = &'a mut dyn FnMut(&str, &Path, u64);

fn local_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Local { path: path.to_path_buf(), source }
}

/// Downloads one file. If `local` is an existing directory the file keeps its name.
/// A partially written local file is removed on failure.
pub fn get_file(xbox: &mut Client, remote: &str, local: &Path, on_file: Progress) -> Result<u64> {
    let remote = path::normalize(remote)?;
    let (_, name) = path::split(&remote);
    let local: PathBuf = if local.is_dir() { local.join(local_name(name)?) } else { local.to_path_buf() };
    let file = File::create(&local).map_err(local_err(&local))?;
    let mut out = BufWriter::new(file);
    let result = xbox.get_file(&remote, &mut out).and_then(|n| {
        out.flush().map_err(local_err(&local))?;
        Ok(n)
    });
    match result {
        Ok(n) => {
            on_file(&remote, &local, n);
            Ok(n)
        }
        Err(e) => {
            drop(out);
            let _ = fs::remove_file(&local);
            Err(e)
        }
    }
}

/// Downloads a directory tree. It is created inside `local` if that is an existing
/// directory, otherwise at `local` itself.
pub fn get_tree(xbox: &mut Client, remote: &str, local: &Path, on_file: Progress) -> Result<Totals> {
    let remote = path::normalize(remote)?;
    let remote = remote.trim_end_matches('\\');
    let (_, name) = path::split(remote);
    let root = if local.is_dir() && !name.is_empty() && !name.ends_with(':') {
        local.join(local_name(name)?)
    } else {
        local.to_path_buf()
    };
    let mut totals = Totals::default();
    get_dir(xbox, &format!("{remote}\\"), &root, &mut totals, on_file)?;
    Ok(totals)
}

fn get_dir(xbox: &mut Client, remote: &str, local: &Path, totals: &mut Totals, on_file: Progress) -> Result<()> {
    let entries = xbox.list_dir(remote)?;
    fs::create_dir_all(local).map_err(local_err(local))?;
    for entry in entries {
        let child_remote = path::join(remote, &entry.name);
        let child_local = local.join(local_name(&entry.name)?);
        if entry.is_dir {
            get_dir(xbox, &child_remote, &child_local, totals, on_file)?;
        } else {
            totals.bytes += get_file(xbox, &child_remote, &child_local, on_file)?;
            totals.files += 1;
        }
    }
    Ok(())
}

/// Uploads one file. A `remote` ending in `\` is a directory and the local name is kept.
pub fn put_file(xbox: &mut Client, local: &Path, remote: &str, on_file: Progress) -> Result<u64> {
    let remote = path::normalize(remote)?;
    if local.is_dir() {
        return Err(Error::InvalidArgument(format!("{} is a directory", local.display())));
    }
    let remote = if remote.ends_with('\\') { path::join(&remote, remote_name(local)?) } else { remote };
    let file = File::open(local).map_err(local_err(local))?;
    let len = file.metadata().map_err(local_err(local))?.len();
    xbox.send_file(&remote, &mut BufReader::new(file), len)?;
    on_file(&remote, local, len);
    Ok(len)
}

/// Uploads a directory tree. A `remote` ending in `\` receives the tree under its local name.
pub fn put_tree(xbox: &mut Client, local: &Path, remote: &str, on_file: Progress) -> Result<Totals> {
    let remote = path::normalize(remote)?;
    if !local.is_dir() {
        return Err(Error::InvalidArgument(format!("{} is not a directory", local.display())));
    }
    let remote = if remote.ends_with('\\') { path::join(&remote, remote_name(local)?) } else { remote };
    let mut totals = Totals::default();
    put_dir(xbox, local, &remote, &mut totals, on_file)?;
    Ok(totals)
}

/// Uploads a file or a directory tree into the console directory `remote_dir`.
pub fn put_any(xbox: &mut Client, local: &Path, remote_dir: &str, on_file: Progress) -> Result<Totals> {
    let dir = format!("{}\\", path::normalize(remote_dir)?.trim_end_matches('\\'));
    if local.is_dir() {
        put_tree(xbox, local, &dir, on_file)
    } else {
        let bytes = put_file(xbox, local, &dir, on_file)?;
        Ok(Totals { files: 1, bytes })
    }
}

fn put_dir(xbox: &mut Client, local: &Path, remote: &str, totals: &mut Totals, on_file: Progress) -> Result<()> {
    match xbox.mkdir(remote) {
        Err(e) if e.code() == Some(ALREADY_EXISTS) => {}
        other => other?,
    }
    let mut entries: Vec<_> = fs::read_dir(local)
        .and_then(|d| d.collect::<std::io::Result<Vec<_>>>())
        .map_err(local_err(local))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let child_local = entry.path();
        let kind = entry.file_type().map_err(local_err(&child_local))?;
        let child_remote = path::join(remote, remote_name(&child_local)?);
        if kind.is_dir() {
            put_dir(xbox, &child_local, &child_remote, totals, on_file)?;
        } else if kind.is_file() {
            totals.bytes += put_file(xbox, &child_local, &child_remote, on_file)?;
            totals.files += 1;
        }
        // Symlinks and special files are skipped.
    }
    Ok(())
}

// ---- sync -------------------------------------------------------------------

/// FAT stores times with 2-second precision, so closer times count as equal.
const TIME_SLACK: Duration = Duration::from_secs(2);

/// What a sync did (or, in a dry run, would do).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub copied: Totals,
    /// Files already identical on both sides.
    pub unchanged: u64,
    /// Console paths of the files that were (or would be) copied.
    pub changed: Vec<String>,
}

fn same_file(size_a: u64, time_a: Option<SystemTime>, size_b: u64, time_b: Option<SystemTime>) -> bool {
    let close = match (time_a, time_b) {
        (Some(a), Some(b)) => a.duration_since(b).or_else(|_| b.duration_since(a)).map(|d| d <= TIME_SLACK).unwrap_or(false),
        _ => false,
    };
    size_a == size_b && close
}

/// Makes `local_dir` a copy of the console directory `remote_dir`: new or changed files
/// (by size and modification time) are downloaded and given the console's timestamp.
/// Nothing is deleted. With `dry_run`, only reports what would be copied.
pub fn sync_pull(
    xbox: &mut Client,
    remote_dir: &str,
    local_dir: &Path,
    dry_run: bool,
    on_file: Progress,
) -> Result<SyncReport> {
    let remote_dir = path::normalize(remote_dir)?;
    let mut report = SyncReport::default();
    pull_dir(xbox, &remote_dir, local_dir, dry_run, &mut report, on_file)?;
    Ok(report)
}

fn pull_dir(
    xbox: &mut Client,
    remote: &str,
    local: &Path,
    dry_run: bool,
    report: &mut SyncReport,
    on_file: Progress,
) -> Result<()> {
    let entries = xbox.list_dir(remote)?;
    if !dry_run {
        fs::create_dir_all(local).map_err(local_err(local))?;
    }
    for entry in entries {
        let child_remote = path::join(remote, &entry.name);
        let child_local = local.join(local_name(&entry.name)?);
        if entry.is_dir {
            pull_dir(xbox, &child_remote, &child_local, dry_run, report, on_file)?;
            continue;
        }
        let existing = fs::metadata(&child_local).ok();
        if let Some(meta) = &existing
            && same_file(entry.size, entry.modified, meta.len(), meta.modified().ok())
        {
            report.unchanged += 1;
            continue;
        }
        report.changed.push(child_remote.clone());
        if dry_run {
            continue;
        }
        let bytes = get_file(xbox, &child_remote, &child_local, on_file)?;
        if let Some(modified) = entry.modified {
            File::options()
                .write(true)
                .open(&child_local)
                .and_then(|f| f.set_modified(modified))
                .map_err(local_err(&child_local))?;
        }
        report.copied.files += 1;
        report.copied.bytes += bytes;
    }
    Ok(())
}

/// Makes the console directory `remote_dir` a copy of `local_dir`: new or changed files
/// are uploaded and given the local timestamps. Nothing is deleted.
pub fn sync_push(
    xbox: &mut Client,
    local_dir: &Path,
    remote_dir: &str,
    dry_run: bool,
    on_file: Progress,
) -> Result<SyncReport> {
    let remote_dir = path::normalize(remote_dir)?;
    if !local_dir.is_dir() {
        return Err(Error::InvalidArgument(format!("{} is not a directory", local_dir.display())));
    }
    let mut report = SyncReport::default();
    push_dir(xbox, local_dir, &remote_dir, dry_run, &mut report, on_file)?;
    Ok(report)
}

fn push_dir(
    xbox: &mut Client,
    local: &Path,
    remote: &str,
    dry_run: bool,
    report: &mut SyncReport,
    on_file: Progress,
) -> Result<()> {
    let remote_entries = match xbox.list_dir(remote) {
        Ok(entries) => entries,
        // A missing directory reports 402 or, on some xbdm builds, 414.
        Err(e) if matches!(e.code(), Some(402 | 414)) => {
            if !dry_run {
                xbox.mkdir(remote)?;
            }
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let mut entries: Vec<_> = fs::read_dir(local)
        .and_then(|d| d.collect::<std::io::Result<Vec<_>>>())
        .map_err(local_err(local))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let child_local = entry.path();
        let kind = entry.file_type().map_err(local_err(&child_local))?;
        let name = remote_name(&child_local)?;
        let child_remote = path::join(remote, name);
        if kind.is_dir() {
            push_dir(xbox, &child_local, &child_remote, dry_run, report, on_file)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let meta = entry.metadata().map_err(local_err(&child_local))?;
        let on_console = remote_entries.iter().find(|e| !e.is_dir && e.name.eq_ignore_ascii_case(name));
        if let Some(e) = on_console
            && same_file(meta.len(), meta.modified().ok(), e.size, e.modified)
        {
            report.unchanged += 1;
            continue;
        }
        report.changed.push(child_remote.clone());
        if dry_run {
            continue;
        }
        let bytes = put_file(xbox, &child_local, &child_remote, on_file)?;
        if let Ok(modified) = meta.modified() {
            let created = meta.created().unwrap_or(modified);
            xbox.set_file_times(&child_remote, created, modified)?;
        }
        report.copied.files += 1;
        report.copied.bytes += bytes;
    }
    Ok(())
}

/// Rejects console names that could escape the local destination directory.
fn local_name(name: &str) -> Result<&str> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(Error::InvalidArgument(format!("unsafe file name from console: {name:?}")));
    }
    Ok(name)
}

fn remote_name(local: &Path) -> Result<&str> {
    local
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::InvalidArgument(format!("{} has no UTF-8 file name", local.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_file_tolerates_fat_time_rounding() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(same_file(5, Some(t), 5, Some(t + Duration::from_secs(1))));
        assert!(!same_file(5, Some(t), 5, Some(t + Duration::from_secs(3))));
        assert!(!same_file(5, Some(t), 6, Some(t)));
        assert!(!same_file(5, None, 5, Some(t)));
    }

    #[test]
    fn unsafe_names_rejected() {
        assert!(local_name("..").is_err());
        assert!(local_name("a/b").is_err());
        assert!(local_name("").is_err());
        assert_eq!(local_name("default.xex").unwrap(), "default.xex");
    }
}
