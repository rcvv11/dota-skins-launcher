//! Item icons in Dota's slot tiles. The hero page shows each slot's equipped item
//! (the default one, or one you own) as a small icon, in the "equipped" row and in
//! the wardrobe's slot list; changer.dll puts the preset's item there instead.
//! A tile is known by the hero page's hero name and the tile's slot caption, so
//! the swap table gets `slotdef|<HERO>|<CAPTION>|<item>|<style>` (keys upper case,
//! letters and digits only; a caption in English and in Russian). The "equipped"
//! row's tiles have no caption: they follow the hero script's slot order, given as
//! `slotdef|<HERO>|#<position>|<item>|<style>` and `slotdef|<HERO>|#LEN|<count>|0`.
//! Then the item's icon as a Panorama image path (an item in a style: that style's
//! icon) and its rarity: the script lays the icon over the game's own and colours the
//! rarity strip itself, so the game redrawing a tile does not show.
//! A persona's slots have the classic ones' captions: theirs are `PERSONA<CAPTION>`,
//! and the persona's "equipped" row `#P<position>`/`#PLEN` (its slots shown in the
//! loadout) and `#Q<position>`/`#QLEN` (all its item slots); the script takes these
//! while the hero page shows the persona.

use crate::visuals::HeroItem;

/// Upper case, letters and digits only: how the interface script compares names.
pub fn key(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_uppercase()).collect()
}

