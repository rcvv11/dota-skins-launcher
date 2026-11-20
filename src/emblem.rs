//! Makes a chosen emblem show in game on any hero, with plain file overrides.
//!
//! An emblem is an account item: the game spawns it only when it is really
//! equipped, so there is no file of it to replace. Instead:
//!
//! - **effect**: with changer.dll (memory mode) the DLL creates the emblem's
//!   hero particle on the hero you play, as a glow of "any hero" (see
//!   [`glow_line`]). Without it (file mode) it is copied over the selection ring
//!   (`particles/ui_mouseactions/selected_ring.vpcf`), which the client draws
//!   under the selected unit (with `r_draw_selected_ring 1`, see `autoexec.rs`):
//!   selecting a creep or a courier moved the emblem there;
//! - **attack colour**: an emblem's `emblem_attack_effect` (Diretide, TI10, the
//!   2021/2022 events alike) is no projectile of its own. The server names it in
//!   an owner's projectile message and the client creates it as a child of the
//!   projectile's effect, which gives it its points; copied over a hero's
//!   `ProjectileModel` it showed nothing. With changer.dll the DLL adds it to the
//!   attacks of the hero you play in the same way (see [`attack_line`]); file
//!   mode has no way to show it.

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

/// The emblem's file swap in file mode: its effect over the selection ring.
/// `None` = emblem not in the table.
pub fn ring_override(table: &Table, emblem: &ItemRef) -> Option<Vec<Req>> {
    let v = visuals(table, emblem)?;
    Some(
        v.effect
            .iter()
            .map(|effect| Req { id: emblem.name.clone(), src: compiled_path(effect), dst: compiled_path(RING) })
            .collect(),
    )
}

/// The DLL's line for the emblem's attack effect: added to the attacks of the
/// hero you play, as the game adds an owned emblem's.
pub fn attack_line(attack: &str) -> String {
    format!("attack|{attack}")
}

/// The DLL's line for the emblem's effect on the hero you play (any hero):
/// `drivers` from its particle file, or its origin on the hero when unknown.
pub fn glow_line(effect: &str, drivers: Vec<crate::glow::Driver>) -> String {
    let drivers = if drivers.is_empty() { vec![(0, "PATTACH_ABSORIGIN_FOLLOW".to_string(), String::new())] } else { drivers };
    crate::glow::config_line(crate::glow::ANY_HERO, effect, &drivers, None, &[effect.to_string()])
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
    fn ring_swap_and_attack_line() {
        let table = Table::new(TABLE.as_bytes());
        let ring = ring_override(&table, &emblem()).unwrap();
        assert_eq!(ring.len(), 1);
        assert_eq!(ring[0].dst, compiled_path(RING));
        assert_eq!(
            attack_line("particles/econ/events/diretide_2020/attack_modifier/attack_modifier_fall20.vpcf"),
            "attack|particles/econ/events/diretide_2020/attack_modifier/attack_modifier_fall20.vpcf"
        );
    }

}
