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
    /// `(npc_dota_hero_*, slot)` -> the slot's caption in the loadout, in English
    /// and Russian (the hero script's `ItemSlots` > `SlotText` token).
    pub slot_texts: HashMap<(String, String), Vec<String>>,
    /// npc_dota_hero_* -> its slots in the order of the hero page's "equipped" row
    /// (the script's order, without persona slots and slots not shown in the loadout).
    pub slot_order: HashMap<String, Vec<String>>,
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
    let (slot_texts, slot_order) = slot_texts(game_dir);
    let table: &'static [u8] = Box::leak(table.into_boxed_slice());
    let data: &'static GameData = Box::leak(Box::new(GameData {
        table,
        index: Table::new(table),
        npcs: crate::schema::hero_npcs(table),
        units,
        slot_texts,
        slot_order,
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

/// The loadout captions of every hero's slots, from the hero scripts and the
/// game's English and Russian texts.
fn slot_texts(game_dir: &Path) -> (HashMap<(String, String), Vec<String>>, HashMap<String, Vec<String>>) {
    let mut tokens: HashMap<String, Vec<String>> = HashMap::new();
    for f in ["resource/localization/dota_english.txt", "resource/localization/dota_russian.txt"] {
        if let Ok(Some(text)) = crate::vpk::read_file(game_dir, f) {
            crate::sets::parse_tokens(&text, &mut tokens);
        }
    }
    let scripts = crate::vpk::read_matching(game_dir, |p| {
        p.strip_prefix("scripts/npc/heroes/").is_some_and(|f| f.ends_with(".txt")) || p == "scripts/npc/npc_heroes.txt"
    })
    .unwrap_or_default();
    let mut out = HashMap::new();
    let mut order: HashMap<String, Vec<String>> = HashMap::new();
    for bytes in scripts.values() {
        // "npc_dota_hero_x" { ... "ItemSlots" { "0" { "SlotName" "weapon" "SlotText" "#LoadoutSlot_Weapon" } } }
        let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let mut path: Vec<String> = Vec::new();
        let mut key: Option<&[u8]> = None;
        let (mut slot, mut text, mut shown) = (String::new(), String::new(), true);
        for tok in crate::sets::tokenize(bytes) {
            match tok {
                b"{" => {
                    path.push(key.take().map(s).unwrap_or_default());
                    // a slot block opens (<hero> > ItemSlots > <n>); nested blocks
                    // (GeneratesUnits of a summon slot) leave the slot's fields alone
                    if path.len() >= 3 && path[path.len() - 2].eq_ignore_ascii_case("ItemSlots") {
                        slot.clear();
                        text.clear();
                        shown = true;
                    }
                }
                b"}" => {
                    // closing a slot block: <hero> > ItemSlots > <n>
                    let slot_block = path.len() >= 3 && path[path.len() - 2].eq_ignore_ascii_case("ItemSlots");
                    if slot_block && path.len() >= 4 && !slot.is_empty() {
                        let hero = path[path.len() - 3].to_ascii_lowercase();
                        let texts = tokens.get(&text.trim_start_matches('#').to_lowercase()).cloned().unwrap_or_default();
                        let name = slot.to_ascii_lowercase();
                        if shown && !name.contains("persona") {
                            order.entry(hero.clone()).or_default().push(name.clone());
                        }
                        out.entry((hero, name)).or_insert(texts);
                    }
                    path.pop();
                    key = None;
                    if slot_block {
                        slot.clear();
                        text.clear();
                        shown = true;
                    }
                }
                _ => match key.take() {
                    None => key = Some(tok),
                    // only the slot block's own keys
                    Some(_) if !(path.len() >= 3 && path[path.len() - 2].eq_ignore_ascii_case("ItemSlots")) => {}
                    Some(k) => {
                        if k.eq_ignore_ascii_case(b"SlotName") {
                            slot = s(tok);
                        } else if k.eq_ignore_ascii_case(b"SlotText") {
                            text = s(tok);
                        } else if k.eq_ignore_ascii_case(b"DisplayInLoadout") {
                            shown = s(tok).trim() != "0";
                        }
                    }
                },
            }
        }
    }
    (out, order)
}
