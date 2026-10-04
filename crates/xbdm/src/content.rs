//! The console's content library: games, saves, DLC and other packages under `HDD:\Content`.
//!
//! Layout: `Content\<profile>\<title ID>\<content type>\<package>`, where profile
//! `0000000000000000` holds content shared by every profile. Package headers (STFS) carry
//! the title's name, so names are read from the smallest package of each title.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::client::Client;
use crate::error::Result;
use crate::path;
use crate::title::stfs_package_info;

/// Profile folder for content shared by all profiles.
pub const SHARED_PROFILE: &str = "0000000000000000";
/// Packages larger than this are never downloaded just to read a name.
const MAX_NAME_PROBE: u64 = 2 * 1024 * 1024;

/// Human-readable name of a content type folder.
pub fn content_type_name(kind: u32) -> &'static str {
    match kind {
        0x0000_0001 => "Saved game",
        0x0000_0002 => "Downloadable content",
        0x0000_0003 => "Publisher content",
        0x0000_1000 => "Xbox 360 title",
        0x0000_4000 => "Installed game",
        0x0000_5000 => "Original Xbox game",
        0x0000_7000 => "Games on Demand",
        0x0000_9000 => "Avatar item",
        0x0001_0000 => "Profile",
        0x0002_0000 => "Gamer picture",
        0x0003_0000 => "Theme",
        0x0004_0000 => "Cache file",
        0x0005_0000 => "Storage download",
        0x0006_0000 => "Original Xbox save",
        0x0008_0000 => "Game demo",
        0x0009_0000 => "Video",
        0x000A_0000 => "Game title",
        0x000B_0000 => "Title update",
        0x000C_0000 => "Game trailer",
        0x000D_0000 => "Arcade game",
        0x000E_0000 => "XNA game",
        0x000F_0000 => "License store",
        0x0010_0000 => "Movie",
        0x0020_0000 => "TV",
        0x0030_0000 => "Music video",
        0x0040_0000 => "Game video",
        0x0050_0000 => "Podcast video",
        0x0060_0000 => "Viral video",
        0x0200_0000 => "Community game",
        _ => "Other",
    }
}

/// True for content types that are playable games.
pub fn is_game(kind: u32) -> bool {
    matches!(kind, 0x4000 | 0x5000 | 0x7000 | 0x8_0000 | 0xA_0000 | 0xD_0000 | 0x200_0000)
}

/// Names of system titles that have no package to read a name from.
fn builtin_name(title_id: u32) -> Option<&'static str> {
    match title_id {
        0xFFFE_07D1 => Some("Xbox 360 Dashboard"),
        0x584E_0000 => Some("Xbox Live Marketplace"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Console path of the package file.
    pub path: String,
    /// Profile folder it lives under (`0000000000000000` for shared content).
    pub profile: String,
    pub kind: u32,
    /// Bytes on disk, including a Games on Demand `.data` folder.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleContent {
    pub title_id: u32,
    pub name: Option<String>,
    pub packages: Vec<Package>,
}

impl TitleContent {
    pub fn size(&self) -> u64 {
        self.packages.iter().map(|p| p.size).sum()
    }

    pub fn has_game(&self) -> bool {
        self.packages.iter().any(|p| is_game(p.kind))
    }

    /// Package count per content type, in a stable order.
    pub fn kinds(&self) -> Vec<(u32, usize)> {
        let mut counts: Vec<(u32, usize)> = Vec::new();
        for p in &self.packages {
            match counts.iter_mut().find(|(k, _)| *k == p.kind) {
                Some((_, n)) => *n += 1,
                None => counts.push((p.kind, 1)),
            }
        }
        counts.sort();
        counts
    }
}

/// A profile folder. (The gamertag is stored encrypted inside the profile package, so
/// only the profile ID is available.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Library {
    pub titles: Vec<TitleContent>,
    pub profiles: Vec<Profile>,
}

/// Title-ID-to-name cache, so names are only downloaded once.
#[derive(Debug, Clone, Default)]
pub struct NameCache(pub HashMap<u32, String>);

impl NameCache {
    /// Loads a cache file of `TITLEID<TAB>name` lines; a missing file gives an empty cache.
    pub fn load(file: &Path) -> NameCache {
        let text = fs::read_to_string(file).unwrap_or_default();
        NameCache(
            text.lines()
                .filter_map(|l| {
                    let (id, name) = l.split_once('\t')?;
                    Some((u32::from_str_radix(id, 16).ok()?, name.to_string()))
                })
                .collect(),
        )
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort();
        let mut out = fs::File::create(file)?;
        for (id, name) in entries {
            writeln!(out, "{id:08X}\t{}", name.replace(['\t', '\n'], " "))?;
        }
        Ok(())
    }
}

