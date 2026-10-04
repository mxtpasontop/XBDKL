//! Background threads that talk to the console so the UI never blocks on the network.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eframe::egui;
use xbdm::content::{self, Library, NameCache};
use xbdm::shell::Shell;
use xbdm::title::RunningTitle;
use xbdm::{
    transfer, Client, Console, Context, DirEntry, FreeSpace, Notification, Pattern, Screenshot, ThreadInfo, XBDM_PORT,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1500);

/// Work the UI asks for.
pub enum Job {
    /// Connect to an IP, IP:port or debug name; empty means "the only console on the network".
    Connect(String),
    Discover,
    Info,
    List(String),
    Download { remote: String, is_dir: bool, local_dir: PathBuf },
    Upload { local: PathBuf, remote_dir: String },
    Delete { path: String, is_dir: bool },
    Mkdir(String),
    Rename { from: String, to: String },
    Launch(String),
    Reboot { cold: bool },
    Screenshot,
    Threads,
    Registers(u32),
    Stop,
    Go,
    /// Address expressions: hex, `module`, or `module+offset`.
    Breakpoint { expr: String, set: bool },
    Step { thread: u32, over: bool },
    ReadMemory { expr: String, len: u32 },
    WriteMemory { expr: String, data: Vec<u8> },
    /// Search a module (`None`: the running title) for a pattern.
    FindMemory { pattern: Pattern, module: Option<String>, max: usize },
    /// Save memory to a file; without a length, `expr` must be a module name.
    DumpMemory { expr: String, len: Option<u32>, path: PathBuf },
    ScanLibrary,
    SyncPull { remote: String, local: PathBuf },
    SyncPush { local: PathBuf, remote: String },
    /// One command line for the Terminal tab.
    Shell(String),
}

pub struct ConsoleInfo {
    pub name: String,
    pub kit: String,
    pub system: Vec<(String, String)>,
    pub title: Option<RunningTitle>,
    pub drives: Vec<(String, Option<FreeSpace>)>,
}

pub struct ThreadRow {
    pub info: ThreadInfo,
    pub stopped: Option<String>,
}

/// Results sent back to the UI.
pub enum Reply {
    Connected { addr: SocketAddr, name: String },
    Discovered(Vec<Console>),
    Info(Box<ConsoleInfo>),
    Listing { path: String, entries: Vec<DirEntry> },
    /// A directory's contents changed; reload it if it is on screen.
    Changed(String),
    Screenshot(Screenshot),
    Threads(Vec<ThreadRow>),
    Registers { thread: u32, context: Context, stopped: Option<String> },
    Memory { addr: u32, bytes: Vec<Option<u8>> },
    Breakpoint { addr: u32, set: bool },
    FindResults(Vec<(u32, String)>),
    Library(Box<Library>),
    /// Output of a Terminal command.
    ShellOutput { line: String, text: String, error: bool, prompt: String },
    Event(Notification),
    EventsStopped,
    Status(String),
    Error(String),
    /// Sent after every job, successful or not.
    JobDone,
}

pub struct Worker {
    jobs: Sender<Job>,
    replies_tx: Sender<Reply>,
    pub replies: Receiver<Reply>,
    ctx: egui::Context,
    events_stop: Option<Arc<AtomicBool>>,
}

