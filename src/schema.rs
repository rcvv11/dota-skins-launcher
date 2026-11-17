//! Patches the game's item table (`scripts/items/items_game.txt`) so a picked
//! skin shows on slots that have no override file to swap.
//!
//! The table lives inside the game's `pak01_dir.vpk` as plain text, and Dota
//! loads it from whatever folder our `SearchPaths` entry mounts first — so a
//! patched copy in the override pak wins over the game's own.
//!
//! Why this is needed: a `default_item` block (what the game wears in a slot
//! nobody filled) often has no `model_player`. Anti-Mage's shoulders are like
//! that: the slot exists, but the piece is baked into the body model, so there
//! is no file for a normal override to replace. Writing `model_player` into that
//! block gives the empty slot a real model path — the game then loads the picked
//! item's model by itself. 255 of the game's 875 `default_item` blocks lack the
//! field, so this covers far more than shoulders.
//!
//! The table is handled as bytes (it is latin1, not UTF-8): a block is found by
//! its own id and spliced back unchanged except for the one value written.

use std::collections::HashMap;

use crate::extract::Skip;

/// Where the item table sits inside the game and inside our override pak.
pub const SCHEMA_PATH: &str = "scripts/items/items_game.txt";

/// One item to put on an empty hero slot.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    /// Hero display name as the catalog has it ("Anti-Mage").
    pub hero: String,
    pub slot: String,
    /// `model_player` from the catalog (a `.vmdl` path).
    pub model: String,
    /// Item name, used in the "skipped" report.
    pub name: String,
}

/// The table's text with every pick written in, and the picks it could not do.
/// Not used by Apply any more: a modified item table fails Dota's check before
/// online play. Kept for the tests and the record of what was tried.
#[allow(dead_code)]
pub fn patch(text: &[u8], picks: &[Pick], npcs: &HashMap<String, String>) -> (Vec<u8>, Vec<Skip>) {
    let mut out = text.to_vec();
    let mut skipped = Vec::new();
    // Later picks must not undo earlier ones: each block is edited once, first
    // pick wins (the profile has one item per slot and hero anyway).
    let mut done: Vec<String> = Vec::new();
    for pick in picks {
        let model = pick.model.trim();
        if model.is_empty() {
            skipped.push(Skip::new(
                &pick.name,
                crate::tr!("у предмета нет модели в каталоге (model_player пустой)", "the item has no model in the catalog (model_player is empty)"),
            ));
            continue;
        }
        let Some(npc) = npcs.get(&norm(&pick.hero)) else {
            skipped.push(Skip::new(
                &pick.name,
                crate::trf!("не удалось определить героя «{}» в схеме игры", "could not determine the hero «{}» in the game schema", pick.hero),
            ));
            continue;
        };
        let Some(block) = find_default_item(&out, npc, &pick.slot) else {
            skipped.push(Skip::new(
                &pick.name,
                crate::trf!("в схеме игры нет стандартного предмета для слота «{}»", "the game schema has no default item for slot «{}»",
                    pick.slot
                ),
            ));
            continue;
        };
        let key = format!("{npc}|{}", pick.slot);
        if done.contains(&key) {
            continue;
        }
        let model_path = if model.to_lowercase().ends_with(".vmdl") {
            model.to_string()
        } else {
            format!("{model}.vmdl")
        };
        match write_model_player(&out, block, &model_path) {
            Ok(next) => {
                out = next;
                done.push(key);
            }
            Err(e) => skipped.push(Skip::new(&pick.name, e)),
        }
    }
    (out, skipped)
}

/// The stand-in model of a hero's empty slot: the item table gives the slot's
/// default item this model (an invisible one, shipped in our pak), so Dota puts
/// a wearable there from its start, and changer.dll swaps the file for a picked
/// item without a restart.
pub fn placeholder(npc: &str, slot: &str) -> String {
    format!("models/items/dota_skins_slots/{}/{}.vmdl", npc.to_ascii_lowercase(), slot.to_ascii_lowercase())
}

/// Slots that get a stand-in: worn models (not personas, abilities, costumes…).
pub fn wearable_slot(slot: &str) -> bool {
    matches!(
        slot.to_ascii_lowercase().as_str(),
        "head" | "weapon" | "shoulder" | "back" | "arms" | "belt" | "offhand_weapon" | "armor" | "legs" | "misc"
            | "tail" | "mount" | "neck" | "body_head" | "gloves"
    )
}

