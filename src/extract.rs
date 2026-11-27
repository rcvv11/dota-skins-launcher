//! Turns selected catalog items into actual override files.
//!
//! The generated catalog carries no file paths (the schema doesn't list them),
//! so a preset applied from it would install nothing. What makes the game show
//! a skin without touching the process: replace the *default* model file of the
//! hero slot with the selected item's compiled model. Both files come straight
//! from the game's own `pak01_dir.vpk` (read-only). Material and texture
//! references inside the model then resolve from the game files as usual, so
//! copying just the model is enough.
//!
//! Limitations of the file-based approach (accepted by design):
//! - an override is visible on the *default* look: if the account owns a real
//!   item in that slot, the game loads that item's files instead;
//! - items without models (taunts, HUD, terrain, music, cursors...) cannot be
//!   swapped this way and are reported as skipped.

use crate::catalog::{self, Catalog, Skin};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One override: copy `src` (inside the game VPK) over `dst` (inside the
/// override pak). Both are compiled paths ending in `_c`.
#[derive(Clone, Debug)]
pub struct Req {
    /// Item display name, used in "skipped" reports.
    pub id: String,
    pub src: String,
    pub dst: String,
}

/// Compiled file name for a dev path: `.../x.vmdl` -> `.../x.vmdl_c`.
pub fn compiled_path(model: &str) -> String {
    let m = model.trim().trim_start_matches('/').to_lowercase();
    if let Some(base) = m.strip_suffix(".vmdl") {
        format!("{base}.vmdl_c")
    } else if m.ends_with("_c") {
        m
    } else {
        format!("{m}_c")
    }
}

/// An item that will not show up in game, with a human-readable reason.
#[derive(Clone, Debug, PartialEq)]
pub struct Skip {
    pub name: String,
    pub reason: String,
}

impl Skip {
    pub fn new(name: &str, reason: impl Into<String>) -> Self {
        Self { name: name.to_string(), reason: reason.into() }
    }
}

/// Why an item without a model cannot be swapped by files.
fn modelless_reason(slot: &str) -> String {
    match slot {
        "taunt" => crate::tr!("насмешка — это анимация, файлом её не подменить", "a taunt is an animation, a file cannot replace it").into(),
        "ambient_effects" | "teleport_effect" | "streak_effect" | "map_effect" => {
            crate::tr!("эффект без модели, файлом не подменяется", "an effect without a model, a file cannot replace it").into()
        }
        "" => crate::tr!("у предмета нет модели", "the item has no model").into(),
        other => crate::trf!("у предмета нет модели (слот «{}»)", "the item has no model (slot «{}»)",
            catalog::slot_label(other)
        ),
    }
}

/// The model a hidden piece becomes (the game's own invisible box).
const HIDDEN_MODEL: &str = "models/development/invisiblebox.vmdl";

/// A persona in the preset (Anti-Mage's Wei): the body becomes the persona's
/// (its `entity_model`, visuals.rs), and the classic pieces would stay on it. Each
/// classic default piece becomes one of the persona's: the persona's slot of the
/// same name (the preset's item for `<slot>_persona_1`, or the persona's default);
/// the persona's pieces left over (slots no classic one has, Legion Commander's
/// automaton's "misc") go on the classic pieces left over, and the classic pieces
/// still left are hidden. An own item the account wears in a classic slot is loaded
/// in place of its default: it becomes the same. These come first, so classic items
/// do not take those files. (They are for a match: in the menu the game shows the
/// persona as worn and changer.dll leaves them out.)
fn persona_defaults(skins: &[&Skin], catalog: &Catalog, owned: &HashSet<u32>) -> Vec<Req> {
    let mut out = Vec::new();
    for persona in skins.iter().filter(|s| s.slot == "persona_selector") {
        for hero in persona.hero_names() {
            let Some(hm) = catalog.hero_models.get(hero) else { continue };
            let piece = |slot: &str, default: &str| -> String {
                skins
                    .iter()
                    .find(|s| s.slot == slot && s.fits_hero(hero))
                    .and_then(|s| s.model.clone())
                    .unwrap_or_else(|| default.to_string())
            };
            let mut classic: Vec<(&String, &String)> =
                hm.defaults.iter().filter(|(slot, _)| !catalog::is_persona_slot(slot)).collect();
            let mut pieces: Vec<(&String, String)> = Vec::new();
            let mut leftover: Vec<String> = Vec::new();
            for (slot, default) in hm.defaults.iter().filter(|(slot, _)| slot.ends_with("_persona_1")) {
                let src = piece(slot, default);
                let base = slot.strip_suffix("_persona_1").unwrap_or(slot);
                match classic.iter().position(|(c, _)| c.as_str() == base) {
                    Some(i) => pieces.push((classic.remove(i).0, src)),
                    None => leftover.push(src),
                }
            }
            for (slot, _) in classic {
                let src = if leftover.is_empty() { HIDDEN_MODEL.to_string() } else { leftover.remove(0) };
                pieces.push((slot, src));
            }
            for (slot, src) in pieces {
                let src = compiled_path(&src);
                let mut dsts: Vec<String> = hm.defaults.get(slot).map(|d| compiled_path(d)).into_iter().collect();
                // the own items worn in that slot instead of its default
                dsts.extend(
                    catalog
                        .skins
                        .iter()
                        .filter(|o| &o.slot == slot && o.fits_hero(hero) && o.index.is_some_and(|i| owned.contains(&i)))
                        .filter_map(|o| o.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(compiled_path)),
                );
                for dst in dsts {
                    if dst != src {
                        out.push(Req { id: persona.name.clone(), src: src.clone(), dst });
                    }
                }
            }
        }
    }
    out
}

/// Override plan for the selected skins plus the skins that have nothing to
/// override (no model, or no default file for their slot). Items with their
/// own attached files are installed from those files and never reported.
///
/// `targets` maps an item id to a donor model path chosen by the user (the item
/// actually owned in that slot). That is the only way to change slots the game
/// has no default file for — e.g. Anti-Mage's shoulders, baked into the body
/// model: the item's model is written over the donor's file instead.
/// `owned`: the definition indexes of the items the account wears (owned.rs).
pub fn plan(
    skins: &[&Skin],
    catalog: &Catalog,
    targets: &std::collections::BTreeMap<String, String>,
    owned: &HashSet<u32>,
) -> (Vec<Req>, Vec<crate::schema::Pick>, Vec<Skip>) {
    let mut reqs = persona_defaults(skins, catalog, owned);
    let mut picks = Vec::new();
    let mut unsupported = Vec::new();
    for s in skins {
        let has_files = !s.files.is_empty();
        // Map items and emblems are applied from the item table (`visuals.rs`, `emblem.rs`).
        if s.is_general() && !has_files {
            continue;
        }
        let Some(model) = s.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) else {
            // Emblems are handled by `emblem.rs` (item table + attack projectile).
            if !has_files && !catalog::is_emblem(&s.slot) {
                unsupported.push(Skip::new(&s.name, modelless_reason(&s.slot)));
            }
            continue;
        };
        let mut any = false;
        let mut empty_slot = false;
        for hero in s.hero_names() {
            let Some(hm) = catalog.hero_models.get(hero) else { continue };
            let slot = s.slot.as_str();
            let base = if catalog::is_body_slot(slot) {
                if catalog::is_persona_slot(slot) {
                    hm.persona_model.as_deref()
                } else {
                    Some(hm.model.as_str())
                }
            } else {
                hm.defaults.get(slot).map(String::as_str)
            };
            let Some(dst) = base else {
                empty_slot = true;
                continue;
            };
            reqs.push(Req {
                id: s.name.clone(),
                src: compiled_path(model),
                dst: compiled_path(dst),
            });
            any = true;
        }
        if any || has_files {
            continue;
        }
        // A donor file the user picked wins over everything else.
        if let Some(donor) = targets.get(&s.id).map(|t| t.trim()).filter(|t| !t.is_empty()) {
            reqs.push(Req {
                id: s.name.clone(),
                src: compiled_path(model),
                dst: compiled_path(donor),
            });
            continue;
        }
        // The hero's default item has no model for this slot: patch the table.
        if empty_slot && !s.hero_names().is_empty() {
            for hero in s.hero_names() {
                picks.push(crate::schema::Pick {
                    hero: hero.to_string(),
                    slot: s.slot.clone(),
                    model: model.to_string(),
                    name: s.name.clone(),
                });
            }
            continue;
        }
        let reason = if empty_slot {
            crate::tr!("у героя по умолчанию пустой слот, заменять нечего", "the hero's default slot is empty, nothing to replace").to_string()
        } else if s.hero_names().is_empty() {
            crate::tr!("предмет не привязан к герою", "the item is not tied to a hero").to_string()
        } else {
            crate::tr!("нет данных о стандартных моделях героя в каталоге", "no data about the hero's default models in the catalog").to_string()
        };
        unsupported.push(Skip::new(&s.name, reason));
    }
    reqs.extend(same_slot_items(skins, catalog, owned));
    (reqs, picks, unsupported)
}

