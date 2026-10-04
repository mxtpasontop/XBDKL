//! Debugging: threads, registers, execution control, breakpoints, memory map, and the
//! notification stream (breaks, exceptions, debug output, module and thread events).

use std::net::{Ipv4Addr, TcpListener};
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::path::quote;
use crate::response::{code, Params};

/// Thread details from `threadinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInfo {
    pub id: u32,
    pub suspend_count: u32,
    pub priority: i32,
    pub start: u32,
    pub stack_base: u32,
    pub stack_limit: u32,
    pub tls_base: u32,
    /// Hardware thread (0-5) the thread runs on.
    pub processor: u32,
}

/// Register values from `getcontext`. Registers the console could not capture (volatile
/// registers of a running thread) are reported as all ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub registers: Vec<(String, u64)>,
}

impl Context {
    pub fn get(&self, name: &str) -> Option<u64> {
        self.registers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
    }

    /// Instruction address register (program counter).
    pub fn pc(&self) -> Option<u32> {
        self.get("Iar").map(|v| v as u32)
    }
}

/// A committed region from `walkmem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub base: u32,
    pub size: u32,
    pub protect: u32,
}

impl Region {
    /// Short `rwx`-style description of the Windows page protection flags.
    pub fn access(&self) -> &'static str {
        match self.protect & 0xFF {
            0x01 => "---",
            0x02 => "r--",
            0x04 => "rw-",
            0x08 => "rw- (copy)",
            0x10 => "--x",
            0x20 => "r-x",
            0x40 => "rwx",
            0x80 => "rwx (copy)",
            _ => "?",
        }
    }
}

/// A section from `modsections`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub base: u32,
    pub size: u32,
    pub flags: u32,
}

/// Kind of data breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watch {
    Read,
    Write,
    Execute,
}

/// How long [`Client::step`] waits for the thread to stop again.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// Addresses where execution can continue after the PowerPC instruction `insn` at `pc`.
/// With `over_calls`, calls (branches that set LR) continue at `pc + 4` only.
pub fn next_addresses(insn: u32, pc: u32, lr: u32, ctr: u32, over_calls: bool) -> Vec<u32> {
    let next = pc.wrapping_add(4);
    let link = insn & 1 != 0;
    let absolute = insn & 2 != 0;
    let sign_extend = |value: u32, bits: u32| ((value << (32 - bits)) as i32 >> (32 - bits)) as u32;
    let relative = |offset: u32| if absolute { offset } else { pc.wrapping_add(offset) };
    // BO bits 0x14 set means "branch always" (no condition, no CTR decrement test).
    let always = (insn >> 21) & 0x14 == 0x14;

    let target = match insn >> 26 {
        18 => Some((relative(sign_extend(insn & 0x03FF_FFFC, 26)), true)),
        16 => Some((relative(sign_extend(insn & 0xFFFC, 16)), always)),
        19 => match (insn >> 1) & 0x3FF {
            16 => Some((lr & !3, always)),
            528 => Some((ctr & !3, always)),
            _ => None,
        },
        _ => None,
    };
    match target {
        None => vec![next],
        Some(_) if link && over_calls => vec![next],
        Some((t, true)) => vec![t],
        Some((t, false)) if t == next => vec![next],
        Some((t, false)) => vec![next, t],
    }
}

/// Parses a thread id, which xbdm prints as signed decimal (`-83886072`) or hex.
fn parse_thread_id(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).ok();
    }
    s.parse::<i32>().map(|v| v as u32).ok().or_else(|| s.parse::<u32>().ok())
}

/// Parses `0x` (32-bit) and `0q` (64-bit) register values.
fn parse_register(s: &str) -> Option<u64> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0q"))?;
    u64::from_str_radix(hex, 16).ok()
}

impl Client {
    // ---- debugger session --------------------------------------------------

    /// Registers this machine as the console's debugger. Needed on some kits before
    /// execution control commands are accepted. `override` takes over from another debugger.
    pub fn debugger_connect(&mut self, name: &str) -> Result<()> {
        self.send_command(&format!("debugger connect override name={} user={}", quote(name)?, quote(name)?))?;
        Ok(())
    }

