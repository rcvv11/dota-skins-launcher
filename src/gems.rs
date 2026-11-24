//! Prismatic gems: the colour of a hero's effects that follow a gem.
//!
//! Dota colours them with the prismatic gem in the hero's items (the Terrorblade
//! arcana's own red without one). The item page's «Replace gem» opens every
//! prismatic colour (the table's `colors`, `unusual_*`); the one picked is kept in
//! the preset per item (`Profile::gems`) and given to changer.dll for the hero's
//! model, which colours the hero and its effects (in a match your own hero's).

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::visuals::{HeroItem, Table};

/// changer.dll's lines: the palette for the item page's menu
/// (`gemcolor|<name>|<RRGGBB>`), the items whose page offers a gem, with their icon for
/// the picker's header (`gemitem|<item>|<image>`: Dota's store page, for an item you do
/// not own, has no sockets), the colour picked
/// per item for the page, by the item's name (`gempick|<item>|<name>`), and the hero
/// models' colours (`gem|<model>|<RRGGBB>`).
/// The body an item puts on its hero (the arcana's model) takes the colour wherever
/// it is shown (the store page's preview too); an item without a body colours its
/// hero while it is worn: in the preset (`in_use`) or the account's own (`owned`).
pub fn lines(
    table: &Table,
    gems: &[(HeroItem, String)],
    in_use: &[HeroItem],
    owned: &HashSet<u32>,
    units: &HashMap<String, String>,
    npcs: &HashMap<String, String>,
) -> Vec<String> {
    let palette: Vec<(String, String)> = table
        .colors()
        .into_iter()
        .filter(|(key, ..)| key.starts_with("unusual_"))
        .map(|(_, name, hex)| (name, hex.trim().trim_start_matches('#').to_ascii_uppercase()))
        .filter(|(name, hex)| !name.is_empty() && hex.len() == 6)
        .collect();
    let mut out: Vec<String> = palette.iter().map(|(name, hex)| format!("gemcolor|{name}|{hex}")).collect();
    for item in table.gem_items() {
        let icon = table
            .image_inventory(&crate::visuals::ItemRef { name: item.clone(), index: None, style: 0 })
            .map(|i| format!("file://{{images}}/{}.png", i.trim()))
            .unwrap_or_default();
        out.push(format!("gemitem|{}|{icon}", item.replace('|', " ")));
    }
    let mut models: BTreeMap<String, String> = BTreeMap::new();
    for (h, color) in gems {
        let Some((name, hex)) = palette.iter().find(|(n, _)| n.eq_ignore_ascii_case(color.trim())) else { continue };
        out.push(format!("gempick|{}|{name}", h.item.name.replace('|', " ")));
        let worn = in_use.iter().any(|w| w.item.name == h.item.name) || h.item.index.is_some_and(|d| owned.contains(&d));
        // the body the item puts on the hero (the arcana's model); an item without one:
        // the hero's own model and the bodies the preset's items give it, while it is worn
        let mut targets = bodies(table, h);
        if targets.is_empty() {
            if !worn {
                continue;
            }
            for hero in &h.heroes {
                if let Some(model) = npcs.get(&crate::schema::norm(hero)).and_then(|npc| units.get(npc)) {
                    targets.push(model.clone());
                }
            }
            for other in in_use.iter().filter(|o| o.heroes.iter().any(|x| h.heroes.contains(x))) {
                targets.extend(bodies(table, other));
            }
        }
        for m in targets {
            let m = m.trim().to_ascii_lowercase();
            if m.ends_with(".vmdl") {
                models.entry(m).or_insert_with(|| hex.clone());
            }
        }
    }
    out.extend(models.into_iter().map(|(model, hex)| format!("gem|{model}|{hex}")));
    out
}

/// The preset's gems (item id -> colour's name) with their catalog items.
pub fn chosen(catalog: &crate::catalog::Catalog, gems: &BTreeMap<String, String>) -> Vec<(HeroItem, String)> {
    gems.iter()
        .filter_map(|(id, color)| {
            let skin = catalog.skins.iter().find(|s| &s.id == id)?;
            let item = crate::visuals::hero_items(&[skin]).into_iter().next()?;
            Some((item, color.clone()))
        })
        .collect()
}

