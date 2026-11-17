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

/// Material groups of `models` (`.vmdl` paths) from the game's pak.
pub fn material_groups(cli: &Path, pak: &Path, models: &[String]) -> Result<Groups, String> {
    use std::os::windows::process::CommandExt;
    if models.is_empty() {
        return Ok(Groups::new());
    }
    let filters: Vec<String> = models.iter().map(|m| compiled_path(m)).collect();
    let output = std::process::Command::new(cli)
        .arg("-i")
        .arg(pak)
        .arg("-f")
        .arg(filters.join(","))
        .args(["-b", "DATA"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| crate::trf!("не удалось запустить Source2Viewer-CLI: {e}", "could not start Source2Viewer-CLI: {e}"))?;
    Ok(parse_groups(&String::from_utf8_lossy(&output.stdout)))
}

/// Material groups of `models`, remembered in `cache\material_groups.txt`
/// (Source2Viewer-CLI takes a moment to start; the cache is reset when the
/// game's pak changes, i.e. after a game update).
pub fn material_groups_cached(cli: &Path, pak: &Path, models: &[String]) -> Groups {
    let path = crate::config::app_dir().join("cache").join("material_groups.txt");
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
        let Ok(found) = material_groups(cli, pak, &missing) else { return Groups::new() };
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
}

#[cfg(test)]
mod drow_style_real {
    #[test]
    #[ignore]
    fn drow_arcana_style1() {
        use crate::visuals::{HeroItem, ItemRef, Table};
        let exe = r"C:\Program Files (x86)\Steam\steamapps\common\dota 2 beta\game\bin\win64\dota2.exe";
        let game = crate::vpk::game_dir_from_exe(exe).unwrap();
        let text = crate::vpk::read_file(&game, crate::schema::SCHEMA_PATH).unwrap().unwrap();
        let table = Table::new(&text);
        let items: Vec<HeroItem> = [
            ("Dread Retribution", 19090, None),
            ("Dread Retribution - Bracers", 19088, Some("models/items/drow/drow_arcana/drow_arcana_arms.vmdl")),
            ("Dread Retribution - Weapon", 37144, Some("models/items/drow/drow_arcana/drow_arcana_bow.vmdl")),
            ("Dread Retribution - Cape", 19089, Some("models/items/drow/drow_arcana/drow_arcana_back.vmdl")),
        ]
        .iter()
        .map(|(n, i, m)| HeroItem {
            item: ItemRef { name: n.to_string(), index: Some(*i), style: 1 },
            heroes: vec!["Drow Ranger".into()],
            slot: String::new(),
            model: m.map(String::from),
        })
        .collect();
        for h in &items {
            println!("{}: skin={:?}", h.item.name, table.visual_skin(&h.item));
            for m in table.modifiers(&h.item).unwrap() {
                println!("   {:?}", m);
            }
        }
        let wanted = super::wanted(&table, &items);
        println!("wanted {:?}", wanted.iter().map(|(h, m, s)| (&h.item.name, m, s)).collect::<Vec<_>>());
        let cli = std::path::Path::new("tools/vrf-cli/Source2Viewer-CLI.exe");
        let models: Vec<String> = wanted.iter().map(|(_, m, _)| m.clone()).collect();
        let groups = super::material_groups(cli, &game.join("pak01_dir.vpk"), &models).unwrap();
        for (m, g) in &groups {
            println!("{m}: {} groups", g.len());
        }
        let reqs = super::overrides(&wanted, &groups);
        for r in &reqs {
            println!("  {} <- {}", r.dst, r.src);
        }
        assert!(!reqs.is_empty());
    }
}
