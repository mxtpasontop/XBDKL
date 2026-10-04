//! `xbdk mount`: exposes the console's drives as a FUSE filesystem.
//!
//! The root directory lists drives (`HDD`, `USB0`, ...). XBDM can only transfer whole
//! files, so an open file is backed by a local temporary copy: it is downloaded on the first
//! read or write and uploaded again on flush/close if it was modified.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::net::SocketAddr;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::{mpsc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use fuser::{
    BsdFileFlags, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use xbdm::{path, Client, DirEntry};

/// How long the kernel may cache attributes and lookups.
const TTL: Duration = Duration::from_secs(1);
/// How long a directory listing fetched from the console is reused.
const LISTING_TTL: Duration = Duration::from_secs(2);
const BLOCK_SIZE: u32 = 4096;
/// FATX file names are limited to 42 characters.
const MAX_NAME: u32 = 42;

pub fn mount(addr: SocketAddr, mountpoint: &Path, read_only: bool) -> anyhow::Result<()> {
    let mut client = Client::connect(addr).with_context(|| format!("connecting to {addr}"))?;
    let name = client.debug_name()?;
    let owner = std::fs::metadata(mountpoint).with_context(|| format!("checking {}", mountpoint.display()))?;

    let (done_tx, done_rx) = mpsc::channel();
    let fs = XboxFs {
        inner: Mutex::new(Inner::new(addr, client)),
        uid: owner.uid(),
        gid: owner.gid(),
        read_only,
        done: done_tx.clone(),
    };

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::FSName(format!("xbdm:{name}")),
        MountOption::Subtype("xbdk".into()),
        MountOption::NoAtime,
        MountOption::DefaultPermissions,
        if read_only { MountOption::RO } else { MountOption::RW },
    ];
    let session = fuser::spawn_mount(fs, mountpoint, &config)
        .with_context(|| format!("mounting at {}", mountpoint.display()))?;
    ctrlc::set_handler(move || {
        let _ = done_tx.send(());
    })?;
    println!("mounted {name} ({addr}) at {}; press Ctrl-C to unmount", mountpoint.display());

    // Ends on Ctrl-C, or when someone runs `fusermount3 -u` (the filesystem's destroy()).
    let _ = done_rx.recv();
    // Fails harmlessly if the filesystem was already unmounted from outside.
    let _ = session.umount_and_join();
    println!("unmounted");
    Ok(())
}

struct XboxFs {
    inner: Mutex<Inner>,
    uid: u32,
    gid: u32,
    read_only: bool,
    done: mpsc::Sender<()>,
}

/// Metadata of one node, independent of FUSE types.
#[derive(Clone, Copy)]
struct Stat {
    is_dir: bool,
    size: u64,
    modified: SystemTime,
    created: SystemTime,
}

impl Stat {
    fn dir() -> Stat {
        Stat { is_dir: true, size: 0, modified: SystemTime::UNIX_EPOCH, created: SystemTime::UNIX_EPOCH }
    }

    fn from_entry(e: &DirEntry) -> Stat {
        Stat {
            is_dir: e.is_dir,
            size: e.size,
            modified: e.modified.unwrap_or(SystemTime::UNIX_EPOCH),
            created: e.created.unwrap_or(SystemTime::UNIX_EPOCH),
        }
    }
}

struct Handle {
    ino: u64,
    path: String,
    /// Local copy of the file contents, filled on first use.
    cache: Option<File>,
    dirty: bool,
}

struct Inner {
    addr: SocketAddr,
    client: Option<Client>,
    /// inode -> console path. "" is the root, "HDD:\" a drive, "HDD:\a\b" anything below.
    paths: HashMap<u64, String>,
    /// lowercase console path -> inode (FATX is case-insensitive).
    inodes: HashMap<String, u64>,
    next_ino: u64,
    listings: HashMap<String, (Instant, Vec<DirEntry>)>,
    drives: Option<(Instant, Vec<String>)>,
    handles: HashMap<u64, Handle>,
    next_fh: u64,
}

type FsResult<T> = Result<T, Errno>;