/// The models an item puts on its hero (`entity_model` modifiers).
fn bodies(table: &Table, h: &HeroItem) -> Vec<String> {
    table
        .modifiers(&h.item)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.kind == "entity_model" && m.asset.as_deref().is_some_and(|a| a.starts_with("npc_dota_hero_")))
        .filter_map(|m| m.modifier)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r##""items_game"
{
	"colors"
	{
		"desc_strange"
		{
			"color_name"		"ItemAttribStrange"
			"hex_color"		"#CF6A32"
		}
		"unusual_dungeon_doom"
		{
			"color_name"		"UnusualDungeonDoom"
			"hex_color"		"#7B68EE"
		}
		"unusual_midas_gold"
		{
			"color_name"		"UnusualMidasGold"
			"hex_color"		"#ffca15"
		}
	}
	"items"
	{
		"5957"
		{
			"name"		"Fractal Horns of Inner Abysm"
			"image_inventory"		"econ/heroes/terrorblade/arcana_terrorblade"
			"item_slot"		"hero_base"
			"used_by_heroes"
			{
				"npc_dota_hero_terrorblade"		"1"
			}
			"visuals"
			{
				"asset_modifier"
				{
					"type"		"entity_model"
					"asset"		"npc_dota_hero_terrorblade"
					"modifier"		"models/heroes/terrorblade/terrorblade_arcana.vmdl"
				}
			}
		}
		"540"
		{
			"name"		"Terrorblade's Ambient Effects"
			"item_slot"		"ambient_effects"
			"used_by_heroes"
			{
				"npc_dota_hero_terrorblade"		"1"
			}
			"visuals"
			{
				"asset_modifier0"
				{
					"type"		"particle_create"
					"modifier"		"particles/units/heroes/hero_terrorblade/terrorblade_feet_effects.vpcf"
					"affected_by_color_gems"		"1"
				}
			}
		}
	}
}
"##;

    fn item(name: &str, index: u32) -> HeroItem {
        HeroItem {
            item: crate::visuals::ItemRef { name: name.into(), index: Some(index), style: 0 },
            heroes: vec!["Terrorblade".into()],
            slot: "hero_base".into(),
            model: None,
            siblings: Vec::new(),
        }
    }

    #[test]
    fn picked_colour_goes_to_the_hero_body() {
        let table = Table::new(TABLE.as_bytes());
        let units: HashMap<String, String> =
            [("npc_dota_hero_terrorblade".to_string(), "models/heroes/terrorblade/terrorblade.vmdl".to_string())].into();
        let npcs: HashMap<String, String> =
            [(crate::schema::norm("Terrorblade"), "npc_dota_hero_terrorblade".to_string())].into();
        let horns = item("Fractal Horns of Inner Abysm", 5957);
        let gems = vec![(horns.clone(), "UnusualMidasGold".to_string())];
        let palette = ["gemcolor|UnusualDungeonDoom|7B68EE", "gemcolor|UnusualMidasGold|FFCA15"];
        // the pages offering a gem: the item whose effects follow one, its hero's body
        let offered = [
            "gemitem|Fractal Horns of Inner Abysm|file://{images}/econ/heroes/terrorblade/arcana_terrorblade.png",
            "gemitem|Terrorblade's Ambient Effects|",
        ];
        // the arcana's body gets the colour, worn or not (the store page's preview)
        for owned in [HashSet::from([5957u32]), HashSet::new()] {
            assert_eq!(
                lines(&table, &gems, &[], &owned, &units, &npcs),
                [&palette[..], &offered[..], &[
                    "gempick|Fractal Horns of Inner Abysm|UnusualMidasGold",
                    "gem|models/heroes/terrorblade/terrorblade_arcana.vmdl|FFCA15",
                ][..]]
                .concat()
            );
        }
        // an item without a body: the hero's model and the preset's arcana body, while worn
        let ambient = item("Terrorblade's Ambient Effects", 540);
        let gems = vec![(ambient.clone(), "UnusualDungeonDoom".to_string())];
        let worn = vec![ambient, horns];
        assert_eq!(
            lines(&table, &gems, &worn, &HashSet::new(), &units, &npcs)[4..],
            [
                "gempick|Terrorblade's Ambient Effects|UnusualDungeonDoom",
                "gem|models/heroes/terrorblade/terrorblade.vmdl|7B68EE",
                "gem|models/heroes/terrorblade/terrorblade_arcana.vmdl|7B68EE",
            ]
        );
        assert_eq!(lines(&table, &gems, &[], &HashSet::new(), &units, &npcs).len(), 5);
    }
}
