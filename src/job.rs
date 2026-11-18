//! Background "apply (and restart)" job shared by the launcher and the companion.

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use eframe::egui;

use crate::apply::{self, Install};
use crate::{dota, gsi, vpk};

pub enum Msg {
    Step(String),
    Done(Result<String, String>),
}

pub struct Plan {
    pub install: Install,
    /// Overrides built from the item catalog: game VPK file -> default slot file.
    pub overrides: Vec<crate::extract::Req>,
    /// Items to write into the game's item table (slots with no file to swap).
    pub schema_picks: Vec<crate::schema::Pick>,
    /// Items that cannot be overridden (no model / no default for the slot).
    pub unsupported: Vec<crate::extract::Skip>,
    /// Map items (creeps, towers, wards…), applied from their table visuals.
    pub general: Vec<crate::visuals::ItemRef>,
    /// Hero items: their particle swaps are applied on top of the model.
    pub hero_items: Vec<crate::visuals::HeroItem>,
    /// Source2Viewer-CLI, to read material groups of models (item colourings).
    pub vrf_cli: Option<std::path::PathBuf>,
    /// The preset's emblem: its effect replaces the selection ring, its
    /// projectile every ranged hero's.
    pub emblem: Option<crate::visuals::ItemRef>,
    pub dota_exe: String,
    pub launch_args: String,
    /// Close a running Dota first and start it again afterwards.
    pub restart: bool,
    /// Apply in memory (see inject.rs): no files in the game folder, Dota is
    /// started by the job with changer.dll injected.
    pub memory_swap: bool,
}

pub fn spawn(plan: Plan, ctx: egui::Context) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("apply-job".into())
        .spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let result = run(&plan, &|s: &str| send(Msg::Step(s.to_string())));
            send(Msg::Done(result));
        })
        .ok();
    rx
}

