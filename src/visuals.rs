//! Item visuals from the game's item table, turned into plain file overrides.
//!
//! An item's `visuals` say things like "unit X gets model Y", "particle A is
//! replaced by particle B" or "the map shader is material M". Copying Y over
//! the unit's default model (from `scripts/npc/npc_units.txt`), B over A, or M
//! over the default shader gives the same picture without touching the game
//! process. Map items (creeps, towers, couriers, wards, shaders…) are applied
//! entirely this way; hero items get their particle swaps (ability effects of
//! immortals and arcanas) on top of their model.

use std::collections::HashMap;

use crate::catalog::Skin;
use crate::extract::{compiled_path, Req, Skip};
use crate::schema::{block_end, find_from, items_section, scalar};

/// The map's post-process material, used when no shader item is equipped.
const DEFAULT_MAP_SHADER: &str = "materials/dev/deferred_post_process.vmat";
/// The item whose courier models are the ones every player has by default.
const DEFAULT_COURIER: &str = "Default Courier";

/// An item looked up in the item table: by definition index, or by name when
/// the catalog has no index.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ItemRef {
    pub name: String,
    pub index: Option<u32>,
    /// Chosen style (`visuals` > `styles`), 0 = the item's default look.
    pub style: u32,
}

impl ItemRef {
    pub fn of(skin: &Skin) -> Self {
        Self { name: skin.name.clone(), index: skin.index, style: 0 }
    }
}

/// Sets each item's chosen style from the preset (`Profile::styles`).
pub fn with_styles(general: &mut [ItemRef], heroes: &mut [HeroItem], styles: &std::collections::BTreeMap<String, u32>) {
    for g in general.iter_mut() {
        g.style = styles.get(&g.name).copied().unwrap_or(0);
    }
    for h in heroes.iter_mut() {
        h.item.style = styles.get(&h.item.name).copied().unwrap_or(0);
    }
}

/// One `asset_modifier` of an item.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modifier {
    pub kind: String,
    pub asset: Option<String>,
    pub modifier: Option<String>,
    /// Style the modifier belongs to (`"style"`), None = every style.
    pub style: Option<u32>,
}

/// Items of the preset that belong to no hero (map items, emblem excluded):
/// they are applied entirely from their table visuals.
pub fn general_items(chosen: &[&Skin]) -> Vec<ItemRef> {
    chosen
        .iter()
        .filter(|s| s.is_general() && s.files.is_empty() && !crate::catalog::is_emblem(&s.slot))
        .map(|s| ItemRef::of(s))
        .collect()
}

/// A hero item of the preset, with what is needed to find its slot's default item.
#[derive(Clone, Debug, PartialEq)]
pub struct HeroItem {
    pub item: ItemRef,
    /// Display names, as the catalog has them.
    pub heroes: Vec<String>,
    pub slot: String,
    /// `model_player` from the catalog (for its material groups).
    pub model: Option<String>,
}

/// Hero items of the preset: their effects are applied on top of the model.
pub fn hero_items(chosen: &[&Skin]) -> Vec<HeroItem> {
    chosen
        .iter()
        .filter(|s| !s.is_general() && s.files.is_empty())
        .map(|s| HeroItem {
            item: ItemRef::of(s),
            heroes: s.hero_names().iter().map(|h| h.to_string()).collect(),
            slot: s.slot.clone(),
            model: s.model.clone(),
        })
        .collect()
}

/// The quoted key right before the `{` at `open`.
fn key_before(text: &[u8], open: usize) -> Option<String> {
    let head = &text[..open];
    let end = head.iter().rposition(|&c| c == b'"')?;
    let start = head[..end].iter().rposition(|&c| c == b'"')?;
    Some(String::from_utf8_lossy(&head[start + 1..end]).into_owned())
}

