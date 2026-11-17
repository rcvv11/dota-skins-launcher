//! Item styles (`visuals` > `styles` in the game's item table): their display
//! names, read once from the game's packs in the background. A preset keeps the
//! chosen style of an item in `Profile::styles` (item name -> style number).

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use crate::sets::{parse_tokens, tokenize};

const LOCALIZATION: &[&str] = &[
    "resource/localization/dota_english.txt",
    "resource/localization/dota_russian.txt",
    "resource/localization/items_english.txt",
    "resource/localization/items_russian.txt",
];

/// Item name (lowercase) -> its styles' display names, style 0 first.
pub type Styles = HashMap<String, Vec<String>>;

static STYLES: OnceLock<Styles> = OnceLock::new();

/// Styles once loaded (empty until then, or if the game was not found).
pub fn get() -> Option<&'static Styles> {
    STYLES.get()
}

/// Display names of the item's styles; empty when it has fewer than two.
pub fn of(item_name: &str) -> &'static [String] {
    get().and_then(|s| s.get(&item_name.trim().to_lowercase())).map(Vec::as_slice).unwrap_or(&[])
}

/// Starts reading the styles from the game's packs (once).
pub fn load_in_background(dota_exe: String, ctx: egui::Context) {
    if STYLES.get().is_some() {
        return;
    }
    std::thread::spawn(move || {
        let Some(dir) = crate::vpk::game_dir_from_exe(&dota_exe) else { return };
        let Ok(Some(table)) = crate::vpk::read_file(&dir, crate::schema::SCHEMA_PATH) else { return };
        let mut tokens = HashMap::new();
        // the UI language's file last: its text is taken first below
        let mut files: Vec<&str> = LOCALIZATION.to_vec();
        if crate::i18n::is_en() {
            files.reverse();
        }
        for f in files {
            if let Ok(Some(text)) = crate::vpk::read_file(&dir, f) {
                parse_tokens(&text, &mut tokens);
            }
        }
        let _ = STYLES.set(resolve(parse(&table), &tokens));
        ctx.request_repaint();
    });
}

/// Tokens -> texts (the token itself when it has no text).
fn resolve(raw: HashMap<String, Vec<String>>, tokens: &HashMap<String, Vec<String>>) -> Styles {
    raw.into_iter()
        .map(|(item, names)| {
            let texts = names
                .into_iter()
                .enumerate()
                .map(|(i, n)| {
                    let key = n.trim_start_matches('#').to_lowercase();
                    // the last file read wins (parse_tokens appends)
                    tokens.get(&key).and_then(|t| t.last().cloned()).filter(|t| !t.is_empty()).unwrap_or_else(|| {
                        if n.is_empty() { crate::trf!("Стиль {}", "Style {}", i + 1) } else { n }
                    })
                })
                .collect();
            (item, texts)
        })
        .collect()
}

/// Item name (lowercase) -> style name tokens, for items with two or more styles.
pub fn parse(text: &[u8]) -> HashMap<String, Vec<String>> {
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let mut out = HashMap::new();
    let mut path: Vec<String> = Vec::new();
    let mut key: Option<&[u8]> = None;
    let mut name = String::new();
    let mut styles: BTreeMap<u32, String> = BTreeMap::new();
    for t in tokenize(text) {
        match t {
            b"{" => {
                path.push(key.take().map(s).unwrap_or_default().to_lowercase());
                if path.len() == 3 && path[1] == "items" {
                    name.clear();
                    styles.clear();
                }
                if path.len() == 6 && path[1] == "items" && path[3] == "visuals" && path[4] == "styles" {
                    if let Ok(n) = path[5].trim().parse() {
                        styles.entry(n).or_default();
                    }
                }
            }
            b"}" => {
                if path.len() == 3 && path[1] == "items" && styles.len() > 1 && !name.is_empty() {
                    let last = *styles.keys().last().unwrap();
                    let list = (0..=last).map(|i| styles.get(&i).cloned().unwrap_or_default()).collect();
                    out.insert(name.to_lowercase(), list);
                }
                path.pop();
                key = None;
            }
            _ => match key.take() {
                None => key = Some(t),
                Some(k) => {
                    if path.len() == 3 && path[1] == "items" && k.eq_ignore_ascii_case(b"name") {
                        name = s(t);
                    } else if path.len() == 6 && path[1] == "items" && path[3] == "visuals" && path[4] == "styles"
                        && k.eq_ignore_ascii_case(b"name")
                    {
                        if let Ok(n) = path[5].trim().parse() {
                            styles.insert(n, s(t));
                        }
                    }
                }
            },
        }
    }
    out
}

/// Style picked in Dota's item card: its «2 / 3» label (1-based), if any.
pub fn from_labels(labels: &[String]) -> Option<u32> {
    labels.iter().find_map(|l| {
        let (a, b) = l.split_once('/')?;
        let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
        (b > 1 && a >= 1 && a <= b).then(|| a - 1)
    })
}

/// Puts the style of `item` into the preset, and the same style to the worn
/// pieces of its set (named "<item> - Bracers" etc.). Style 0 is not stored.
pub fn set(styles: &mut BTreeMap<String, u32>, item: &str, style: u32, worn: &[String]) {
    let base = item.trim();
    let mut names = vec![base.to_string()];
    for w in worn {
        if w != base && w.starts_with(base) && w[base.len()..].trim_start().starts_with('-') {
            names.push(w.clone());
        }
    }
    for n in names {
        if style == 0 {
            styles.remove(&n);
        } else {
            styles.insert(n, style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = r##""items_game"
{
	"items"
	{
		"100"
		{
			"name"		"Dread Retribution"
			"visuals"
			{
				"asset_modifier" { "type" "entity_model" }
				"styles"
				{
					"0" { "name" "#Style0" "skin" "0" }
					"1" { "name" "#Style1" "skin" "1" }
				}
			}
		}
		"101"
		{
			"name"		"Plain"
			"visuals" { "styles" { "0" { "name" "#Only" } } }
		}
	}
}"##;

    #[test]
    fn styles_by_item_name() {
        let raw = parse(TABLE.as_bytes());
        assert_eq!(raw.get("dread retribution"), Some(&vec!["#Style0".to_string(), "#Style1".to_string()]));
        assert!(!raw.contains_key("plain"));
        let mut tokens = HashMap::new();
        tokens.insert("style0".to_string(), vec!["Cold of Heart".to_string()]);
        let named = resolve(raw, &tokens);
        assert_eq!(named["dread retribution"], vec!["Cold of Heart".to_string(), "#Style1".to_string()]);
    }

    #[test]
    fn card_label_gives_style() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(from_labels(&l(&["Dread Retribution", "2 / 2"])), Some(1));
        assert_eq!(from_labels(&l(&["X", "1 / 2"])), Some(0));
        assert_eq!(from_labels(&l(&["X", "1 / 0"])), None);
        assert_eq!(from_labels(&l(&["X", "1 200"])), None);
    }
}
