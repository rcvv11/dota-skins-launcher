

use std::time::{Duration, Instant, SystemTime};

use eframe::egui;

use crate::apply::Install;
use crate::catalog::{Catalog, Skin};
use crate::config::{self, Config, Profile};
use crate::{job, win};

pub fn start(ctx: egui::Context) {
    std::thread::Builder::new()
        .name("ingame-picks".into())
        .spawn(move || run(ctx))
        .ok();
}

fn run(ctx: egui::Context) {
    let assets = config::assets_dir();
    let mut catalog = Catalog::load(&assets).unwrap_or_default();
    catalog.merge_custom(&config::load_custom_files());

    // names of items and sets as Dota shows them (built in the background: ~1 s)
    let names: std::sync::Arc<std::sync::OnceLock<crate::sets::NameIndex>> = Default::default();
    {
        let names = names.clone();
        let dota = config::load_config().dota_path;
        std::thread::spawn(move || {
            if let Some(dir) = crate::vpk::game_dir_from_exe(&dota) {
                if let Ok(index) = crate::sets::NameIndex::load(&dir) {
                    let _ = names.set(index);
                }
            }
        });
    }
    // the game's item table, parsed in advance: the first pick is fast too
    crate::gamedata::warm_up(config::load_config().dota_path);
    let mut enabled = config::load_config().memory_swap;
    let mut slow_check = Instant::now();
    let mut equips_mtime: Option<SystemTime> = None;
    // presses made before the launcher started are not replayed
    let mut handled = crate::picks::latest_equip().map_or(0, |p| p.tick);

    loop {
        if slow_check.elapsed() >= Duration::from_secs(2) {
            slow_check = Instant::now();
            enabled = config::load_config().memory_swap;
        }

        let mtime = std::fs::metadata(crate::picks::equips_path()).and_then(|m| m.modified()).ok();
        if mtime != equips_mtime {
            equips_mtime = mtime;
            // every press since last time, in order: while one was being applied,
            // more may have come; their items are put on in one apply
            let mut picks: Vec<(Vec<String>, Option<u32>)> = Vec::new();
            for press in crate::picks::presses_after(handled) {
                handled = press.tick;
                crate::picks::trace(&format!("press {} seen ({} ms after it)", press.tick, crate::picks::now_ms().saturating_sub(press.tick)));
                if !enabled {
                    continue;
                }
                // Dota's persona switch: «@persona», hero name
                if press.labels.first().map(String::as_str) == Some("@persona") {
                    if !picks.is_empty() {
                        let _ = wear(&ctx, &std::mem::take(&mut picks));
                    }
                    let hero = press.labels.get(1).cloned().unwrap_or_default();
                    let _ = toggle_persona(&ctx, &hero);
                    continue;
                }
                let ids: Vec<String> = crate::picks::items_at(&catalog, &press, names.get()).iter().map(|s| s.id.clone()).collect();
                crate::picks::trace(&format!("  items: {}", ids.join(", ")));
                if !ids.is_empty() {
                    // the style picked with the arrows on Dota's item card («2 / 2»)
                    picks.push((ids, crate::styles::from_labels(&press.labels)));
                }
            }
            if !picks.is_empty() {
                // blocks until applied; presses meanwhile are taken next time
                let _ = wear(&ctx, &picks);
            }
        }

        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Hero name as Dota shows it ("ANTI-MAGE") compared with the catalog's.
fn same_hero(a: &str, b: &str) -> bool {
    let n = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    !a.trim().is_empty() && n(a) == n(b)
}

/// Puts the hero's persona into the active preset, or takes it out (back to
/// the classic look) when it is there, and applies the preset.
pub fn toggle_persona(ctx: &egui::Context, hero: &str) -> Result<bool, ()> {
    let cfg = config::load_config();
    if !cfg.memory_swap {
        return Err(());
    }
    let assets = config::assets_dir();
    let mut catalog = Catalog::load(&assets).unwrap_or_default();
    catalog.merge_custom(&config::load_custom_files());
    let personas: Vec<&Skin> = catalog
        .skins
        .iter()
        .filter(|s| s.slot == "persona_selector" && s.hero_names().iter().any(|h| same_hero(h, hero)))
        .collect();
    let Some(first) = personas.first() else { return Err(()) };
    let mut profiles = config::load_profiles();
    let index = cfg.active_profile.min(profiles.len().saturating_sub(1));
    let profile = profiles.get_mut(index).ok_or(())?;
    let worn = profile.skins.iter().any(|id| personas.iter().any(|p| &p.id == id));
    if worn {
        profile.skins.retain(|id| !personas.iter().any(|p| &p.id == id));
    } else {
        crate::picks::wear(profile, &catalog, &[*first]);
    }
    let plan = plan_for(&cfg, &catalog, &assets, profile);
    config::save_profiles(&profiles).map_err(|_| ())?;
    let rx = job::spawn(plan, ctx.clone());
    let ok = rx.iter().find_map(|m| match m {
        job::Msg::Done(r) => Some(r.is_ok()),
        job::Msg::Step(_) => None,
    });
    ctx.request_repaint();
    if ok == Some(true) { Ok(!worn) } else { Err(()) }
}

/// Install plan for a preset, as the launcher's «Apply» builds it.
fn plan_for(config: &Config, catalog: &Catalog, assets: &std::path::Path, profile: &Profile) -> job::Plan {
    let chosen: Vec<&Skin> = catalog.skins.iter().filter(|s| profile.skins.contains(&s.id)).collect();
    let (overrides, schema_picks, unsupported) = crate::extract::plan(&chosen, catalog, &config.override_targets);
    let mut general = crate::visuals::general_items(&chosen);
    let mut hero_items = crate::visuals::hero_items(&chosen);
    crate::visuals::with_styles(&mut general, &mut hero_items, &profile.styles);
    job::Plan {
        install: Install::new(&chosen, assets, config::resolve_mod_dir(config), config.pack_vpk),
        overrides,
        schema_picks,
        unsupported,
        general,
        hero_items,
        vrf_cli: crate::preview::find_cli(&config.vrf_cli_path),
        emblem: crate::emblem::from_preset(&chosen),
        dota_exe: config.dota_path.clone(),
        launch_args: config.launch_args.clone(),
        restart: false,
        memory_swap: config.memory_swap,
    }
}

/// Puts items into the active preset and applies it in memory, blocking until
/// done. Ok(false) = already worn.
pub fn wear(ctx: &egui::Context, picks: &[(Vec<String>, Option<u32>)]) -> Result<bool, ()> {
    let cfg = config::load_config();
    if !cfg.memory_swap {
        return Err(());
    }
    let assets = config::assets_dir();
    let mut catalog = Catalog::load(&assets).unwrap_or_default();
    catalog.merge_custom(&config::load_custom_files());
    let mut profiles = config::load_profiles();
    let index = cfg.active_profile.min(profiles.len().saturating_sub(1));
    let profile = profiles.get_mut(index).ok_or(())?;
    let mut changed = false;
    // in the order pressed: a later pick of the same slot wins
    for (ids, style) in picks {
        let items: Vec<&Skin> = catalog.skins.iter().filter(|s| ids.contains(&s.id)).collect();
        if items.is_empty() {
            continue;
        }
        changed |= crate::picks::wear(profile, &catalog, &items);
        if let Some(style) = *style {
            let before = profile.styles.clone();
            let worn: Vec<String> =
                catalog.skins.iter().filter(|s| profile.skins.contains(&s.id)).map(|s| s.name.clone()).collect();
            for item in &items {
                // only items that have styles: a set's name row picks all its pieces
                if !crate::styles::of(&item.name).is_empty() {
                    crate::styles::set(&mut profile.styles, &item.name, style, &worn);
                }
            }
            changed |= profile.styles != before;
        }
    }
    if !changed {
        return Ok(false);
    }
    let plan = plan_for(&cfg, &catalog, &assets, profile);
    config::save_profiles(&profiles).map_err(|_| ())?;
    let start = std::time::Instant::now();
    crate::picks::trace("apply: preset saved, job started");
    let rx = job::spawn(plan, ctx.clone());
    let ok = rx.iter().find_map(|m| match m {
        job::Msg::Done(r) => {
            crate::picks::trace(&format!("apply: done in {} ms: {}", start.elapsed().as_millis(), match &r { Ok(s) | Err(s) => s }));
            Some(r.is_ok())
        }
        job::Msg::Step(s) => {
            crate::picks::trace(&format!("  {} ms  {s}", start.elapsed().as_millis()));
            None
        }
    });
    ctx.request_repaint();
    if ok == Some(true) { Ok(true) } else { Err(()) }
}

/// The hotkey companion of older versions is gone: drop its Windows autostart
/// entry and close a copy that is still running.
pub fn remove_legacy_companion() {
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    if win::read_hkcu_string(RUN_KEY, "DotaSkinsCompanion").is_some() {
        let _ = win::write_hkcu_string(RUN_KEY, "DotaSkinsCompanion", None);
    }
    if let Some(hwnd) = win::find_window("Dota Skins — пресеты") {
        win::terminate_window_process(hwnd);
    }
}