/// `(key, open, close)` of every `"key" { ... }` child of the block at `(open, close)`.
fn children(text: &[u8], (open, close): (usize, usize)) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = open + 1;
    while pos < close {
        let Some(child_open) = find_from(text, b"{", pos).filter(|&o| o < close) else { break };
        let Some(child_close) = block_end(text, child_open) else { break };
        if let Some(key) = key_before(text, child_open) {
            out.push((key, child_open, child_close));
        }
        pos = child_close + 1;
    }
    out
}

/// The item table with its `"items"` blocks indexed once (the table is ~50 MB).
pub struct Table<'a> {
    text: &'a [u8],
    items: HashMap<String, (usize, usize)>,
    /// `(npc_dota_hero_*, slot)` -> the hero's `default_item` block for that slot.
    defaults: HashMap<(String, String), (usize, usize)>,
    /// `(type, asset) -> modifier` of the default courier: what a courier item replaces.
    couriers: HashMap<(String, String), String>,
}

impl<'a> Table<'a> {
    pub fn new(text: &'a [u8]) -> Self {
        let items = items_section(text)
            .and_then(|open| Some((open, block_end(text, open)?)))
            .map(|range| {
                children(text, range)
                    .into_iter()
                    .map(|(key, open, close)| (key, (open, close)))
                    .collect()
            })
            .unwrap_or_default();
        let mut table = Self { text, items, defaults: HashMap::new(), couriers: HashMap::new() };
        let mut defaults = HashMap::new();
        for &(open, close) in table.items.values() {
            let block = &text[open..=close];
            if !crate::schema::has_scalar(block, b"prefab", b"default_item") {
                continue;
            }
            let Some(hero) = crate::schema::hero_of(block) else { continue };
            // No `item_slot` means the schema default, "weapon".
            let slot = crate::schema::slot_of(block).unwrap_or_else(|| "weapon".into());
            let key = (hero.to_ascii_lowercase(), slot);
            match defaults.get(&key) {
                Some(&(o, _)) if o < open => {}
                _ => {
                    defaults.insert(key, (open, close));
                }
            }
        }
        table.defaults = defaults;
        let default = ItemRef { name: DEFAULT_COURIER.into(), index: None, style: 0 };
        table.couriers = table
            .modifiers(&default)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| Some(((m.kind, m.asset?), m.modifier?)))
            .collect();
        table
    }

    /// The hero's `default_item` block for `slot` (first one in the table).
    fn default_item(&self, npc: &str, slot: &str) -> Option<(usize, usize)> {
        self.defaults.get(&(npc.to_ascii_lowercase(), slot.to_string())).copied()
    }

    /// Byte range (`{` .. `}`) of the item's block.
    fn find(&self, item: &ItemRef) -> Option<(usize, usize)> {
        if let Some(i) = item.index {
            return self.items.get(&i.to_string()).copied();
        }
        self.items.values().copied().find(|&(open, close)| {
            scalar(&self.text[open..=close], b"name")
                .is_some_and(|n| n.eq_ignore_ascii_case(item.name.trim()))
        })
    }

    /// Whether the item's visuals say `"skip_model_combine" "1"`.
    pub fn skips_model_combine(&self, item: &ItemRef) -> bool {
        let text = self.text;
        let Some(block) = self.find(item) else { return false };
        children(text, block)
            .into_iter()
            .find(|(k, ..)| k.eq_ignore_ascii_case("visuals"))
            .is_some_and(|(_, open, close)| {
                scalar(&text[open..=close], b"skip_model_combine").is_some_and(|v| v.trim() == "1")
            })
    }

    /// `weather_type` of a weather item's `"type" "weather"` modifier.
    pub fn weather_type(&self, item: &ItemRef) -> Option<u32> {
        let text = self.text;
        let block = self.find(item)?;
        let (_, v_open, v_close) = children(text, block)
            .into_iter()
            .find(|(k, ..)| k.eq_ignore_ascii_case("visuals"))?;
        children(text, (v_open, v_close)).into_iter().find_map(|(_, open, close)| {
            let body = &text[open..=close];
            (scalar(body, b"type").as_deref() == Some("weather"))
                .then(|| scalar(body, b"weather_type")?.trim().parse().ok())
                .flatten()
        })
    }

    /// The material group the item's `visuals` pick (`"skin" "2"` right in
    /// `visuals`, not inside an `asset_modifier`).
    pub fn visual_skin(&self, item: &ItemRef) -> Option<u32> {
        let text = self.text;
        let block = self.find(item)?;
        let (_, v_open, v_close) = children(text, block)
            .into_iter()
            .find(|(k, ..)| k.eq_ignore_ascii_case("visuals"))?;
        // Blank out the nested blocks, then look for the key at the top level.
        let mut top = text[v_open + 1..v_close].to_vec();
        for (_, open, close) in children(text, (v_open, v_close)) {
            for b in &mut top[open - v_open - 1..=close - v_open - 1] {
                *b = b' ';
            }
        }
        // the chosen style's material group: "styles" > "<n>" > "skin", or a
        // style-bound model_skin modifier; else the visuals' own "skin"
        if let Some((_, s_open, s_close)) = children(text, (v_open, v_close))
            .into_iter()
            .find(|(k, ..)| k.eq_ignore_ascii_case("styles"))
        {
            if let Some((_, o, c)) = children(text, (s_open, s_close))
                .into_iter()
                .find(|(k, ..)| k.trim() == item.style.to_string())
            {
                if let Some(skin) = scalar(&text[o..=c], b"skin").and_then(|v| v.trim().parse().ok()) {
                    return Some(skin);
                }
            }
        }
        for (_, o, c) in children(text, (v_open, v_close)) {
            let body = &text[o..=c];
            if scalar(body, b"type").as_deref() == Some("model_skin")
                && scalar(body, b"style").and_then(|v| v.trim().parse::<u32>().ok()) == Some(item.style)
            {
                if let Some(skin) = scalar(body, b"skin").and_then(|v| v.trim().parse().ok()) {
                    return Some(skin);
                }
            }
        }
        scalar(&top, b"skin")?.trim().parse().ok()
    }

    /// Every `asset_modifier` in the item's `visuals`; `None` if the item is not in the table.
    pub fn modifiers(&self, item: &ItemRef) -> Option<Vec<Modifier>> {
        let mut mods = self.modifiers_in(self.find(item)?);
        // style-bound modifiers only for the chosen style
        mods.retain(|m| m.style.map_or(true, |s| s == item.style));
        Some(mods)
    }

    /// Every `asset_modifier` in the `visuals` of the block at `block`.
    fn modifiers_in(&self, block: (usize, usize)) -> Vec<Modifier> {
        let text = self.text;
        let Some((_, v_open, v_close)) = children(text, block)
            .into_iter()
            .find(|(k, ..)| k.eq_ignore_ascii_case("visuals"))
        else {
            return Vec::new();
        };
        children(text, (v_open, v_close))
                .into_iter()
                .filter(|(k, ..)| k.to_ascii_lowercase().starts_with("asset_modifier"))
                .map(|(_, open, close)| {
                    let body = &text[open..=close];
                    Modifier {
                        kind: scalar(body, b"type").unwrap_or_default(),
                        asset: scalar(body, b"asset"),
                        modifier: scalar(body, b"modifier"),
                        style: scalar(body, b"style").and_then(|v| v.trim().parse().ok()),
                    }
                })
                .collect()
    }

}