/// The table with a stand-in `model_player` in each default item of `slots`
/// (`(npc, slot)`) that has no model, and the slots that got one. One pass:
/// all edits are collected, then the text is rebuilt once.
pub fn with_placeholders(text: &[u8], slots: &std::collections::HashSet<(String, String)>) -> (Vec<u8>, Vec<(String, String)>) {
    let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut placed = Vec::new();
    if let (Some(items_open), Some(items_close)) =
        (items_section(text), items_section(text).and_then(|o| block_end(text, o)))
    {
        let mut pos = items_open + 1;
        while pos < items_close {
            let Some(open) = find_from(text, b"{", pos) else { break };
            if open >= items_close {
                break;
            }
            let Some(close) = block_end(text, open) else { break };
            pos = close + 1;
            let block = &text[open..=close];
            if !has_scalar(block, b"prefab", b"default_item") {
                continue;
            }
            let Some(hero) = hero_of(block) else { continue };
            let slot = slot_of(block).unwrap_or_else(|| "weapon".into());
            let key = (hero.to_ascii_lowercase(), slot.to_ascii_lowercase());
            // the first default item of a hero and slot is the one Dota uses
            if !slots.contains(&key) || !seen.insert(key.clone()) {
                continue;
            }
            let model = placeholder(&key.0, &key.1);
            if let Some(at) = find_from(block, b"\"model_player\"", 0) {
                let after = at + b"\"model_player\"".len();
                let Some(v) = quoted_value(&block[after..]) else { continue };
                if !v.iter().all(|c| c.is_ascii_whitespace()) {
                    continue; // has a model: the file is swapped as usual
                }
                let start = open + after + (v.as_ptr() as usize - block[after..].as_ptr() as usize);
                edits.push((start, start + v.len(), model.into_bytes()));
            } else {
                let eol: &[u8] = if find_from(block, b"\r\n", 0).is_some() { b"\r\n" } else { b"\n" };
                let mut line = eol.to_vec();
                line.extend_from_slice(b"\t\t\t\"model_player\"\t\t\"");
                line.extend_from_slice(model.as_bytes());
                line.push(b'"');
                edits.push((close, close, line));
            }
            placed.push(key);
        }
    }
    edits.sort_by_key(|e| e.0);
    let mut out = Vec::with_capacity(text.len() + edits.iter().map(|e| e.2.len()).sum::<usize>());
    let mut last = 0;
    for (start, end, bytes) in edits {
        out.extend_from_slice(&text[last..start]);
        out.extend_from_slice(&bytes);
        last = end;
    }
    out.extend_from_slice(&text[last..]);
    (out, placed)
}

/// Hero display name (normalised: letters and digits only) -> `npc_dota_hero_*`
/// as the table spells it. The generator's cached heroes.json is the exact
/// source; the table itself is the fallback, matching on the name with
/// separators dropped ("Anti-Mage" == "antimage").
pub fn hero_npcs(text: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (npc, names) in table_heroes(text) {
        for n in names {
            out.entry(n).or_insert_with(|| npc.clone());
        }
    }
    if let Ok(raw) = std::fs::read_to_string(
        crate::config::app_dir().join("cache").join("heroes.json"),
    ) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(map) = v.as_object() {
                for h in map.values() {
                    let (Some(local), Some(name)) = (
                        h.get("localized_name").and_then(|s| s.as_str()),
                        h.get("name").and_then(|s| s.as_str()),
                    ) else {
                        continue;
                    };
                    out.insert(norm(local), name.to_string());
                }
            }
        }
    }
    out
}

/// A hero name reduced to letters and digits, lower-cased: the catalog says
/// "Anti-Mage" and "Shadow Fiend", the table says `antimage` and `nevermore`.
pub fn norm(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Every hero the table names, with the spellings it uses for them:
/// `npc_dota_hero_antimage` gives `npc_dota_hero_antimage` and `antimage`.
fn table_heroes(text: &[u8]) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = find_from(text, b"\"used_by_heroes\"", from) {
        from = at + 1;
        let Some(open) = find_from(text, b"{", at) else { break };
        let Some(close) = block_end(text, open) else { break };
        let body = &text[open + 1..close];
        let mut pos = 0;
        while let Some(q) = find_from(body, b"\"", pos) {
            let Some(end) = find_from(body, b"\"", q + 1) else { break };
            let name = String::from_utf8_lossy(&body[q + 1..end]).into_owned();
            pos = end + 1;
            if !name.starts_with("npc_dota_hero_") {
                continue;
            }
            let plain = norm(name.trim_start_matches("npc_dota_hero_"));
            let entry = (name.clone(), vec![norm(&name), plain]);
            if !out.iter().any(|(n, _): &(String, Vec<String>)| n == &name) {
                out.push(entry);
            }
        }
    }
    out
}

