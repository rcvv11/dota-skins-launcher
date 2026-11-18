//! Makes a chosen emblem show in game on any hero, with plain file overrides.
//!
//! An emblem is an account item: the game spawns it only when it is really
//! equipped, so there is no file of it to replace. Instead:
//!
//! - **effect**: the emblem's hero particle is copied over the selection ring
//!   (`particles/ui_mouseactions/selected_ring.vpcf`). The client draws that
//!   ring under the hero you control (with `r_draw_selected_ring 1`, see
//!   `autoexec.rs`), whichever hero it is;
//! - **attack colour**: Diretide emblems carry an `emblem_attack_effect` — a
//!   complete tracking projectile (CP0 → CP1, speed from CP2, same layout as a
//!   hero's `*_base_attack.vpcf`). It is copied over the `ProjectileModel` of
//!   every ranged hero.

use std::path::Path;

use crate::extract::{compiled_path, Req};
use crate::visuals::{ItemRef, Table};

/// The particle the client draws under the selected unit.
pub const RING: &str = "particles/ui_mouseactions/selected_ring.vpcf";

/// Entities that only exist in the loadout preview, not in a match.
const PREVIEW_ASSET: &str = "npc_dota_loadout_generic";

/// The preset's emblem (a preset holds one per slot).
pub fn from_preset(chosen: &[&crate::catalog::Skin]) -> Option<ItemRef> {
    chosen
        .iter()
        .find(|s| crate::catalog::is_emblem(&s.slot))
        .map(|s| ItemRef::of(s))
}

/// What an emblem shows, read from its item-table block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Visuals {
    /// `.vpcf` spawned on the hero in a match.
    pub effect: Option<String>,
    /// `.vpcf` of the attack projectile, if the emblem has one.
    pub attack: Option<String>,
}

pub fn visuals(table: &Table, emblem: &ItemRef) -> Option<Visuals> {
    let mut out = Visuals::default();
    for m in table.modifiers(emblem)? {
        let Some(modifier) = m.modifier.filter(|p| p.ends_with(".vpcf")) else { continue };
        match (m.kind.as_str(), m.asset.as_deref()) {
            ("particle_create", asset) if asset != Some(PREVIEW_ASSET) => {
                out.effect.get_or_insert(modifier);
            }
            ("diretide_effect", Some("emblem_attack_effect")) => {
                out.attack.get_or_insert(modifier);
            }
            _ => {}
        }
    }
    Some(out)
}

/// `ProjectileModel` of a ranged hero from its script, `None` for melee heroes.
pub fn projectile(script: &str) -> Option<String> {
    let mut ranged = false;
    let mut model = None;
    for line in script.lines() {
        let line = line.split("//").next().unwrap_or("");
        let mut parts = line.split('"').skip(1).step_by(2);
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else { continue };
        if key.eq_ignore_ascii_case("AttackCapabilities") && value.contains("RANGED") {
            ranged = true;
        }
        if model.is_none() && key.eq_ignore_ascii_case("ProjectileModel") && value.ends_with(".vpcf") {
            model = Some(value.to_string());
        }
    }
    model.filter(|_| ranged)
}

/// File overrides for the emblem: its effect over the selection ring and its
/// projectile over every ranged hero's. `Ok(None)` = emblem not in the table.
pub fn overrides(game_dir: &Path, table: &Table, emblem: &ItemRef) -> Result<Option<Vec<Req>>, String> {
    let Some(v) = visuals(table, emblem) else { return Ok(None) };
    let mut reqs = Vec::new();
    if let Some(effect) = &v.effect {
        reqs.push(Req {
            id: emblem.name.clone(),
            src: compiled_path(effect),
            dst: compiled_path(RING),
        });
    }
    if let Some(attack) = &v.attack {
        const DIR: &str = "scripts/npc/heroes/";
        let scripts = crate::vpk::read_matching(game_dir, |p| {
            p.strip_prefix(DIR)
                .and_then(|f| f.strip_suffix(".txt"))
                .is_some_and(|f| f.starts_with("npc_dota_hero_") && !f.ends_with("_base"))
        })?;
        let mut paths: Vec<&String> = scripts.keys().collect();
        paths.sort();
        let src = compiled_path(attack);
        for path in paths {
            if let Some(own) = projectile(&String::from_utf8_lossy(&scripts[path])) {
                reqs.push(Req {
                    id: emblem.name.clone(),
                    src: src.clone(),
                    dst: compiled_path(&own),
                });
            }
        }
    }
    Ok(Some(reqs))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r#""items_game"
{
	"items"
	{
		"13453"
		{
			"name"		"Enigmatic Emblem of the Diretide - Yellow"
			"prefab"		"emblem"
			"visuals"
			{
				"asset_modifier"
				{
					"type"		"particle_create"
					"modifier"		"particles/econ/events/diretide_2020/emblem/fall20_emblem_effect.vpcf"
					"spawn_in_loadout_only"		"2"
				}
				"asset_modifier"
				{
					"type"		"diretide_effect"
					"asset"		"emblem_attack_effect"
					"modifier"		"particles/econ/events/diretide_2020/attack_modifier/attack_modifier_fall20.vpcf"
				}
				"asset_modifier"
				{
					"type"		"particle_create"
					"asset"		"npc_dota_loadout_generic"
					"modifier"		"particles/econ/events/diretide_2020/emblem/fall20_emblem_loadout.vpcf"
				}
			}
		}
	}
}
"#;

    fn emblem() -> ItemRef {
        ItemRef { name: "Enigmatic Emblem of the Diretide - Yellow".into(), index: Some(13453), style: 0 }
    }

    #[test]
    fn reads_effect_and_attack() {
        let table = Table::new(TABLE.as_bytes());
        let v = visuals(&table, &emblem()).unwrap();
        assert_eq!(
            v.effect.as_deref(),
            Some("particles/econ/events/diretide_2020/emblem/fall20_emblem_effect.vpcf"),
            "loadout preview effect must be left out"
        );
        assert_eq!(
            v.attack.as_deref(),
            Some("particles/econ/events/diretide_2020/attack_modifier/attack_modifier_fall20.vpcf")
        );
        let mut by_name = emblem();
        by_name.index = None;
        assert_eq!(visuals(&table, &by_name), Some(v));
    }

    #[test]
    fn projectile_only_for_ranged() {
        let lina = "\"npc_dota_hero_lina\"\n{\n\t\"AttackCapabilities\"\t\t\"DOTA_UNIT_CAP_RANGED_ATTACK\"\n\t\"ProjectileModel\"\t\t\"particles/units/heroes/hero_lina/lina_base_attack.vpcf\" // fire\n}";
        assert_eq!(
            projectile(lina).as_deref(),
            Some("particles/units/heroes/hero_lina/lina_base_attack.vpcf")
        );
        let am = "\"AttackCapabilities\"\t\"DOTA_UNIT_CAP_MELEE_ATTACK\"\n\"ProjectileModel\"\t\"\"";
        assert_eq!(projectile(am), None);
    }

}