fn errno(e: &xbdm::Error) -> Errno {
    match e.code() {
        Some(402) => Errno::ENOENT,
        Some(410) => Errno::EEXIST,
        Some(411) => Errno::ENOTEMPTY,
        Some(412) => Errno::EINVAL,
        Some(414) => Errno::EACCES,
        Some(415) => Errno::ENOSPC,
        _ => match e {
            xbdm::Error::InvalidArgument(_) => Errno::EINVAL,
            _ => {
                eprintln!("xbdk: {e}");
                Errno::EIO
            }
        },
    }
}

fn io_errno(e: std::io::Error) -> Errno {
    eprintln!("xbdk: local cache: {e}");
    Errno::EIO
}

/// Splits a console path into (parent, name). Drive roots and the root have no parent.
fn parent_and_name(p: &str) -> Option<(String, &str)> {
    let i = p.rfind('\\')?;
    let name = &p[i + 1..];
    if name.is_empty() {
        return Some((String::new(), p.trim_end_matches(":\\")));
    }
    let parent = if p[..i].ends_with(':') { &p[..=i] } else { &p[..i] };
    Some((parent.to_string(), name))
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() { format!("{name}:\\") } else { path::join(parent, name) }
}

impl Inner {
    fn new(addr: SocketAddr, client: Client) -> Inner {
        let mut inner = Inner {
            addr,
            client: Some(client),
            paths: HashMap::new(),
            inodes: HashMap::new(),
            next_ino: INodeNo::ROOT.0 + 1,
            listings: HashMap::new(),
            drives: None,
            handles: HashMap::new(),
            next_fh: 1,
        };
        inner.paths.insert(INodeNo::ROOT.0, String::new());
        inner.inodes.insert(String::new(), INodeNo::ROOT.0);
        inner
    }

    /// Runs `f` on the connection, reconnecting once if the connection broke.
    fn xbox<T>(&mut self, mut f: impl FnMut(&mut Client) -> xbdm::Result<T>) -> FsResult<T> {
        let mut last = None;
        for _ in 0..2 {
            if self.client.is_none() {
                match Client::connect(self.addr) {
                    Ok(c) => self.client = Some(c),
                    Err(e) => return Err(errno(&e)),
                }
            }
            match f(self.client.as_mut().expect("connected above")) {
                Err(e @ (xbdm::Error::Io(_) | xbdm::Error::Protocol(_))) => {
                    self.client = None;
                    last = Some(e);
                }
                other => return other.map_err(|e| errno(&e)),
            }
        }
        Err(errno(&last.expect("loop ran")))
    }

    fn path(&self, ino: INodeNo) -> FsResult<String> {
        self.paths.get(&ino.0).cloned().ok_or(Errno::ENOENT)
    }

    fn ino(&mut self, path: &str) -> u64 {
        let key = path.to_lowercase();
        if let Some(&ino) = self.inodes.get(&key) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(key, ino);
        self.paths.insert(ino, path.to_string());
        ino
    }

    fn drives(&mut self) -> FsResult<Vec<String>> {
        if let Some((at, drives)) = &self.drives
            && at.elapsed() < LISTING_TTL
        {
            return Ok(drives.clone());
        }
        let drives = self.xbox(|c| c.drives())?;
        self.drives = Some((Instant::now(), drives.clone()));
        Ok(drives)
    }

    fn listing(&mut self, dir: &str) -> FsResult<Vec<DirEntry>> {
        let key = dir.to_lowercase();
        if let Some((at, entries)) = self.listings.get(&key)
            && at.elapsed() < LISTING_TTL
        {
            return Ok(entries.clone());
        }
        let entries = self.xbox(|c| c.list_dir(dir))?;
        self.listings.insert(key, (Instant::now(), entries.clone()));
        Ok(entries)
    }

    /// Forgets the cached listing of the directory containing `path`.
    fn invalidate_parent(&mut self, path: &str) {
        if let Some((parent, _)) = parent_and_name(path) {
            self.listings.remove(&parent.to_lowercase());
        }
    }

