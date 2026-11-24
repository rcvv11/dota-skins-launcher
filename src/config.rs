use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// Full path to dota2.exe
    pub dota_path: String,
    /// Folder where override files are written (relative to Dota install or absolute).
    pub mod_dir: String,
    /// Extra launch arguments, e.g. "-novid -console"
    pub launch_args: String,
    /// Pack preset files into pak01_dir.vpk instead of loose files.
    pub pack_vpk: bool,
    /// Apply presets in memory (changer.dll injected into dota2.exe), the
    /// default; `false` = file mode, override files in the game folder.
    pub memory_swap: bool,
    /// Index of the selected preset.
    pub active_profile: usize,
    /// Interface language: "ru", "en" or empty = as Windows (i18n.rs).
    pub language: String,
    /// Last opened hero on the Heroes tab, restored on start.
    pub ui_hero: String,
    /// Source2Viewer-CLI.exe for the 3D preview; empty = look in tools\vrf-cli.
    pub vrf_cli_path: String,
    /// Starred item ids.
    pub favorites: std::collections::BTreeSet<String>,
    /// Dota build id (appmanifest_570.acf) the catalog was last refreshed for.
    pub catalog_build: String,
    /// Item id -> model path that is actually loaded in game, used when the
    /// hero has no default file for that slot (see extract::requests).
    #[serde(default)]
    pub override_targets: std::collections::BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dota_path: String::new(),
            mod_dir: String::new(),
            launch_args: "-novid".to_string(),
            pack_vpk: true,
            memory_swap: true,
            active_profile: 0,
            language: String::new(),
            ui_hero: String::new(),
            vrf_cli_path: String::new(),
            favorites: Default::default(),
            catalog_build: String::new(),
            override_targets: Default::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq)]
pub struct Profile {
    pub name: String,
    pub skins: Vec<String>,
    /// Chosen style per item id (absent = default style 0).
    #[serde(default)]
    pub styles: std::collections::BTreeMap<String, u32>,
    /// Chosen prismatic gem per item id: the colour's name ("UnusualDungeonDoom"),
    /// picked on Dota's item page (see gems.rs). Absent = the item's own gem.
    #[serde(default)]
    pub gems: std::collections::BTreeMap<String, String>,
}

/// %APPDATA%\dota-skins
pub fn app_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("dota-skins")
}

/// Places where an `assets` folder with `catalog.json` may live, most specific first.
pub fn assets_candidates() -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        // exe dir and up to three parents (covers target\release, target, repo root)
        for _ in 0..4 {
            match dir {
                Some(d) => {
                    list.push(d.join("assets"));
                    list.push(d.join("dist").join("assets"));
                    dir = d.parent().map(|p| p.to_path_buf());
                }
                None => break,
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        list.push(cwd.join("assets"));
    }

    list.push(app_dir().join("assets"));
    let mut seen = std::collections::HashSet::new();
    list.retain(|p| seen.insert(p.clone()));
    list
}

/// The candidate whose catalog.json is the most recently written, so a stale
/// copy next to an old build never shadows a freshly generated catalog.
pub fn assets_dir() -> PathBuf {
    assets_candidates()
        .into_iter()
        .filter_map(|dir| {
            let modified = std::fs::metadata(dir.join("catalog.json"))
                .and_then(|m| m.modified())
                .ok()?;
            Some((modified, dir))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, dir)| dir)
        .unwrap_or_else(|| app_dir().join("assets"))
}

fn config_file() -> PathBuf {
    app_dir().join("config.json")
}

fn profiles_file() -> PathBuf {
    app_dir().join("profiles.json")
}

/// Last write time of profiles.json (in-game picks edit presets too).
pub fn profiles_modified() -> Option<std::time::SystemTime> {
    std::fs::metadata(profiles_file()).and_then(|m| m.modified()).ok()
}

fn ensure_app_dir() -> Result<(), String> {
    std::fs::create_dir_all(app_dir()).map_err(|e| format!("cannot create app dir: {e}"))
}

pub fn load_config() -> Config {
    let mut cfg: Config = std::fs::read_to_string(config_file())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();

    if cfg.dota_path.is_empty() {
        if let Some(found) = detect_dota() {
            cfg.dota_path = found.to_string_lossy().to_string();
        }
    }
    cfg
}

