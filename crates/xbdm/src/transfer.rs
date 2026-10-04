//! Copying files and directory trees between the local machine and the console.
//!
//! Every function takes an `on_file` callback that is told about each file once it has
//! been copied: (console path, local path, bytes).

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

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
    fn unsafe_names_rejected() {
        assert!(local_name("..").is_err());
        assert!(local_name("a/b").is_err());
        assert!(local_name("").is_err());
        assert_eq!(local_name("default.xex").unwrap(), "default.xex");
    }
}
