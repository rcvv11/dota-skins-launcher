//! Names shown in Dota's item rows -> catalog items. A row of the hero page
//! shows the item's (or set's) localized name; the game's item table
//! (`scripts/items/items_game.txt`) gives each item its token and each set
//! ("bundle") its members, the localization files give the token's text.

use std::collections::HashMap;
use std::path::Path;

const LOCALIZATION: &[&str] = &[
    "resource/localization/items_english.txt",
    "resource/localization/items_russian.txt",
];

/// Display name (lowercase) -> internal item names (= catalog ids).
#[derive(Default)]
pub struct NameIndex {
    by_name: HashMap<String, Vec<String>>,
}

impl NameIndex {
    /// Reads the table and localizations from the game's packs.
    pub fn load(game_dir: &Path) -> Result<Self, String> {
        let table = crate::vpk::read_file(game_dir, crate::schema::SCHEMA_PATH)?
            .ok_or(crate::tr!("в паке нет items_game.txt", "the pak has no items_game.txt"))?;
        let mut tokens = HashMap::new();
        for file in LOCALIZATION {
            if let Ok(Some(text)) = crate::vpk::read_file(game_dir, file) {
                parse_tokens(&text, &mut tokens);
            }
        }
        Ok(Self::build(&table, &tokens))
    }

    fn build(table: &[u8], tokens: &HashMap<String, Vec<String>>) -> Self {
        let mut index = Self::default();
        for item in parse_items(table) {
            let members = if item.bundle.is_empty() { vec![item.name.clone()] } else { item.bundle.clone() };
            let mut names = vec![item.name.clone()];
            if let Some(texts) = item.token.as_deref().and_then(|t| tokens.get(&t.trim_start_matches('#').to_lowercase())) {
                names.extend(texts.iter().cloned());
            }
            // a set's card names it without the word «set» («The Eminence of Ristul»,
            // the set's type shown apart): its name without "Bundle" / «Комплект» too
            if !item.bundle.is_empty() {
                let short: Vec<String> = names.iter().filter_map(|n| short_set_name(n)).collect();
                names.extend(short);
            }
            for n in names {
                // the first definition of a name wins (sets and items rarely share one)
                index.by_name.entry(norm(&n)).or_insert_with(|| members.clone());
            }
        }
        index
    }

    /// Items named by the first label that is a known item or set name.
    pub fn resolve(&self, labels: &[String]) -> Vec<String> {
        labels.iter().find_map(|l| self.by_name.get(&norm(l))).cloned().unwrap_or_default()
    }
}

