//! Item colourings that are a material group of a shared model.
//!
//! Some items reuse another item's model with a different set of materials:
//! "Twilight Schism of the Crimson Witness" is the Twilight Schism model with
//! `"skin" "2"` in its visuals, i.e. its third material group (the red one).
//! Copying the model alone shows material group 0 (blue), so the materials of
//! group N are copied over the materials of group 0 as well. The groups are
//! inside the compiled model (binary KV3), read with Source2Viewer-CLI.

use std::collections::HashMap;
use std::path::Path;

use crate::extract::{compiled_path, Req};
use crate::visuals::{HeroItem, Table};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Model (`.vmdl`, lower-case) -> its material groups, each a list of `.vmat`.
pub type Groups = HashMap<String, Vec<Vec<String>>>;

/// Material groups from Source2Viewer-CLI's `-b DATA` output for one or more models.
pub fn parse_groups(output: &str) -> Groups {
    let mut out = Groups::new();
    let mut model: Option<String> = None;
    let mut depth: Option<i32> = None;
    let mut groups: Vec<Vec<String>> = Vec::new();
    for line in output.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.contains("] ") && t.ends_with("_c") {
            // "[1/2] models/x/y.vmdl_c"
            let path = t.split_once("] ").map(|(_, p)| p).unwrap_or("");
            model = Some(path.trim_end_matches("_c").to_ascii_lowercase());
            depth = None;
            groups.clear();
            continue;
        }
        if depth.is_none() {
            if t.starts_with("m_materialGroups") {
                depth = Some(0);
            }
            continue;
        }
        let d = depth.as_mut().unwrap();
        if t.starts_with("m_name") {
            groups.push(Vec::new());
        } else if let Some(rest) = t.strip_prefix("resource:\"") {
            if let (Some(path), Some(group)) = (rest.split('"').next(), groups.last_mut()) {
                if path.ends_with(".vmat") {
                    group.push(path.to_string());
                }
            }
        }
        *d += t.matches('[').count() as i32 - t.matches(']').count() as i32;
        if *d <= 0 && t.contains(']') {
            if let Some(m) = model.take() {
                out.insert(m, std::mem::take(&mut groups));
            }
            depth = None;
        }
    }
    out
}

/// Mesh groups (body group choices, `"<group>_@<choice>_#&<name>"`) from
/// Source2Viewer-CLI's `-b DATA` output, each model's as one list: bit N of a model's
/// mesh group mask is its group N.
pub fn parse_mesh_groups(output: &str) -> Groups {
    let mut out = Groups::new();
    let mut model: Option<String> = None;
    let mut names: Option<Vec<String>> = None;
    for line in output.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.contains("] ") && t.ends_with("_c") {
            let path = t.split_once("] ").map(|(_, p)| p).unwrap_or("");
            model = Some(path.trim_end_matches("_c").to_ascii_lowercase());
            names = None;
            continue;
        }
        match names.as_mut() {
            None => {
                if let Some(rest) = t.strip_prefix("m_meshGroups") {
                    let rest = rest.trim_start_matches([' ', '=']).trim();
                    if rest.starts_with('[') && rest.ends_with(']') {
                        // "m_meshGroups = [  ]"
                        if let Some(m) = model.take() {
                            out.insert(m, vec![Vec::new()]);
                        }
                    } else {
                        names = Some(Vec::new());
                    }
                }
            }
            Some(list) => {
                if let Some(name) = t.strip_prefix('"').and_then(|r| r.split('"').next()) {
                    list.push(name.to_string());
                } else if t.starts_with(']') {
                    if let Some(m) = model.take() {
                        out.insert(m, vec![std::mem::take(list)]);
                    }
                    names = None;
                }
            }
        }
    }
    out
}