fn run(plan: &Plan, step: &dyn Fn(&str)) -> Result<String, String> {
    let was_running = dota::is_running();

    if plan.restart && was_running {
        step(crate::tr!("Проверяю, что вы не в матче…", "Checking that you are not in a match…"));
        if let gsi::GameState::InMatch(_) = gsi::query() {
            return Err(crate::tr!("Идёт матч, перезапуск отменён. Повторите после игры.", "A match is on, restart cancelled. Try again after the game.").into());
        }
        step(crate::tr!("Закрываю Dota 2…", "Closing Dota 2…"));
        dota::close_gracefully(Duration::from_secs(45))?;
    } else if was_running {
        // Applying under a running game may hit locked files; say so clearly.
        step(crate::tr!("Dota запущена, применяю без перезапуска…", "Dota is running, applying without a restart…"));
    }

    if !plan.memory_swap {
        step(crate::tr!("Проверяю монтирование оверрайдов…", "Checking the overrides mount…"));
        crate::mount::ensure(&plan.dota_exe, &plan.install.mod_dir)?;
    }

    let mut skipped = plan.unsupported.clone();
    let mut install = plan.install.clone();
    // Item models first: the first override of a file wins.
    let mut reqs = plan.overrides.clone();
    let mut glow_lines: Vec<String> = Vec::new();
    let mut empty_slots = 0usize;
    let mut weather: Option<u32> = None;
    let mut no_combine = false;
    // the item table with the stand-ins of empty slots, written after the swap
    // table when Dota is running (it takes a few seconds when it changes and
    // only counts after a restart anyway)
    type Layout = (std::collections::HashSet<(String, String)>, Vec<(String, String)>);
    let mut deferred_schema: Option<(&'static crate::gamedata::GameData, Layout)> = None;
    // empty-slot picks the running Dota shows only after a restart
    let mut need_restart: Vec<String> = Vec::new();

    // Everything below needs the game's item table: item effects and the
    // emblem are read from it. Items for slots the hero has no file for
    // (memory mode) get a changed copy of it, see schema_mount.rs; the file
    // mode reports them as not applied.
    if !plan.memory_swap {
        for pick in &plan.schema_picks {
            skipped.push(crate::extract::Skip::new(&pick.name, crate::tr!("пустой слот, файла для подмены нет", "empty slot, no file to replace")));
        }
    }
    // memory mode: always, the item table with the stand-ins is mounted from
    // Dota's start whatever the preset
    let needs_table = !plan.general.is_empty() || !plan.hero_items.is_empty() || plan.emblem.is_some() || plan.memory_swap;
    let game_dir = if needs_table || !reqs.is_empty() {
        Some(
            vpk::game_dir_from_exe(&plan.dota_exe)
                .ok_or(crate::tr!("не найдена папка game/dota для извлечения файлов игры", "game/dota folder not found for extracting game files"))?,
        )
    } else {
        None
    };
    if let (true, Some(game_dir)) = (needs_table, &game_dir) {
        // parsed once per game version (gamedata.rs)
        let data = crate::gamedata::load(game_dir)?;
        let (table, index, npcs) = (data.table, &data.index, &data.npcs);

        if !plan.general.is_empty() || !plan.hero_items.is_empty() {
            step(crate::tr!("Подменяю эффекты предметов…", "Swapping item effects…"));
            let units = &data.units;
            // An arcana swapping another item's model (Bladeform Legacy turns the
            // Ancient Exile pieces into their arcana versions): use the swapped
            // model for those pieces when they are in the preset too.
            let swaps = crate::visuals::model_swaps(&index, &plan.hero_items);
            for r in &mut reqs {
                if let Some(to) = swaps.get(&r.src) {
                    r.src = to.clone();
                }
            }
            let (hero_reqs, glows) = crate::visuals::hero_overrides_and_glows(&index, &plan.hero_items, &units, &npcs);
            reqs.extend(hero_reqs);
            // Glows no file can place: the DLL creates them on the hero in a match.
            if let (false, Some(cli)) = (glows.is_empty(), &plan.vrf_cli) {
                let pak = game_dir.join("pak01_dir.vpk");
                let particles: Vec<&str> = glows.iter().map(|g| g.particle.as_str()).collect();
                let drivers = crate::glow::drivers_cached(cli, &pak, &particles);
                for g in &glows {
                    let d = drivers.get(&g.particle).cloned().unwrap_or_default();
                    glow_lines.push(crate::glow::config_line(&g.npc, &g.particle, &d));
                }
            }
            // Taunts: their animation, played by changer.dll on your own hero (client-side).
            if let Some(cli) = &plan.vrf_cli {
                let pak = game_dir.join("pak01_dir.vpk");
                let model_swaps: std::collections::HashMap<String, String> = reqs
                    .iter()
                    .filter(|r| r.dst.ends_with(".vmdl_c"))
                    .map(|r| (r.dst.to_ascii_lowercase(), r.src.clone()))
                    .collect();
                glow_lines.extend(crate::taunts::config_lines(index, &plan.hero_items, npcs, units, &model_swaps, cli, &pak));
            }
            // The picked items' icons in Dota's slot tiles.
            if plan.memory_swap {
                glow_lines.extend(crate::slot_icons::config_lines(data, &plan.hero_items));
            }
            // Colourings that are another material group of the model.
            let wanted = crate::skins::wanted(&index, &plan.hero_items);
            if let (false, Some(cli)) = (wanted.is_empty(), &plan.vrf_cli) {
                step(crate::tr!("Подбираю раскраску предметов…", "Matching item colourings…"));
                let models: Vec<String> = wanted.iter().map(|(_, m, _)| m.clone()).collect();
                let groups = crate::skins::material_groups_cached(cli, &game_dir.join("pak01_dir.vpk"), &models);
                reqs.extend(crate::skins::overrides(&wanted, &groups));
            }
            let (r, missed) = crate::visuals::overrides(&index, &plan.general, &units, true);
            reqs.extend(r);
            skipped.extend(missed);
        }

        if plan.memory_swap {
            // Items for empty slots: the slot's stand-in model is swapped for the item's.
            for pick in &plan.schema_picks {
                let Some(npc) = npcs.get(&crate::schema::norm(&pick.hero)) else { continue };
                if !crate::schema::wearable_slot(&pick.slot) || !pick.model.trim().ends_with(".vmdl") {
                    skipped.push(crate::extract::Skip::new(&pick.name, crate::tr!("пустой слот, файла для подмены нет", "empty slot, no file to replace")));
                    continue;
                }
                reqs.push(crate::extract::Req {
                    id: pick.name.clone(),
                    src: crate::extract::compiled_path(pick.model.trim()),
                    dst: crate::extract::compiled_path(&crate::schema::placeholder(npc, &pick.slot)),
                });
                need_restart.push(pick.name.clone());
            }
            if !was_running || plan.restart || crate::schema_mount::loaded_current(&plan.dota_exe) {
                need_restart.clear();
            }
            let catalog = crate::catalog::Catalog::load(&crate::config::assets_dir()).unwrap_or_default();
            let layout = schema_layout(data, &catalog);
            if was_running && !plan.restart {
                deferred_schema = Some((data, layout));
            } else {
                step(crate::tr!("Записываю таблицу предметов для пустых слотов…", "Writing the item table for empty slots…"));
                if let Err(e) = crate::schema_mount::apply(&plan.dota_exe, table, &layout.0, &layout.1) {
                    for pick in &plan.schema_picks {
                        skipped.push(crate::extract::Skip::new(&pick.name, crate::trf!("пустой слот: {e}", "empty slot: {e}")));
                    }
                }
            }
            empty_slots = need_restart.len();
        }

        // Weather: no file to swap, the game turns it on by its type (cl_weather).
        weather = plan.general.iter().find_map(|g| index.weather_type(g));
        no_combine = plan.hero_items.iter().any(|h| index.skips_model_combine(&h.item));

        if let Some(emblem) = &plan.emblem {
            step(crate::tr!("Применяю эмблему…", "Applying the emblem…"));
            match crate::emblem::overrides(game_dir, &index, emblem)? {
                Some(r) if !r.is_empty() => reqs.extend(r),
                _ => skipped.push(crate::extract::Skip::new(&emblem.name, crate::tr!("нет эффектов", "no effects"))),
            }
        }
    }
    // An item that got any file (an arcana with no model of its own, but a
    // body swap and effects) is applied.
    skipped.retain(|s| !reqs.iter().any(|r| r.id == s.name));

    if plan.memory_swap {
        // items for empty slots wait for a Dota restart (the running one started
        // before the table had their stand-ins): the in-game button says so
        if let Some(line) = crate::schema_mount::restart_line(&need_restart) {
            glow_lines.push(line);
        }
        let result = run_in_memory(plan, &reqs, &glow_lines, empty_slots, weather, no_combine, skipped, was_running, step);
        if let (Ok(_), Some((data, layout))) = (&result, deferred_schema) {
            step(crate::tr!("Записываю таблицу предметов для пустых слотов…", "Writing the item table for empty slots…"));
            let _ = crate::schema_mount::apply(&plan.dota_exe, data.table, &layout.0, &layout.1);
        }
        return result;
    }
    // the file mode leaves the game's item table as Steam shipped it
    let _ = crate::schema_mount::remove(&plan.dota_exe);

    if let (false, Some(game_dir)) = (reqs.is_empty(), &game_dir) {
        step(crate::tr!("Извлекаю файлы предметов из игры…", "Extracting item files from the game…"));
        let cache = crate::config::app_dir().join("cache").join("item_files");
        let (extra, missed) = crate::extract::extract(&reqs, game_dir, &cache)?;
        skipped.extend(missed);
        install.files.extend(extra);
    }

    step(crate::tr!("Применяю пресет…", "Applying the preset…"));
    let written = apply::install(&install).map_err(|e| {
        if was_running && !plan.restart {
            crate::trf!("{e} (возможно, файлы заняты игрой, нужен перезапуск)", "{e} (the game may be holding the files, a restart is needed)")
        } else {
            e
        }
    })?;

    if let Some(game_dir) = vpk::game_dir_from_exe(&plan.dota_exe) {
        // The emblem effect replaces the selection ring, which is off by default.
        if let Err(e) = crate::autoexec::keep_ring(&game_dir, plan.emblem.is_some()) {
            skipped.push(crate::extract::Skip::new(crate::tr!("Кольцо эмблемы", "Emblem ring"), e));
        }
        if let Err(e) = crate::autoexec::keep_weather(&game_dir, weather) {
            skipped.push(crate::extract::Skip::new(crate::tr!("Погода", "Weather"), e));
        }
        let _ = gsi::install_cfg(&game_dir);
    }

    let mut summary = crate::trf!("Пресет применён: предметов {}, файлов {written}", "Preset applied: items {}, files {written}",
        install.ids.len()
    );
    if let Some(note) = skipped_note(&skipped) {
        summary.push_str(&note);
    }

    if plan.restart && was_running {
        step(crate::tr!("Запускаю Dota 2…", "Starting Dota 2…"));
        let msg = dota::launch(&plan.dota_exe, &gsi::with_launch_flag(&plan.launch_args))?;
        summary = format!("{summary}. {msg}");
    }
    Ok(summary)
}

/// What the mounted item table changes, from the whole catalog (not the
/// preset, so it stays the same and Dota needs no restart for a pick): every
/// hero's wearable slots that items exist for (the empty ones get a stand-in),
/// and the slots of items that must not be merged into the combined hero model
/// (Drow's arcana crossbow: own skeleton) for "skip_model_combine".
fn schema_layout(
    data: &crate::gamedata::GameData,
    catalog: &crate::catalog::Catalog,
) -> (std::collections::HashSet<(String, String)>, Vec<(String, String)>) {
    let mut slots = std::collections::HashSet::new();
    let mut no_merge = Vec::new();
    for s in catalog.skins.iter().filter(|s| !s.is_general()) {
        let npcs: Vec<String> =
            s.hero_names().iter().filter_map(|h| data.npcs.get(&crate::schema::norm(h)).cloned()).collect();
        if s.model.is_some() && crate::schema::wearable_slot(&s.slot) {
            for npc in &npcs {
                slots.insert((npc.to_ascii_lowercase(), s.slot.to_ascii_lowercase()));
            }
        }
        if data.index.skips_model_combine(&crate::visuals::ItemRef::of(s)) {
            for npc in npcs {
                no_merge.push((npc, s.slot.clone()));
            }
        }
    }
    no_merge.sort();
    no_merge.dedup();
    (slots, no_merge)
}

/// Memory mode: the overrides become changer.dll's table instead of files.
/// A running Dota gets the new table without a restart; otherwise Dota is
/// started with the DLL.
fn run_in_memory(
    plan: &Plan,
    reqs: &[crate::extract::Req],
    glows: &[String],
    empty_slots: usize,
    weather: Option<u32>,
    no_combine: bool,
    mut skipped: Vec<crate::extract::Skip>,
    was_running: bool,
    step: &dyn Fn(&str),
) -> Result<String, String> {
    step(crate::tr!("Записываю таблицу подмен…", "Writing the swap table…"));
    let rules = crate::inject::write_config(reqs, glows)?;
    crate::picks::trace("  swap table written");
    if !plan.install.files.is_empty() {
        skipped.push(crate::extract::Skip::new(crate::tr!("Свои файлы", "Own files"), crate::tr!("в режиме «в памяти» не применяются", "do not apply in memory mode")));
    }
    // Files left by the file mode would apply on top of the table.
    if apply::clear_mod(&plan.install.mod_dir).is_ok() {
        let _ = crate::mount::remove(&plan.dota_exe, &plan.install.mod_dir);
    }

    if let Some(game_dir) = vpk::game_dir_from_exe(&plan.dota_exe) {
        if let Err(e) = crate::autoexec::keep_ring(&game_dir, plan.emblem.is_some()) {
            skipped.push(crate::extract::Skip::new(crate::tr!("Кольцо эмблемы", "Emblem ring"), e));
        }
        // Weather: no file to swap, the game turns it on by its type.
        if let Err(e) = crate::autoexec::keep_weather(&game_dir, weather) {
            skipped.push(crate::extract::Skip::new(crate::tr!("Погода", "Weather"), e));
        }
        // dota_combine_models is not a console variable in the release game: the line
        // did nothing. Kept only to take an old line out of autoexec.cfg.
        let _ = (no_combine, crate::autoexec::keep_combine_off(&game_dir, false));
        let _ = gsi::install_cfg(&game_dir);
    }

    let _ = (rules, &skipped);
    if was_running && !plan.restart {
        step(crate::tr!("Передаю пресет в запущенную Dota…", "Passing the preset to the running Dota…"));
        crate::inject::apply_to_running()?;
        let mut msg = String::from(crate::tr!("Пресет применён", "Preset applied"));
        if empty_slots > 0 {
            msg.push_str(&crate::trf!(" · предметы для пустых слотов ({empty_slots}) появятся после перезапуска Dota", " · items for empty slots ({empty_slots}) appear after Dota restarts"));
        }
        return Ok(msg);
    }
    crate::schema_mount::remember_loaded(&plan.dota_exe);
    crate::inject::launch_and_inject(&plan.dota_exe, &gsi::with_launch_flag(&plan.launch_args), step)?;
    Ok(crate::tr!("Dota 2 успешно запущена", "Dota 2 launched successfully").into())
}

/// " · not applied: A, B" (in the UI language) or `None` when nothing was skipped.
fn skipped_note(skipped: &[crate::extract::Skip]) -> Option<String> {
    const SHOWN: usize = 4;
    let mut names: Vec<&str> = skipped.iter().map(|s| s.name.as_str()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return None;
    }
    let mut note = crate::trf!(" · не применились: {}", " · not applied: {}", names[..names.len().min(SHOWN)].join(", "));
    if names.len() > SHOWN {
        note.push_str(&crate::trf!(" и ещё {}", " and {} more", names.len() - SHOWN));
    }
    Some(note)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::Skip;

    #[test]
    fn note_keeps_names_with_colons() {
        let s = vec![
            Skip { name: "Taunt: Bring It On".into(), reason: "анимация".into() },
            Skip { name: "Taunt: Bring It On".into(), reason: "дубль".into() },
        ];
        let note = skipped_note(&s).unwrap();
        assert_eq!(note, " · не применились: Taunt: Bring It On");
        assert!(skipped_note(&[]).is_none());
    }
}