/// The player may wear an own item in the slot instead of the default one (a
/// bought bow on Drow): the game then loads that item's model, not the default
/// file. So the picked skin also takes the files of the items of that hero and
/// slot the account wears (`owned`). Only those: an item swapped for the picked
/// one showed the picked one on its wardrobe card and its try-on too (every
/// Anti-Mage weapon as Blade of the Survivor). These come after the default-file
/// swaps.
fn same_slot_items(skins: &[&Skin], catalog: &Catalog, owned: &HashSet<u32>) -> Vec<Req> {
    let chosen_models: std::collections::HashSet<String> = skins
        .iter()
        .filter_map(|s| s.model.as_deref())
        .map(compiled_path)
        .collect();
    let mut out = Vec::new();
    for s in skins {
        if s.is_general() || catalog::is_body_slot(&s.slot) || !s.files.is_empty() {
            continue;
        }
        let Some(model) = s.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) else { continue };
        let src = compiled_path(model);
        for other in &catalog.skins {
            if other.id == s.id || other.slot != s.slot || !other.hero_names().iter().any(|h| s.fits_hero(h)) {
                continue;
            }
            if !other.index.is_some_and(|i| owned.contains(&i)) {
                continue;
            }
            let Some(m) = other.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) else { continue };
            let dst = compiled_path(m);
            if dst == src || chosen_models.contains(&dst) {
                continue;
            }
            out.push(Req { id: s.name.clone(), src: src.clone(), dst });
        }
    }
    out
}

