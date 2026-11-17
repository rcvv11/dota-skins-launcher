//! The game's item table, parsed once per game version and kept in memory: a
//! pick in Dota is applied in a fraction of a second instead of re-reading and
//! re-indexing the ~50 MB table every time.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::visuals::Table;

pub struct GameData {
    pub table: &'static [u8],
    pub index: Table<'static>,
    /// Hero display name (normalized) -> `npc_dota_hero_*`.
    pub npcs: HashMap<String, String>,
    /// Unit name -> its model (heroes, creeps, buildings…).
    pub units: HashMap<String, String>,
}

/// `(game version key, data)`. A new game version replaces the entry; the old
/// one stays allocated (a game update is rare and the launcher restarts often).
static CACHE: Mutex<Option<(String, &'static GameData)>> = Mutex::new(None);

/// Size and time of the game's pak: changes with every game update.
fn version_key(game_dir: &Path) -> String {
    std::fs::metadata(game_dir.join("pak01_dir.vpk"))
        .map(|m| format!("{}|{:?}", m.len(), m.modified().ok()))
        .unwrap_or_default()
}

/// The parsed table of the installed game (read on first use).
pub fn load(game_dir: &Path) -> Result<&'static GameData, String> {
    let key = version_key(game_dir);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, data)) = cache.as_ref() {
        if !key.is_empty() && *k == key {
            return Ok(data);
        }
    }
    let table = crate::vpk::read_file(game_dir, crate::schema::SCHEMA_PATH)?.ok_or_else(|| {
        crate::trf!("в pak01_dir.vpk игры нет {}", "the game's pak01_dir.vpk has no {}", crate::schema::SCHEMA_PATH)
    })?;
    let units = crate::visuals::all_unit_models(game_dir)?;
    let table: &'static [u8] = Box::leak(table.into_boxed_slice());
    let data: &'static GameData = Box::leak(Box::new(GameData {
        table,
        index: Table::new(table),
        npcs: crate::schema::hero_npcs(table),
        units,
    }));
    *cache = Some((key, data));
    Ok(data)
}

/// Reads the table in the background, so the first pick is fast too.
pub fn warm_up(dota_exe: String) {
    std::thread::spawn(move || {
        if let Some(dir) = crate::vpk::game_dir_from_exe(&dota_exe) {
            let _ = load(&dir);
        }
    });
}
