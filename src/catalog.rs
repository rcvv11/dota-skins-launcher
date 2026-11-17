use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Catalog {
    #[serde(default)]
    pub skins: Vec<Skin>,
    /// Hero display name -> base models and default wearables (3D preview).
    #[serde(default)]
    pub hero_models: std::collections::HashMap<String, HeroModel>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct HeroModel {
    pub model: String,
    #[serde(default)]
    pub persona_model: Option<String>,
    /// slot -> model of the default item in that slot
    #[serde(default)]
    pub defaults: std::collections::BTreeMap<String, String>,
}

/// Slots whose item model is the whole hero body, not a piece worn on it.
pub const BODY_SLOTS: &[&str] = &["hero_base", "costume"];

/// Whether the item replaces the hero's whole body model.
pub fn is_body_slot(slot: &str) -> bool {
    BODY_SLOTS.contains(&slot)
}

/// Persona (alternate hero look) slots: `*_persona_1` and the persona item itself.
pub fn is_persona_slot(slot: &str) -> bool {
    slot.ends_with("_persona_1") || slot == "persona_selector"
}

/// Player emblems: drawn as particles on the selection ring, not worn as a model.
pub fn is_emblem(slot: &str) -> bool {
    slot == "emblem"
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Skin {
    pub id: String,
    pub name: String,
    /// Display label, e.g. "Axe" or "Axe +2" for multi-hero items.
    #[serde(default)]
    pub hero: String,
    /// Every hero that can wear the item (display names).
    #[serde(default)]
    pub heroes: Vec<String>,
    #[serde(default)]
    pub slot: String,
    #[serde(default)]
    pub rarity: String,
    /// Path relative to the assets folder, e.g. "previews/axe_arcana.png"
    #[serde(default)]
    pub preview: Option<String>,
    /// `image_inventory` from the item schema; resolved against the game's VPK.
    #[serde(default)]
    pub icon: Option<String>,
    /// `model_player` from the schema (e.g. "models/items/pudge/x.vmdl"), for the 3D preview.
    #[serde(default)]
    pub model: Option<String>,
    /// [original, replacement] model swaps applied while the item is worn
    /// (e.g. an arcana replacing the hero body or another slot's model).
    #[serde(default)]
    pub swaps: Vec<[String; 2]>,
    /// Item definition index from the schema; higher means newer.
    #[serde(default)]
    pub index: Option<u32>,
    /// Files copied into the override folder when this skin is applied.
    #[serde(default)]
    pub files: Vec<FileMap>,
    /// Lower-cased text used by the search box (computed on load).
    #[serde(skip)]
    pub search: String,
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
pub struct FileMap {
    /// Source, relative to the assets folder (or absolute for attached files).
    pub from: String,
    /// Destination, relative to the override folder. Empty with a `.vpk`
    /// source means "every file inside that VPK".
    pub to: String,
}

impl Skin {
    /// Heroes for filtering: the explicit list, or the single `hero` label.
    pub fn hero_names(&self) -> Vec<&str> {
        if !self.heroes.is_empty() {
            self.heroes.iter().map(String::as_str).collect()
        } else if !self.hero.is_empty() && self.hero != "—" {
            vec![self.hero.as_str()]
        } else {
            Vec::new()
        }
    }

    pub fn fits_hero(&self, hero: &str) -> bool {
        self.hero_names().iter().any(|h| *h == hero)
    }

    /// Two items conflict when they share a slot and a hero. General items
    /// (terrain, courier, HUD...) conflict with any other item of the same slot.
    pub fn conflicts_with(&self, other: &Skin) -> bool {
        if self.id == other.id || self.slot.is_empty() || self.slot != other.slot {
            return false;
        }
        let mine = self.hero_names();
        let theirs = other.hero_names();
        if mine.is_empty() {
            return theirs.is_empty();
        }
        theirs.iter().any(|h| mine.contains(h))
    }

    pub fn is_general(&self) -> bool {
        self.hero_names().is_empty()
    }
}

impl Catalog {
    pub fn load(assets: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(assets.join("catalog.json"))
            .map_err(|e| format!("cannot read catalog.json: {e}"))?;
        let mut catalog: Catalog =
            serde_json::from_str(&raw).map_err(|e| format!("invalid catalog.json: {e}"))?;
        for skin in &mut catalog.skins {
            if skin.rarity.is_empty() {
                skin.rarity = "common".into();
            }
            skin.rarity.make_ascii_lowercase();
            skin.search = format!(
                "{} {} {} {} {}",
                skin.name,
                skin.hero,
                skin.heroes.join(" "),
                slot_label(&skin.slot),
                skin.id
            )
            .to_lowercase();
        }
        Ok(catalog)
    }

    /// Whether a 3D preview can be built: its own model, or a hero to wear it
    /// on (arcanas may have no model and work only through swaps).
    pub fn can_preview(&self, skin: &Skin) -> bool {
        skin.model.is_some()
            || (!skin.swaps.is_empty()
                && skin.hero_names().iter().any(|h| self.hero_models.contains_key(*h)))
    }

    /// Adds the user's attached files to the matching items.
    pub fn merge_custom(&mut self, custom: &crate::config::CustomFiles) {
        for skin in &mut self.skins {
            if let Some(files) = custom.get(&skin.id) {
                for f in files {
                    if !skin.files.contains(f) {
                        skin.files.push(f.clone());
                    }
                }
            }
        }
    }

    pub fn heroes(&self) -> Vec<String> {
        let mut list: Vec<String> = self
            .skins
            .iter()
            .flat_map(|s| s.hero_names())
            .map(str::to_string)
            .collect();
        list.sort();
        list.dedup();
        list
    }

    /// Models that make up a hero as worn: base body, then for every slot the
    /// selected item (or `forced`, which wins its slot) or the default item.
    /// None when the catalog has no model data for that hero / look.
    pub fn outfit(
        &self,
        hero: &str,
        persona: bool,
        selected: &std::collections::BTreeSet<String>,
        forced: Option<&str>,
    ) -> Option<Vec<String>> {
        let hm = self.hero_models.get(hero)?;
        let base = if persona {
            hm.persona_model.clone()?
        } else {
            hm.model.clone()
        };
        let mut slots: std::collections::BTreeMap<String, String> = hm
            .defaults
            .iter()
            .filter(|(slot, _)| is_persona_slot(slot) == persona)
            .map(|(s, m)| (s.clone(), m.clone()))
            .collect();

        let forced = forced.and_then(|id| self.skins.iter().find(|s| s.id == id));
        let worn: Vec<&Skin> = self
            .skins
            .iter()
            .filter(|s| {
                selected.contains(&s.id)
                    && s.fits_hero(hero)
                    && is_persona_slot(&s.slot) == persona
                    && forced.map_or(true, |f| f.slot != s.slot)
            })
            .chain(forced)
            .collect();

        let mut base = base;
        for skin in &worn {
            if let Some(model) = &skin.model {
                if BODY_SLOTS.contains(&skin.slot.as_str()) {
                    // a full-body item replaces the hero model itself
                    base = model.clone();
                    slots.remove(&skin.slot);
                } else {
                    slots.insert(skin.slot.clone(), model.clone());
                }
            }
        }

        let mut out = vec![base];
        for m in slots.into_values() {
            if !out.contains(&m) {
                out.push(m);
            }
        }
        // Asset modifiers: e.g. an arcana swapping the body or a refit of another piece.
        for skin in &worn {
            for [from, to] in &skin.swaps {
                for m in out.iter_mut() {
                    if m.eq_ignore_ascii_case(from) {
                        *m = to.clone();
                    }
                }
            }
        }
        out.dedup();
        Some(out)
    }

    pub fn slots(&self) -> Vec<String> {
        let mut list: Vec<String> = self
            .skins
            .iter()
            .map(|s| s.slot.clone())
            .filter(|s| !s.is_empty())
            .collect();
        list.sort_by(|a, b| slot_label(a).cmp(&slot_label(b)));
        list.dedup();
        list
    }
}

pub fn rarity_color(rarity: &str) -> egui::Color32 {
    match rarity.to_ascii_lowercase().as_str() {
        "common" => egui::Color32::from_rgb(0xb0, 0xc3, 0xd9),
        "uncommon" => egui::Color32::from_rgb(0x5e, 0x98, 0xd9),
        "rare" => egui::Color32::from_rgb(0x4b, 0x69, 0xff),
        "mythical" => egui::Color32::from_rgb(0x88, 0x47, 0xff),
        "legendary" => egui::Color32::from_rgb(0xd3, 0x2c, 0xe6),
        "immortal" => egui::Color32::from_rgb(0xe4, 0xae, 0x39),
        "arcana" => egui::Color32::from_rgb(0xad, 0xe5, 0x5c),
        "ancient" => egui::Color32::from_rgb(0xeb, 0x4b, 0x4b),
        "seasonal" => egui::Color32::from_rgb(0xff, 0xf3, 0x4f),
        _ => egui::Color32::from_rgb(0x55, 0x5f, 0x6b),
    }
}

pub fn rarity_label(rarity: &str) -> String {
    match rarity.to_ascii_lowercase().as_str() {
        "common" => "Common".into(),
        "uncommon" => "Uncommon".into(),
        "rare" => "Rare".into(),
        "mythical" => "Mythical".into(),
        "legendary" => "Legendary".into(),
        "immortal" => "Immortal".into(),
        "arcana" => "Arcana".into(),
        "ancient" => "Ancient".into(),
        "seasonal" => "Seasonal".into(),
        other => other.to_string(),
    }
}

pub fn slot_label(slot: &str) -> String {
    if let Some(base) = slot.strip_suffix("_persona_1") {
        return crate::trf!("{} (персона)", "{} (persona)", slot_label(base));
    }
    let known = match slot {
        "terrain" => crate::tr!("Ландшафт", "Terrain"),
        "weather" => crate::tr!("Погода", "Weather"),
        "emblem" => crate::tr!("Эмблема", "Emblem"),
        "courier" => crate::tr!("Курьер", "Courier"),
        "ward" => crate::tr!("Вард", "Ward"),
        "hud_skin" => crate::tr!("Интерфейс (HUD)", "Interface (HUD)"),
        "announcer" => crate::tr!("Комментатор", "Announcer"),
        "mega_kills" => crate::tr!("Мега-убийства", "Mega-kills"),
        "music" => crate::tr!("Музыка", "Music"),
        "cursor_pack" => crate::tr!("Курсоры", "Cursors"),
        "loading_screen" => crate::tr!("Загрузочный экран", "Loading screen"),
        "versus_screen" => crate::tr!("Экран противостояния", "Versus screen"),
        "heroic_statue" => crate::tr!("Статуя", "Statue"),
        "pet_effigy" => crate::tr!("Питомец", "Pet"),
        "teleport_effect" => crate::tr!("Эффект телепорта", "Teleport effect"),
        "streak_effect" => crate::tr!("Эффект серии", "Streak effect"),
        "map_effect" => crate::tr!("Эффект карты", "Map effect"),
        "multikill_banner" => crate::tr!("Баннер мультикилла", "Multikill banner"),
        "radiant_creeps" => crate::tr!("Крипы Radiant", "Radiant creeps"),
        "dire_creeps" => crate::tr!("Крипы Dire", "Dire creeps"),
        "radiant_siege_creeps" => crate::tr!("Осадные крипы Radiant", "Radiant siege creeps"),
        "dire_siege_creeps" => crate::tr!("Осадные крипы Dire", "Dire siege creeps"),
        "radiant_towers" => crate::tr!("Башни Radiant", "Radiant towers"),
        "dire_towers" => crate::tr!("Башни Dire", "Dire towers"),
        "roshan" => crate::tr!("Рошан", "Roshan"),
        "tormentor" => "Tormentor",
        "ability1" => crate::tr!("Способность 1", "Ability 1"),
        "ability2" => crate::tr!("Способность 2", "Ability 2"),
        "ability3" => crate::tr!("Способность 3", "Ability 3"),
        "ability4" => crate::tr!("Способность 4", "Ability 4"),
        "ability_ultimate" => crate::tr!("Ультимейт", "Ultimate"),
        "body_head" => crate::tr!("Тело и голова", "Body and head"),
        "costume" => crate::tr!("Костюм", "Costume"),
        "shapeshift" => crate::tr!("Превращение", "Transformation"),
        "hero_base" => crate::tr!("Модель героя", "Hero model"),
        "weapon" => crate::tr!("Оружие", "Weapon"),
        "offhand_weapon" => crate::tr!("Второе оружие", "Off-hand weapon"),
        "weapon2" => crate::tr!("Оружие 2", "Weapon 2"),
        "head" => crate::tr!("Голова", "Head"),
        "shoulder" => crate::tr!("Плечи", "Shoulders"),
        "arms" => crate::tr!("Руки", "Arms"),
        "armor" => crate::tr!("Броня", "Armor"),
        "back" => crate::tr!("Спина", "Back"),
        "belt" => crate::tr!("Пояс", "Belt"),
        "neck" => crate::tr!("Шея", "Neck"),
        "legs" => crate::tr!("Ноги", "Legs"),
        "tail" => crate::tr!("Хвост", "Tail"),
        "gloves" => crate::tr!("Перчатки", "Gloves"),
        "misc" => crate::tr!("Разное", "Misc"),
        "mount" => crate::tr!("Скакун", "Mount"),
        "taunt" => crate::tr!("Насмешка", "Taunt"),
        "summon" => crate::tr!("Призыв", "Summon"),
        "ambient_effects" => crate::tr!("Эффекты", "Effects"),
        "persona_selector" => crate::tr!("Персона", "Persona"),
        "voice" => crate::tr!("Голос", "Voice"),
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    // armor_persona_1 -> "Armor (persona 1)" style fallback
    let mut s = slot.replace('_', " ");
    if let Some(first) = s.get(..1) {
        let upper = first.to_uppercase();
        s.replace_range(..1, &upper);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skin(id: &str, heroes: &[&str], slot: &str) -> Skin {
        Skin {
            id: id.into(),
            name: id.into(),
            hero: heroes.first().copied().unwrap_or("").into(),
            heroes: heroes.iter().map(|h| h.to_string()).collect(),
            slot: slot.into(),
            ..Skin::default()
        }
    }

    #[test]
    fn same_hero_same_slot_conflicts() {
        let a = skin("a", &["Pudge"], "weapon");
        let b = skin("b", &["Pudge"], "weapon");
        assert!(a.conflicts_with(&b));
    }

    #[test]
    fn different_slot_or_hero_does_not_conflict() {
        let a = skin("a", &["Pudge"], "weapon");
        assert!(!a.conflicts_with(&skin("b", &["Pudge"], "head")));
        assert!(!a.conflicts_with(&skin("c", &["Axe"], "weapon")));
        assert!(!a.conflicts_with(&a.clone()));
    }

    #[test]
    fn multi_hero_items_conflict_on_shared_hero() {
        let a = skin("a", &["Axe", "Pudge"], "misc");
        assert!(a.conflicts_with(&skin("b", &["Pudge"], "misc")));
    }

    #[test]
    fn general_items_conflict_only_within_slot() {
        let terrain_a = skin("t1", &[], "terrain");
        let terrain_b = skin("t2", &[], "terrain");
        let courier = skin("c", &[], "courier");
        assert!(terrain_a.conflicts_with(&terrain_b));
        assert!(!terrain_a.conflicts_with(&courier));
        assert!(!terrain_a.conflicts_with(&skin("w", &["Pudge"], "terrain")));
    }

    #[test]
    fn outfit_uses_defaults_then_selected_then_forced() {
        let mut cat = Catalog::default();
        let mut hm = HeroModel {
            model: "body.vmdl".into(),
            persona_model: Some("persona_body.vmdl".into()),
            ..HeroModel::default()
        };
        hm.defaults.insert("weapon".into(), "default_hook.vmdl".into());
        hm.defaults.insert("head".into(), "default_hair.vmdl".into());
        hm.defaults.insert("weapon_persona_1".into(), "persona_hook.vmdl".into());
        cat.hero_models.insert("Pudge".into(), hm);
        let mut hook = skin("hook", &["Pudge"], "weapon");
        hook.model = Some("dragonclaw.vmdl".into());
        let mut hat = skin("hat", &["Pudge"], "head");
        hat.model = Some("hat.vmdl".into());
        cat.skins = vec![hook, hat];

        let none = std::collections::BTreeSet::new();
        let base = cat.outfit("Pudge", false, &none, None).unwrap();
        assert_eq!(base, vec!["body.vmdl", "default_hair.vmdl", "default_hook.vmdl"]);

        let worn: std::collections::BTreeSet<String> = ["hat".to_string()].into();
        let with_hat = cat.outfit("Pudge", false, &worn, Some("hook")).unwrap();
        assert_eq!(with_hat, vec!["body.vmdl", "hat.vmdl", "dragonclaw.vmdl"]);

        let persona = cat.outfit("Pudge", true, &none, None).unwrap();
        assert_eq!(persona, vec!["persona_body.vmdl", "persona_hook.vmdl"]);
        assert!(cat.outfit("Axe", false, &none, None).is_none());
    }

    #[test]
    fn outfit_applies_model_swaps_and_body_items() {
        let mut cat = Catalog::default();
        let mut hm = HeroModel {
            model: "body.vmdl".into(),
            ..HeroModel::default()
        };
        hm.defaults.insert("weapon".into(), "sword.vmdl".into());
        cat.hero_models.insert("Lc".into(), hm);
        let mut arcana = skin("arcana", &["Lc"], "head");
        arcana.model = Some("arcana_head.vmdl".into());
        arcana.swaps = vec![
            ["body.vmdl".into(), "arcana_body.vmdl".into()],
            ["sword.vmdl".into(), "arcana_sword.vmdl".into()],
        ];
        let mut costume = skin("costume", &["Lc"], "costume");
        costume.model = Some("costume_body.vmdl".into());
        cat.skins = vec![arcana, costume];

        let none = std::collections::BTreeSet::new();
        let swapped = cat.outfit("Lc", false, &none, Some("arcana")).unwrap();
        assert_eq!(swapped, vec!["arcana_body.vmdl", "arcana_head.vmdl", "arcana_sword.vmdl"]);

        let worn: std::collections::BTreeSet<String> = ["costume".to_string()].into();
        let body = cat.outfit("Lc", false, &worn, None).unwrap();
        assert_eq!(body[0], "costume_body.vmdl");
        assert!(!body.contains(&"body.vmdl".to_string()));
    }

    #[test]
    fn slot_labels() {
        assert_eq!(slot_label("weapon"), "Оружие");
        assert_eq!(slot_label("head_persona_1"), "Голова (персона)");
        assert_eq!(slot_label("custom_slot"), "Custom slot");
    }
}