/// Byte range of the `default_item` block for `npc` in `slot`.
pub(crate) fn find_default_item(text: &[u8], npc: &str, slot: &str) -> Option<(usize, usize)> {
    let items_open = items_section(text)?;
    let items_close = block_end(text, items_open)?;
    let mut pos = items_open + 1;
    while pos < items_close {
        let Some(open) = find_from(text, b"{", pos) else { break };
        if open >= items_close {
            break;
        }
        let Some(close) = block_end(text, open) else { break };
        let block = &text[open..=close];
        pos = close + 1;
        if !has_scalar(block, b"prefab", b"default_item") {
            continue;
        }
        // No `item_slot` means the schema default, "weapon" (Anti-Mage's Glaive).
        if slot_of(block).as_deref().unwrap_or("weapon") != slot {
            continue;
        }
        if !hero_of(block).is_some_and(|h| h.eq_ignore_ascii_case(npc)) {
            continue;
        }
        return Some((open, close));
    }
    None
}

/// The `{` that opens the table's `"items"` section.
pub(crate) fn items_section(text: &[u8]) -> Option<usize> {
    let at = find_from(text, b"\"items\"", 0)?;
    find_from(text, b"{", at)
}

/// Whether the block has `"key"` followed by exactly `value`.
pub(crate) fn has_scalar(block: &[u8], key: &[u8], value: &[u8]) -> bool {
    let mut pat = Vec::with_capacity(key.len() + 2);
    pat.push(b'"');
    pat.extend_from_slice(key);
    pat.push(b'"');
    let mut from = 0;
    while let Some(at) = find_from(block, &pat, from) {
        from = at + pat.len();
        let Some(v) = quoted_value(&block[from..]) else { continue };
        if v == value {
            return true;
        }
    }
    false
}

/// `item_slot` of a block.
pub(crate) fn slot_of(block: &[u8]) -> Option<String> {
    scalar(block, b"item_slot").or_else(|| scalar(block, b"item_region"))
}

/// The first hero a block is for.
pub(crate) fn hero_of(block: &[u8]) -> Option<String> {
    let at = find_from(block, b"\"used_by_heroes\"", 0)?;
    let open = find_from(block, b"{", at)?;
    let close = block_end(block, open)?;
    let body = &block[open + 1..close];
    let q = find_from(body, b"\"", 0)?;
    let end = find_from(body, b"\"", q + 1)?;
    Some(String::from_utf8_lossy(&body[q + 1..end]).into_owned())
}

/// A `"key"  "value"` scalar of a block.
pub(crate) fn scalar(block: &[u8], key: &[u8]) -> Option<String> {
    let mut pat = Vec::with_capacity(key.len() + 2);
    pat.push(b'"');
    pat.extend_from_slice(key);
    pat.push(b'"');
    let at = find_from(block, &pat, 0)?;
    let v = quoted_value(&block[at + pat.len()..])?;
    Some(String::from_utf8_lossy(v).into_owned())
}

/// The next `"..."` value in `rest`, without its quotes.
pub(crate) fn quoted_value(rest: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i < rest.len() && (rest[i] as char).is_whitespace() {
        i += 1;
    }
    if rest.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    let end = find_from(rest, b"\"", start)?;
    Some(&rest[start..end])
}

