//! «SELECT» presses in Dota: changer.dll writes them to equips.txt (item definition
//! index and the row's texts); the item goes into the active preset and is applied in memory.

use crate::catalog::{Catalog, Skin};
use crate::config::Profile;

/// Presses older than this are not replayed.
const MAX_AGE_MS: u64 = 10 * 60 * 1000;

#[link(name = "kernel32")]
extern "system" {
    fn GetTickCount64() -> u64;
}

/// Puts the items into the preset, dropping whatever they conflict with
/// (same hero and slot). Returns whether the preset changed.
pub fn wear(profile: &mut Profile, catalog: &Catalog, items: &[&Skin]) -> bool {
    let before = profile.skins.clone();
    profile.skins.retain(|id| {
        let Some(worn) = catalog.skins.iter().find(|s| &s.id == id) else { return true };
        !items.iter().any(|new| new.conflicts_with(worn))
    });
    for s in items {
        if !profile.skins.contains(&s.id) {
            profile.skins.push(s.id.clone());
        }
    }
    dedupe_slots(profile, catalog);
    profile.skins != before
}

/// One item per hero and slot: of conflicting items the last added stays
/// (presets from older versions could collect several).
fn dedupe_slots(profile: &mut Profile, catalog: &Catalog) {
    let mut kept: Vec<String> = Vec::new();
    for id in profile.skins.iter().rev() {
        let clash = catalog.skins.iter().find(|s| &s.id == id).is_some_and(|me| {
            kept.iter().any(|k| catalog.skins.iter().find(|s| &s.id == k).is_some_and(|o| me.conflicts_with(o)))
        });
        if !clash {
            kept.push(id.clone());
        }
    }
    kept.reverse();
    profile.skins = kept;
}

/// A line in `launcher.log`, stamped with the same clock as equips.txt and
/// changer.dll's log: where the time between a press and the new model goes.
pub fn trace(msg: &str) {
    use std::io::Write;
    let path = crate::config::app_dir().join("launcher.log");
    // kept small: started over past 256 KB
    let append = std::fs::metadata(&path).map_or(true, |m| m.len() < 256 * 1024);
    let file = std::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(&path);
    if let Ok(mut f) = file {
        let _ = writeln!(f, "{} {msg}", now_ms());
    }
}

/// Milliseconds since boot, the clock equips.txt is stamped with.
pub fn now_ms() -> u64 {
    unsafe { GetTickCount64() }
}

pub fn equips_path() -> std::path::PathBuf {
    crate::config::app_dir().join("equips.txt")
}

/// A press reported by changer.dll in equips.txt (`tick|class|from|def|labels`).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Press {
    pub tick: u64,
    /// Item definition index read from the clicked row, 0 if unknown.
    pub def: u32,
    /// Texts of the clicked row (item or set name among them).
    pub labels: Vec<String>,
}

pub fn parse_presses(text: &str) -> Vec<Press> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('|').map(str::trim);
            let tick = f.next()?.parse().ok()?;
            let def = f.nth(2).and_then(|v| v.parse().ok()).unwrap_or(0);
            let labels = f
                .next()
                .map(|v| v.split('\t').map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            Some(Press { tick, def, labels })
        })
        .collect()
}

/// Newest press, if it is recent.
pub fn latest_equip() -> Option<Press> {
    let text = std::fs::read_to_string(equips_path()).ok()?;
    let press = parse_presses(&text).into_iter().max_by_key(|p| p.tick)?;
    (now_ms().saturating_sub(press.tick) <= MAX_AGE_MS).then_some(press)
}

/// Every recent press after `handled`, oldest first: presses made while the
/// previous one was being applied are not lost.
pub fn presses_after(handled: u64) -> Vec<Press> {
    let Ok(text) = std::fs::read_to_string(equips_path()) else { return Vec::new() };
    let now = now_ms();
    let mut out: Vec<Press> = parse_presses(&text)
        .into_iter()
        .filter(|p| p.tick > handled && now.saturating_sub(p.tick) <= MAX_AGE_MS)
        .collect();
    out.sort_by_key(|p| p.tick);
    out
}

/// Items picked by a press: the row's name (a set name gives all its items),
/// else its item definition index.
pub fn items_at<'a>(catalog: &'a Catalog, press: &Press, names: Option<&crate::sets::NameIndex>) -> Vec<&'a Skin> {
    if let Some(index) = names {
        let ids = index.resolve(&press.labels);
        let by_name: Vec<&Skin> = catalog.skins.iter().filter(|s| ids.contains(&s.id)).collect();
        if !by_name.is_empty() {
            return by_name;
        }
    }
    if press.def > 0 {
        return catalog.skins.iter().filter(|s| s.index == Some(press.def)).collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skin(id: &str, hero: &str, slot: &str, model: &str, index: u32) -> Skin {
        Skin {
            id: id.into(),
            name: id.into(),
            hero: hero.into(),
            heroes: vec![hero.into()],
            slot: slot.into(),
            model: Some(model.into()),
            index: Some(index),
            ..Default::default()
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            skins: vec![
                skin("arms", "Crystal Maiden", "arms", "models/items/cm/arms.vmdl", 10),
                skin("arms_gold", "Crystal Maiden", "arms", "models/items/cm/arms.vmdl", 20),
                skin("back", "Crystal Maiden", "back", "models/items/cm/back.vmdl", 11),
                skin("old_arms", "Crystal Maiden", "arms", "models/items/cm/old.vmdl", 5),
                skin("axe_head", "Axe", "head", "models/items/axe/head.vmdl", 7),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn presses_are_read_from_equips() {
        assert_eq!(
            parse_presses("10|0|7|5|Guilt of the Survivor	1 200
20|1
"),
            vec![
                Press { tick: 10, def: 5, labels: vec!["Guilt of the Survivor".into(), "1 200".into()] },
                Press { tick: 20, ..Default::default() }
            ]
        );
    }

    #[test]
    fn pick_by_item_index() {
        let c = catalog();
        let press = Press { tick: 1, def: 11, labels: Vec::new() };
        let ids: Vec<&str> = items_at(&c, &press, None).iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["back"]);
    }

    #[test]
    fn wearing_cleans_old_duplicates() {
        let c = catalog();
        let mut p = Profile { name: "p".into(), skins: vec!["old_arms".into(), "arms_gold".into(), "back".into()], styles: Default::default() };
        let items: Vec<&Skin> = c.skins.iter().filter(|s| s.id == "axe_head").collect();
        assert!(wear(&mut p, &c, &items));
        assert_eq!(p.skins, vec!["arms_gold", "back", "axe_head"]);
    }

    #[test]
    fn wearing_replaces_the_same_slot_only() {
        let c = catalog();
        let mut p = Profile { name: "p".into(), skins: vec!["old_arms".into(), "axe_head".into()], styles: Default::default() };
        let items: Vec<&Skin> = c.skins.iter().filter(|s| s.id == "arms" || s.id == "back").collect();
        assert!(wear(&mut p, &c, &items));
        assert_eq!(p.skins, vec!["axe_head", "arms", "back"]);
        assert!(!wear(&mut p, &c, &items));
    }
}

