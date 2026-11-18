//! "Heroes" tab: pick a hero, see its slots like the in-game armory, and choose
//! an item per slot for the active preset.

use super::*;

const TILE_W: f32 = 128.0;
const TILE_H: f32 = 98.0;
const HERO_W: f32 = 150.0;
const HERO_H: f32 = 112.0;
const GAP: f32 = 12.0;

/// Display order of slots; unknown slots go last, persona variants after their base.
const SLOT_ORDER: &[&str] = &[
    "weapon", "offhand_weapon", "weapon2", "head", "shoulder", "arms", "armor", "back", "belt",
    "neck", "legs", "tail", "gloves", "mount", "summon", "misc", "body_head", "costume",
    "shapeshift", "hero_base", "ability1", "ability2", "ability3", "ability4", "ability_ultimate",
    "ambient_effects", "taunt", "voice", "persona_selector",
    // general (not hero) slots
    "courier", "ward", "terrain", "weather", "emblem", "hud_skin", "cursor_pack", "announcer",
    "mega_kills", "music", "loading_screen", "versus_screen", "teleport_effect", "streak_effect",
    "map_effect", "multikill_banner", "heroic_statue", "pet_effigy", "radiant_creeps",
    "dire_creeps", "radiant_siege_creeps", "dire_siege_creeps", "radiant_towers", "dire_towers",
    "roshan", "tormentor",
];

/// The "Sets" tile's key among the slots.
const SETS: &str = "__sets";

fn slot_rank(slot: &str) -> (usize, usize, String) {
    let (base, persona) = match slot.strip_suffix("_persona_1") {
        Some(b) => (b, 1),
        None => (slot, 0),
    };
    let pos = SLOT_ORDER.iter().position(|s| *s == base).unwrap_or(SLOT_ORDER.len());
    (persona, pos, slot.to_string())
}

use crate::catalog::is_persona_slot;

/// Slot caption without the "(persona)" suffix — the tab already says it.
fn tile_label(slot: &str) -> String {
    if slot == SETS {
        return crate::tr!("Наборы", "Sets").to_string();
    }
    match slot.strip_suffix("_persona_1") {
        Some(base) => catalog::slot_label(base),
        None if slot == "persona_selector" => crate::tr!("Персона", "Persona").to_string(),
        None => catalog::slot_label(slot),
    }
}

fn belongs(skin: &Skin, hero: &str) -> bool {
    if hero == NO_HERO {
        skin.is_general()
    } else {
        skin.fits_hero(hero)
    }
}

fn hero_title(hero: &str) -> &str {
    if hero == NO_HERO {
        crate::tr!("Общее", "General")
    } else {
        hero
    }
}

impl LauncherApp {
    pub(super) fn heroes_central(&mut self, ctx: &egui::Context) {
        self.icons.begin_frame(ctx);
        if self.hero_filter.is_empty() {
            self.hero_picker(ctx);
        } else {
            let hero = self.hero_filter.clone();
            self.hero_slots(ctx, &hero);
        }
    }

    // ------------------------------------------------------------ hero grid