    pub fn debugger_disconnect(&mut self) -> Result<()> {
        self.send_command("debugger disconnect")?;
        Ok(())
    }

    // ---- threads -----------------------------------------------------------

    pub fn threads(&mut self) -> Result<Vec<u32>> {
        let lines = self.command_lines("threads")?;
        lines
            .iter()
            .map(|l| parse_thread_id(l).ok_or_else(|| Error::Protocol(format!("bad thread id {l:?}"))))
            .collect()
    }

    pub fn thread_info(&mut self, thread: u32) -> Result<ThreadInfo> {
        let lines = self.command_lines(&format!("threadinfo thread=0x{thread:08x}"))?;
        let p = Params::parse(&lines.join(" "));
        Ok(ThreadInfo {
            id: thread,
            suspend_count: p.u32("suspend").unwrap_or(0),
            priority: p.get("priority").and_then(|v| v.parse().ok()).unwrap_or(0),
            start: p.u32("start").unwrap_or(0),
            stack_base: p.u32("base").unwrap_or(0),
            stack_limit: p.u32("limit").unwrap_or(0),
            tls_base: p.u32("tlsbase").unwrap_or(0),
            processor: p.u32("proc").unwrap_or(0),
        })
    }

    /// Reads a thread's control and integer registers (plus floating point if `fp`).
    pub fn context(&mut self, thread: u32, fp: bool) -> Result<Context> {
        let extra = if fp { " fp" } else { "" };
        let lines = self.command_lines(&format!("getcontext thread=0x{thread:08x} control int{extra}"))?;
        let mut registers = Vec::new();
        for line in &lines {
            for (name, value) in Params::parse(line).iter() {
                if let Some(v) = value.and_then(parse_register) {
                    registers.push((name.to_string(), v));
                }
            }
        }
        Ok(Context { registers })
    }

