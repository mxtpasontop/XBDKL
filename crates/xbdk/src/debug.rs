//! `xbdk debug`, `xbdk mem` and `xbdk modules`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use clap::{Subcommand, ValueEnum};
use xbdm::{Client, Watch};

/// Parses an address or thread id: hex with or without `0x`.
pub fn parse_hex(s: &str) -> Result<u32, String> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u32::from_str_radix(&digits.replace('_', ""), 16).map_err(|_| format!("not a hex number: {s}"))
}

fn parse_hex64(s: &str) -> Result<u64, String> {
    let digits = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(&digits.replace('_', ""), 16).map_err(|_| format!("not a hex number: {s}"))
}

#[derive(Clone, Copy, ValueEnum)]
pub enum WatchKind {
    Read,
    Write,
    Execute,
}

impl From<WatchKind> for Watch {
    fn from(k: WatchKind) -> Watch {
        match k {
            WatchKind::Read => Watch::Read,
            WatchKind::Write => Watch::Write,
            WatchKind::Execute => Watch::Execute,
        }
    }
}

#[derive(Subcommand)]
pub enum DebugCmd {
    /// List threads with priority, start address, processor and state.
    Threads,
    /// Show a thread's registers.
    Regs {
        #[arg(value_parser = parse_hex)]
        thread: u32,
        /// Include floating-point registers.
        #[arg(long)]
        fp: bool,
    },
    /// Set a register of a stopped thread (e.g. `setreg FB000008 Gpr3 0`).
    Setreg {
        #[arg(value_parser = parse_hex)]
        thread: u32,
        register: String,
        #[arg(value_parser = parse_hex64)]
        value: u64,
    },
    /// Stop all title threads.
    Stop,
    /// Resume execution.
    Go,
    /// Stop one thread.
    Halt {
        #[arg(value_parser = parse_hex)]
        thread: u32,
    },
    /// Let a stopped thread continue when execution resumes.
    Continue {
        #[arg(value_parser = parse_hex)]
        thread: u32,
        /// Pass the pending exception to the title.
        #[arg(long)]
        exception: bool,
    },
    /// Execute one instruction of a stopped thread.
    Step {
        #[arg(value_parser = parse_hex)]
        thread: u32,
        /// Run calls to completion instead of stepping into them.
        #[arg(long)]
        over: bool,
    },
    /// Set or clear a code breakpoint.
    Break {
        #[arg(value_parser = parse_hex, required_unless_present = "clear_all")]
        addr: Option<u32>,
        #[arg(long)]
        clear: bool,
        /// Remove every breakpoint.
        #[arg(long, conflicts_with_all = ["addr", "clear"])]
        clear_all: bool,
    },
    /// Set or clear a hardware data breakpoint.
    Watch {
        kind: WatchKind,
        #[arg(value_parser = parse_hex)]
        addr: u32,
        /// Bytes to watch (hex).
        #[arg(value_parser = parse_hex, default_value = "4")]
        size: u32,
        #[arg(long)]
        clear: bool,
    },
    /// Print events (debug output, breaks, exceptions, module/thread events) until Ctrl-C.
    Events,
}

#[derive(Subcommand)]
pub enum MemCmd {
    /// Hex dump of console memory.
    Read {
        #[arg(value_parser = parse_hex)]
        addr: u32,
        /// Number of bytes (hex).
        #[arg(value_parser = parse_hex, default_value = "100")]
        len: u32,
    },
    /// Write bytes given as hex, e.g. `mem write 82001000 60000000`.
    Write {
        #[arg(value_parser = parse_hex)]
        addr: u32,
        hex: String,
    },
    /// List committed memory regions.
    Map,
}