    fn hero_picker(&mut self, ctx: &egui::Context) {
        let query = self.search.trim().to_lowercase();

        // hero -> (portrait, item count); portraits come from single-hero items
        let mut heroes: std::collections::BTreeMap<String, (Option<String>, usize)> =
            Default::default();
        let mut general = 0usize;
        for skin in &self.catalog.skins {
            if skin.is_general() {
                general += 1;
                continue;
            }
            let names = skin.hero_names();
            for h in &names {
                let entry = heroes.entry(h.to_string()).or_default();
                entry.1 += 1;
                if entry.0.is_none() && names.len() == 1 {
                    entry.0 = skin.preview.clone();
                }
            }
        }
        let mut tiles: Vec<(String, Option<String>, usize)> = Vec::new();
        if general > 0 {
            tiles.push((NO_HERO.to_string(), None, general));
        }
        tiles.extend(heroes.into_iter().map(|(h, (p, n))| (h, p, n)));
        tiles.retain(|(h, _, _)| query.is_empty() || hero_title(h).to_lowercase().contains(&query));

        let mut picked: Option<String> = None;
        let assets = &self.assets;
        let icons = &mut self.icons;

        content_panel(ctx, |ui| {
            ui.label(
                egui::RichText::new(crate::tr!("Выберите героя", "Choose a hero"))
                    .font(bold_font(18.0))
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new(crate::tr!("Слоты героя и выбранные предметы текущего пресета", "Hero slots and the selected items of the current preset"))
                    .size(12.0)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(10.0);
            if tiles.is_empty() {
                ui.label(egui::RichText::new(crate::tr!("Никого не нашлось", "Nobody found")).color(theme::TEXT_DIM));
                return;
            }

            let avail = ui.available_width() - 8.0;
            let cols = (((avail + GAP) / (HERO_W + GAP)).floor() as usize).max(1);
            let w = (avail - GAP * (cols as f32 - 1.0)) / cols as f32;
            let rows = tiles.len().div_ceil(cols);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_rows(ui, HERO_H + GAP, rows, |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for row in range {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = GAP;
                            for (hero, portrait, count) in
                                &tiles[row * cols..((row + 1) * cols).min(tiles.len())]
                            {
                                let key = format!("hero:{hero}");
                                let tex = icons.get(&key, None, portrait.as_deref().map(|p| assets.join(p)));
                                if hero_tile(ui, w, hero_title(hero), *count, tex.as_ref()).clicked() {
                                    picked = Some(hero.clone());
                                }
                            }
                        });
                        ui.add_space(GAP);
                    }
                });
        });

        if let Some(h) = picked {
            self.hero_filter = h;
            self.hero_slot = None;
        }
    }

    // ------------------------------------------------------------ hero slots

    fn hero_slots(&mut self, ctx: &egui::Context, hero: &str) {
        let selected = self.selected_ids();
        let query = self.search.trim().to_lowercase();
        let rarity = self.rarity_filter.clone();

        let all_skins: Vec<&Skin> = self.catalog.skins.iter().filter(|s| belongs(s, hero)).collect();
        let has_persona = all_skins.iter().any(|s| is_persona_slot(&s.slot));
        let persona = has_persona && self.hero_persona;
        // Normal look and persona never share a screen.
        let skins: Vec<&Skin> = all_skins
            .iter()
            .copied()
            .filter(|s| is_persona_slot(&s.slot) == persona)
            .collect();
        let persona_name = all_skins
            .iter()
            .find(|s| s.slot == "persona_selector")
            .map(|s| s.name.clone());
        let mut slots: Vec<String> = skins.iter().map(|s| s.slot.clone()).filter(|s| !s.is_empty()).collect();
        slots.sort_by_key(|s| slot_rank(s));
        slots.dedup();

        // the hero's sets, shown like items: (card, members); worked out once per
        // hero and look (and again when the sets or the catalog change)
        let sets_key = (hero.to_string(), persona, crate::bundles::all().len(), self.catalog.skins.len());
        if self.hero_sets.as_ref().map(|(k, _)| k) != Some(&sets_key) {
            let built = if hero == NO_HERO {
                Vec::new()
            } else {
                crate::bundles::for_hero(&self.catalog, hero, persona)
                .into_iter()
                .map(|(b, members)| {
                    let card = Skin {
                        id: format!("set:{}", b.name),
                        name: b.name.clone(),
                        hero: hero.to_string(),
                        slot: "set".into(),
                        rarity: if b.rarity.is_empty() { "common".into() } else { b.rarity.clone() },
                        icon: b.icon.clone(),
                        index: b.index,
                        ..Default::default()
                    };
                    (card, members.iter().map(|s| s.id.clone()).collect())
                })
                .collect()
            };
            self.hero_sets = Some((sets_key, built));
        }
        let sets: Vec<(Skin, Vec<String>)> = self.hero_sets.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
        let set_worn = |members: &Vec<String>| members.iter().all(|m| selected.contains(m));
        let active_slot = self
            .hero_slot
            .clone()
            .filter(|s| slots.contains(s) || (s == SETS && !sets.is_empty()))
            .or_else(|| slots.first().cloned())
            .unwrap_or_default();
        let set_cards: Vec<&(Skin, Vec<String>)> = sets
            .iter()
            .filter(|_| active_slot == SETS)
            .filter(|(c, _)| rarity.as_ref().map_or(true, |r| c.rarity == *r))
            .filter(|(c, _)| query.is_empty() || query.split_whitespace().all(|w| c.name.to_lowercase().contains(w)))
            .collect();

        let equipped = |slot: &str| -> Option<&Skin> {
            skins
                .iter()
                .copied()
                .find(|s| s.slot == slot && selected.contains(&s.id))
        };
        let equipped_count = slots.iter().filter(|s| equipped(s).is_some()).count();

        let mut options: Vec<&Skin> = skins
            .iter()
            .copied()
            .filter(|s| s.slot == active_slot)
            .filter(|s| rarity.as_ref().map_or(true, |r| s.rarity == *r))
            .filter(|s| query.is_empty() || query.split_whitespace().all(|w| s.search.contains(w)))
            .collect();
        let sort = self.sort;
        sort_skins(&mut options, sort);
        let current = equipped(&active_slot).map(|s| s.id.clone());
        let rarities = present_rarities(&self.catalog);
        let portrait = skins
            .iter()
            .find(|s| s.hero_names().len() == 1)
            .and_then(|s| s.preview.clone());

        let mut back = false;
        let mut switch_persona: Option<bool> = None;
        let mut clear_hero = false;
        let mut new_slot: Option<String> = None;
        let mut toggles: Vec<String> = Vec::new();
        let mut new_rarity: Option<Option<String>> = None;
        let mut events: Vec<(String, CardAction)> = Vec::new();
        let mut new_sort: Option<SortMode> = None;
        let mut open_hero_3d = false;
        let mut set_events: Vec<(String, Vec<String>)> = Vec::new();
        let favorites = &self.config.favorites;
        let active_styles = self.profiles[self.active_index()].styles.clone();
        let has_cli = self.vrf_cli.is_some();
        let cat = &self.catalog;
        let can_hero_3d = has_cli
            && self
                .catalog
                .hero_models
                .get(hero)
                .is_some_and(|h| !persona || h.persona_model.is_some());

        let assets = &self.assets;
        let icons = &mut self.icons;

        content_panel(ctx, |ui| {
            // ---- header
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(egui::RichText::new(crate::tr!("← Все герои", "← All heroes")).size(12.5)).frame(false))
                    .clicked()
                {
                    back = true;
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(96.0, 54.0), Sense::hover());
                let key = format!("hero:{hero}");
                match icons.get(&key, None, portrait.as_deref().map(|p| assets.join(p))) {
                    Some(tex) => {
                        let [tw, th] = tex.size();
                        egui::Image::new(egui::load::SizedTexture::new(tex.id(), rect.size()))
                            .uv(cover_uv(tw as f32 / th.max(1) as f32, rect.aspect_ratio()))
                            .rounding(Rounding::same(8.0))
                            .paint_at(ui, rect);
                    }
                    None => {
                        ui.painter().rect_filled(rect, Rounding::same(8.0), theme::SURFACE_HI);
                    }
                }
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        egui::RichText::new(hero_title(hero))
                            .font(bold_font(20.0))
                            .color(theme::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(crate::trf!("Выбрано в слотах: {equipped_count} из {} · {} предметов", "Selected in slots: {equipped_count} of {} · {} items",
                            slots.len(),
                            skins.len()
                        ))
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if equipped_count > 0
                        && theme::ghost_button(ui, crate::tr!("Снять всё с героя", "Remove everything from the hero"), egui::vec2(150.0, 32.0), theme::DANGER)
                            .clicked()
                    {
                        clear_hero = true;
                    }
                    if can_hero_3d
                        && theme::ghost_button(ui, crate::tr!("3D на герое", "3D on the hero"), egui::vec2(120.0, 32.0), theme::ACCENT_HI)
                            .on_hover_text(crate::tr!("Герой с предметами текущего пресета, обновляется при смене предметов", "The hero with the current preset's items, updates when items change"))
                            .clicked()
                    {
                        open_hero_3d = true;
                    }
                });
            });
            ui.add_space(10.0);
            if has_persona {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if theme::chip(ui, crate::tr!("ОБЫЧНЫЙ ОБЛИК", "CLASSIC LOOK"), !persona, theme::ACCENT_HI).clicked() {
                        switch_persona = Some(false);
                    }
                    let label = match &persona_name {
                        Some(n) => crate::trf!("ПЕРСОНА: {}", "PERSONA: {}", n.to_uppercase()),
                        None => crate::tr!("ПЕРСОНА", "PERSONA").to_string(),
                    };
                    if theme::chip(ui, &label, persona, catalog::rarity_color("arcana"))
                        .on_hover_text(crate::tr!("Альтернативный облик героя со своими слотами", "The hero's alternative look with its own slots"))
                        .clicked()
                    {
                        switch_persona = Some(true);
                    }
                });
                ui.add_space(8.0);
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    // ---- slot tiles
                    theme::caption(ui, if persona { crate::tr!("Слоты персоны", "Persona slots") } else { crate::tr!("Слоты", "Slots") });
                    ui.add_space(2.0);
                    let avail = ui.available_width() - 8.0;
                    let cols = (((avail + GAP) / (TILE_W + GAP)).floor() as usize).max(1);
                    let w = (avail - GAP * (cols as f32 - 1.0)) / cols as f32;
                    let mut tiles: Vec<String> = Vec::new();
                    if !sets.is_empty() {
                        tiles.push(SETS.to_string());
                    }
                    tiles.extend(slots.iter().cloned());
                    for row in tiles.chunks(cols) {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = GAP;
                            for slot in row {
                                let item = if slot == SETS {
                                    sets.iter().find(|(_, m)| set_worn(m)).map(|(c, _)| c)
                                } else {
                                    equipped(slot)
                                };
                                let tex = item.and_then(|s| {
                                    icons.get(&s.id, s.icon.as_deref(), s.preview.as_deref().map(|p| assets.join(p)))
                                });
                                let count = if slot == SETS { sets.len() } else { skins.iter().filter(|s| &s.slot == slot).count() };
                                if slot_tile(ui, w, slot, item, tex.as_ref(), count, *slot == active_slot)
                                    .clicked()
                                {
                                    new_slot = Some(slot.clone());
                                }
                            }
                        });
                        ui.add_space(GAP);
                    }

                    // ---- options for the active slot
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(crate::trf!(
                                "{}  ·  вариантов: {}", "{}  ·  variants: {}",
                                tile_label(&active_slot),
                                if active_slot == SETS { set_cards.len() } else { options.len() }
                            ))
                            .font(bold_font(15.0))
                            .color(theme::TEXT),
                        );
                        if let Some(id) = &current {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if theme::ghost_button(ui, crate::tr!("Вернуть стандартный", "Back to default"), egui::vec2(170.0, 30.0), theme::TEXT)
                                    .on_hover_text(crate::tr!("Убрать выбранный предмет из этого слота", "Remove the selected item from this slot"))
                                    .clicked()
                                {
                                    toggles.push(id.clone());
                                }
                            });
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        if theme::chip(ui, crate::tr!("ВСЕ", "ALL"), rarity.is_none(), theme::TEXT).clicked() {
                            new_rarity = Some(None);
                        }
                        for r in &rarities {
                            let active = rarity.as_deref() == Some(r.as_str());
                            let label = catalog::rarity_label(r).to_uppercase();
                            if theme::chip(ui, &label, active, catalog::rarity_color(r)).clicked() {
                                new_rarity = Some(if active { None } else { Some(r.clone()) });
                            }
                        }
                        ui.add_space(8.0);
                        if let Some(s) = sort_combo(ui, "hero_sort", sort) {
                            new_sort = Some(s);
                        }
                    });
                    ui.add_space(10.0);

                    if active_slot == SETS {
                        let avail = ui.available_width() - 8.0;
                        let cols = (((avail + CARD_GAP) / (CARD_MIN_W + CARD_GAP)).floor() as usize).max(1);
                        let card_w = (avail - CARD_GAP * (cols as f32 - 1.0)) / cols as f32;
                        for row in set_cards.chunks(cols) {
                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = CARD_GAP;
                                for (card, members) in row.iter().map(|x| (&x.0, &x.1)) {
                                    let probe = ui.available_rect_before_wrap();
                                    let visible = ui.is_rect_visible(Rect::from_min_size(probe.min, egui::vec2(card_w, CARD_H)));
                                    let tex = if visible { icons.get(&card.id, card.icon.as_deref(), None) } else { None };
                                    let action = draw_card(ui, card, set_worn(members), false, tex.as_ref(), card_w, false, &[], 0);
                                    if action == CardAction::Toggle {
                                        set_events.push((card.name.clone(), members.clone()));
                                    }
                                }
                            });
                            ui.add_space(CARD_GAP);
                        }
                        return;
                    }
                    if options.is_empty() {
                        ui.label(
                            egui::RichText::new(crate::tr!("Нет вариантов под текущие фильтры", "No variants for the current filters"))
                                .color(theme::TEXT_DIM),
                        );
                        return;
                    }
                    let avail = ui.available_width() - 8.0;
                    let cols = (((avail + CARD_GAP) / (CARD_MIN_W + CARD_GAP)).floor() as usize).max(1);
                    let card_w = (avail - CARD_GAP * (cols as f32 - 1.0)) / cols as f32;
                    for row in options.chunks(cols) {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = CARD_GAP;
                            for skin in row {
                                let is_selected = selected.contains(&skin.id);
                                // decode icons only for cards that are on screen
                                let probe = ui.available_rect_before_wrap();
                                let visible = ui.is_rect_visible(Rect::from_min_size(
                                    probe.min,
                                    egui::vec2(card_w, CARD_H),
                                ));
                                let tex = if visible {
                                    icons.get(&skin.id, skin.icon.as_deref(), skin.preview.as_deref().map(|p| assets.join(p)))
                                } else {
                                    None
                                };
                                let can_3d = has_cli && cat.can_preview(skin);
                                let is_fav = favorites.contains(&skin.id);
                                let styles = crate::styles::of(&skin.name);
                                let style = active_styles.get(&skin.name).copied().unwrap_or(0);
                                let action = draw_card(ui, skin, is_selected, is_fav, tex.as_ref(), card_w, can_3d, styles, style);
                                if action != CardAction::None {
                                    events.push((skin.id.clone(), action));
                                }
                            }
                        });
                        ui.add_space(CARD_GAP);
                    }
                });
        });

        if back {
            self.hero_filter.clear();
            self.hero_slot = None;
            self.hero_persona = false;
        }
        if let Some(p) = switch_persona {
            self.hero_persona = p;
            self.hero_slot = None;
        }
        if let Some(s) = new_slot {
            self.hero_slot = Some(s);
        }
        if let Some(r) = new_rarity {
            self.rarity_filter = r;
        }
        if clear_hero {
            let ids: Vec<String> = self
                .catalog
                .skins
                .iter()
                .filter(|s| belongs(s, hero))
                .map(|s| s.id.clone())
                .collect();
            let idx = self.active_index();
            self.profiles[idx].skins.retain(|id| !ids.contains(id));
            self.profiles_dirty = true;
            self.set_status(StatusKind::Ok, crate::trf!("С героя {} сняты все предметы", "All items removed from {}", hero_title(hero)));
        }
        for id in toggles {
            self.toggle_skin(&id);
        }
        for (name, members) in set_events {
            self.wear_set(&name, &members);
        }
        self.handle_card_events(events);
        if let Some(s) = new_sort {
            self.sort = s;
        }
        if open_hero_3d {
            self.pending_preview = Some(PreviewRequest::Hero {
                hero: hero.to_string(),
                persona,
            });
        }
    }
}