/// Scans `HDD:\Content`. `progress` receives a short description of each step.
pub fn scan(client: &mut Client, names: &mut NameCache, progress: &mut dyn FnMut(&str)) -> Result<Library> {
    let root = "HDD:\\Content";
    let mut titles: HashMap<u32, TitleContent> = HashMap::new();
    let mut profiles = Vec::new();

    for profile in client.list_dir(root)?.into_iter().filter(|e| e.is_dir) {
        let profile_dir = path::join(root, &profile.name);
        progress(&format!("scanning {profile_dir}"));
        for title in client.list_dir(&profile_dir)?.into_iter().filter(|e| e.is_dir) {
            // Some folders are not title IDs (stray copies, tool folders); skip them.
            let Ok(title_id) = u32::from_str_radix(&title.name, 16) else { continue };
            if title.name.len() != 8 {
                continue;
            }
            let title_dir = path::join(&profile_dir, &title.name);
            for kind_dir in client.list_dir(&title_dir)?.into_iter().filter(|e| e.is_dir) {
                let Ok(kind) = u32::from_str_radix(&kind_dir.name, 16) else { continue };
                let kind_path = path::join(&title_dir, &kind_dir.name);
                let entries = client.list_dir(&kind_path)?;
                for file in entries.iter().filter(|e| !e.is_dir) {
                    let package_path = path::join(&kind_path, &file.name);
                    let mut size = file.size;
                    let data_dir = format!("{}.data", file.name);
                    if entries.iter().any(|e| e.is_dir && e.name.eq_ignore_ascii_case(&data_dir)) {
                        size += client
                            .list_dir(&path::join(&kind_path, &data_dir))?
                            .iter()
                            .map(|e| e.size)
                            .sum::<u64>();
                    }
                    titles
                        .entry(title_id)
                        .or_insert_with(|| TitleContent { title_id, name: None, packages: Vec::new() })
                        .packages
                        .push(Package { path: package_path, profile: profile.name.clone(), kind, size });
                }
            }
        }
        if profile.name != SHARED_PROFILE {
            profiles.push(Profile { id: profile.name.clone() });
        }
    }

    for title in titles.values_mut() {
        title.name = names.0.get(&title.title_id).cloned().or_else(|| builtin_name(title.title_id).map(str::to_string));
        if title.name.is_none() {
            progress(&format!("reading name of {:08X}", title.title_id));
            title.name = read_name(client, title);
            if let Some(name) = &title.name {
                names.0.insert(title.title_id, name.clone());
            }
        }
    }

    let mut titles: Vec<TitleContent> = titles.into_values().collect();
    titles.sort_by_key(|t| (!t.has_game(), t.name.is_none(), t.name.clone().unwrap_or_default().to_lowercase(), t.title_id));
    Ok(Library { titles, profiles })
}

/// Reads a title's name from its smallest package header.
fn read_name(client: &mut Client, title: &TitleContent) -> Option<String> {
    let mut candidates: Vec<&Package> = title.packages.iter().filter(|p| header_size(p) <= MAX_NAME_PROBE).collect();
    candidates.sort_by_key(|p| header_size(p));
    for package in candidates.into_iter().take(3) {
        let mut buf = Vec::new();
        if client.get_file(&package.path, &mut buf).is_err() {
            continue;
        }
        let Ok(info) = stfs_package_info(&buf) else { continue };
        // Games carry the game's name as their display name; other packages (DLC, saves)
        // carry their own name there and the game's name as the title name.
        let name = if info.title_name.is_empty() || is_game(package.kind) && !info.display_name.is_empty() {
            info.display_name
        } else {
            info.title_name
        };
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Cost of downloading a package to read its header. Games on Demand headers are small
/// files (their data lives in the `.data` folder), so they are always the cheapest choice.
fn header_size(p: &Package) -> u64 {
    if p.kind == 0x7000 { 0 } else { p.size }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_names() {
        assert_eq!(content_type_name(0x7000), "Games on Demand");
        assert_eq!(content_type_name(0x1234_5678), "Other");
        assert!(is_game(0x7000) && !is_game(1));
        let t = TitleContent {
            title_id: 1,
            name: None,
            packages: vec![
                Package { path: "a".into(), profile: SHARED_PROFILE.into(), kind: 2, size: 10 },
                Package { path: "b".into(), profile: SHARED_PROFILE.into(), kind: 0x7000, size: 5 },
                Package { path: "c".into(), profile: SHARED_PROFILE.into(), kind: 2, size: 1 },
            ],
        };
        assert_eq!(t.kinds(), [(2, 2), (0x7000, 1)]);
        assert_eq!(t.size(), 16);
        assert!(t.has_game());
    }

    #[test]
    fn name_cache_round_trip() {
        let dir = std::env::temp_dir().join(format!("xbdm-test-{}", std::process::id()));
        let file = dir.join("titles");
        let mut cache = NameCache::default();
        cache.0.insert(0x454108E6, "Skate 3".into());
        cache.save(&file).unwrap();
        assert_eq!(NameCache::load(&file).0.get(&0x454108E6).map(String::as_str), Some("Skate 3"));
        fs::remove_dir_all(dir).unwrap();
        assert!(NameCache::load(Path::new("/nonexistent/titles")).0.is_empty());
    }
}
