//! Settings remembered between runs, in `$XDG_CONFIG_HOME/xbdk/gui.conf` (`key=value` lines).

use std::fs;
use std::path::PathBuf;

pub struct Settings {
    /// Last console connected to (IP or name).
    pub console: String,
    pub download_dir: String,
}

impl Default for Settings {
    fn default() -> Settings {
        let download_dir = std::env::var("HOME").map(|h| format!("{h}/Downloads")).unwrap_or_else(|_| ".".into());
        Settings { console: String::new(), download_dir }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("xbdk").join("gui.conf"))
}

impl Settings {
    /// Loads saved settings; anything missing or unreadable falls back to the defaults.
    pub fn load() -> Settings {
        let mut settings = Settings::default();
        let text = path().and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("console", v)) => settings.console = v.trim().to_string(),
                Some(("download_dir", v)) if !v.trim().is_empty() => settings.download_dir = v.trim().to_string(),
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(file) = path() else { return Ok(()) };
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let clean = |v: &str| v.replace(['\n', '\r'], "");
        fs::write(file, format!("console={}\ndownload_dir={}\n", clean(&self.console), clean(&self.download_dir)))
    }
}