/// Central panel with the launcher's background and padding.
fn content_panel(ctx: &egui::Context, add: impl FnOnce(&mut egui::Ui)) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::none()
                .fill(theme::BG_DEEP)
                .inner_margin(egui::Margin {
                    left: 20.0,
                    right: 12.0,
                    top: 16.0,
                    bottom: 0.0,
                }),
        )
        .show(ctx, |ui| {
            let bg = ui.max_rect().expand2(egui::vec2(20.0, 16.0));
            let glow = Rect::from_min_max(bg.min, Pos2::new(bg.max.x, bg.min.y + 220.0));
            ui.painter().add(theme::vgradient(
                glow,
                theme::ACCENT.gamma_multiply(0.07),
                Color32::TRANSPARENT,
            ));
            add(ui);
        });
}

fn hero_tile(
    ui: &mut egui::Ui,
    w: f32,
    name: &str,
    count: usize,
    tex: Option<&egui::TextureHandle>,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, HERO_H), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
        let p = ui.painter();
        let r = Rounding::same(theme::R_MD);
        p.rect_filled(rect, r, theme::lerp_color(theme::SURFACE, theme::SURFACE_HI, hover));
        let pic = Rect::from_min_size(rect.min, egui::vec2(w, HERO_H - 30.0));
        let top = Rounding { nw: theme::R_MD, ne: theme::R_MD, sw: 0.0, se: 0.0 };
        match tex {
            Some(tex) => {
                let [tw, th] = tex.size();
                egui::Image::new(egui::load::SizedTexture::new(tex.id(), pic.size()))
                    .uv(cover_uv(tw as f32 / th.max(1) as f32, pic.aspect_ratio()))
                    .rounding(top)
                    .tint(theme::lerp_color(Color32::from_gray(200), Color32::WHITE, hover))
                    .paint_at(ui, pic);
            }
            None => {
                p.rect_filled(pic, top, theme::ACCENT.gamma_multiply(0.12));
                p.text(
                    pic.center(),
                    egui::Align2::CENTER_CENTER,
                    name.chars().next().unwrap_or('?').to_string(),
                    bold_font(30.0),
                    theme::ACCENT.gamma_multiply(0.6),
                );
            }
        }
        let g = theme::galley(ui, name, bold_font(13.0), theme::TEXT, w - 50.0);
        p.galley(Pos2::new(rect.min.x + 10.0, pic.max.y + 7.0), g, theme::TEXT);
        let cg = theme::galley(ui, &count.to_string(), FontId::proportional(11.5), theme::TEXT_FAINT, 40.0);
        p.galley(
            Pos2::new(rect.max.x - 10.0 - cg.size().x, pic.max.y + 8.0),
            cg,
            theme::TEXT_FAINT,
        );
        p.rect_stroke(
            rect,
            r,
            Stroke::new(1.0_f32, theme::lerp_color(theme::BORDER, theme::ACCENT, hover)),
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn slot_tile(
    ui: &mut egui::Ui,
    w: f32,
    slot: &str,
    item: Option<&Skin>,
    tex: Option<&egui::TextureHandle>,
    count: usize,
    active: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, TILE_H), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.12);
        let act = ui.ctx().animate_bool_with_time(resp.id.with("a"), active, 0.15);
        let p = ui.painter();
        let r = Rounding::same(theme::R_MD);
        let accent = item.map(|s| catalog::rarity_color(&s.rarity)).unwrap_or(theme::BORDER_HI);
        p.rect_filled(rect, r, theme::lerp_color(theme::SURFACE, theme::SURFACE_HI, hover.max(act)));

        // slot caption
        let cap = tile_label(slot).to_uppercase();
        let g = theme::galley(ui, &cap, bold_font(10.0), theme::TEXT_FAINT, w - 50.0);
        p.galley(rect.min + egui::vec2(10.0, 8.0), g, theme::TEXT_FAINT);
        let cg = theme::galley(ui, &count.to_string(), FontId::proportional(10.5), theme::TEXT_FAINT, 40.0);
        p.galley(Pos2::new(rect.max.x - 10.0 - cg.size().x, rect.min.y + 8.0), cg, theme::TEXT_FAINT);

        // picture
        let pic = Rect::from_min_max(
            rect.min + egui::vec2(10.0, 24.0),
            Pos2::new(rect.max.x - 10.0, rect.max.y - 29.0),
        );
        match (item, tex) {
            (Some(_), Some(tex)) => {
                let [tw, th] = tex.size();
                egui::Image::new(egui::load::SizedTexture::new(tex.id(), pic.size()))
                    .uv(cover_uv(tw as f32 / th.max(1) as f32, pic.aspect_ratio()))
                    .rounding(Rounding::same(6.0))
                    .paint_at(ui, pic);
            }
            (Some(_), None) => {
                p.rect_filled(pic, Rounding::same(6.0), accent.gamma_multiply(0.15));
            }
            (None, _) => {
                p.rect_filled(pic, Rounding::same(6.0), theme::INPUT);
                p.text(
                    pic.center(),
                    egui::Align2::CENTER_CENTER,
                    "+",
                    bold_font(22.0),
                    theme::lerp_color(theme::TEXT_FAINT, theme::TEXT_DIM, hover),
                );
            }
        }

        // item name or default
        let (text, color) = match item {
            Some(s) => (s.name.as_str(), theme::TEXT),
            None => (crate::tr!("Стандартный", "Default"), theme::TEXT_FAINT),
        };
        let font = if item.is_some() { bold_font(12.5) } else { FontId::proportional(12.5) };
        let g = theme::galley(ui, text, font, color, w - 20.0);
        p.galley(Pos2::new(rect.min.x + 10.0, rect.max.y - 23.0), g, color);

        if item.is_some() {
            let stripe = Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - 3.0), rect.max);
            p.rect_filled(
                stripe,
                Rounding { nw: 0.0, ne: 0.0, sw: theme::R_MD, se: theme::R_MD },
                accent.gamma_multiply(0.85),
            );
        }
        let outline = theme::lerp_color(
            theme::lerp_color(theme::BORDER, theme::BORDER_HI, hover),
            theme::ACCENT,
            act,
        );
        p.rect_stroke(rect, r, Stroke::new(1.0 + act, outline));
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    match item {
        Some(s) => resp.on_hover_text(format!("{}: {}", tile_label(slot), s.name)),
        None => resp.on_hover_text(crate::trf!("{}: стандартный предмет", "{}: default item", tile_label(slot))),
    }
}