/// Writes `model_player` into the block at `(open, close)`, replacing the value
/// it has or adding the line before the closing brace.
/// The table with `"skip_model_combine" "1"` in the visuals of each hero's
/// default item for `slot`: `(npc, slot)` pairs. Dota merges wearables into one
/// combined model unless their item says so, and a skin that must not be merged
/// (Drow's Dread Retribution crossbow: own skeleton) put on that default item's
/// file lies at the hero's feet when merged.
pub fn skip_combine(text: &[u8], targets: &[(String, String)]) -> Vec<u8> {
    if targets.is_empty() {
        return text.to_vec();
    }
    // Every item of that hero and slot: the default one and any the player may
    // wear instead (a bought bow). Spans first, then inserted from the end so
    // earlier offsets stay valid.
    let wanted: std::collections::HashSet<(String, String)> =
        targets.iter().map(|(npc, s)| (npc.to_ascii_lowercase(), s.clone())).collect();
    let wanted_slots: std::collections::HashSet<&str> = targets.iter().map(|(_, s)| s.as_str()).collect();
    let mut spans = Vec::new();
    if let (Some(items_open), Some(items_close)) =
        (items_section(text), items_section(text).and_then(|o| block_end(text, o)))
    {
        let mut pos = items_open + 1;
        while pos < items_close {
            let Some(open) = find_from(text, b"{", pos) else { break };
            if open >= items_close {
                break;
            }
            let Some(close) = block_end(text, open) else { break };
            pos = close + 1;
            let block = &text[open..=close];
            let slot = slot_of(block).unwrap_or_else(|| "weapon".into());
            let fits = wanted_slots.contains(slot.as_str())
                && hero_of(block).is_some_and(|h| wanted.contains(&(h.to_ascii_lowercase(), slot.clone())));
            if fits {
                spans.push((open, close));
            }
        }
    }
    // all insertions at once: one rebuild of the ~50 MB text, not one per item
    let mut edits: Vec<(usize, Vec<u8>)> =
        spans.into_iter().filter_map(|(open, close)| skip_flag_edit(text, open, close)).collect();
    edits.sort_by_key(|e| e.0);
    let mut out = Vec::with_capacity(text.len() + edits.iter().map(|e| e.1.len()).sum::<usize>());
    let mut last = 0;
    for (at, bytes) in edits {
        out.extend_from_slice(&text[last..at]);
        out.extend_from_slice(&bytes);
        last = at;
    }
    out.extend_from_slice(&text[last..]);
    out
}

/// `"skip_model_combine" "1"` into the visuals of the item block at `(open, close)`.
fn skip_flag_edit(out: &[u8], open: usize, close: usize) -> Option<(usize, Vec<u8>)> {
        let block = &out[open..=close];
        if find_from(block, b"\"skip_model_combine\"", 0).is_some() {
            return None;
        }
        let eol: &[u8] = if find_from(block, b"\r\n", 0).is_some() { b"\r\n" } else { b"\n" };
        let line = |indent: &[u8]| {
            let mut l = Vec::new();
            l.extend_from_slice(eol);
            l.extend_from_slice(indent);
            l.extend_from_slice(b"\"skip_model_combine\"		\"1\"");
            l
        };
        let next = match find_from(block, b"\"visuals\"", 0)
            .and_then(|at| find_from(block, b"{", at))
        {
            // right after the visuals' opening brace
            Some(v) => {
                (open + v + 1, line(b"				"))
            }
            // no visuals yet: a new block before the item's closing brace
            None => {
                let mut b = Vec::new();
                b.extend_from_slice(eol);
                b.extend_from_slice(b"			\"visuals\"");
                b.extend_from_slice(eol);
                b.extend_from_slice(b"			{");
                b.extend_from_slice(&line(b"				"));
                b.extend_from_slice(eol);
                b.extend_from_slice(b"			}");
                (close, b)
            }
        };
        Some(next)
}

fn write_model_player(text: &[u8], (open, close): (usize, usize), model: &str) -> Result<Vec<u8>, String> {
    let block = &text[open..=close];
    if let Some(at) = find_from(block, b"\"model_player\"", 0) {
        let after = at + b"\"model_player\"".len();
        let Some(v) = quoted_value(&block[after..]) else {
            return Err(crate::tr!("в схеме битое поле model_player", "broken model_player field in the schema").into());
        };
        let value_start = open + after + (v.as_ptr() as usize - block[after..].as_ptr() as usize);
        let value_end = value_start + v.len();
        let mut out = Vec::with_capacity(text.len());
        out.extend_from_slice(&text[..value_start]);
        out.extend_from_slice(model.as_bytes());
        out.extend_from_slice(&text[value_end..]);
        return Ok(out);
    }
    // No field yet: put it right before the block's closing brace, keeping the
    // file's own line ending.
    let eol: &[u8] = if find_from(block, b"\r\n", 0).is_some() { b"\r\n" } else { b"\n" };
    let mut line = Vec::new();
    line.extend_from_slice(eol);
    line.extend_from_slice(b"\t\t\t\"model_player\"\t\t\"");
    line.extend_from_slice(model.as_bytes());
    line.push(b'"');
    let mut out = Vec::with_capacity(text.len() + line.len());
    out.extend_from_slice(&text[..close]);
    out.extend_from_slice(&line);
    out.extend_from_slice(&text[close..]);
    Ok(out)
}

