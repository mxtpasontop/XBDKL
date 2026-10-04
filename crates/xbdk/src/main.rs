//! `xbdk`: Xbox 360 Neighborhood for the Linux command line.

mod debug;
mod extra;
mod fuse;

use std::fs::File;
use std::io::BufWriter;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use xbdm::{path, transfer, Client, XBDM_PORT};

const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Parser)]
#[command(version, about = "Xbox 360 Neighborhood for Linux")]
pub(crate) struct Cli {
    /// Console IP address or debug name. If omitted, the only console on the network is used.
    #[arg(short, long, env = "XBDK_CONSOLE", global = true)]
    console: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Find consoles on the local network.
    Discover,
    /// Show debug name, console type and system info.
    Info,
    /// Identify the title that is currently running.
    Title,
    /// List drives.
    Drives,
    /// Show size and free space of each drive.
    Df,
    /// List a directory (default HDD:\).
    Ls {
        path: Option<String>,
        /// Show size, date and type.
        #[arg(short, long)]
        long: bool,
    },
    /// Download a file (or a directory with -r) from the console.
    Get {
        remote: String,
        /// Local destination. An existing directory receives the item under its own name.
        /// Defaults to the current directory.
        local: Option<PathBuf>,
        /// Copy a directory and everything in it.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Upload a file (or a directory with -r). If REMOTE ends with \ or /, the local name is kept.
    Put {
        local: PathBuf,
        remote: String,
        /// Copy a directory and everything in it.
        #[arg(short, long)]
        recursive: bool,
    },
    /// Delete a file, or an empty directory with --dir.
    Rm {
        path: String,
        #[arg(short, long)]
        dir: bool,
    },
    /// Create a directory.
    Mkdir { path: String },
    /// Rename or move a file or directory.
    Mv { from: String, to: String },
    /// Save a PNG of what is on screen.
    Screenshot {
        /// Output file. Defaults to screenshot-<date>-<time>.png in the current directory.
        output: Option<PathBuf>,
    },
    /// Mount the console's drives as a local folder (unmount with Ctrl-C or `fusermount3 -u`).
    Mount {
        mountpoint: PathBuf,
        /// Mount read-only.
        #[arg(long)]
        read_only: bool,
    },
    /// List loaded modules, or the sections of one module.
    Modules {
        /// Show the sections of this module (e.g. default.xex).
        #[arg(long, value_name = "MODULE")]
        sections: Option<String>,
    },
    /// Read, write and map console memory.
    #[command(subcommand)]
    Mem(debug::MemCmd),
    /// Threads, registers, execution control, breakpoints and events.
    #[command(subcommand)]
    Debug(debug::DebugCmd),
    /// List installed games, saves and DLC with their names.
    Games {
        /// Also show titles that only have saves or other content.
        #[arg(short, long)]
        all: bool,
        /// List every package.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Copy only new and changed files between a console folder and a local folder.
    #[command(subcommand)]
    Sync(extra::SyncCmd),
    /// Interactive shell (cd, ls, get, put, mem, find, ...).
    Shell,
    /// Print a shell completion script, e.g. `xbdk completions bash > ~/.local/share/bash-completion/completions/xbdk`.
    Completions { shell: clap_complete::Shell },
    /// Launch an executable (.xex).
    Launch { xex: String },
    /// Reboot to the dashboard.
    Reboot {
        /// Full cold restart instead of a warm (title) reboot.
        #[arg(long)]
        cold: bool,
    },
}

fn main() -> Result<()> {
    // Behave like other command-line tools when piped into `head` and the like.
    // SAFETY: called before any threads exist; restores the default disposition.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    match &cli.command {
        Command::Discover => return discover(),
        Command::Completions { shell } => {
            extra::completions(*shell);
            return Ok(());
        }
        _ => {}
    }

    let target = find_console(cli.console.as_deref())?;
    match &cli.command {
        Command::Mount { mountpoint, read_only } => return fuse::mount(target, mountpoint, *read_only),
        Command::Shell => return extra::shell(target),
        _ => {}
    }
    let mut xbox = Client::connect(target).with_context(|| format!("connecting to {target}"))?;

    match cli.command {
        Command::Discover | Command::Mount { .. } | Command::Shell | Command::Completions { .. } => unreachable!(),
        Command::Games { all, verbose } => extra::games(&mut xbox, all, verbose)?,
        Command::Sync(cmd) => extra::sync(&mut xbox, cmd)?,
        Command::Modules { sections } => return debug::run_modules(xbox, sections),
        Command::Mem(cmd) => return debug::run_mem(xbox, cmd),
        Command::Debug(cmd) => return debug::run_debug(xbox, cmd),
        Command::Info => {
            println!("Name:     {}", xbox.debug_name()?);
            println!("Kit:      {}", xbox.console_type()?);
            for (key, value) in xbox.system_info()? {
                println!("{:<9} {value}", format!("{key}:"));
            }
        }
        Command::Title => show_title(&mut xbox)?,
        Command::Drives => {
            for drive in xbox.drives()? {
                println!("{drive}:");
            }
        }
        Command::Df => {
            println!("{:<16} {:>10} {:>10} {:>10}", "Drive", "Size", "Used", "Free");
            for drive in xbox.drives()? {
                // Optical and virtual drives have no free-space information.
                if let Ok(space) = xbox.drive_free_space(&drive)
                    && space.total > 0
                {
                    let used = space.total.saturating_sub(space.free);
                    println!(
                        "{:<16} {:>10} {:>10} {:>10}",
                        format!("{drive}:"),
                        human_size(space.total),
                        human_size(used),
                        human_size(space.available)
                    );
                }
            }
        }
        Command::Ls { path, long } => {
            let dir = path.as_deref().unwrap_or("HDD:\\");
            let mut entries = xbox.list_dir(dir)?;
            entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
            for e in entries {
                let name = if e.is_dir { format!("{}\\", e.name) } else { e.name };
                if long {
                    let size = if e.is_dir { "<DIR>".to_string() } else { e.size.to_string() };
                    println!("{:>12}  {}  {name}", size, format_time(e.modified));
                } else {
                    println!("{name}");
                }
            }
        }
        Command::Get { remote, local, recursive } => {
            let remote = path::normalize(&remote)?;
            let local = local.unwrap_or_else(|| PathBuf::from("."));
            if recursive {
                let t = transfer::get_tree(&mut xbox, &remote, &local, &mut print_get)?;
                println!("{} files, {}", t.files, human_size(t.bytes));
            } else {
                transfer::get_file(&mut xbox, &remote, &local, &mut print_get)?;
            }
        }
        Command::Put { local, remote, recursive } => {
            let remote = path::normalize(&remote)?;
            if recursive {
                let t = transfer::put_tree(&mut xbox, &local, &remote, &mut print_put)?;
                println!("{} files, {}", t.files, human_size(t.bytes));
            } else {
                transfer::put_file(&mut xbox, &local, &remote, &mut print_put)?;
            }
        }
        Command::Rm { path, dir } => xbox.delete(&path, dir)?,
        Command::Mkdir { path } => xbox.mkdir(&path)?,
        Command::Mv { from, to } => xbox.rename(&from, &to)?,
        Command::Screenshot { output } => {
            let output = output.unwrap_or_else(|| {
                let (y, mo, d, h, mi, s) = civil_time(SystemTime::now());
                PathBuf::from(format!("screenshot-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}.png"))
            });
            let shot = xbox.screenshot()?;
            let file = File::create(&output).with_context(|| format!("creating {}", output.display()))?;
            let mut encoder = png::Encoder::new(BufWriter::new(file), shot.width, shot.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header()?.write_image_data(&shot.rgb)?;
            println!("{}x{} -> {}", shot.width, shot.height, output.display());
        }
        Command::Launch { xex } => {
            xbox.launch(&xex)?;
            println!("launching {xex}");
        }
        Command::Reboot { cold } => {
            xbox.reboot(cold)?;
            println!("{} reboot requested", if cold { "cold" } else { "warm" });
        }
    }
    Ok(())
}

/// Prints what is running: executable, module, title ID/version and, if available, the name.
fn show_title(xbox: &mut Client) -> Result<()> {
    let title = xbdm::title::running_title(xbox)?;
    println!("Executable: {}", title.executable);
    if let Some(m) = &title.module {
        let built = UNIX_EPOCH + Duration::from_secs(m.timestamp.into());
        println!("Module:     {} at {:#010x}, {:#x} bytes, built {}", m.name, m.base, m.size, format_time(Some(built)));
    }
    match &title.info {
        Some(info) => {
            println!("Title ID:   {:08X}", info.title_id);
            println!("Media ID:   {:08X}", info.media_id);
            println!("Version:    {} (base {})", info.version, info.base_version);
            if info.disc_count > 1 {
                println!("Disc:       {} of {}", info.disc_number, info.disc_count);
            }
        }
        None => println!("(title ID not available)"),
    }
    if let Some(name) = &title.name {
        println!("Name:       {name}");
    }
    Ok(())
}

/// Progress callbacks for copies: one line per file.
pub(crate) fn print_get(remote: &str, local: &std::path::Path, bytes: u64) {
    println!("{remote} -> {} ({})", local.display(), human_size(bytes));
}

pub(crate) fn print_put(remote: &str, local: &std::path::Path, bytes: u64) {
    println!("{} -> {remote} ({})", local.display(), human_size(bytes));
}

fn discover() -> Result<()> {
    let consoles = xbdm::discover(DISCOVERY_TIMEOUT)?;
    if consoles.is_empty() {
        println!("no consoles answered (is xbdm loaded and the console on this subnet?)");
    }
    for c in consoles {
        println!("{:<16} {}", c.ip, c.name);
    }
    Ok(())
}

/// Resolves `--console` (IP, IP:port or debug name) to an address, or auto-discovers.
fn find_console(spec: Option<&str>) -> Result<SocketAddr> {
    let Some(spec) = spec else {
        let consoles = xbdm::discover(DISCOVERY_TIMEOUT)?;
        return match consoles.as_slice() {
            [one] => Ok(SocketAddr::new(one.ip, XBDM_PORT)),
            [] => bail!("no console found; pass --console <ip> or set XBDK_CONSOLE"),
            _ => bail!("several consoles found; pick one with --console (see `xbdk discover`)"),
        };
    };
    if let Ok(addr) = spec.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = spec.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, XBDM_PORT));
    }
    match xbdm::resolve(spec, DISCOVERY_TIMEOUT)? {
        Some(c) => Ok(SocketAddr::new(c.ip, XBDM_PORT)),
        None => bail!("no console named {spec:?} answered"),
    }
}

pub(crate) use xbdm::display::{civil_time, format_time, human_size};