impl Worker {
    pub fn spawn(ctx: egui::Context) -> Worker {
        let (jobs, job_rx) = mpsc::channel();
        let (replies_tx, replies) = mpsc::channel();
        let tx = replies_tx.clone();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let local_dir = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
            let mut state = State { addr: None, client: None, shell: Some(Shell::new(local_dir)), repaint: repaint.clone() };
            for job in job_rx {
                let result = state.run(job, &tx);
                if let Err(e) = result {
                    let _ = tx.send(Reply::Error(e.to_string()));
                }
                let _ = tx.send(Reply::JobDone);
                repaint.request_repaint();
            }
        });
        Worker { jobs, replies_tx, replies, ctx, events_stop: None }
    }

    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// Starts streaming notifications from `addr` on a separate connection.
    pub fn start_events(&mut self, addr: SocketAddr) {
        self.stop_events();
        let stop = Arc::new(AtomicBool::new(false));
        self.events_stop = Some(stop.clone());
        let tx = self.replies_tx.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let result = (|| -> xbdm::Result<()> {
                let mut events = Client::connect(addr)?.into_notifications()?;
                while !stop.load(Ordering::Relaxed) {
                    if let Some(event) = events.next(Duration::from_millis(200))? {
                        let _ = tx.send(Reply::Event(event));
                        ctx.request_repaint();
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                let _ = tx.send(Reply::Error(format!("event stream: {e}")));
            }
            let _ = tx.send(Reply::EventsStopped);
            ctx.request_repaint();
        });
    }

    pub fn stop_events(&mut self) {
        if let Some(stop) = self.events_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

struct State {
    addr: Option<SocketAddr>,
    client: Option<Client>,
    /// Taken out while a command runs, so it can borrow the connection.
    shell: Option<Shell>,
    repaint: egui::Context,
}

impl State {
    /// Runs `f` on the connection, reconnecting once if it broke.
    fn xbox<T>(&mut self, mut f: impl FnMut(&mut Client) -> xbdm::Result<T>) -> xbdm::Result<T> {
        let addr = self.addr.ok_or_else(|| xbdm::Error::InvalidArgument("not connected to a console".into()))?;
        let mut last = None;
        for _ in 0..2 {
            if self.client.is_none() {
                self.client = Some(Client::connect(addr)?);
            }
            match f(self.client.as_mut().expect("connected above")) {
                Err(e @ (xbdm::Error::Io(_) | xbdm::Error::Protocol(_))) => {
                    self.client = None;
                    last = Some(e);
                }
                other => return other,
            }
        }
        Err(last.expect("loop ran"))
    }

    fn fresh_client(&self) -> xbdm::Result<Client> {
        let addr = self.addr.ok_or_else(|| xbdm::Error::InvalidArgument("not connected to a console".into()))?;
        Client::connect(addr)
    }

    fn run(&mut self, job: Job, tx: &Sender<Reply>) -> xbdm::Result<()> {
        let repaint = self.repaint.clone();
        let send = |r: Reply| {
            let _ = tx.send(r);
            repaint.request_repaint();
        };
        // Progress of long memory operations, at most every 2%.
        let progress = |what: &str| {
            let mut last = u64::MAX;
            let what = what.to_string();
            move |done: u64, total: u64| {
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                if last == u64::MAX || pct >= last + 2 || pct == 100 {
                    last = pct;
                    send(Reply::Status(format!("{what}: {pct}% of {}", human_size(total))));
                }
            }
        };
        match job {
            Job::Connect(spec) => {
                let addr = resolve(spec.trim())?;
                send(Reply::Status(format!("connecting to {addr}…")));
                let mut client = Client::connect(addr)?;
                let name = client.debug_name()?;
                self.addr = Some(addr);
                self.client = Some(client);
                send(Reply::Connected { addr, name });
            }
            Job::Discover => send(Reply::Discovered(xbdm::discover(DISCOVERY_TIMEOUT)?)),
            Job::Info => {
                let info = self.xbox(|c| {
                    let drives = c.drives()?;
                    Ok(ConsoleInfo {
                        name: c.debug_name()?,
                        kit: c.console_type()?,
                        system: c.system_info()?,
                        title: xbdm::title::running_title(c).ok(),
                        drives: drives.into_iter().map(|d| {
                            let space = c.drive_free_space(&d).ok().filter(|s| s.total > 0);
                            (d, space)
                        }).collect(),
                    })
                })?;
                send(Reply::Info(Box::new(info)));
            }
            Job::List(path) => {
                let mut entries = self.xbox(|c| c.list_dir(&path))?;
                entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
                send(Reply::Listing { path, entries });
            }
            Job::Download { remote, is_dir, local_dir } => {
                send(Reply::Status(format!("downloading {remote}…")));
                let mut progress = |r: &str, _: &std::path::Path, _: u64| send(Reply::Status(format!("downloaded {r}")));
                let totals = self.xbox(|c| {
                    if is_dir {
                        transfer::get_tree(c, &remote, &local_dir, &mut progress)
                    } else {
                        let bytes = transfer::get_file(c, &remote, &local_dir, &mut progress)?;
                        Ok(transfer::Totals { files: 1, bytes })
                    }
                })?;
                send(Reply::Status(format!(
                    "downloaded {} file(s), {} to {}",
                    totals.files,
                    human_size(totals.bytes),
                    local_dir.display()
                )));
            }
            Job::Upload { local, remote_dir } => {
                send(Reply::Status(format!("uploading {}…", local.display())));
                let mut progress = |r: &str, _: &std::path::Path, _: u64| send(Reply::Status(format!("uploaded {r}")));
                let totals = self.xbox(|c| transfer::put_any(c, &local, &remote_dir, &mut progress))?;
                send(Reply::Status(format!("uploaded {} file(s), {}", totals.files, human_size(totals.bytes))));
                send(Reply::Changed(remote_dir));
            }
            Job::Delete { path, is_dir } => {
                self.xbox(|c| c.delete(&path, is_dir))?;
                send(Reply::Status(format!("deleted {path}")));
                send(Reply::Changed(parent_of(&path)));
            }
            Job::Mkdir(path) => {
                self.xbox(|c| c.mkdir(&path))?;
                send(Reply::Changed(parent_of(&path)));
            }
            Job::Rename { from, to } => {
                self.xbox(|c| c.rename(&from, &to))?;
                send(Reply::Changed(parent_of(&from)));
            }
            // Both end the console session, so they get a connection of their own.
            Job::Launch(xex) => {
                self.fresh_client()?.launch(&xex)?;
                send(Reply::Status(format!("launching {xex}")));
            }
            Job::Reboot { cold } => {
                self.fresh_client()?.reboot(cold)?;
                send(Reply::Status(format!("{} reboot requested", if cold { "cold" } else { "warm" })));
            }
            Job::Screenshot => send(Reply::Screenshot(self.xbox(|c| c.screenshot())?)),
            Job::Threads => {
                let rows = self.xbox(|c| {
                    c.threads()?
                        .into_iter()
                        .map(|t| Ok(ThreadRow { info: c.thread_info(t)?, stopped: c.stop_reason(t)? }))
                        .collect::<xbdm::Result<Vec<_>>>()
                })?;
                send(Reply::Threads(rows));
            }
            Job::Registers(thread) => {
                let (context, stopped) = self.xbox(|c| Ok((c.context(thread, false)?, c.stop_reason(thread)?)))?;
                send(Reply::Registers { thread, context, stopped });
            }
            Job::Stop => {
                self.xbox(|c| c.debugger_connect("xbdk").and_then(|_| c.stop()))?;
                send(Reply::Status("execution stopped".into()));
            }
            Job::Go => {
                self.xbox(|c| c.debugger_connect("xbdk").and_then(|_| c.go()))?;
                send(Reply::Status("execution resumed".into()));
            }
            Job::Breakpoint { expr, set } => {
                let addr = self.xbox(|c| {
                    let addr = c.resolve_address(&expr)?;
                    // See xbdk debug: breakpoints are only safe with a registered debugger.
                    c.debugger_connect("xbdk")?;
                    if set { c.set_breakpoint(addr) } else { c.clear_breakpoint(addr) }?;
                    Ok(addr)
                })?;
                send(Reply::Breakpoint { addr, set });
            }
            Job::Step { thread, over } => {
                let pc = self.xbox(|c| c.debugger_connect("xbdk").and_then(|_| c.step(thread, over)))?;
                send(Reply::Status(format!("thread {thread:08X} at {pc:08X}")));
                let (context, stopped) = self.xbox(|c| Ok((c.context(thread, false)?, c.stop_reason(thread)?)))?;
                send(Reply::Registers { thread, context, stopped });
            }
            Job::ReadMemory { expr, len } => {
                let (addr, bytes) = self.xbox(|c| {
                    let addr = c.resolve_address(&expr)?;
                    Ok((addr, c.get_memory(addr, len)?))
                })?;
                send(Reply::Memory { addr, bytes });
            }
            Job::WriteMemory { expr, data } => {
                let addr = self.xbox(|c| {
                    let addr = c.resolve_address(&expr)?;
                    c.set_memory(addr, &data)?;
                    Ok(addr)
                })?;
                send(Reply::Status(format!("wrote {} bytes at {addr:08X}", data.len())));
            }
            Job::FindMemory { pattern, module, max } => {
                let mut report = progress("searching");
                let hits = self.xbox(|c| {
                    let (name, base, size) = match &module {
                        Some(m) => {
                            let (base, size) = c.module_range(m)?;
                            (m.clone(), base, size)
                        }
                        None => c.title_module()?,
                    };
                    let hits = c.find_memory(base, size, &pattern, max, &mut report)?;
                    Ok(hits.into_iter().map(|h| (h, format!("{name}+{:#x}", h - base))).collect::<Vec<_>>())
                })?;
                send(Reply::Status(format!("{} match(es)", hits.len())));
                send(Reply::FindResults(hits));
            }
            Job::DumpMemory { expr, len, path } => {
                let mut report = progress("dumping");
                let (start, len, unmapped) = self.xbox(|c| {
                    let (start, len) = match len {
                        Some(len) => (c.resolve_address(&expr)?, len),
                        None => c.module_range(&expr)?,
                    };
                    let file = std::fs::File::create(&path).map_err(|source| xbdm::Error::Local { path: path.clone(), source })?;
                    let mut out = std::io::BufWriter::new(file);
                    let unmapped = c.dump_memory(start, len, &mut out, &mut report)?;
                    std::io::Write::flush(&mut out).map_err(|source| xbdm::Error::Local { path: path.clone(), source })?;
                    Ok((start, len, unmapped))
                })?;
                let note = if unmapped > 0 { format!(", {unmapped} unmapped bytes as zeros") } else { String::new() };
                send(Reply::Status(format!("dumped {start:08X} ({}) to {}{note}", human_size(len.into()), path.display())));
            }
            Job::ScanLibrary => {
                let cache = name_cache_path();
                let mut names = NameCache::load(&cache);
                let library = self.xbox(|c| content::scan(c, &mut names, &mut |step| send(Reply::Status(step.to_string()))))?;
                let _ = names.save(&cache);
                send(Reply::Status(format!("{} titles", library.titles.len())));
                send(Reply::Library(Box::new(library)));
            }
            Job::SyncPull { remote, local } => {
                send(Reply::Status(format!("syncing {remote} -> {}…", local.display())));
                let mut log = |r: &str, _: &std::path::Path, _: u64| send(Reply::Status(format!("downloaded {r}")));
                let report = self.xbox(|c| transfer::sync_pull(c, &remote, &local, false, &mut log))?;
                send(Reply::Status(format!(
                    "{} copied ({}), {} unchanged -> {}",
                    report.copied.files,
                    human_size(report.copied.bytes),
                    report.unchanged,
                    local.display()
                )));
            }
            Job::SyncPush { local, remote } => {
                send(Reply::Status(format!("syncing {} -> {remote}…", local.display())));
                let mut log = |r: &str, _: &std::path::Path, _: u64| send(Reply::Status(format!("uploaded {r}")));
                let report = self.xbox(|c| transfer::sync_push(c, &local, &remote, false, &mut log))?;
                send(Reply::Status(format!(
                    "{} copied ({}), {} unchanged",
                    report.copied.files,
                    human_size(report.copied.bytes),
                    report.unchanged
                )));
                send(Reply::Changed(remote));
            }
            Job::Shell(line) => {
                let mut shell = self.shell.take().expect("shell is put back after every command");
                let result = self.xbox(|c| shell.run(c, &line));
                let prompt = shell.prompt();
                self.shell = Some(shell);
                let (text, error) = match result {
                    Ok(reply) => (reply.text, false),
                    Err(e) => (e.to_string(), true),
                };
                send(Reply::ShellOutput { line, text, error, prompt });
            }
        }
        Ok(())
    }
}

/// Turns user input into an address: IP, IP:port, debug name, or empty for auto-discovery.
fn resolve(spec: &str) -> xbdm::Result<SocketAddr> {
    if spec.is_empty() {
        let consoles = xbdm::discover(DISCOVERY_TIMEOUT)?;
        return match consoles.as_slice() {
            [one] => Ok(SocketAddr::new(one.ip, XBDM_PORT)),
            [] => Err(xbdm::Error::InvalidArgument("no console found on the network; enter its IP".into())),
            _ => Err(xbdm::Error::InvalidArgument("several consoles found; pick one".into())),
        };
    }
    if let Ok(addr) = spec.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = spec.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, XBDM_PORT));
    }
    xbdm::resolve(spec, DISCOVERY_TIMEOUT)?
        .map(|c| SocketAddr::new(c.ip, XBDM_PORT))
        .ok_or_else(|| xbdm::Error::InvalidArgument(format!("no console named {spec:?} answered")))
}

/// Console directory containing `path` (`HDD:\a\b` -> `HDD:\a`, `HDD:\a` -> `HDD:\`).
pub fn parent_of(path: &str) -> String {
    let trimmed = path.trim_end_matches('\\');
    match trimmed.rfind('\\') {
        Some(i) if trimmed[..i].ends_with(':') => trimmed[..=i].to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => format!("{trimmed}\\"),
    }
}

pub use xbdm::display::human_size;

/// `$XDG_CACHE_HOME/xbdk/titles`, shared with the command-line tool.
fn name_cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("xbdk").join("titles")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents() {
        assert_eq!(parent_of("HDD:\\a\\b"), "HDD:\\a");
        assert_eq!(parent_of("HDD:\\a"), "HDD:\\");
        assert_eq!(parent_of("HDD:\\a\\"), "HDD:\\");
        assert_eq!(parent_of("HDD:\\"), "HDD:\\");
    }
}
