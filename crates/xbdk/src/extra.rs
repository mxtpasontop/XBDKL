//! `xbdk games`, `xbdk sync`, `xbdk shell` and `xbdk completions`.

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{CommandFactory, Subcommand};
use xbdm::content::{self, NameCache};
use xbdm::display::human_size;
use xbdm::shell::Shell;
use xbdm::{transfer, Client};

/// `$XDG_CACHE_HOME/xbdk/titles`, where title names are remembered between runs.
pub fn name_cache_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("xbdk").join("titles")
}

/// `xbdk games`
pub fn games(xbox: &mut Client, all: bool, verbose: bool) -> Result<()> {
    let cache_path = name_cache_path();
    let mut names = NameCache::load(&cache_path);
    let interactive = std::io::stderr().is_terminal();
    let library = content::scan(xbox, &mut names, &mut |step| {
        if interactive {
            eprint!("\r\x1b[K{step}");
        }
    })?;
    if interactive {
        eprint!("\r\x1b[K");
    }
    // A cache that can't be written only costs a re-download next time.
    let _ = names.save(&cache_path);

    let ids: Vec<&str> = library.profiles.iter().map(|p| p.id.as_str()).collect();
    if !ids.is_empty() {
        println!("Profiles: {}\n", ids.join(", "));
    }
    println!("{:<8}  {:<40} {:>10}  Contents", "Title ID", "Name", "Size");
    for title in library.titles.iter().filter(|t| all || t.has_game()) {
        let kinds: Vec<String> = title
            .kinds()
            .iter()
            .map(|(k, n)| if *n > 1 { format!("{} ×{n}", content::content_type_name(*k)) } else { content::content_type_name(*k).to_string() })
            .collect();
        println!(
            "{:08X}  {:<40} {:>10}  {}",
            title.title_id,
            ellipsize(title.name.as_deref().unwrap_or("(unknown)"), 40),
            human_size(title.size()),
            kinds.join(", ")
        );
        if verbose {
            for p in &title.packages {
                println!("          {:<22} {:>10}  {}", content::content_type_name(p.kind), human_size(p.size), p.path);
            }
        }
    }
    if !all {
        let hidden = library.titles.iter().filter(|t| !t.has_game()).count();
        if hidden > 0 {
            println!("\n{hidden} more titles have saves or other content only (show them with --all).");
        }
    }
    Ok(())
}

/// Shortens `s` to at most `max` characters, ending with `…` if cut.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

#[derive(Subcommand)]
pub enum SyncCmd {
    /// Copy new and changed files from a console folder to a local folder.
    Get {
        remote: String,
        local: PathBuf,
        /// Only show what would be copied.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Copy new and changed files from a local folder to a console folder.
    Put {
        local: PathBuf,
        remote: String,
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
}

/// `xbdk sync get|put`
pub fn sync(xbox: &mut Client, cmd: SyncCmd) -> Result<()> {
    let print = |what: &str, report: &transfer::SyncReport, dry_run: bool| {
        if dry_run {
            for path in &report.changed {
                println!("would {what} {path}");
            }
            println!("{} to {what}, {} unchanged", report.changed.len(), report.unchanged);
        } else {
            println!(
                "{} files copied ({}), {} unchanged",
                report.copied.files,
                human_size(report.copied.bytes),
                report.unchanged
            );
        }
    };
    match cmd {
        SyncCmd::Get { remote, local, dry_run } => {
            let report = transfer::sync_pull(xbox, &remote, &local, dry_run, &mut crate::print_get)?;
            print("download", &report, dry_run);
        }
        SyncCmd::Put { local, remote, dry_run } => {
            let report = transfer::sync_push(xbox, &local, &remote, dry_run, &mut crate::print_put)?;
            print("upload", &report, dry_run);
        }
    }
    Ok(())
}

/// `xbdk shell`: an interactive session with history and line editing.
pub fn shell(addr: SocketAddr) -> Result<()> {
    let mut xbox = Client::connect(addr).with_context(|| format!("connecting to {addr}"))?;
    let name = xbox.debug_name()?;
    let mut sh = Shell::new(std::env::current_dir()?);
    let mut editor = rustyline::DefaultEditor::new()?;
    let history = name_cache_path().with_file_name("history");
    let _ = editor.load_history(&history);
    println!("connected to {name} ({addr}); `help` lists commands, Ctrl-D exits");

    loop {
        let line = match editor.readline(&sh.prompt()) {
            Ok(line) => line,
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        if line.trim().is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(line.as_str());
        let mut result = sh.run(&mut xbox, &line);
        // After a reboot or a dropped connection, reconnect once and retry.
        if let Err(xbdm::Error::Io(_) | xbdm::Error::Protocol(_)) = &result
            && let Ok(fresh) = Client::connect(addr)
        {
            xbox = fresh;
            result = sh.run(&mut xbox, &line);
        }
        match result {
            Ok(reply) => {
                print!("{}", reply.text);
                if !reply.text.is_empty() && !reply.text.ends_with('\n') {
                    println!();
                }
                if reply.exit {
                    break;
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }
    if let Some(dir) = history.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = editor.save_history(&history);
    Ok(())
}

/// `xbdk completions SHELL`
pub fn completions(shell: clap_complete::Shell) {
    clap_complete::generate(shell, &mut crate::Cli::command(), "xbdk", &mut std::io::stdout());
}