pub fn run_debug(mut xbox: Client, cmd: DebugCmd) -> Result<()> {
    // Without a registered debugger xbdm reports breakpoint hits but lets the thread run on,
    // and threads can then take the trap as a real exception (seen crashing a game).
    if !matches!(cmd, DebugCmd::Threads | DebugCmd::Regs { .. } | DebugCmd::Events) {
        xbox.debugger_connect("xbdk")?;
    }
    match cmd {
        DebugCmd::Threads => {
            println!("{:<10} {:>4} {:>4} {:>8}  {:<10} {:<10} State", "Thread", "Pri", "CPU", "Suspend", "Start", "Stack");
            for id in xbox.threads()? {
                let info = xbox.thread_info(id)?;
                let state = xbox.stop_reason(id)?.map(|r| format!("stopped: {r}")).unwrap_or_else(|| "running".into());
                println!(
                    "{:08X}   {:>4} {:>4} {:>8}  {:08X}   {:08X}   {state}",
                    id, info.priority, info.processor, info.suspend_count, info.start, info.stack_base
                );
            }
        }
        DebugCmd::Regs { thread, fp } => {
            let ctx = xbox.context(thread, fp)?;
            if xbox.stop_reason(thread)?.is_none() {
                eprintln!("note: thread is running; only non-volatile registers are meaningful");
            }
            for chunk in ctx.registers.chunks(4) {
                let line: Vec<String> = chunk.iter().map(|(n, v)| format!("{n:>6} {v:016X}")).collect();
                println!("{}", line.join("  "));
            }
        }
        DebugCmd::Setreg { thread, register, value } => xbox.set_register(thread, &register, value)?,
        DebugCmd::Stop => {
            xbox.stop()?;
            println!("stopped");
        }
        DebugCmd::Go => {
            xbox.go()?;
            println!("running");
        }
        DebugCmd::Halt { thread } => xbox.halt(thread)?,
        DebugCmd::Continue { thread, exception } => xbox.continue_thread(thread, exception)?,
        DebugCmd::Step { thread, over } => {
            let pc = xbox.step(thread, over)?;
            println!("thread {thread:08X} at {pc:08X}");
        }
        DebugCmd::Break { addr, clear, clear_all } => match (addr, clear_all) {
            (_, true) => xbox.clear_all_breakpoints()?,
            (Some(a), _) if clear => xbox.clear_breakpoint(a)?,
            (Some(a), _) => xbox.set_breakpoint(a)?,
            (None, false) => bail!("an address is required"),
        },
        DebugCmd::Watch { kind, addr, size, clear } => {
            if clear {
                xbox.clear_watchpoint(kind.into(), addr, size)?;
            } else {
                xbox.set_watchpoint(kind.into(), addr, size)?;
            }
        }
        DebugCmd::Events => {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))?;
            let mut events = xbox.into_notifications()?;
            eprintln!("listening for events; Ctrl-C to stop");
            while !stop.load(Ordering::Relaxed) {
                if let Some(event) = events.next(Duration::from_millis(250))? {
                    println!("{event}");
                }
            }
        }
    }
    Ok(())
}

pub fn run_mem(mut xbox: Client, cmd: MemCmd) -> Result<()> {
    match cmd {
        MemCmd::Read { addr, len } => {
            let bytes = xbox.get_memory(addr, len)?;
            for (row, chunk) in bytes.chunks(16).enumerate() {
                let hex: Vec<String> = chunk.iter().map(|b| b.map_or("??".into(), |b| format!("{b:02X}"))).collect();
                let ascii: String = chunk
                    .iter()
                    .map(|b| match b {
                        Some(c) if c.is_ascii_graphic() || *c == b' ' => *c as char,
                        _ => '.',
                    })
                    .collect();
                println!("{:08X}  {:<47}  {ascii}", addr.wrapping_add(row as u32 * 16), hex.join(" "));
            }
        }
        MemCmd::Write { addr, hex } => {
            let hex: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
            if !hex.len().is_multiple_of(2) {
                bail!("hex data must have an even number of digits");
            }
            let data = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .context("invalid hex data")?;
            xbox.set_memory(addr, &data)?;
            println!("wrote {} bytes at {addr:08X}", data.len());
        }
        MemCmd::Map => {
            println!("{:<10} {:<10} {:>10}  Access", "Start", "End", "Size");
            for r in xbox.memory_map()? {
                println!(
                    "{:08X}   {:08X}   {:>10}  {}",
                    r.base,
                    r.base.wrapping_add(r.size),
                    crate::human_size(r.size.into()),
                    r.access()
                );
            }
        }
    }
    Ok(())
}

/// `xbdk modules [--sections NAME]`
pub fn run_modules(mut xbox: Client, sections: Option<String>) -> Result<()> {
    if let Some(module) = sections {
        println!("{:<10} {:<10} {:<10} Flags", "Section", "Start", "Size");
        for s in xbox.module_sections(&module)? {
            println!("{:<10} {:08X}   {:<10} {:#x}", s.name, s.base, format!("{:#x}", s.size), s.flags);
        }
    } else {
        println!("{:<24} {:<10} {:<10}", "Module", "Base", "Size");
        for m in xbox.modules()? {
            println!("{:<24} {:08X}   {:#x}", m.name, m.base, m.size);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_arguments() {
        assert_eq!(parse_hex("0x82001000"), Ok(0x82001000));
        assert_eq!(parse_hex("FB000008"), Ok(0xFB000008));
        assert_eq!(parse_hex("8200_1000"), Ok(0x82001000));
        assert!(parse_hex("xyz").is_err());
    }
}