pub(crate) fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Index of the `}` that closes the `{` at `open`, braces counted.
pub(crate) fn block_end(text: &[u8], open: usize) -> Option<usize> {
    if text.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i64;
    for (i, &c) in text[open..].iter().enumerate() {
        match c {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#""items_game"
{
	"items"
	{
		"563"
		{
			"name"		"Antimage's Shoulders"
			"prefab"		"default_item"
			"item_slot"		"shoulder"
			"used_by_heroes"
			{
				"npc_dota_hero_antimage"		"1"
			}
		}
		"572"
		{
			"name"		"Anti-Mage's Head"
			"prefab"		"default_item"
			"item_slot"		"head"
			"model_player"		"models/heroes/antimage/antimage_head.vmdl"
			"used_by_heroes"
			{
				"npc_dota_hero_antimage"		"1"
			}
		}
		"9725"
		{
			"name"		"Oathbound Defiant Pauldrons"
			"prefab"		"wearable"
			"item_slot"		"shoulder"
			"model_player"		"models/items/antimage/avenging_anchorite_shoulder/avenging_anchorite_shoulder.vmdl"
			"used_by_heroes"
			{
				"npc_dota_hero_antimage"		"1"
			}
		}
	}
}
"#;

    fn npcs() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("antimage".to_string(), "npc_dota_hero_antimage".to_string());
        m
    }

    fn pick(slot: &str, model: &str) -> Pick {
        Pick {
            hero: "Anti-Mage".into(),
            slot: slot.into(),
            model: model.into(),
            name: "Oathbound Defiant Pauldrons".into(),
        }
    }

    #[test]
    fn adds_missing_model_player_to_the_default_item() {
        let (out, skipped) = patch(
            SAMPLE.as_bytes(),
            &[pick("shoulder", "models/items/antimage/aveng/aveng.vmdl")],
            &npcs(),
        );
        let text = String::from_utf8_lossy(&out);
        assert!(skipped.is_empty(), "{skipped:?}");
        // written into the default_item block, not into the wearable
        let def = text.find("\"Antimage's Shoulders\"").unwrap();
        let wear = text.find("\"Oathbound Defiant Pauldrons\"").unwrap();
        let written = text.find("\"model_player\"\t\t\"models/items/antimage/aveng/aveng.vmdl\"").unwrap();
        assert!(def < written && written < wear, "поле попало в стандартный предмет");
        assert!(!text.contains("aveng.vmdl\"\n\t\t\t\"prefab\"\t\t\"wearable\""), "wearable не тронут");
    }

    #[test]
    fn replaces_an_existing_model_player() {
        let (out, skipped) = patch(
            SAMPLE.as_bytes(),
            &[pick("head", "models/items/antimage/hat/hat.vmdl")],
            &npcs(),
        );
        let text = String::from_utf8_lossy(&out);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(text.contains("\"model_player\"\t\t\"models/items/antimage/hat/hat.vmdl\""));
        assert!(!text.contains("antimage_head.vmdl"), "старое значение заменено");
    }

    #[test]
    fn unknown_slot_is_reported_not_written() {
        let (out, skipped) = patch(SAMPLE.as_bytes(), &[pick("neck", "models/x/y.vmdl")], &npcs());
        assert_eq!(out, SAMPLE.as_bytes(), "схема не изменилась");
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].reason.contains("нет стандартного предмета"), "{}", skipped[0].reason);
    }

    #[test]
    fn missing_model_is_reported() {
        let (out, skipped) = patch(SAMPLE.as_bytes(), &[pick("shoulder", "")], &npcs());
        assert_eq!(out, SAMPLE.as_bytes());
        assert!(skipped[0].reason.contains("нет модели"), "{}", skipped[0].reason);
    }

    #[test]
    fn hero_names_come_from_the_table() {
        let map = table_heroes(SAMPLE.as_bytes());
        let am = map.iter().find(|(n, _)| n == "npc_dota_hero_antimage").unwrap();
        assert!(am.1.contains(&"antimage".to_string()));
    }

    #[test]
    fn names_are_matched_without_separators() {
        // The catalog writes "Anti-Mage", the table writes `antimage`.
        assert_eq!(norm("Anti-Mage"), norm("antimage"));
        assert_eq!(norm("Shadow Fiend"), "shadowfiend");
        assert_eq!(norm("npc_dota_hero_antimage"), "npcdotaheroantimage");
    }

    #[test]
    fn crlf_files_get_crlf_lines() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        let (out, _) = patch(crlf.as_bytes(), &[pick("shoulder", "models/x/y.vmdl")], &npcs());
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\r\n\t\t\t\"model_player\""), "строка добавлена с CRLF");
        assert!(!text.replace("\r\n", "").contains('\n'), "одиночных \\n не появилось");
    }

    /// Diagnostic against the real game table: run with
    /// `cargo test real_table -- --ignored --nocapture`.
    /// Confirms the method on the slot that has no file to swap at all.
    #[test]
    #[ignore]
    fn real_table_antimage_shoulders() {
        let dota_exe = std::env::var("DOTA_EXE").unwrap_or_else(|_| {
            r"C:\Program Files (x86)\Steam\steamapps\common\dota 2 beta\game\bin\win64\dota2.exe"
                .into()
        });
        let game_dir = crate::vpk::game_dir_from_exe(&dota_exe).expect("dota2.exe не найден");
        let text = crate::vpk::read_file(&game_dir, SCHEMA_PATH)
            .expect("не удалось прочитать пак")
            .expect("в паке нет items_game.txt");
        println!("таблица: {} байт", text.len());

        let npcs = hero_npcs(&text);
        println!("героев в таблице: {}", npcs.len());
        assert_eq!(
            npcs.get(&norm("Anti-Mage")).map(String::as_str),
            Some("npc_dota_hero_antimage")
        );
        // Catalog spellings that differ from the table's folder names.
        for hero in [
            "Anti-Mage",
            "Shadow Fiend",
            "Doom",
            "Drow Ranger",
            "Templar Assassin",
            "Nyx Assassin",
            "Largo",
        ] {
            println!("  {hero} -> {:?}", npcs.get(&norm(hero)));
        }

        let model = "models/items/antimage/avenging_anchorite_shoulder/avenging_anchorite_shoulder.vmdl";
        let (out, skipped) = patch(
            &text,
            &[Pick {
                hero: "Anti-Mage".into(),
                slot: "shoulder".into(),
                model: model.into(),
                name: "Oathbound Defiant Pauldrons".into(),
            }],
            &npcs,
        );
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(out.len() > text.len(), "в таблицу добавлена строка");

        // The block for the shoulders must now carry the model.
        let block = find_default_item(&out, "npc_dota_hero_antimage", "shoulder")
            .expect("плечи Anti-Mage не найдены");
        let body = String::from_utf8_lossy(&out[block.0..=block.1]);
        println!("блок плеч:\n{body}");
        assert!(
            body.contains(model),
            "model_player записан в блок стандартных плеч"
        );

        // Nothing else moved: the head default still points at its own file.
        let head = find_default_item(&out, "npc_dota_hero_antimage", "head")
            .expect("голова Anti-Mage не найдена");
        let head_body = String::from_utf8_lossy(&out[head.0..=head.1]);
        assert!(head_body.contains("antimage_head.vmdl"), "{head_body}");
    }

    #[test]
    fn braces_are_counted_not_guessed() {
        // a nested block inside the item must not end it early
        let text = SAMPLE.as_bytes();
        let open = items_section(text).unwrap();
        let close = block_end(text, open).unwrap();
        assert_eq!(text[close], b'}');
        assert!(find_from(&text[close..], b"\"9725\"", 0).is_none(), "закрытие секции верное");
    }
}

#[cfg(test)]
mod skip_combine_real {
    /// `cargo test skip_combine_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn drow_default_bow_gets_skip_model_combine() {
        let exe = r"C:\Program Files (x86)\Steam\steamapps\common\dota 2 beta\game\bin\win64\dota2.exe";
        let game = crate::vpk::game_dir_from_exe(exe).unwrap();
        let text = crate::vpk::read_file(&game, super::SCHEMA_PATH).unwrap().unwrap();
        let out = super::skip_combine(&text, &[("npc_dota_hero_drow_ranger".into(), "weapon".into())]);
        let (open, close) = super::find_default_item(&out, "npc_dota_hero_drow_ranger", "weapon").unwrap();
        let block = String::from_utf8_lossy(&out[open..=close]).to_string();
        let name = block.lines().find(|l| l.contains("\"name\"")).unwrap_or("").to_string();
        println!("{name}");
        assert!(block.contains("skip_model_combine"), "строка не добавлена");
        let count = |t: &[u8]| t.windows(20).filter(|w| *w == b"\"skip_model_combine\"").count();
        println!("помечено предметов: {}", count(&out) - count(&text));
        let v = block.find("\"visuals\"").expect("нет visuals");
        println!("{}", &block[v..]);
    }
}
