//! Background threads that talk to the console so the UI never blocks on the network.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eframe::egui;
use xbdm::title::RunningTitle;
use xbdm::{
    transfer, Client, Console, Context, DirEntry, FreeSpace, Notification, Screenshot, ThreadInfo, XBDM_PORT,
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
    Breakpoint { addr: u32, set: bool },
    Step { thread: u32, over: bool },
    ReadMemory { addr: u32, len: u32 },
    WriteMemory { addr: u32, data: Vec<u8> },
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
            let mut state = State { addr: None, client: None };
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
        let send = |r: Reply| {
            let _ = tx.send(r);
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
            Job::Breakpoint { addr, set } => {
                self.xbox(|c| {
                    // See xbdk debug: breakpoints are only safe with a registered debugger.
                    c.debugger_connect("xbdk")?;
                    if set { c.set_breakpoint(addr) } else { c.clear_breakpoint(addr) }
                })?;
                send(Reply::Breakpoint { addr, set });
            }
            Job::Step { thread, over } => {
                let pc = self.xbox(|c| c.debugger_connect("xbdk").and_then(|_| c.step(thread, over)))?;
                send(Reply::Status(format!("thread {thread:08X} at {pc:08X}")));
                let (context, stopped) = self.xbox(|c| Ok((c.context(thread, false)?, c.stop_reason(thread)?)))?;
                send(Reply::Registers { thread, context, stopped });
            }
            Job::ReadMemory { addr, len } => {
                let bytes = self.xbox(|c| c.get_memory(addr, len))?;
                send(Reply::Memory { addr, bytes });
            }
            Job::WriteMemory { addr, data } => {
                self.xbox(|c| c.set_memory(addr, &data))?;
                send(Reply::Status(format!("wrote {} bytes at {addr:08X}", data.len())));
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

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
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