pub fn save_config(cfg: &Config) -> Result<(), String> {
    ensure_app_dir()?;
    let raw = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(config_file(), raw).map_err(|e| format!("cannot save config: {e}"))
}

pub fn load_profiles() -> Vec<Profile> {
    std::fs::read_to_string(profiles_file())
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<Profile>>(&raw).ok())
        .unwrap_or_default()
}

pub fn save_profiles(profiles: &[Profile]) -> Result<(), String> {
    ensure_app_dir()?;
    let raw = serde_json::to_string_pretty(profiles).map_err(|e| e.to_string())?;
    std::fs::write(profiles_file(), raw).map_err(|e| format!("cannot save profiles: {e}"))
}

// ---------------------------------------------------------------- own files

/// Item id -> files the user attached (copied into %APPDATA%\dota-skins\custom).
pub type CustomFiles = std::collections::BTreeMap<String, Vec<crate::catalog::FileMap>>;

fn custom_file() -> PathBuf {
    app_dir().join("custom_files.json")
}

/// Folder that holds the user's copies for one item.
pub fn custom_dir(item_id: &str) -> PathBuf {
    let safe: String = item_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    app_dir().join("custom").join(safe)
}

pub fn load_custom_files() -> CustomFiles {
    std::fs::read_to_string(custom_file())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save_custom_files(files: &CustomFiles) -> Result<(), String> {
    ensure_app_dir()?;
    let raw = serde_json::to_string_pretty(files).map_err(|e| e.to_string())?;
    std::fs::write(custom_file(), raw).map_err(|e| format!("cannot save custom files: {e}"))
}

/// Steam build id of the installed Dota 2 (from appmanifest_570.acf).
pub fn dota_build_id(dota_exe: &str) -> Option<String> {
    let steamapps = Path::new(dota_exe)
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("steamapps")))?;
    let text = std::fs::read_to_string(steamapps.join("appmanifest_570.acf")).ok()?;
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("\"buildid\""))
        .and_then(last_quoted)
        .map(str::to_string)
}

/// Best-effort lookup of dota2.exe across the usual Steam library layouts.
pub fn detect_dota() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Some(steam) = crate::dota::steam_exe() {
        if let Some(root) = steam.parent() {
            roots.push(root.to_path_buf());
        }
    }
    for base in [
        r"C:\Program Files (x86)\Steam",
        r"C:\Program Files\Steam",
        r"D:\Steam",
        r"E:\Steam",
        r"F:\Steam",
    ] {
        roots.push(PathBuf::from(base));
    }

    if let Ok(steam_path) = std::env::var("STEAM_PATH") {
        roots.push(PathBuf::from(steam_path));
    }

    // Additional library folders declared in libraryfolders.vdf
    let mut extra: Vec<PathBuf> = Vec::new();
    for root in roots.clone() {
        let vdf = root.join("steamapps").join("libraryfolders.vdf");
        if let Ok(text) = std::fs::read_to_string(&vdf) {
            for line in text.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with("\"path\"") {
                    continue;
                }
                if let Some(value) = last_quoted(trimmed) {
                    extra.push(PathBuf::from(value.replace("\\\\", "\\")));
                }
            }
        }
    }
    roots.extend(extra);

    for root in roots {
        let exe = root
            .join("steamapps")
            .join("common")
            .join("dota 2 beta")
            .join("game")
            .join("bin")
            .join("win64")
            .join("dota2.exe");
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

fn last_quoted(line: &str) -> Option<&str> {
    let mut end = line.len();
    while end > 1 {
        if let Some(start) = line[..end].rfind('"') {
            let inner_end = start;
            if let Some(inner_start) = line[..inner_end].rfind('"') {
                let value = &line[inner_start + 1..inner_end];
                if !value.is_empty() {
                    return Some(value);
                }
            }
            end = inner_end;
        } else {
            return None;
        }
    }
    None
}

/// Resolve a possibly-relative override folder against the Dota install root.
pub fn resolve_mod_dir(cfg: &Config) -> PathBuf {
    let raw = cfg.mod_dir.trim();
    if raw.is_empty() {
        // Inside game\ — the only place the game can mount (see mount.rs).
        return crate::mount::default_mod_dir(&cfg.dota_path).unwrap_or_default();
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        crate::mount::game_root(&cfg.dota_path)
            .map(|root| root.join("game").join(path))
            .unwrap_or_else(|| PathBuf::from(raw))
    }
}