/// Unit name -> its `Model`, from `npc_units.txt` or a hero script. Also
/// gives `<unit>_variant_<n>` for `Model<n>` (Tiny's growth stages, which items
/// call `npc_dota_hero_tiny_variant_<n>`) and the `BaseClass` of a unit
/// (`npc_dota_lone_druid_bear` for `npc_dota_lone_druid_bear1`).
pub fn unit_models(script: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut bases: Vec<(String, String)> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut pending: Option<String> = None;
    for line in script.lines() {
        let line = line.split("//").next().unwrap_or("").as_bytes();
        let mut i = 0;
        while i < line.len() {
            match line[i] {
                b'"' => {
                    let Some(len) = line[i + 1..].iter().position(|&c| c == b'"') else { break };
                    let word = String::from_utf8_lossy(&line[i + 1..i + 1 + len]).into_owned();
                    i += len + 2;
                    match pending.take() {
                        Some(key) if stack.len() == 2 => {
                            let unit = &stack[1];
                            let lower = key.to_ascii_lowercase();
                            if lower == "model" {
                                out.entry(format!("{unit}_variant_0")).or_insert(word.clone());
                                out.entry(unit.clone()).or_insert(word);
                            } else if let Some(n) = lower
                                .strip_prefix("model")
                                .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                            {
                                if word.ends_with(".vmdl") {
                                    out.entry(format!("{unit}_variant_{n}")).or_insert(word);
                                }
                            } else if lower == "baseclass" {
                                bases.push((unit.clone(), word));
                            }
                        }
                        Some(_) => {}
                        None => pending = Some(word),
                    }
                    continue;
                }
                b'{' => stack.push(pending.take().unwrap_or_default()),
                b'}' => {
                    pending = None;
                    stack.pop();
                }
                _ => {}
            }
            i += 1;
        }
    }
    for (unit, base) in bases {
        if let Some(model) = out.get(&unit).cloned() {
            out.entry(base).or_insert(model);
        }
    }
    out
}