pub fn config_lines(data: &crate::gamedata::GameData, items: &[HeroItem]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // always our icon (an item in a style: that style's): Dota redraws the tile with the
    // account's own item, and only our image over it keeps the preset's showing
    let icon = |h: &HeroItem| -> String {
        data.index.style_icon(&h.item).map(|i| format!("file://{{images}}/{}.png", i.trim())).unwrap_or_default()
    };
    // the tile's rarity strip (the script colours it itself: Dota's redraws reset it)
    let rarity = |h: &HeroItem| data.index.rarity(&h.item).unwrap_or_default();
    for h in items {
        let Some(def) = h.item.index else { continue };
        let icon = icon(h);
        let rarity = rarity(h);
        for hero in &h.heroes {
            let Some(npc) = data.npcs.get(&crate::schema::norm(hero)) else { continue };
            let mut captions = data.slot_texts.get(&(npc.to_ascii_lowercase(), h.slot.to_ascii_lowercase())).cloned().unwrap_or_default();
            if captions.is_empty() {
                captions.push(h.slot.clone());
            }
            let prefix = if h.slot.ends_with("_persona_1") { "PERSONA" } else { "" };
            for caption in captions {
                let caption = format!("{prefix}{caption}");
                let line = format!("slotdef|{}|{}|{def}|{}|{icon}|{rarity}", key(hero), key(&caption), h.item.style);
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
        let npc = npc.to_ascii_lowercase();
        let orders = [("#", data.slot_order.get(&npc)), ("#P", data.persona_order.get(&npc)), ("#Q", data.persona_all.get(&npc))];
        for (tag, order) in orders {
            let Some(order) = order else { continue };
            lines.push(format!("slotdef|{}|{tag}LEN|{}|0", key(hero), order.len()));
            for (pos, slot) in order.iter().enumerate() {
                let picked = items.iter().find(|h| h.slot.eq_ignore_ascii_case(slot) && h.heroes.iter().any(|x| x == hero));
                if let Some(h) = picked {
                    if let Some(def) = h.item.index {
                        lines.push(format!("slotdef|{}|{tag}{pos}|{def}|{}|{}|{}", key(hero), h.item.style, icon(h), rarity(h)));
                    }
                }
            }
        }
    }
    lines
}

/// Dota's persona switch (changer.dll): every hero's persona, `personadef|<HERO>|<hero
/// id>|<persona item>|<npc>` (a press on the switch, on the hero page or the strategy
/// screen, shows it at once), and the ones the preset has, `persona|<hero id>`: the hero
/// page shows those as worn.
pub fn persona_lines(data: &crate::gamedata::GameData, catalog: &crate::catalog::Catalog, items: &[HeroItem]) -> Vec<String> {
    let mut lines = Vec::new();
    for s in catalog.skins.iter().filter(|s| s.slot == "persona_selector") {
        let Some(def) = s.index else { continue };
        for hero in s.hero_names() {
            let Some(npc) = data.npcs.get(&crate::schema::norm(hero)) else { continue };
            let Some(id) = data.hero_ids.get(&npc.to_ascii_lowercase()) else { continue };
            lines.push(format!("personadef|{}|{id}|{def}|{}", key(hero), npc.to_ascii_lowercase()));
            if items.iter().any(|h| h.slot == "persona_selector" && h.item.index == Some(def)) {
                lines.push(format!("persona|{id}"));
            }
        }
    }
    lines
}

/// The heroes the game dresses itself in the menu (changer.dll): a hero whose preset has a
/// body item (`hero_base`, an arcana: Legion Commander's Blades of Voth Domosh) gets its
/// skin, stance and effects from the game only, so all its preset items are shown as worn,
/// `wear|<hero id>|<npc>|<slot>|<item>|<style>|<own glows>`. Also a hero with an item giving
/// it another body (`entity_model`, Razor's Voidstorm Asylum Tormentor): the game then knows
/// the body is the item's, and makes the hero anew when the item goes (the page kept him,
/// given his own body by name in place, and drew nothing of it). The game makes none of such
/// an item's effects: "1", the script's glows stay on the hero.
pub fn wear_lines(data: &crate::gamedata::GameData, items: &[HeroItem]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut bodies: Vec<(&String, bool)> = Vec::new();
    for h in items.iter().filter(|h| h.item.index.is_some()) {
        let entity_model = data
            .index
            .modifiers(&h.item)
            .is_some_and(|mods| mods.iter().any(|m| m.kind == "entity_model"));
        if h.slot != "hero_base" && !entity_model {
            continue;
        }
        for hero in &h.heroes {
            match bodies.iter_mut().find(|(n, _)| *n == hero) {
                Some(b) => b.1 |= entity_model,
                None => bodies.push((hero, entity_model)),
            }
        }
    }
    for (hero, own_glows) in bodies {
        let Some(npc) = data.npcs.get(&crate::schema::norm(hero)) else { continue };
        let Some(id) = data.hero_ids.get(&npc.to_ascii_lowercase()) else { continue };
        for h in items.iter().filter(|h| h.heroes.contains(hero) && !crate::catalog::is_persona_slot(&h.slot)) {
            if let Some(def) = h.item.index {
                lines.push(format!("wear|{id}|{npc}|{}|{def}|{}|{}", h.slot, h.item.style, u8::from(own_glows)));
            }
        }
    }
    lines
}

/// The stance the preset's items give the hero in the menu (changer.dll): their activity
/// modifiers, `actmod|<npc>|<hero id>|<modifier>|<1: a persona's>` (Legion Commander's
/// arcana: `dualwield` and `arcana`, both swords in hand). A taunt's is its animation, not
/// a stance.
pub fn actmod_lines(data: &crate::gamedata::GameData, items: &[HeroItem]) -> Vec<String> {
    let mut lines = Vec::new();
    for h in items.iter().filter(|h| !h.slot.eq_ignore_ascii_case("taunt")) {
        let Some(mods) = data.index.modifiers(&h.item) else { continue };
        let persona = u8::from(crate::catalog::is_persona_slot(&h.slot));
        for m in mods.iter().filter(|m| m.kind == "activity") {
            // on every activity ("ALL", or none said): one for a single activity is a taunt's
            if m.asset.as_deref().is_some_and(|a| !a.is_empty() && !a.eq_ignore_ascii_case("ALL")) {
                continue;
            }
            let Some(modifier) = m.modifier.as_deref().map(str::trim).filter(|s| !s.is_empty() && !s.contains('|')) else {
                continue;
            };
            for hero in &h.heroes {
                let Some(npc) = data.npcs.get(&crate::schema::norm(hero)) else { continue };
                let id = data.hero_ids.get(&npc.to_ascii_lowercase()).copied().unwrap_or_default();
                let line = format!("actmod|{npc}|{id}|{modifier}|{persona}");
                if !lines.contains(&line) {
                    lines.push(line);
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