/// A set's name without its "Bundle" / «Комплект» word, when it has one.
fn short_set_name(name: &str) -> Option<String> {
    let n = name.trim();
    let lower = n.to_lowercase();
    for suffix in [" bundle", " set"] {
        if lower.ends_with(suffix) {
            return Some(n[..n.len() - suffix.len()].trim().to_string());
        }
    }
    for prefix in ["комплект ", "набор "] {
        if lower.starts_with(prefix) {
            return Some(n.chars().skip(prefix.chars().count()).collect::<String>().trim().to_string());
        }
    }
    None
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

struct ItemDef {
    name: String,
    token: Option<String>,
    bundle: Vec<String>,
}

/// Quoted strings and braces of a KeyValues text.
pub(crate) fn tokenize(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < text.len() && text[j] != b'"' {
                    if text[j] == b'\\' {
                        j += 1;
                    }
                    j += 1;
                }
                out.push(&text[start..j.min(text.len())]);
                i = j + 1;
            }
            b'{' | b'}' => {
                out.push(&text[i..i + 1]);
                i += 1;
            }
            b'/' if text.get(i + 1) == Some(&b'/') => {
                while i < text.len() && text[i] != b'\n' {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Items under "items_game" > "items": name, item_name token, bundle members.
fn parse_items(text: &[u8]) -> Vec<ItemDef> {
    let toks = tokenize(text);
    let mut out = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut key: Option<&[u8]> = None;
    let mut cur: Option<ItemDef> = None;
    for t in toks {
        match t {
            b"{" => {
                path.push(key.take().map(s).unwrap_or_default().to_lowercase());
                if path.len() == 3 && path[1] == "items" {
                    cur = Some(ItemDef { name: String::new(), token: None, bundle: Vec::new() });
                }
            }
            b"}" => {
                if path.len() == 3 && path[1] == "items" {
                    if let Some(item) = cur.take().filter(|i| !i.name.is_empty()) {
                        out.push(item);
                    }
                }
                path.pop();
                key = None;
            }
            _ => match key.take() {
                None => key = Some(t),
                Some(k) => {
                    if let Some(item) = cur.as_mut() {
                        if path.len() == 3 {
                            match s(k).to_lowercase().as_str() {
                                "name" => item.name = s(t),
                                "item_name" => item.token = Some(s(t)),
                                _ => {}
                            }
                        } else if path.len() == 4 && path[3] == "bundle" {
                            item.bundle.push(s(k));
                        }
                    }
                }
            },
        }
    }
    out
}

/// "Tokens" { "key" "text" } of a localization file, keys lowercased.
pub(crate) fn parse_tokens(text: &[u8], out: &mut HashMap<String, Vec<String>>) {
    let text = text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(text);
    let toks = tokenize(text);
    let mut depth = 0;
    let mut key: Option<&[u8]> = None;
    for t in toks {
        match t {
            b"{" => {
                depth += 1;
                key = None;
            }
            b"}" => {
                depth -= 1;
                key = None;
            }
            _ => match key.take() {
                None => key = Some(t),
                Some(k) if depth == 2 => {
                    let texts = out.entry(s(k).to_lowercase()).or_default();
                    let v = s(t);
                    if !texts.contains(&v) {
                        texts.push(v);
                    }
                }
                Some(_) => {}
            },
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
			"name"		"Blade of the Survivor"
			"item_name"		"#DOTA_Item_Blade_of_the_Survivor"
		}
		"21051"
		{
			"name"		"Guilt of the Survivor"
			"prefab"		"bundle"
			"bundle"
			{
				"Blade of the Survivor"		"1"
				"Hair of the Survivor"		"1"
			}
			"item_name"		"#DOTA_Item_Guilt_of_the_Survivor"
		}
	}
}"##;

    #[test]
    fn sets_resolve_to_members_items_to_themselves() {
        let mut tokens = HashMap::new();
        parse_tokens(
            "\u{FEFF}\"lang\"\n{\n\t\"Tokens\"\n\t{\n\t\t\"DOTA_Item_Guilt_of_the_Survivor\"\t\"Вина выжившего\"\n\t}\n}".as_bytes(),
            &mut tokens,
        );
        let index = NameIndex::build(TABLE.as_bytes(), &tokens);
        let labels = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(index.resolve(&labels(&["1 200", "Guilt of the  Survivor"])), vec!["Blade of the Survivor", "Hair of the Survivor"]);
        assert_eq!(index.resolve(&labels(&["вина выжившего"])), vec!["Blade of the Survivor", "Hair of the Survivor"]);
        assert_eq!(index.resolve(&labels(&["Blade of the Survivor"])), vec!["Blade of the Survivor"]);
        assert!(index.resolve(&labels(&["Купить"])).is_empty());
        // a set's card names it without "Bundle" (and in Russian without «Комплект»)
        let bundled = TABLE.replace("\"Guilt of the Survivor\"", "\"Guilt of the Survivor Bundle\"");
        let index = NameIndex::build(bundled.as_bytes(), &tokens);
        assert_eq!(index.resolve(&labels(&["Guilt of the Survivor"])), vec!["Blade of the Survivor", "Hair of the Survivor"]);
        assert_eq!(short_set_name("The Eminence of Ristul Bundle").as_deref(), Some("The Eminence of Ristul"));
        assert_eq!(short_set_name("Комплект The Eminence of Ristul").as_deref(), Some("The Eminence of Ristul"));
        assert_eq!(short_set_name("Blade of the Survivor"), None);
    }
}
