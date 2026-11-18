//! Item icons in Dota's slot tiles. The hero page shows each slot's equipped item
//! (the default one, or one you own) as a small icon, in the "equipped" row and in
//! the wardrobe's slot list; changer.dll puts the preset's item there instead.
//! A tile is known by the hero page's hero name and the tile's slot caption, so
//! the swap table gets `slotdef|<HERO>|<CAPTION>|<item>|<style>` (keys upper case,
//! letters and digits only; a caption in English and in Russian). The "equipped"
//! row's tiles have no caption: they follow the hero script's slot order, given as
//! `slotdef|<HERO>|#<position>|<item>|<style>` and `slotdef|<HERO>|#LEN|<count>|0`.
//! A last field is the item's icon as a Panorama image path: the script puts it
//! back cheaply when the game redraws a tile (empty for an item in a style).

use crate::visuals::HeroItem;

/// Upper case, letters and digits only: how the interface script compares names.
pub fn key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

pub fn config_lines(data: &crate::gamedata::GameData, items: &[HeroItem]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let icon = |h: &HeroItem| -> String {
        if h.item.style > 0 {
            return String::new();
        }
        data.index.image_inventory(&h.item).map(|i| format!("file://{{images}}/{}.png", i.trim())).unwrap_or_default()
    };
    for h in items {
        let Some(def) = h.item.index else { continue };
        let icon = icon(h);
        for hero in &h.heroes {
            let Some(npc) = data.npcs.get(&crate::schema::norm(hero)) else { continue };
            let mut captions = data.slot_texts.get(&(npc.to_ascii_lowercase(), h.slot.to_ascii_lowercase())).cloned().unwrap_or_default();
            if captions.is_empty() {
                captions.push(h.slot.clone());
            }
            for caption in captions {
                let line = format!("slotdef|{}|{}|{def}|{}|{icon}", key(hero), key(&caption), h.item.style);
                // one item per hero and caption: the first wins
                if seen.insert((key(hero), key(&caption))) {
                    lines.push(line);
                }
            }
        }
    }
    // the "equipped" row: positions in the hero's slot order
    let mut heroes: Vec<(&String, String)> = Vec::new();
    for h in items {
        for hero in &h.heroes {
            if let Some(npc) = data.npcs.get(&crate::schema::norm(hero)) {
                if !heroes.iter().any(|(_, n)| n == npc) {
                    heroes.push((hero, npc.clone()));
                }
            }
        }
    }
    for (hero, npc) in heroes {
        let Some(order) = data.slot_order.get(&npc.to_ascii_lowercase()) else { continue };
        lines.push(format!("slotdef|{}|#LEN|{}|0", key(hero), order.len()));
        for (pos, slot) in order.iter().enumerate() {
            let picked = items.iter().find(|h| h.slot.eq_ignore_ascii_case(slot) && h.heroes.iter().any(|x| x == hero));
            if let Some(h) = picked {
                if let Some(def) = h.item.index {
                    lines.push(format!("slotdef|{}|#{pos}|{def}|{}|{}", key(hero), h.item.style, icon(h)));
                }
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn keys_compare_like_the_script() {
        assert_eq!(super::key("Anti-Mage"), "ANTIMAGE");
        assert_eq!(super::key("Доп. оружие"), "ДОПОРУЖИЕ");
        assert_eq!(super::key("LIFESTEALER"), "LIFESTEALER");
    }
}
