//! Item sets ("bundles" in the game's item table): their members, icon and
//! rarity, read once from the game's packs in the background (the catalog keeps
//! single items only). The Heroes tab shows a hero's sets and puts a whole set on
//! with one click.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use crate::catalog::{Catalog, Skin};
use crate::sets::tokenize;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bundle {
    /// The set's name in the table (e.g. "Jagged Honor").
    pub name: String,
    /// Member item names (= catalog ids).
    pub members: Vec<String>,
    /// `image_inventory` of the set item.
    pub icon: Option<String>,
    pub rarity: String,
    pub index: Option<u32>,
}

static BUNDLES: OnceLock<Vec<Bundle>> = OnceLock::new();

/// Sets once loaded (empty until then, or if the game was not found).
pub fn all() -> &'static [Bundle] {
    BUNDLES.get().map(Vec::as_slice).unwrap_or(&[])
}

/// Starts reading the sets from the game's packs (once).
pub fn load_in_background(dota_exe: String, ctx: egui::Context) {
    if BUNDLES.get().is_some() {
        return;
    }
    std::thread::spawn(move || {
        let Some(dir) = crate::vpk::game_dir_from_exe(&dota_exe) else { return };
        let Ok(Some(table)) = crate::vpk::read_file(&dir, crate::schema::SCHEMA_PATH) else { return };
        let _ = BUNDLES.set(parse(&table));
        ctx.request_repaint();
    });
}

/// The hero's sets: at least two catalog pieces the hero wears, none for another
/// hero (persona sets only on the persona screen, the others only on the normal one).
pub fn for_hero<'a>(catalog: &'a Catalog, hero: &str, persona: bool) -> Vec<(&'static Bundle, Vec<&'a Skin>)> {
    let by_id: std::collections::HashMap<&str, &Skin> = catalog.skins.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut out = Vec::new();
    for b in all() {
        // the set's pieces this hero wears; pieces of no hero (its loading screen)
        // come along, pieces missing from the catalog are left out
        let found: Vec<&Skin> = b.members.iter().filter_map(|m| by_id.get(m.as_str()).copied()).collect();
        let hero_pieces = found.iter().filter(|s| s.fits_hero(hero)).count();
        if hero_pieces < 2 || found.iter().any(|s| !s.fits_hero(hero) && !s.is_general()) {
            continue;
        }
        let persona_set = found.iter().any(|s| crate::catalog::is_persona_slot(&s.slot));
        if persona_set == persona {
            out.push((b, found));
        }
    }
    out.sort_by_key(|(b, _)| std::cmp::Reverse(b.index.unwrap_or(0)));
    out
}

/// Sets under "items_game" > "items": items with a `bundle` block.
pub fn parse(text: &[u8]) -> Vec<Bundle> {
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut key: Option<&[u8]> = None;
    let mut cur = Bundle::default();
    for t in tokenize(text) {
        match t {
            b"{" => {
                path.push(key.take().map(s).unwrap_or_default().to_lowercase());
                if path.len() == 3 && path[1] == "items" {
                    cur = Bundle { index: path[2].trim().parse().ok(), ..Default::default() };
                }
            }
            b"}" => {
                if path.len() == 3 && path[1] == "items" && !cur.name.is_empty() && !cur.members.is_empty() {
                    // a set lists its members once; keep their order unique
                    let mut seen = BTreeSet::new();
                    cur.members.retain(|m| seen.insert(m.clone()));
                    out.push(std::mem::take(&mut cur));
                }
                path.pop();
                key = None;
            }
            _ => match key.take() {
                None => key = Some(t),
                Some(k) => {
                    if path.len() == 3 && path[1] == "items" {
                        match s(k).to_lowercase().as_str() {
                            "name" => cur.name = s(t),
                            "image_inventory" => cur.icon = Some(s(t)).filter(|i| !i.is_empty()),
                            "item_rarity" => cur.rarity = s(t).to_lowercase(),
                            _ => {}
                        }
                    } else if path.len() == 4 && path[1] == "items" && path[3] == "bundle" {
                        cur.members.push(s(k));
                    }
                }
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r##""items_game"
{
	"items"
	{
		"200"
		{
			"name"		"Jagged Honor"
			"prefab"		"bundle"
			"image_inventory"		"econ/sets/jagged_honor"
			"item_rarity"		"mythical"
			"bundle"
			{
				"Jagged Honor Mask"		"1"
				"Jagged Honor Blade"		"1"
			}
		}
		"201"
		{
			"name"		"Jagged Honor Mask"
			"prefab"		"wearable"
		}
	}
}"##;

    #[test]
    fn sets_with_members() {
        let b = parse(TABLE.as_bytes());
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name, "Jagged Honor");
        assert_eq!(b[0].members, vec!["Jagged Honor Mask".to_string(), "Jagged Honor Blade".to_string()]);
        assert_eq!(b[0].icon.as_deref(), Some("econ/sets/jagged_honor"));
        assert_eq!(b[0].rarity, "mythical");
        assert_eq!(b[0].index, Some(200));
    }
}