/// Whether `path` looks like a model path usable as an override target.
pub fn is_model_path(path: &str) -> bool {
    let p = path.trim().to_lowercase();
    (p.ends_with(".vmdl_c") || p.ends_with(".vmdl")) && p.contains('/')
}

/// Reads every requested file from the game VPK into the cache folder and
/// returns `(files, skipped)`. `files` are ready to append to `Install::files`.
/// A missing model is reported, a broken VPK is an error.
pub fn extract(
    reqs: &[Req],
    game_dir: &Path,
    cache_dir: &Path,
) -> Result<(Vec<(String, PathBuf)>, Vec<Skip>), String> {
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| crate::trf!("не удалось создать кэш {}: {e}", "could not create the cache {}: {e}", cache_dir.display()))?;
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = HashSet::new();
    for r in reqs {
        if !seen.insert(r.dst.clone()) {
            continue;
        }
        match crate::vpk::read_file(game_dir, &r.src) {
            Ok(Some(bytes)) => {
                // destination path is unique per override, so it is a safe key
                let name = r.dst.replace(['/', '\\'], "_");
                let path = cache_dir.join(&name);
                std::fs::write(&path, &bytes)
                    .map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", path.display()))?;
                files.push((r.dst.clone(), path));
            }
            Ok(None) => skipped.push(Skip::new(&r.id, crate::trf!("{} нет в pak01_dir.vpk", "{} is not in pak01_dir.vpk", r.src))),
            Err(e) => return Err(e),
        }
    }
    Ok((files, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::HeroModel;

    fn catalog(hero: &str, model: &str, defaults: &[(&str, &str)]) -> Catalog {
        let mut c = Catalog::default();
        c.hero_models.insert(
            hero.into(),
            HeroModel {
                model: model.into(),
                persona_model: Some(format!("{hero}_persona.vmdl")),
                defaults: defaults
                    .iter()
                    .map(|(s, m)| (s.to_string(), m.to_string()))
                    .collect(),
            },
        );
        c
    }

    fn skin(hero: &str, slot: &str, model: &str) -> Skin {
        Skin {
            id: "id".into(),
            name: "name".into(),
            hero: hero.into(),
            heroes: vec![hero.into()],
            slot: slot.into(),
            model: Some(model.into()),
            ..Default::default()
        }
    }

    #[test]
    fn compiled_suffix() {
        assert_eq!(compiled_path("models/x/y.vmdl"), "models/x/y.vmdl_c");
        assert_eq!(compiled_path("models/x/y.vmdl_c"), "models/x/y.vmdl_c");
        assert_eq!(compiled_path("/models/x/y"), "models/x/y_c");
    }

    #[test]
    fn wearable_uses_slot_default() {
        let c = catalog("Axe", "models/heroes/axe/axe.vmdl", &[("weapon", "models/heroes/axe/axe_weapon.vmdl")]);
        let s = skin("Axe", "weapon", "models/items/axe/arc/arc.vmdl");
        let (reqs, _, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert!(uns.is_empty());
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].src, "models/items/axe/arc/arc.vmdl_c");
        assert_eq!(reqs[0].dst, "models/heroes/axe/axe_weapon.vmdl_c");
    }

    #[test]
    fn body_slot_uses_hero_model() {
        let c = catalog("Axe", "models/heroes/axe/axe.vmdl", &[]);
        let s = skin("Axe", "hero_base", "models/items/axe/arc/arc.vmdl");
        let (reqs, _, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert!(uns.is_empty());
        assert_eq!(reqs[0].dst, "models/heroes/axe/axe.vmdl_c");
    }

    #[test]
    fn missing_default_goes_to_the_item_table() {
        // No model file for the slot, but the hero has a default item block:
        // the table gets a model_player instead of the item being dropped.
        let c = catalog("Axe", "models/heroes/axe/axe.vmdl", &[]);
        let s = skin("Axe", "weapon", "models/items/axe/arc/arc.vmdl");
        let (reqs, picks, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert!(reqs.is_empty());
        assert!(uns.is_empty(), "предмет не пропускается: {uns:?}");
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].hero, "Axe");
        assert_eq!(picks[0].slot, "weapon");
        assert_eq!(picks[0].model, "models/items/axe/arc/arc.vmdl");
    }

    #[test]
    fn modelless_item_is_unsupported() {
        let c = catalog("Axe", "models/heroes/axe/axe.vmdl", &[("taunt", "models/x.vmdl")]);
        let s = skin("Axe", "taunt", "");
        let (_, _, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert_eq!(uns.len(), 1);
        assert!(uns[0].reason.contains("насмешка"), "{}", uns[0].reason);
    }

    #[test]
    fn item_with_attached_files_is_not_reported() {
        let c = catalog("Axe", "models/heroes/axe/axe.vmdl", &[]);
        let mut s = skin("Axe", "taunt", "");
        s.files = vec![crate::catalog::FileMap { from: "a.vpk".into(), to: String::new() }];
        let (reqs, _, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert!(reqs.is_empty());
        assert!(uns.is_empty());
    }

    #[test]
    fn donor_target_fills_missing_default() {
        // Anti-Mage shoulders: no default file for the slot, so the user points
        // at the model they actually wear and the item model goes over it.
        let c = catalog("Anti-Mage", "models/heroes/antimage/antimage.vmdl", &[]);
        let s = skin(
            "Anti-Mage",
            "shoulder",
            "models/items/antimage/avenging_anchorite_shoulder/avenging_anchorite_shoulder.vmdl",
        );
        let mut targets = std::collections::BTreeMap::new();
        targets.insert(
            "id".to_string(),
            "models/items/antimage/worn_shoulder/worn_shoulder.vmdl".to_string(),
        );
        let (reqs, _, uns) = plan(&[&s], &c, &targets, &Default::default());
        assert!(uns.is_empty(), "с выбранным донором предмет не пропускается");
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0].dst,
            "models/items/antimage/worn_shoulder/worn_shoulder.vmdl_c"
        );
        assert_eq!(
            reqs[0].src,
            "models/items/antimage/avenging_anchorite_shoulder/avenging_anchorite_shoulder.vmdl_c"
        );
    }

    #[test]
    fn shoulders_without_donor_use_the_item_table() {
        // Anti-Mage's shoulders have no default model file, so the table gets a
        // model_player and no donor is needed any more.
        let c = catalog("Anti-Mage", "models/heroes/antimage/antimage.vmdl", &[]);
        let s = skin("Anti-Mage", "shoulder", "models/items/x/y.vmdl");
        let (reqs, picks, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert!(reqs.is_empty());
        assert!(uns.is_empty(), "{uns:?}");
        assert_eq!(picks.len(), 1);
        assert_eq!(picks[0].slot, "shoulder");
    }

    #[test]
    fn unrelated_hero_is_reported() {
        // A hero the catalog has no base model for cannot be patched either.
        let c = Catalog::default();
        let s = skin("Nobody", "weapon", "models/items/x/y.vmdl");
        let (reqs, picks, uns) = plan(&[&s], &c, &Default::default(), &Default::default());
        assert!(reqs.is_empty());
        assert!(picks.is_empty());
        assert_eq!(uns.len(), 1);
        assert!(uns[0].reason.contains("нет данных"), "{}", uns[0].reason);
    }


    #[test]
    fn model_path_check() {
        assert!(is_model_path("models/items/x/y.vmdl"));
        assert!(is_model_path("models/items/x/y.vmdl_c"));
        assert!(!is_model_path("y.vmdl"));
        assert!(!is_model_path("models/x/y.vmat"));
    }
}