    fn stat(&mut self, path: &str) -> FsResult<Stat> {
        let Some((parent, name)) = parent_and_name(path) else {
            return Ok(Stat::dir()); // root
        };
        if parent.is_empty() {
            let drives = self.drives()?;
            return if drives.iter().any(|d| d.eq_ignore_ascii_case(name)) { Ok(Stat::dir()) } else { Err(Errno::ENOENT) };
        }
        let mut stat = self
            .listing(&parent)?
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case(name))
            .map(Stat::from_entry)
            .ok_or(Errno::ENOENT)?;
        // An open, modified file is bigger or smaller locally than on the console.
        let ino = self.inodes.get(&path.to_lowercase()).copied();
        if let Some(cache) = self.handles.values().find(|h| Some(h.ino) == ino).and_then(|h| h.cache.as_ref()) {
            stat.size = cache.metadata().map_err(io_errno)?.len();
        }
        Ok(stat)
    }

    /// Downloads a file into a new anonymous temporary file.
    fn download(&mut self, path: &str) -> FsResult<File> {
        let file = tempfile::tempfile().map_err(io_errno)?;
        self.xbox(|c| {
            let mut out = &file;
            out.set_len(0)?;
            out.seek(SeekFrom::Start(0))?;
            c.get_file(path, &mut out)
        })?;
        Ok(file)
    }

    fn upload(&mut self, path: &str, file: &File) -> FsResult<()> {
        let len = file.metadata().map_err(io_errno)?.len();
        self.xbox(|c| {
            let mut src = file;
            src.seek(SeekFrom::Start(0))?;
            c.send_file(path, &mut BufReader::new(src), len)
        })?;
        self.invalidate_parent(path);
        Ok(())
    }

    /// Makes sure the handle has its local copy and returns it.
    fn loaded(&mut self, fh: FileHandle) -> FsResult<&mut Handle> {
        let handle = self.handles.get(&fh.0).ok_or(Errno::EBADF)?;
        if handle.cache.is_none() {
            let path = handle.path.clone();
            let file = self.download(&path)?;
            self.handles.get_mut(&fh.0).ok_or(Errno::EBADF)?.cache = Some(file);
        }
        self.handles.get_mut(&fh.0).ok_or(Errno::EBADF)
    }

    /// Uploads the handle's local copy if it changed.
    fn sync(&mut self, fh: FileHandle) -> FsResult<()> {
        let Some(handle) = self.handles.get_mut(&fh.0) else { return Ok(()) };
        if !handle.dirty {
            return Ok(());
        }
        let (path, file) = (handle.path.clone(), handle.cache.as_ref().ok_or(Errno::EIO)?.try_clone().map_err(io_errno)?);
        self.upload(&path, &file)?;
        if let Some(h) = self.handles.get_mut(&fh.0) {
            h.dirty = false;
        }
        Ok(())
    }

    fn open_handle(&mut self, ino: u64, path: String, cache: Option<File>, dirty: bool) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, Handle { ino, path, cache, dirty });
        fh
    }

    /// Updates every remembered path at or below `from` after a rename.
    fn rename_paths(&mut self, from: &str, to: &str) {
        let from_key = from.to_lowercase();
        let is_below = |p: &str| {
            let lower = p.to_lowercase();
            lower == from_key || lower.starts_with(&format!("{from_key}\\"))
        };
        let moved: Vec<(u64, String)> = self
            .paths
            .iter()
            .filter(|(_, p)| is_below(p))
            .map(|(&ino, p)| (ino, format!("{to}{}", &p[from.len()..])))
            .collect();
        for (ino, new_path) in moved {
            if let Some(old) = self.paths.insert(ino, new_path.clone()) {
                self.inodes.remove(&old.to_lowercase());
            }
            self.inodes.insert(new_path.to_lowercase(), ino);
        }
        for h in self.handles.values_mut().filter(|h| is_below(&h.path)) {
            h.path = format!("{to}{}", &h.path[from.len()..]);
        }
        self.listings.clear();
    }
}

impl XboxFs {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn attr(&self, ino: u64, stat: Stat) -> FileAttr {
        let (kind, perm) = if stat.is_dir { (FileType::Directory, 0o755) } else { (FileType::RegularFile, 0o644) };
        let perm = if self.read_only { perm & 0o555 } else { perm };
        FileAttr {
            ino: INodeNo(ino),
            size: stat.size,
            blocks: stat.size.div_ceil(512),
            atime: stat.modified,
            mtime: stat.modified,
            ctime: stat.modified,
            crtime: stat.created,
            kind,
            perm,
            nlink: if stat.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    fn writable(&self) -> FsResult<()> {
        if self.read_only { Err(Errno::EROFS) } else { Ok(()) }
    }

    /// Resolves (parent inode, name) to a console path.
    fn child(inner: &Inner, parent: INodeNo, name: &OsStr) -> FsResult<String> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        if name.len() > MAX_NAME as usize {
            return Err(Errno::ENAMETOOLONG);
        }
        Ok(child_path(&inner.path(parent)?, name))
    }