    /// Sets one register of a stopped thread, e.g. `("Gpr3", 0)` or `("Iar", 0x82001000)`.
    pub fn set_register(&mut self, thread: u32, name: &str, value: u64) -> Result<()> {
        if !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(Error::InvalidArgument(format!("bad register name {name:?}")));
        }
        self.send_command(&format!("setcontext thread=0x{thread:08x} {name}=0q{value:016x}"))?;
        Ok(())
    }

    /// `Some(reason)` if the thread is stopped in the debugger, `None` if it is running.
    pub fn stop_reason(&mut self, thread: u32) -> Result<Option<String>> {
        match self.send_command(&format!("isstopped thread=0x{thread:08x}")) {
            Ok(status) if status.code == code::MULTILINE => Ok(Some(self.read_body()?.join(" "))),
            Ok(status) => Ok(Some(status.message)),
            Err(e) if e.code() == Some(408) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ---- execution control -------------------------------------------------

    /// Stops every title thread.
    pub fn stop(&mut self) -> Result<()> {
        self.send_command("stop")?;
        Ok(())
    }

    /// Resumes execution after `stop` or a break.
    pub fn go(&mut self) -> Result<()> {
        self.send_command("go")?;
        Ok(())
    }

    /// Stops one thread.
    pub fn halt(&mut self, thread: u32) -> Result<()> {
        self.send_command(&format!("halt thread=0x{thread:08x}"))?;
        Ok(())
    }

    /// Lets a stopped thread run again when execution resumes. `exception` passes the
    /// current exception on to the title instead of swallowing it.
    pub fn continue_thread(&mut self, thread: u32, exception: bool) -> Result<()> {
        let extra = if exception { " exception" } else { "" };
        self.send_command(&format!("continue thread=0x{thread:08x}{extra}"))?;
        Ok(())
    }

    // ---- breakpoints -------------------------------------------------------

    pub fn set_breakpoint(&mut self, addr: u32) -> Result<()> {
        self.send_command(&format!("break addr=0x{addr:08x}"))?;
        Ok(())
    }

    pub fn clear_breakpoint(&mut self, addr: u32) -> Result<()> {
        self.send_command(&format!("break addr=0x{addr:08x} clear"))?;
        Ok(())
    }

    pub fn clear_all_breakpoints(&mut self) -> Result<()> {
        self.send_command("break clearall")?;
        Ok(())
    }

    /// Hardware data breakpoint on `size` bytes at `addr`.
    pub fn set_watchpoint(&mut self, kind: Watch, addr: u32, size: u32) -> Result<()> {
        let key = match kind {
            Watch::Read => "read",
            Watch::Write => "write",
            Watch::Execute => "execute",
        };
        self.send_command(&format!("break {key}=0x{addr:08x} size=0x{size:x}"))?;
        Ok(())
    }

    pub fn clear_watchpoint(&mut self, kind: Watch, addr: u32, size: u32) -> Result<()> {
        let key = match kind {
            Watch::Read => "read",
            Watch::Write => "write",
            Watch::Execute => "execute",
        };
        self.send_command(&format!("break {key}=0x{addr:08x} size=0x{size:x} clear"))?;
        Ok(())
    }

    // ---- stepping ----------------------------------------------------------

    /// Executes one instruction of a stopped thread and returns its new program counter.
    ///
    /// xbdm has no single-step command, so this puts temporary breakpoints on every address
    /// the instruction can continue at (decoding PowerPC branches), resumes, and waits for the
    /// thread to stop again. With `over_calls`, a call (`bl`, `bctrl`, ...) runs to completion.
    /// Other threads run while the step is in progress.
    pub fn step(&mut self, thread: u32, over_calls: bool) -> Result<u32> {
        if self.stop_reason(thread)?.is_none() {
            return Err(Error::InvalidArgument(format!("thread {thread:08X} is not stopped")));
        }
        let ctx = self.context(thread, false)?;
        let pc = ctx.pc().ok_or_else(|| Error::Protocol("context without Iar".into()))?;
        let insn = u32::from_be_bytes(self.read_memory(pc, 4)?.try_into().expect("4 bytes"));
        let lr = ctx.get("Lr").unwrap_or(0) as u32;
        let ctr = ctx.get("Ctr").unwrap_or(0) as u32;
        let targets = next_addresses(insn, pc, lr, ctr, over_calls);

        for &t in &targets {
            self.set_breakpoint(t)?;
        }
        let result = (|| {
            self.continue_thread(thread, false)?;
            self.go()?;
            let deadline = Instant::now() + STEP_TIMEOUT;
            while self.stop_reason(thread)?.is_none() {
                if Instant::now() > deadline {
                    return Err(Error::Protocol(format!(
                        "thread {thread:08X} did not reach the next instruction within {}s",
                        STEP_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            self.context(thread, false)?.pc().ok_or_else(|| Error::Protocol("context without Iar".into()))
        })();
        for &t in &targets {
            let _ = self.clear_breakpoint(t);
        }
        result
    }

    // ---- memory ------------------------------------------------------------

    /// Writes bytes to console memory.
    pub fn set_memory(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        // Keep command lines short; xbdm rejects very long ones.
        for (i, chunk) in data.chunks(128).enumerate() {
            let hex: String = chunk.iter().map(|b| format!("{b:02X}")).collect();
            let at = addr.wrapping_add((i * 128) as u32);
            self.send_command(&format!("setmem addr=0x{at:08x} data={hex}"))?;
        }
        Ok(())
    }

    /// Committed virtual memory regions.
    pub fn memory_map(&mut self) -> Result<Vec<Region>> {
        let lines = self.command_lines("walkmem")?;
        Ok(lines
            .iter()
            .filter_map(|l| {
                let p = Params::parse(l);
                Some(Region { base: p.u32("base")?, size: p.u32("size")?, protect: p.u32("protect").unwrap_or(0) })
            })
            .collect())
    }

    pub fn module_sections(&mut self, module: &str) -> Result<Vec<Section>> {
        let lines = self.command_lines(&format!("modsections name={}", quote(module)?))?;
        Ok(lines
            .iter()
            .filter_map(|l| {
                let p = Params::parse(l);
                Some(Section {
                    name: p.get("name")?.to_string(),
                    base: p.u32("base")?,
                    size: p.u32("size")?,
                    flags: p.u32("flags").unwrap_or(0),
                })
            })
            .collect())
    }

    // ---- notifications -----------------------------------------------------

    /// Turns this connection into a notification stream.
    ///
    /// xbdm requires a port it could reconnect to; events are delivered on this same
    /// connection (`reverse`), so nothing ever needs to connect to it.
    pub fn into_notifications(mut self) -> Result<Notifications> {
        let placeholder = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = placeholder.local_addr()?.port();
        self.send_command(&format!("notify reconnectport={port} reverse"))?
            .expect(code::DEDICATED)?;
        Ok(Notifications { client: self, _placeholder: placeholder })
    }
}

/// An event pushed by the console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// A software breakpoint was hit.
    Break { thread: u32, addr: u32 },
    /// A data breakpoint (watchpoint) triggered.
    DataBreak { thread: u32, addr: u32, kind: String, data_addr: u32 },
    SingleStep { thread: u32, addr: u32 },
    Exception { thread: u32, code: u32, addr: u32, first_chance: bool, detail: String },
    /// `started`, `stopped`, `pending`, `rebooting`, ...
    Execution(String),
    ModuleLoad { name: String, base: u32, size: u32 },
    ModuleUnload { name: String },
    ThreadCreate { thread: u32, start: u32 },
    ThreadTerminate { thread: u32 },
    /// Output of the title's `OutputDebugString` / `DbgPrint`.
    DebugString { thread: u32, text: String },
    Assert { thread: u32, text: String },
    /// Anything not recognised, verbatim.
    Other(String),
}

impl Notification {
    pub fn parse(line: &str) -> Notification {
        let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
        let p = Params::parse(rest);
        let thread = || p.get("thread").and_then(parse_thread_id).unwrap_or(0);
        let addr = |key: &str| p.u32(key).unwrap_or(0);
        // `string=` runs to the end of the line and is not quoted.
        let text = || rest.split_once("string=").map(|(_, s)| s.to_string()).unwrap_or_default();
        match kind.to_ascii_lowercase().as_str() {
            "break" => Notification::Break { thread: thread(), addr: addr("addr") },
            "data" => {
                let (kind, data_addr) = ["read", "write", "execute"]
                    .iter()
                    .find_map(|k| p.u32(k).map(|a| (k.to_string(), a)))
                    .unwrap_or_default();
                Notification::DataBreak { thread: thread(), addr: addr("addr"), kind, data_addr }
            }
            "singlestep" => Notification::SingleStep { thread: thread(), addr: addr("addr") },
            "exception" => Notification::Exception {
                thread: thread(),
                code: addr("code"),
                addr: addr("address"),
                first_chance: p.has("first"),
                detail: rest.to_string(),
            },
            "execution" => Notification::Execution(rest.trim().to_string()),
            "modload" => Notification::ModuleLoad {
                name: p.get("name").unwrap_or_default().to_string(),
                base: addr("base"),
                size: addr("size"),
            },
            "modunload" => Notification::ModuleUnload { name: p.get("name").unwrap_or_default().to_string() },
            "create" => Notification::ThreadCreate { thread: thread(), start: addr("start") },
            "terminate" => Notification::ThreadTerminate { thread: thread() },
            "debugstr" => Notification::DebugString { thread: thread(), text: text() },
            "assert" => Notification::Assert { thread: thread(), text: text() },
            _ => Notification::Other(line.to_string()),
        }
    }
}

impl std::fmt::Display for Notification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Notification::Break { thread, addr } => write!(f, "breakpoint at {addr:08X} (thread {thread:08X})"),
            Notification::DataBreak { thread, addr, kind, data_addr } => {
                write!(f, "data {kind} of {data_addr:08X} at {addr:08X} (thread {thread:08X})")
            }
            Notification::SingleStep { thread, addr } => write!(f, "step to {addr:08X} (thread {thread:08X})"),
            Notification::Exception { thread, code, addr, first_chance, .. } => write!(
                f,
                "{}exception {code:08X} at {addr:08X} (thread {thread:08X})",
                if *first_chance { "first-chance " } else { "" }
            ),
            Notification::Execution(state) => write!(f, "execution {state}"),
            Notification::ModuleLoad { name, base, size } => write!(f, "loaded {name} at {base:08X} (+{size:#x})"),
            Notification::ModuleUnload { name } => write!(f, "unloaded {name}"),
            Notification::ThreadCreate { thread, start } => write!(f, "thread {thread:08X} created, start {start:08X}"),
            Notification::ThreadTerminate { thread } => write!(f, "thread {thread:08X} exited"),
            Notification::DebugString { text, .. } => write!(f, "{}", text.trim_end()),
            Notification::Assert { thread, text } => write!(f, "assert (thread {thread:08X}): {text}"),
            Notification::Other(line) => write!(f, "{line}"),
        }
    }
}

/// A connection dedicated to receiving notifications.
pub struct Notifications {
    client: Client,
    _placeholder: TcpListener,
}

impl Notifications {
    /// Waits up to `timeout` for the next event. `Ok(None)` means nothing arrived in time.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Notification>> {
        match self.client.read_line_timeout(timeout)? {
            Some(line) => Ok(Some(Notification::parse(&line))),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_ids() {
        assert_eq!(parse_thread_id("-83886072"), Some(0xFB000008));
        assert_eq!(parse_thread_id("0xfb000008"), Some(0xFB000008));
        assert_eq!(parse_thread_id("42"), Some(42));
        assert_eq!(parse_thread_id("x"), None);
    }

    #[test]
    fn registers() {
        assert_eq!(parse_register("0q000000007a01fbc0"), Some(0x7a01fbc0));
        assert_eq!(parse_register("0x80075360"), Some(0x80075360));
        // Seen on a real console: a malformed value is skipped, not fatal.
        assert_eq!(parse_register("0qzz"), None);
    }

    #[test]
    fn step_targets() {
        let pc = 0x8200_1000;
        // addi r3, r3, 1: falls through.
        assert_eq!(next_addresses(0x3863_0001, pc, 0, 0, false), [pc + 4]);
        // b -0x10
        assert_eq!(next_addresses(0x4BFF_FFF0, pc, 0, 0, false), [pc - 0x10]);
        // bl +0x100: into the call, or over it.
        assert_eq!(next_addresses(0x4800_0101, pc, 0, 0, false), [pc + 0x100]);
        assert_eq!(next_addresses(0x4800_0101, pc, 0, 0, true), [pc + 4]);
        // beq +0x20 (BO=12, BI=2): either way.
        assert_eq!(next_addresses(0x4182_0020, pc, 0, 0, false), [pc + 4, pc + 0x20]);
        // blr (BO=20) goes to LR only.
        assert_eq!(next_addresses(0x4E80_0020, pc, 0x8200_2000, 0, false), [0x8200_2000]);
        // bctrl: into CTR, or over the call.
        assert_eq!(next_addresses(0x4E80_0421, pc, 0, 0x8250_0000, false), [0x8250_0000]);
        assert_eq!(next_addresses(0x4E80_0421, pc, 0, 0x8250_0000, true), [pc + 4]);
    }

    #[test]
    fn notifications() {
        assert_eq!(Notification::parse("execution started"), Notification::Execution("started".into()));
        assert_eq!(
            Notification::parse("break addr=0x82001000 thread=0xfb000008 stop"),
            Notification::Break { thread: 0xFB000008, addr: 0x82001000 }
        );
        assert_eq!(
            Notification::parse("debugstr thread=0xfb000010 lf string=hello world = 3"),
            Notification::DebugString { thread: 0xFB000010, text: "hello world = 3".into() }
        );
        assert_eq!(
            Notification::parse(r#"modload name="hud.xex" base=0x913e0000 size=0x00049000"#),
            Notification::ModuleLoad { name: "hud.xex".into(), base: 0x913e0000, size: 0x49000 }
        );
        assert!(matches!(Notification::parse("something new"), Notification::Other(_)));
    }
}