/// Source2Viewer-CLI's `-b DATA` output for `models` (`.vmdl` paths) in the game's pak.
fn model_data(cli: &Path, pak: &Path, models: &[String]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let mut out = String::new();
    // a few at a time: the paths go on one command line
    for chunk in models.chunks(120) {
        let filters: Vec<String> = chunk.iter().map(|m| compiled_path(m)).collect();
        let output = std::process::Command::new(cli)
            .arg("-i")
            .arg(pak)
            .arg("-f")
            .arg(filters.join(","))
            .args(["-b", "DATA"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| crate::trf!("не удалось запустить Source2Viewer-CLI: {e}", "could not start Source2Viewer-CLI: {e}"))?;
        out.push_str(&String::from_utf8_lossy(&output.stdout));
        out.push('\n');
    }
    Ok(out)
}

/// Material groups of `models`, remembered in `cache\material_groups.txt`
/// (Source2Viewer-CLI takes a moment to start; the cache is reset when the
/// game's pak changes, i.e. after a game update).
pub fn material_groups_cached(cli: &Path, pak: &Path, models: &[String]) -> Groups {
    data_cached(cli, pak, models, "material_groups.txt", parse_groups)
}

/// Mesh groups of `models` (see `parse_mesh_groups`), remembered in `cache\mesh_groups.txt`.
pub fn mesh_groups_cached(cli: &Path, pak: &Path, models: &[String]) -> Groups {
    data_cached(cli, pak, models, "mesh_groups.txt", parse_mesh_groups)
}

/// Something read from files' `DATA` blocks (`parse`), per file, remembered in
/// `cache\<file>` until the game's pak changes.
pub(crate) fn data_cached(cli: &Path, pak: &Path, models: &[String], file: &str, parse: fn(&str) -> Groups) -> Groups {
    // one at a time (a pick and the background warm-up): each writes the whole file
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = crate::config::app_dir().join("cache").join(file);
    let stamp = std::fs::metadata(pak).map(|m| format!("{}|{:?}", m.len(), m.modified().ok())).unwrap_or_default();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines = text.lines();
    let mut cache = Groups::new();
    if lines.next() == Some(stamp.as_str()) {
        // model TAB mat,mat;mat,mat (groups split by ';')
        for l in lines {
            let Some((m, g)) = l.split_once('\t') else { continue };
            let groups = if g.is_empty() {
                Vec::new()
            } else {
                g.split(';').map(|grp| grp.split(',').filter(|s| !s.is_empty()).map(String::from).collect()).collect()
            };
            cache.insert(m.to_string(), groups);
        }
    }
    let missing: Vec<String> =
        models.iter().filter(|m| !cache.contains_key(&m.to_ascii_lowercase())).cloned().collect();
    if !missing.is_empty() {
        // the CLI failing is not remembered: it is asked again next time
        let Ok(data) = model_data(cli, pak, &missing) else { return Groups::new() };
        let found = parse(&data);
        for m in &missing {
            // a model without groups is remembered too (empty), so it is not asked again
            let key = m.to_ascii_lowercase();
            let g = found.get(&key).cloned().unwrap_or_default();
            cache.insert(key, g);
        }
        let mut out = format!("{stamp}\n");
        for (m, g) in &cache {
            let groups: Vec<String> = g.iter().map(|grp| grp.join(",")).collect();
            out.push_str(&format!("{m}\t{}\n", groups.join(";")));
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(&path, out);
    }
    models
        .iter()
        .filter_map(|m| {
            let key = m.to_ascii_lowercase();
            cache.get(&key).filter(|g| !g.is_empty()).map(|g| (key, g.clone()))
        })
        .collect()
}

/// Hero items whose visuals pick a material group other than the default,
/// with that group: `(item, model, group)`.
pub fn wanted<'a>(table: &Table, items: &'a [HeroItem]) -> Vec<(&'a HeroItem, String, usize)> {
    let swaps = crate::visuals::model_swaps(table, items);
    items
        .iter()
        .flat_map(|h| {
            // a set piece without styles of its own follows the set's style
            // (Drow's arcana crossbow has a group "1" for «Master of Madness»);
            // a model without that group is left as it is
            let skin = table.visual_skin(&h.item).filter(|&s| s > 0).unwrap_or(h.item.style);
            if skin == 0 {
                return Vec::new();
            }
            // the item's own model and the models it puts on the hero (an
            // arcana's body: Drow's style «Master of Madness» is its group 1)
            let mut models: Vec<String> = h.model.iter().map(|m| m.trim().to_string()).collect();
            for m in table.modifiers(&h.item).unwrap_or_default() {
                if m.kind == "entity_model" {
                    models.extend(m.modifier.map(|p| p.trim().to_string()));
                }
            }
            // the model another item turns this one into (the arcana's refits)
            let swapped: Vec<String> = models
                .iter()
                .filter_map(|m| swaps.get(&compiled_path(m)))
                .map(|c| c.strip_suffix("_c").unwrap_or(c).to_string())
                .collect();
            models.extend(swapped);
            models.retain(|m| m.ends_with(".vmdl"));
            models.dedup();
            models.into_iter().map(|m| (h, m, skin as usize)).collect()
        })
        .collect()
}

/// changer.dll's lines for the chosen styles' material groups, `matgroup|<model>|<group>`:
/// a style picked while its model is loaded shows only when the group is switched on
/// the model itself (a model takes its materials when it loads). An item that puts a
/// body on the hero (an arcana) gives its group to the hero's other preset models too
/// (a model without that group is left as it is). Groups are named by their number, as
/// the items' models name them ("1", "2").
pub fn group_lines(table: &Table, items: &[HeroItem], wanted: &[(&HeroItem, String, usize)]) -> Vec<String> {
    let mut out: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    // a piece another item turns into another model (the arcana's refits): that one too
    let swaps = crate::visuals::model_swaps(table, items);
    for (h, model, skin) in wanted {
        out.entry(model.trim().to_ascii_lowercase()).or_insert(*skin);
        let body = table
            .modifiers(&h.item)
            .unwrap_or_default()
            .iter()
            .any(|m| m.kind == "entity_model" && m.asset.as_deref().is_some_and(|a| a.starts_with("npc_dota_hero_")));
        if !body {
            continue;
        }
        for other in items.iter().filter(|o| o.heroes.iter().any(|x| h.heroes.contains(x))) {
            if let Some(m) = other.model.as_deref().map(str::trim).filter(|m| m.ends_with(".vmdl")) {
                out.entry(m.to_ascii_lowercase()).or_insert(*skin);
                if let Some(to) = swaps.get(&compiled_path(m)) {
                    out.entry(to.strip_suffix("_c").unwrap_or(to).to_ascii_lowercase()).or_insert(*skin);
                }
            }
        }
    }
    out.into_iter().map(|(model, skin)| format!("matgroup|{model}|{skin}")).collect()
}

/// The arcanas' levels for their heroes' preset models (model -> level). An arcana's
/// style is its level (`arcana_level` of the chosen style), and the game shows its
/// hero's pieces' body group "arcana" of that level: Windranger's autumn style is her
/// hair's, cape's, quiver's, bow's and shoulders' choice 2 (another mesh), not a
/// material group.
pub fn arcana_levels(table: &Table, items: &[HeroItem]) -> std::collections::BTreeMap<String, u32> {
    let swaps = crate::visuals::model_swaps(table, items);
    let mut levels: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for h in items {
        let mods = table.modifiers(&h.item).unwrap_or_default();
        let Some(level) = mods.iter().find(|m| m.kind == "arcana_level").and_then(|m| m.level) else { continue };
        for other in items.iter().filter(|o| o.heroes.iter().any(|x| h.heroes.contains(x))) {
            let mut models: Vec<String> = other.model.iter().map(|m| m.trim().to_string()).collect();
            for m in table.modifiers(&other.item).unwrap_or_default() {
                if m.kind == "entity_model" {
                    models.extend(m.modifier.map(|p| p.trim().to_string()));
                }
            }
            for m in models.iter().filter(|m| m.ends_with(".vmdl")) {
                levels.entry(m.to_ascii_lowercase()).or_insert(level);
                if let Some(to) = swaps.get(&compiled_path(m)) {
                    levels.entry(to.strip_suffix("_c").unwrap_or(to).to_ascii_lowercase()).or_insert(level);
                }
            }
        }
    }
    levels
}

/// changer.dll's lines for the arcanas' levels, `meshmask|<model>|<clear>|<set>|<reset>`:
/// the model's mesh group mask without the body group "arcana"'s bits (`clear`), with
/// the level's choice (`set`; choice 0, `reset`, when the item leaves). A model without
/// that group, or without that choice, is left as it is.
pub fn mask_lines(levels: &std::collections::BTreeMap<String, u32>, meshes: &Groups) -> Vec<String> {
    let mut out = Vec::new();
    for (model, level) in levels {
        let Some(names) = meshes.get(model).and_then(|g| g.first()) else { continue };
        let (mut clear, mut set, mut reset) = (0u64, 0u64, 0u64);
        for (bit, name) in names.iter().enumerate().take(64) {
            let Some(choice) = name.strip_prefix("arcana_@").and_then(|r| r.split('_').next()) else { continue };
            let Ok(choice) = choice.parse::<u32>() else { continue };
            clear |= 1 << bit;
            if choice == *level {
                set = 1 << bit;
            }
            if choice == 0 {
                reset = 1 << bit;
            }
        }
        if set != 0 {
            out.push(format!("meshmask|{model}|{clear}|{set}|{reset}"));
        }
    }
    out
}

/// Group N's materials over group 0's, for every wanted item.
pub fn overrides(wanted: &[(&HeroItem, String, usize)], groups: &Groups) -> Vec<Req> {
    let mut reqs = Vec::new();
    for (h, model, skin) in wanted {
        let Some(g) = groups.get(&model.to_ascii_lowercase()) else { continue };
        let (Some(base), Some(chosen)) = (g.first(), g.get(*skin)) else { continue };
        for (dst, src) in base.iter().zip(chosen) {
            let (src, dst) = (compiled_path(src), compiled_path(dst));
            if src != dst {
                reqs.push(Req { id: h.item.name.clone(), src, dst });
            }
        }
    }
    reqs
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = r#"[1/2] models/heroes/luna/luna_mount.vmdl_c
--- Data for block "DATA" ---
{
	m_materialGroups =
	[
		{
			m_name = "0"
			m_materials =
			[
				resource:"materials/models/heroes/luna/mount_color.vmat",
			]
		},
	]
	m_nDefaultMeshGroupMask = 1
}
[2/2] models/items/luna/luna_ti9_immortal/luna_ti9_immortal.vmdl_c
--- Data for block "DATA" ---
{
	m_materialGroups =
	[
		{
			m_name = "0"
			m_materials =
			[
				resource:"materials/models/items/luna/luna_ti9_immortal/luna_ti9_immortal.vmat",
				resource:"materials/models/items/luna/luna_ti9_immortal/luna_ti9_immortal_blade.vmat",
			]
		},
		{
			m_name = "1"
			m_materials =
			[
				resource:"materials/models/items/luna/luna_ti9_immortal/luna_ti9_immortal_gold.vmat",
				resource:"materials/models/items/luna/luna_ti9_immortal/luna_ti9_immortal_blade_gold.vmat",
			]
		},
		{
			m_name = "2"
			m_materials =
			[
				resource:"materials/models/items/luna/luna_ti9_immortal/luna_ti9_crimson_immortal.vmat",
				resource:"materials/models/items/luna/luna_ti9_immortal/luna_ti9_crimson_immortal_blade.vmat",
			]
		},
	]
	m_nDefaultMeshGroupMask = 1
}
"#;

    #[test]
    fn parses_groups_of_several_models() {
        let g = parse_groups(OUT);
        assert_eq!(g["models/heroes/luna/luna_mount.vmdl"].len(), 1);
        let ti9 = &g["models/items/luna/luna_ti9_immortal/luna_ti9_immortal.vmdl"];
        assert_eq!(ti9.len(), 3);
        assert_eq!(ti9[2][1], "materials/models/items/luna/luna_ti9_immortal/luna_ti9_crimson_immortal_blade.vmat");
    }

    #[test]
    fn chosen_group_goes_over_the_default() {
        let item = HeroItem {
            item: crate::visuals::ItemRef { name: "Crimson".into(), index: Some(1), style: 0 },
            heroes: vec!["Luna".into()],
            slot: "weapon".into(),
            model: Some("models/items/luna/luna_ti9_immortal/luna_ti9_immortal.vmdl".into()),
            siblings: Vec::new(),
        };
        let wanted = vec![(&item, item.model.clone().unwrap(), 2usize)];
        let reqs = overrides(&wanted, &parse_groups(OUT));
        let pairs: Vec<(&str, &str)> = reqs.iter().map(|r| (r.src.as_str(), r.dst.as_str())).collect();
        assert_eq!(
            pairs,
            vec![
                (
                    "materials/models/items/luna/luna_ti9_immortal/luna_ti9_crimson_immortal.vmat_c",
                    "materials/models/items/luna/luna_ti9_immortal/luna_ti9_immortal.vmat_c"
                ),
                (
                    "materials/models/items/luna/luna_ti9_immortal/luna_ti9_crimson_immortal_blade.vmat_c",
                    "materials/models/items/luna/luna_ti9_immortal/luna_ti9_immortal_blade.vmat_c"
                ),
            ]
        );
    }

    #[test]
    fn arcana_style_is_its_pieces_body_group() {
        let text = r#""items_game"
{
	"items"
	{
		"13806"
		{
			"name"		"Compass of the Rising Gale"
			"item_slot"		"hero_base"
			"visuals"
			{
				"styles"
				{
					"0"	{	"skin"	"0"	}
					"1"	{	"skin"	"1"	}
				}
				"asset_modifier"
				{
					"type"		"entity_model"
					"asset"		"npc_dota_hero_windrunner"
					"modifier"		"models/items/windrunner/windrunner_arcana/wr_arcana_base.vmdl"
				}
				"asset_modifier"
				{
					"type"		"arcana_level"
					"level"		"1"
					"style"		"0"
				}
				"asset_modifier"
				{
					"type"		"arcana_level"
					"level"		"2"
					"style"		"1"
				}
			}
		}
		"32650"
		{
			"name"		"Hair of the Rising Gale"
			"item_slot"		"head"
		}
		"500"
		{
			"name"		"Some Axe Helm"
			"item_slot"		"head"
		}
	}
}
"#;
        let table = Table::new(text.as_bytes());
        let piece = |name: &str, index: u32, style: u32, hero: &str, model: Option<&str>| HeroItem {
            item: crate::visuals::ItemRef { name: name.into(), index: Some(index), style },
            heroes: vec![hero.into()],
            slot: "head".into(),
            model: model.map(Into::into),
            siblings: Vec::new(),
        };
        let base = "models/items/windrunner/windrunner_arcana/wr_arcana_base.vmdl";
        let hair = "models/items/windrunner/windrunner_arcana/wr_arcana_head.vmdl";
        // the body: body groups without "arcana"; the hair: style0, its copy, style1
        let meshes = parse_mesh_groups(MESHES);
        assert_eq!(meshes[hair], vec![vec!["arcana_@0_#&style0", "arcana_@1_#&style0_copy", "arcana_@2_#&style1"]]);
        assert!(meshes["models/a.vmdl"][0].is_empty());
        let lines_for = |style: u32| {
            let items = vec![
                piece("Compass of the Rising Gale", 13806, style, "Windranger", None),
                piece("Hair of the Rising Gale", 32650, 0, "Windranger", Some(hair)),
                piece("Some Axe Helm", 500, 0, "Axe", Some("models/items/axe/helm.vmdl")),
            ];
            let wanted = wanted(&table, &items);
            let mut lines = group_lines(&table, &items, &wanted);
            lines.extend(mask_lines(&arcana_levels(&table, &items), &meshes));
            lines
        };
        // autumn: group "1" of the body (and of the hair, which has none); the hair's mesh
        // of level 2 (bit 2 instead of bits 0-2)
        assert_eq!(
            lines_for(1),
            vec![format!("matgroup|{base}|1"), format!("matgroup|{hair}|1"), format!("meshmask|{hair}|7|4|1")]
        );
        // the first style: no group, level 1; another hero's items are not touched
        assert_eq!(lines_for(0), vec![format!("meshmask|{hair}|7|2|1")]);
    }

    const MESHES: &str = r#"[1/3] models/items/windrunner/windrunner_arcana/wr_arcana_base.vmdl_c
	m_meshGroups =
	[
		"arms_@0_#&0",
		"arms_@1_#&1",
	]
	m_materialGroups = [  ]
[2/3] models/items/windrunner/windrunner_arcana/wr_arcana_head.vmdl_c
	m_meshGroups =
	[
		"arcana_@0_#&style0",
		"arcana_@1_#&style0_copy",
		"arcana_@2_#&style1",
	]
	m_materialGroups = [  ]
[3/3] models/a.vmdl_c
	m_meshGroups = [  ]
"#;
}