/// The model of the unit an item names. Items and unit scripts number levels
/// differently (`npc_dota_venomancer_plague_ward_1` vs `..._plague_ward`,
/// `npc_dota_lycan_wolf` vs `npc_dota_lycan_wolf1`), so the level suffix is
/// tried both ways.
fn unit_model<'u>(units: &'u HashMap<String, String>, asset: &str) -> Option<&'u String> {
    if let Some(m) = units.get(asset) {
        return Some(m);
    }
    let base = asset.trim_end_matches(|c: char| c.is_ascii_digit());
    if base.len() < asset.len() {
        if let Some(m) = units.get(base.trim_end_matches('_')) {
            return Some(m);
        }
    }
    let mut levels: Vec<(&String, &String)> = units
        .iter()
        .filter(|(k, _)| {
            k.strip_prefix(asset)
                .map(|rest| rest.trim_start_matches('_'))
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    levels.sort();
    levels.first().map(|(_, m)| *m)
}

/// Unit and hero name -> its `Model`, from `npc_units.txt` and the hero scripts
/// in the game's pak (heroes are what arcanas like Bladeform Legacy swap).
pub fn all_unit_models(game_dir: &std::path::Path) -> Result<HashMap<String, String>, String> {
    let scripts = crate::vpk::read_matching(game_dir, |p| {
        p == "scripts/npc/npc_units.txt"
            || p.strip_prefix("scripts/npc/heroes/").is_some_and(|f| f.ends_with(".txt"))
    })?;
    let mut out = HashMap::new();
    for bytes in scripts.values() {
        for (unit, model) in unit_models(&String::from_utf8_lossy(bytes)) {
            out.entry(unit).or_insert(model);
        }
    }
    Ok(out)
}

/// Compiled `asset -> modifier` of the hero items' `model` swaps.
pub fn model_swaps(table: &Table, items: &[HeroItem]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for h in items {
        for m in table.modifiers(&h.item).unwrap_or_default() {
            if m.kind != "model" {
                continue;
            }
            let (Some(asset), Some(modifier)) = (m.asset, m.modifier) else { continue };
            if asset.ends_with(".vmdl") && modifier.ends_with(".vmdl") {
                out.entry(compiled_path(&asset)).or_insert(compiled_path(&modifier));
            }
        }
    }
    out
}

/// File overrides from the items' visuals. With `report_empty`, items with
/// nothing a file can swap are returned as skipped (map items); without it
/// they are silently left as they are (hero items, already applied by model).
pub fn overrides(
    table: &Table,
    items: &[ItemRef],
    units: &HashMap<String, String>,
    report_empty: bool,
) -> (Vec<Req>, Vec<Skip>) {
    let couriers = &table.couriers;
    let mut reqs = Vec::new();
    let mut skipped = Vec::new();
    for item in items {
        let Some(mods) = table.modifiers(item) else {
            if report_empty {
                skipped.push(Skip::new(&item.name, crate::tr!("нет в таблице предметов игры", "not in the game's item table")));
            }
            continue;
        };
        let before = reqs.len();
        // A terrain is mostly its map, which no file swap can change.
        let whole_map = mods.iter().any(|m| m.kind == "map_override");
        for m in mods {
            let (Some(asset), Some(modifier)) = (m.asset.as_deref(), m.modifier.as_deref()) else {
                continue;
            };
            let dst = match m.kind.as_str() {
                "entity_model" | "entity_clientside_model" if modifier.ends_with(".vmdl") => {
                    unit_model(units, asset).cloned()
                }
                "particle" | "particle_clientside"
                    if asset.ends_with(".vpcf") && modifier.ends_with(".vpcf") =>
                {
                    Some(asset.to_string())
                }
                "model" | "hero_model_change"
                    if asset.ends_with(".vmdl") && modifier.ends_with(".vmdl") =>
                {
                    Some(asset.to_string())
                }
                "map_effect" if asset == "map_shader" && modifier.ends_with(".vmat") => {
                    Some(DEFAULT_MAP_SHADER.to_string())
                }
                "courier" | "courier_flying" if modifier.ends_with(".vmdl") => {
                    couriers.get(&(m.kind.clone(), asset.to_string())).cloned()
                }
                _ => None,
            };
            let Some(dst) = dst else { continue };
            let (src, dst) = (compiled_path(modifier), compiled_path(&dst));
            if src != dst {
                reqs.push(Req { id: item.name.clone(), src, dst });
            }
        }
        if report_empty && (reqs.len() == before || whole_map) {
            skipped.push(Skip::new(&item.name, crate::tr!("нет замен, которые можно сделать файлами", "nothing a file can replace")));
        }
    }
    (reqs, skipped)
}

/// Hero particles spawned in a match (the loadout-preview ones left out). A
/// preset shows an item in its default style, so other styles' ones too.
fn hero_particles(mods: &[Modifier]) -> Vec<&str> {
    mods.iter()
        .filter(|m| m.kind == "particle_create" && m.asset.as_deref() != Some("npc_dota_loadout_generic"))
        .filter_map(|m| m.modifier.as_deref())
        .filter(|p| p.ends_with(".vpcf"))
        .collect()
}

/// File overrides for hero items, on top of their model: their particle swaps,
/// and their own particles in place of the ones the slot's default item spawns
/// (e.g. the Survivor blade glow instead of the default glaive glow, which
/// would otherwise float around the new model).
#[cfg(test)]
pub fn hero_overrides(
    table: &Table,
    items: &[HeroItem],
    units: &HashMap<String, String>,
    npcs: &HashMap<String, String>,
) -> Vec<Req> {
    hero_overrides_and_glows(table, items, units, npcs).0
}

/// A glow an item creates that no file swap could place (the slot's default
/// item creates none): the DLL creates it on the hero in a match.
#[derive(Debug, Clone, PartialEq)]
pub struct Glow {
    pub npc: String,
    pub particle: String,
}

/// `hero_overrides`, plus the item glows left without a place.
pub fn hero_overrides_and_glows(
    table: &Table,
    items: &[HeroItem],
    units: &HashMap<String, String>,
    npcs: &HashMap<String, String>,
) -> (Vec<Req>, Vec<Glow>) {
    let refs: Vec<ItemRef> = items.iter().map(|h| h.item.clone()).collect();
    let (mut reqs, _) = overrides(table, &refs, units, false);
    let mut glows = Vec::new();
    for h in items {
        let Some(mods) = table.modifiers(&h.item) else { continue };
        let own = hero_particles(&mods);
        if own.is_empty() {
            continue;
        }
        // How many of the item's glows already took a default item's place.
        let mut placed = 0;
        for hero in &h.heroes {
            let Some(npc) = npcs.get(&crate::schema::norm(hero)) else { continue };
            let Some(block) = table.default_item(npc, &h.slot) else { continue };
            let defaults = table.modifiers_in(block);
            let defaults = hero_particles(&defaults);
            placed = placed.max(defaults.len().min(own.len()));
            for (src, dst) in own.iter().zip(defaults) {
                let (src, dst) = (compiled_path(src), compiled_path(dst));
                if src != dst {
                    reqs.push(Req { id: h.item.name.clone(), src, dst });
                }
            }
        }
        // An item that blanks out one of the hero's own effects (Twilight
        // Schism turns Luna's luna_ambient_moon_glaive into null.vpcf) and
        // brings its own glow: the glow goes where the blanked effect was.
        let null = compiled_path(NULL_PARTICLE);
        let mut spare = own.iter().skip(placed);
        for r in reqs.iter_mut().filter(|r| r.id == h.item.name && r.src == null) {
            let Some(glow) = spare.next() else { break };
            r.src = compiled_path(glow);
        }
        for particle in spare {
            for hero in &h.heroes {
                if let Some(npc) = npcs.get(&crate::schema::norm(hero)) {
                    glows.push(Glow { npc: npc.clone(), particle: particle.to_string() });
                }
            }
        }
    }
    (reqs, glows)
}

/// What items swap a particle to when they switch it off.
const NULL_PARTICLE: &str = "particles/error/null.vpcf";

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#""items_game"
{
	"items"
	{
		"595"
		{
			"name"		"Default Courier"
			"prefab"		"courier"
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"courier"
					"modifier"		"models/props_gameplay/donkey.vmdl"
					"asset"		"radiant"
				}
				"asset_modifier1"
				{
					"type"		"courier_flying"
					"modifier"		"models/props_gameplay/donkey_wings.vmdl"
					"asset"		"radiant"
				}
			}
		}
		"19014"
		{
			"name"		"Dire Nemestice Creeps"
			"prefab"		"dire_creeps"
			"visuals"
			{
				"asset_modifier"
				{
					"type"		"entity_clientside_model"
					"asset"		"npc_dota_creep_badguys_melee"
					"modifier"		"models/creeps/lane_creeps/creep_bad_melee/creep_bad_melee_crystal.vmdl"
					"replace_animations"		"1"
				}
				"asset_modifier"
				{
					"type"		"particle_clientside"
					"asset"		"particles/siege_fx/siege_bad_death_01.vpcf"
					"modifier"		"particles/econ/creeps/x/death.vpcf"
				}
				"asset_modifier"
				{
					"type"		"portrait_game"
					"asset"		"npc_dota_creep_badguys_melee"
					"modifier"
					{
						"PortraitFOV"		"16"
					}
				}
			}
		}
		"500"
		{
			"name"		"Some Teleport"
			"prefab"		"teleport_effect"
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"particle"
					"asset"		"particles/items2_fx/teleport_start.vpcf"
					"modifier"		"particles/econ/events/x/teleport_start_x.vpcf"
				}
			}
		}
		"18713"
		{
			"name"		"Aghanim's Labyrinth Shader Effect"
			"prefab"		"map_effect"
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"map_effect"
					"asset"		"map_shader"
					"modifier"		"materials/dev/deferred_post_process_aghanim_bp.vmat"
				}
			}
		}
		"700"
		{
			"name"		"Some Courier"
			"prefab"		"courier"
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"courier"
					"modifier"		"models/items/courier/x/x.vmdl"
					"asset"		"radiant"
				}
				"asset_modifier1"
				{
					"type"		"courier_flying"
					"modifier"		"models/items/courier/x/x_flying.vmdl"
					"asset"		"radiant"
				}
			}
		}
		"600"
		{
			"name"		"Music"
			"prefab"		"music"
		}
	}
}
"#;

    const UNITS: &str = "\"DOTAUnits\"\n{\n\t\"Version\"\t\"1\"\n\t\"npc_dota_creep_badguys_melee\"\n\t{\n\t\t\"Model\"\t\"models/creeps/lane_creeps/creep_bad_melee/creep_bad_melee.vmdl\" // base\n\t\t\"Creature\"\n\t\t{\n\t\t\t\"Model\"\t\"nested.vmdl\"\n\t\t}\n\t}\n}\n";

    fn item(name: &str, index: Option<u32>) -> ItemRef {
        ItemRef { name: name.into(), index, style: 0 }
    }

    fn pairs(reqs: &[Req]) -> Vec<(&str, &str)> {
        reqs.iter().map(|r| (r.src.as_str(), r.dst.as_str())).collect()
    }

    #[test]
    fn unit_models_take_the_top_level_model() {
        let u = unit_models(UNITS);
        assert_eq!(
            u.get("npc_dota_creep_badguys_melee").map(String::as_str),
            Some("models/creeps/lane_creeps/creep_bad_melee/creep_bad_melee.vmdl")
        );
        assert_eq!(u.len(), 2, "unit + its _variant_0");
    }

    #[test]
    fn unit_lookup_tries_level_suffixes() {
        let units: HashMap<String, String> = [
            ("npc_dota_venomancer_plague_ward".to_string(), "ward.vmdl".to_string()),
            ("npc_dota_lycan_wolf2".to_string(), "wolf2.vmdl".to_string()),
            ("npc_dota_lycan_wolf1".to_string(), "wolf1.vmdl".to_string()),
        ]
        .into();
        assert_eq!(unit_model(&units, "npc_dota_venomancer_plague_ward_3").map(String::as_str), Some("ward.vmdl"));
        assert_eq!(unit_model(&units, "npc_dota_lycan_wolf").map(String::as_str), Some("wolf1.vmdl"));
        assert_eq!(unit_model(&units, "npc_dota_lycan"), None);
    }

    #[test]
    fn unit_models_know_variants_and_base_classes() {
        let tiny = "\"DOTAHeroes\"
{
	\"npc_dota_hero_tiny\"
	{
		\"Model\"	\"t1.vmdl\"
		\"Model1\"	\"t2.vmdl\"
		\"ModelScale\"	\"0.93\"
	}
	\"npc_dota_lone_druid_bear1\"
	{
		\"Model\"	\"bear.vmdl\"
		\"BaseClass\"	\"npc_dota_lone_druid_bear\"
	}
}
";
        let u = unit_models(tiny);
        assert_eq!(u.get("npc_dota_hero_tiny_variant_0").map(String::as_str), Some("t1.vmdl"));
        assert_eq!(u.get("npc_dota_hero_tiny_variant_1").map(String::as_str), Some("t2.vmdl"));
        assert_eq!(u.get("npc_dota_lone_druid_bear").map(String::as_str), Some("bear.vmdl"));
        assert!(!u.contains_key("npc_dota_hero_tiny_variant_scale"));
    }

    #[test]
    fn map_items_become_file_overrides() {
        let units = unit_models(UNITS);
        let table = Table::new(TABLE.as_bytes());
        let items = [
            item("Dire Nemestice Creeps", Some(19014)),
            item("Some Teleport", None),
            item("Aghanim's Labyrinth Shader Effect", Some(18713)),
            item("Some Courier", Some(700)),
            item("Music", Some(600)),
            item("Missing", Some(1)),
        ];
        let (reqs, skipped) = overrides(&table, &items, &units, true);
        assert_eq!(
            pairs(&reqs),
            vec![
                (
                    "models/creeps/lane_creeps/creep_bad_melee/creep_bad_melee_crystal.vmdl_c",
                    "models/creeps/lane_creeps/creep_bad_melee/creep_bad_melee.vmdl_c"
                ),
                ("particles/econ/creeps/x/death.vpcf_c", "particles/siege_fx/siege_bad_death_01.vpcf_c"),
                ("particles/econ/events/x/teleport_start_x.vpcf_c", "particles/items2_fx/teleport_start.vpcf_c"),
                (
                    "materials/dev/deferred_post_process_aghanim_bp.vmat_c",
                    "materials/dev/deferred_post_process.vmat_c"
                ),
                ("models/items/courier/x/x.vmdl_c", "models/props_gameplay/donkey.vmdl_c"),
                ("models/items/courier/x/x_flying.vmdl_c", "models/props_gameplay/donkey_wings.vmdl_c"),
            ]
        );
        let names: Vec<&str> = skipped.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Music", "Missing"]);
    }

    #[test]
    fn hero_particles_replace_the_default_items() {
        let text = r#""items_game"
{
	"items"
	{
		"1"
		{
			"name"		"Anti-Mage's Glaive"
			"prefab"		"default_item"
			"item_slot"		"weapon"
			"used_by_heroes"
			{
				"npc_dota_hero_antimage"		"1"
			}
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"particle_create"
					"modifier"		"particles/units/heroes/hero_antimage/antimage_blade_primary.vpcf"
				}
			}
		}
		"8734"
		{
			"name"		"Blade of the Survivor"
			"prefab"		"wearable"
			"item_slot"		"weapon"
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"particle_create"
					"modifier"		"particles/econ/items/antimage/godeater/blade_primary_godeater.vpcf"
				}
				"asset_modifier1"
				{
					"type"		"particle"
					"asset"		"particles/units/heroes/hero_antimage/antimage_blink_start.vpcf"
					"modifier"		"particles/econ/items/antimage/godeater/blink_start.vpcf"
				}
			}
		}
	}
}
"#;
        let table = Table::new(text.as_bytes());
        let npcs: HashMap<String, String> =
            [("antimage".to_string(), "npc_dota_hero_antimage".to_string())].into();
        let item = HeroItem {
            item: item("Blade of the Survivor", Some(8734)),
            heroes: vec!["Anti-Mage".into()],
            slot: "weapon".into(),
            model: None,
        };
        let reqs = hero_overrides(&table, &[item], &HashMap::new(), &npcs);
        assert_eq!(
            pairs(&reqs),
            vec![
                (
                    "particles/econ/items/antimage/godeater/blink_start.vpcf_c",
                    "particles/units/heroes/hero_antimage/antimage_blink_start.vpcf_c"
                ),
                (
                    "particles/econ/items/antimage/godeater/blade_primary_godeater.vpcf_c",
                    "particles/units/heroes/hero_antimage/antimage_blade_primary.vpcf_c"
                ),
            ]
        );
    }

    #[test]
    fn own_glow_replaces_a_blanked_hero_effect() {
        let text = r#""items_game"
{
	"items"
	{
		"9000"
		{
			"name"		"Twilight Schism"
			"prefab"		"wearable"
			"item_slot"		"weapon"
			"visuals"
			{
				"asset_modifier"
				{
					"type"		"particle_create"
					"modifier"		"particles/econ/items/luna/ti9/weapon_ambient.vpcf"
				}
				"asset_modifier"
				{
					"type"		"particle"
					"asset"		"particles/units/heroes/hero_luna/luna_ambient_moon_glaive.vpcf"
					"modifier"		"particles/error/null.vpcf"
				}
			}
		}
	}
}
"#;
        let table = Table::new(text.as_bytes());
        assert_eq!(table.visual_skin(&item("Twilight Schism", Some(9000))), None);
        let schism = HeroItem {
            item: item("Twilight Schism", Some(9000)),
            heroes: vec!["Luna".into()],
            slot: "weapon".into(),
            model: None,
        };
        let reqs = hero_overrides(&table, &[schism], &HashMap::new(), &HashMap::new());
        assert_eq!(
            pairs(&reqs),
            vec![(
                "particles/econ/items/luna/ti9/weapon_ambient.vpcf_c",
                "particles/units/heroes/hero_luna/luna_ambient_moon_glaive.vpcf_c"
            )]
        );
    }

    #[test]
    fn hero_items_are_not_reported() {
        let table = Table::new(TABLE.as_bytes());
        let (reqs, skipped) = overrides(&table, &[item("Music", Some(600))], &HashMap::new(), false);
        assert!(reqs.is_empty() && skipped.is_empty());
    }

}