    fn entry(&self, inner: &mut Inner, path: &str) -> FsResult<FileAttr> {
        let stat = inner.stat(path)?;
        let ino = inner.ino(path);
        Ok(self.attr(ino, stat))
    }
}

macro_rules! tri {
    ($reply:ident, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return $reply.error(e),
        }
    };
}

impl Filesystem for XboxFs {
    fn destroy(&mut self) {
        let _ = self.done.send(());
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let mut inner = self.lock();
        let path = tri!(reply, Self::child(&inner, parent, name));
        let attr = tri!(reply, self.entry(&mut inner, &path));
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let mut inner = self.lock();
        let path = tri!(reply, inner.path(ino));
        let stat = tri!(reply, inner.stat(&path));
        reply.attr(&TTL, &self.attr(ino.0, stat));
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Only size changes are meaningful; mode, owner and times are not stored by XBDM.
        let mut inner = self.lock();
        let path = tri!(reply, inner.path(ino));
        if let Some(size) = size {
            tri!(reply, self.writable());
            match fh {
                Some(fh) if inner.handles.contains_key(&fh.0) => {
                    let handle = tri!(reply, inner.loaded(fh));
                    tri!(reply, handle.cache.as_ref().expect("loaded").set_len(size).map_err(io_errno));
                    handle.dirty = true;
                }
                _ => {
                    let file = if size == 0 {
                        tri!(reply, tempfile::tempfile().map_err(io_errno))
                    } else {
                        tri!(reply, inner.download(&path))
                    };
                    tri!(reply, file.set_len(size).map_err(io_errno));
                    tri!(reply, inner.upload(&path, &file));
                }
            }
        }
        let stat = tri!(reply, inner.stat(&path));
        reply.attr(&TTL, &self.attr(ino.0, stat));
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        tri!(reply, self.writable());
        let mut inner = self.lock();
        let path = tri!(reply, Self::child(&inner, parent, name));
        tri!(reply, inner.xbox(|c| c.mkdir(&path)));
        inner.invalidate_parent(&path);
        let attr = tri!(reply, self.entry(&mut inner, &path));
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        tri!(reply, self.writable());
        let mut inner = self.lock();
        let path = tri!(reply, Self::child(&inner, parent, name));
        tri!(reply, inner.xbox(|c| c.delete(&path, false)));
        inner.invalidate_parent(&path);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        tri!(reply, self.writable());
        let mut inner = self.lock();
        let path = tri!(reply, Self::child(&inner, parent, name));
        if !tri!(reply, inner.listing(&path)).is_empty() {
            return reply.error(Errno::ENOTEMPTY);
        }
        tri!(reply, inner.xbox(|c| c.delete(&path, true)));
        inner.invalidate_parent(&path);
        inner.listings.remove(&path.to_lowercase());
        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        tri!(reply, self.writable());
        let mut inner = self.lock();
        let from = tri!(reply, Self::child(&inner, parent, name));
        let to = tri!(reply, Self::child(&inner, newparent, newname));
        let no_replace = flags.contains(RenameFlags::RENAME_NOREPLACE);
        if !(flags - RenameFlags::RENAME_NOREPLACE).is_empty() {
            return reply.error(Errno::EINVAL);
        }
        // POSIX rename replaces an existing target; XBDM refuses, so remove it first.
        if !from.eq_ignore_ascii_case(&to)
            && let Ok(existing) = inner.stat(&to)
        {
            if no_replace {
                return reply.error(Errno::EEXIST);
            }
            if existing.is_dir && !tri!(reply, inner.listing(&to)).is_empty() {
                return reply.error(Errno::ENOTEMPTY);
            }
            tri!(reply, inner.xbox(|c| c.delete(&to, existing.is_dir)));
        }
        tri!(reply, inner.xbox(|c| c.rename(&from, &to)));
        inner.rename_paths(&from, &to);
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let writing = flags.acc_mode() != OpenAccMode::O_RDONLY;
        if writing {
            tri!(reply, self.writable());
        }
        let mut inner = self.lock();
        let path = tri!(reply, inner.path(ino));
        let truncate = writing && flags.0 & libc::O_TRUNC != 0;
        let cache = if truncate { Some(tri!(reply, tempfile::tempfile().map_err(io_errno))) } else { None };
        let fh = inner.open_handle(ino.0, path, cache, truncate);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        tri!(reply, self.writable());
        let mut inner = self.lock();
        let path = tri!(reply, Self::child(&inner, parent, name));
        tri!(reply, inner.xbox(|c| c.send_file(&path, &mut std::io::empty(), 0)));
        inner.invalidate_parent(&path);
        let cache = tri!(reply, tempfile::tempfile().map_err(io_errno));
        let ino = inner.ino(&path);
        let fh = inner.open_handle(ino, path, Some(cache), false);
        let now = SystemTime::now();
        let attr = self.attr(ino, Stat { is_dir: false, size: 0, modified: now, created: now });
        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut inner = self.lock();
        let handle = tri!(reply, inner.loaded(fh));
        let file = handle.cache.as_ref().expect("loaded");
        let mut buf = vec![0u8; size as usize];
        let mut filled = 0;
        while filled < buf.len() {
            match file.read_at(&mut buf[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return reply.error(io_errno(e)),
            }
        }
        reply.data(&buf[..filled]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        tri!(reply, self.writable());
        let mut inner = self.lock();
        let handle = tri!(reply, inner.loaded(fh));
        tri!(reply, handle.cache.as_ref().expect("loaded").write_all_at(data, offset).map_err(io_errno));
        handle.dirty = true;
        reply.written(data.len() as u32);
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        tri!(reply, self.lock().sync(fh));
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        tri!(reply, self.lock().sync(fh));
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut inner = self.lock();
        let result = inner.sync(fh);
        inner.handles.remove(&fh.0);
        tri!(reply, result);
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let mut inner = self.lock();
        let dir = tri!(reply, inner.path(ino));
        let children: Vec<(String, bool)> = if dir.is_empty() {
            tri!(reply, inner.drives()).into_iter().map(|d| (d, true)).collect()
        } else {
            tri!(reply, inner.listing(&dir)).into_iter().map(|e| (e.name, e.is_dir)).collect()
        };
        let parent_ino = match parent_and_name(&dir) {
            Some((parent, _)) => inner.ino(&parent),
            None => INodeNo::ROOT.0,
        };
        let mut entries = vec![(ino.0, FileType::Directory, ".".to_string()), (parent_ino, FileType::Directory, "..".to_string())];
        for (name, is_dir) in children {
            let child_ino = inner.ino(&child_path(&dir, &name));
            entries.push((child_ino, if is_dir { FileType::Directory } else { FileType::RegularFile }, name));
        }
        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child_ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        let mut inner = self.lock();
        let path = inner.path(ino).unwrap_or_default();
        let drive = path.split(':').next().filter(|d| !d.is_empty()).unwrap_or("HDD").to_string();
        let space = inner.xbox(|c| c.drive_free_space(&drive)).unwrap_or(xbdm::FreeSpace { total: 0, free: 0, available: 0 });
        let block = BLOCK_SIZE as u64;
        reply.statfs(space.total / block, space.free / block, space.available / block, 0, 0, BLOCK_SIZE, MAX_NAME, BLOCK_SIZE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_structure() {
        assert_eq!(parent_and_name("HDD:\\a\\b"), Some(("HDD:\\a".to_string(), "b")));
        assert_eq!(parent_and_name("HDD:\\a"), Some(("HDD:\\".to_string(), "a")));
        assert_eq!(parent_and_name("HDD:\\"), Some((String::new(), "HDD")));
        assert_eq!(parent_and_name(""), None);
        assert_eq!(child_path("", "USB0"), "USB0:\\");
        assert_eq!(child_path("HDD:\\", "a"), "HDD:\\a");
        assert_eq!(child_path("HDD:\\a", "b"), "HDD:\\a\\b");
    }
}
