use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, FontId, Pos2, Rect, Rounding, Sense, Stroke};

use crate::apply::{self, Install};
use crate::catalog::{self, Catalog, Skin};
use crate::config::{self, Config, Profile};
use crate::icons::IconLoader;
use crate::preview::{self, Preview, Subject};
use crate::theme::{self, bold_font, Icon, Kind};
use crate::{attach, dota, gsi, job, vpk};

mod hero;

#[derive(Clone, Copy, PartialEq)]
enum View {
    Catalog,
    Heroes,
    Settings,
}

/// Used to seed a working catalog when none is found on disk.
const DEMO_CATALOG: &str = include_str!("../assets/catalog.json");

const CARD_MIN_W: f32 = 200.0;
const CARD_H: f32 = 190.0;
const CARD_GAP: f32 = 14.0;
/// Inset of a card's picture.
const CARD_PAD: f32 = 6.0;
const PREVIEW_H: f32 = 124.0;
const SIDE_W: f32 = 236.0;
/// Gap between the content panel and the window's edges.
const PANEL_GAP: f32 = 10.0;

/// Hero-filter value for items that are not tied to a hero (terrain, HUD...).
const NO_HERO: &str = "\u{0}general";

/// Display order for rarity chips.
const RARITY_ORDER: [&str; 9] = [
    "common", "uncommon", "rare", "mythical", "legendary", "immortal", "arcana", "seasonal",
    "ancient",
];

#[derive(Clone, Copy)]
pub(crate) enum StatusKind {
    Ok,
    Warn,
    Error,
}

impl StatusKind {
    pub(crate) fn color(self) -> Color32 {
        match self {
            StatusKind::Ok => theme::EQUIPPED,
            StatusKind::Warn => theme::WARN,
            StatusKind::Error => theme::DANGER,
        }
    }
}

pub struct LauncherApp {
    assets: PathBuf,
    catalog: Catalog,
    catalog_error: Option<String>,
    config: Config,
    profiles: Vec<Profile>,
    active: usize,
    icons: IconLoader,
    search: String,
    hero_filter: String,
    slot_filter: String,
    rarity_filter: Option<String>,
    only_selected: bool,
    status: (StatusKind, String),
    dota_running: Arc<AtomicBool>,
    game_state: (Instant, gsi::GameState),
    job: Option<Receiver<job::Msg>>,
    confirm_restart: bool,
    /// The "revert everything" confirmation is open.
    confirm_revert: bool,
    profiles_dirty: bool,
    /// profiles.json write time after our own last load/save; a newer one
    /// means a preset changed on disk (e.g. «SELECT» in Dota).
    profiles_seen: Option<std::time::SystemTime>,
    view: View,
    /// Slot opened on the hero screen.
    hero_slot: Option<String>,
    /// Hero screen shows persona slots instead of the normal look.
    hero_persona: bool,
    /// Source2Viewer-CLI used for the 3D preview, if found.
    vrf_cli: Option<PathBuf>,
    preview: Option<Preview>,
    /// Preview to open on the next frame (needs the GL context).
    pending_preview: Option<PreviewRequest>,
    only_favorites: bool,
    sort: SortMode,
    /// Files attached by the user, per item id.
    custom: config::CustomFiles,
    /// Item whose "own files" window is open.
    files_window: Option<String>,
    /// Running catalog refresh (gencatalog), if any.
    catalog_job: Option<Receiver<Result<String, String>>>,
    /// Source2Viewer-CLI being downloaded on first start.
    tools_job: Option<Receiver<Result<PathBuf, String>>>,
    /// First-start setup (download the CLI, build the catalog) was started.
    setup_started: bool,
    /// Dota build id when the game was updated after the last catalog refresh.
    catalog_outdated: Option<String>,
    /// Override folder currently mounted in gameinfo.gi.
    mounted_mod_dir: PathBuf,
    /// The Heroes tab's sets of one hero, as cards: (hero, persona, sets loaded,
    /// catalog size) -> (card, members). Worked out once, not every frame.
    hero_sets: Option<((String, bool, usize, usize), Vec<(Skin, Vec<String>)>)>,
    /// Letters typed on the hero picker (type-to-find) and when the last one came.
    hero_typed: String,
    hero_typed_at: Instant,
}

enum PreviewRequest {
    /// "3D" on a card: the item on its hero, or alone for general items.
    Item(String),
    /// "3D on hero": the hero wearing the active preset.
    Hero { hero: String, persona: bool },
}

impl LauncherApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        gsi::start_server();

        let config = config::load_config();
        let (assets, mut catalog, catalog_error) = load_catalog_with_fallback();
        let custom = config::load_custom_files();
        catalog.merge_custom(&custom);
        let mut config = config;
        let catalog_outdated = check_catalog_build(&mut config);
        let game_dir = vpk::game_dir_from_exe(&config.dota_path);
        if let Some(dir) = &game_dir {
            let _ = gsi::install_cfg(dir);
        }
        // Make sure the override folder is actually mounted by the game.
        let mod_dir = config::resolve_mod_dir(&config);
        let mut mount_result = if config.memory_swap {
            Ok(crate::tr!("подмена в памяти, папка оверрайдов не используется", "memory mode, the overrides folder is not used").to_string())
        } else {
            crate::mount::ensure(&config.dota_path, &mod_dir)
        };
        if mount_result.is_ok() && !config.memory_swap {
            if let Some(moved) = crate::mount::migrate_legacy_dir(&config.dota_path, &mod_dir) {
                mount_result = Ok(moved);
            }
        }
        let icons = IconLoader::new(&cc.egui_ctx, game_dir);

        let mut profiles = config::load_profiles();
        if profiles.is_empty() {
            profiles.push(Profile {
                name: crate::tr!("Основной", "Main").to_string(),
                skins: Vec::new(),
                styles: Default::default(),
                gems: Default::default(),
            });
            // in-game picks read presets from disk
            let _ = config::save_profiles(&profiles);
        }
        let active = config.active_profile.min(profiles.len() - 1);

        let no_icons = !catalog.skins.is_empty() && catalog.skins.iter().all(|s| s.icon.is_none());
        let mut status = match &catalog_error {
            Some(err) => (StatusKind::Warn, crate::trf!("Каталог: {err}", "Catalog: {err}")),
            None if catalog_outdated.is_some() => (
                StatusKind::Warn,
                crate::tr!("Dota обновилась после последнего обновления каталога: нажмите «Обновить каталог» вверху", "Dota has updated since the last catalog update: press «Update catalog» at the top")
                    .to_string(),
            ),
            None if no_icons => (
                StatusKind::Warn,
                crate::trf!("Каталог без иконок ({}): пересоберите его через gencatalog", "Catalog has no icons ({}): rebuild it with gencatalog",
                    assets.join("catalog.json").display()
                ),
            ),
            None => (
                StatusKind::Ok,
                crate::trf!("Загружено предметов: {}, {}", "Items loaded: {}, {}", catalog.skins.len(), icons.source),
            ),
        };
        if let Err(e) = &mount_result {
            if !config.dota_path.is_empty() {
                status = (
                    StatusKind::Error,
                    crate::trf!("Монтирование оверрайдов не удалось: {e}", "Mounting the overrides failed: {e}"),
                );
            }
        }

        crate::card::remove_legacy_companion();
        // «SELECT» in Dota puts an item on (memory mode)
        crate::card::start(cc.egui_ctx.clone());
        // style names of items (style switcher on the cards)
        crate::styles::load_in_background(config::load_config().dota_path, cc.egui_ctx.clone());
        // item sets (the Heroes tab's "Sets" tile)
        crate::bundles::load_in_background(config::load_config().dota_path, cc.egui_ctx.clone());

        // The launcher always opens on the Heroes tab (last hero is restored).
        let view = View::Heroes;
        let hero_filter = config.ui_hero.clone();
        let vrf_cli_path = config.vrf_cli_path.clone();

        Self {
            assets,
            catalog,
            catalog_error,
            config,
            profiles,
            active,
            icons,
            search: String::new(),
            hero_filter,
            slot_filter: String::new(),
            rarity_filter: None,
            only_selected: false,
            status,
            dota_running: dota::spawn_watcher(cc.egui_ctx.clone()),
            game_state: (Instant::now() - Duration::from_secs(60), gsi::GameState::Unknown),
            job: None,
            confirm_restart: false,
            confirm_revert: false,
            profiles_dirty: false,
            profiles_seen: config::profiles_modified(),
            view,
            hero_slot: None,
            hero_persona: false,
            vrf_cli: preview::find_cli(&vrf_cli_path),
            preview: None,
            // `--preview <item id>` opens the 3D preview right away
            pending_preview: std::env::args()
                .skip_while(|a| a != "--preview")
                .nth(1)
                .map(PreviewRequest::Item),
            only_favorites: false,
            sort: SortMode::Default,
            custom,
            files_window: None,
            catalog_job: None,
            tools_job: None,
            setup_started: false,
            catalog_outdated,
            mounted_mod_dir: mod_dir,
            hero_sets: None,
            hero_typed: String::new(),
            hero_typed_at: Instant::now(),
        }
    }

    // ------------------------------------------------------------ card actions

    fn handle_card_events(&mut self, events: Vec<(String, CardAction)>) {
        for (id, action) in events {
            match action {
                CardAction::Toggle => self.toggle_skin(&id),
                CardAction::Preview => self.pending_preview = Some(PreviewRequest::Item(id)),
                CardAction::Favorite => {
                    if !self.config.favorites.remove(&id) {
                        self.config.favorites.insert(id);
                    }
                    let _ = config::save_config(&self.config);
                }
                CardAction::Files => self.files_window = Some(id),
                CardAction::Style(n) => self.set_style(&id, n),
                CardAction::None => {}
            }
        }
    }

    // ------------------------------------------------------------ own files

    fn attach_files(&mut self, id: &str, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let model = self
            .catalog
            .skins
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.model.clone());
        let outcome = attach::attach(id, model.as_deref(), &paths);
        let added = outcome.added.len();
        if added > 0 {
            let list = self.custom.entry(id.to_string()).or_default();
            for f in &outcome.added {
                list.retain(|x| x.to != f.to || x.from != f.from);
                list.push(f.clone());
            }
            self.catalog.merge_custom(&self.custom);
            if let Err(e) = config::save_custom_files(&self.custom) {
                self.set_status(StatusKind::Error, e);
                return;
            }
        }
        if outcome.errors.is_empty() {
            self.set_status(StatusKind::Ok, crate::trf!("Добавлено файлов: {added}", "Files added: {added}"));
        } else {
            self.set_status(
                StatusKind::Warn,
                crate::trf!("Добавлено: {added}. Пропущено: {}", "Added: {added}. Skipped: {}", outcome.errors.join("; ")),
            );
        }
    }

    fn remove_attached(&mut self, id: &str, file: &catalog::FileMap) {
        if let Some(list) = self.custom.get_mut(id) {
            list.retain(|f| f != file);
            if list.is_empty() {
                self.custom.remove(id);
            }
        }
        if let Some(skin) = self.catalog.skins.iter_mut().find(|s| s.id == id) {
            skin.files.retain(|f| f != file);
        }
        attach::remove_copy(file);
        let _ = config::save_custom_files(&self.custom);
    }

    fn files_dialog(&mut self, ctx: &egui::Context) {
        let Some(id) = self.files_window.clone() else { return };
        let Some(skin) = self.catalog.skins.iter().find(|s| s.id == id) else {
            self.files_window = None;
            return;
        };
        let name = skin.name.clone();
        let files = skin.files.clone();
        let has_model = skin.model.is_some();
        let slot_label = catalog::slot_label(&skin.slot);
        // Slots the game has no default file for (e.g. Anti-Mage's shoulders):
        // the override has to go over whatever model is actually loaded.
        let body = catalog::is_body_slot(&skin.slot);
        let has_default = skin.hero_names().iter().any(|h| {
            self.catalog
                .hero_models
                .get(*h)
                .is_some_and(|hm| body || hm.defaults.contains_key(&skin.slot))
        });
        let needs_target = has_model && !has_default;
        let mut donor = self
            .config
            .override_targets
            .get(&id)
            .cloned()
            .unwrap_or_default();
        // Items of the same hero and slot that the user could be wearing:
        // picking one of them beats typing a model path by hand.
        let donors: Vec<(String, String)> = if needs_target {
            self.catalog
                .skins
                .iter()
                .filter(|s| {
                    s.id != id
                        && s.slot == skin.slot
                        && s.model.as_deref().is_some_and(|m| !m.trim().is_empty())
                        && s.hero_names().iter().any(|h| skin.hero_names().contains(h))
                })
                .map(|s| (s.name.clone(), s.model.clone().unwrap_or_default()))
                .collect()
        } else {
            Vec::new()
        };
        let donor_label = donors
            .iter()
            .find(|(_, m)| m.trim() == donor.trim())
            .map(|(n, _)| n.clone());
        let mut picked_donor: Option<String> = None;

        // files dropped onto the window while this dialog is open belong to this item
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        let mut open = true;
        let mut pick_files = false;
        let mut pick_folder = false;
        let mut remove: Option<catalog::FileMap> = None;
        egui::Window::new(egui::RichText::new(crate::trf!("Свои файлы – {name}", "Own files – {name}")).font(bold_font(13.5)))
            .id(egui::Id::new("own_files"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([520.0, 380.0])
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(
                        crate::tr!("Файлы мода для этого предмета. Путь в игре берётся из папок models/, materials/, \
                         particles/… в пути файла. Одиночный .vmdl_c заменяет модель предмета, .vpk \
                         добавляется целиком. При «Применить» всё это попадёт в pak01_dir.vpk.", "Mod files for this item. The in-game path is taken from the models/, materials/, particles/… folders in the file path. A single .vmdl_c replaces the item model, a .vpk is added whole. On «Apply» all of it goes into pak01_dir.vpk."),
                    )
                    .size(11.5)
                    .color(theme::TEXT_DIM),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if theme::button(ui, Some(Icon::Add), crate::tr!("Добавить файлы…", "Add files…"), egui::Vec2::ZERO, Kind::Secondary).clicked() {
                        pick_files = true;
                    }
                    if theme::button(ui, Some(Icon::Folder), crate::tr!("Добавить папку…", "Add folder…"), egui::Vec2::ZERO, Kind::Secondary).clicked() {
                        pick_folder = true;
                    }
                });
                let hint = if hovering {
                    crate::tr!("Отпустите файлы, чтобы добавить их", "Release the files to add them")
                } else {
                    crate::tr!("…или перетащите файлы и папки прямо в окно лаунчера", "…or drag files and folders right into the launcher window")
                };
                ui.label(egui::RichText::new(hint).size(11.5).color(if hovering {
                    theme::EQUIPPED
                } else {
                    theme::TEXT_FAINT
                }));
                if !has_model {
                    ui.label(
                        egui::RichText::new(crate::tr!("У предмета нет модели в схеме: одиночный .vmdl_c без пути models/… не подставится.", "The item has no model in the schema: a single .vmdl_c without a models/… path will not be used."))
                            .size(11.0)
                            .color(theme::TEXT_FAINT),
                    );
                }
                if needs_target {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(
                        egui::RichText::new(crate::trf!("Слот «{slot_label}»: у героя нет стандартного файла — подменять нечего. \
                             Наденьте в игре любой предмет в этом слоте и укажите его: модель выбранного \
                             предмета запишется по пути надетого, и игра покажет её.", "Slot «{slot_label}»: the hero has no default file, nothing to replace. Equip any item in this slot in the game and pick it here: the selected item's model is written to the equipped item's path, and the game will show it."
                        ))
                        .size(11.0)
                        .color(theme::ACCENT),
                    );
                    if donors.is_empty() {
                        ui.label(
                            egui::RichText::new(
                                crate::tr!("В каталоге нет других предметов этого героя в этом слоте — впишите путь вручную.", "The catalog has no other items of this hero in this slot, type the path by hand."),
                            )
                            .size(11.0)
                            .color(theme::TEXT_FAINT),
                        );
                    } else {
                        let shown = donor_label
                            .clone()
                            .unwrap_or_else(|| crate::tr!("выберите надетый предмет…", "choose the equipped item…").to_string());
                        let width = ui.available_width() - 10.0;
                        theme::combo(ui, "donor_pick", egui::RichText::new(shown).size(12.5), width, |ui| {
                            for (n, m) in &donors {
                                let is_sel = donor_label.as_deref() == Some(n.as_str());
                                if ui.selectable_label(is_sel, egui::RichText::new(n).size(12.5)).clicked() {
                                    picked_donor = Some(m.clone());
                                }
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        let width = ui.available_width() - 80.0;
                        theme::input(
                            ui,
                            &mut donor,
                            crate::tr!("или путь вручную: models/items/…/надетый.vmdl", "or a path by hand: models/items/…/equipped.vmdl"),
                            width,
                            None,
                            false,
                        );
                        if theme::button(ui, None, crate::tr!("Сброс", "Reset"), egui::Vec2::ZERO, Kind::Ghost).clicked() {
                            donor.clear();
                        }
                    });
                    if !donor.trim().is_empty() && !crate::extract::is_model_path(&donor) {
                        ui.label(
                            egui::RichText::new(crate::tr!("Путь должен содержать папку и оканчиваться на .vmdl или .vmdl_c", "The path must contain a folder and end with .vmdl or .vmdl_c"))
                                .size(11.0)
                                .color(theme::DANGER),
                        );
                    }
                }
                ui.add_space(8.0);
                theme::caption(ui, &crate::trf!("Файлы ({})", "Files ({})", files.len()));
                egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                    if files.is_empty() {
                        ui.label(egui::RichText::new(crate::tr!("Пока пусто", "Empty so far")).color(theme::TEXT_FAINT));
                    }
                    for f in &files {
                        ui.horizontal(|ui| {
                            let target = if f.to.is_empty() {
                                crate::trf!(
                                    "VPK целиком: {}", "Whole VPK: {}",
                                    Path::new(&f.from).file_name().and_then(|n| n.to_str()).unwrap_or("?")
                                )
                            } else {
                                f.to.clone()
                            };
                            if attach::is_custom(f) {
                                if theme::icon_button_ex(ui, Icon::Close, 22.0, theme::TEXT_FAINT, crate::tr!("Убрать файл", "Remove file")).clicked() {
                                    remove = Some(f.clone());
                                }
                            } else {
                                ui.label(egui::RichText::new(crate::tr!("из каталога", "from the catalog")).size(10.5).color(theme::TEXT_FAINT));
                            }
                            ui.label(egui::RichText::new(target).monospace().size(11.5).color(theme::TEXT));
                        });
                    }
                });
            });

        if pick_files {
            if let Some(paths) = rfd::FileDialog::new()
                .set_title(crate::tr!("Файлы мода", "Mod files"))
                .add_filter(crate::tr!("Файлы Source 2 и VPK", "Source 2 files and VPK"), &["vpk", "vmdl_c", "vmat_c", "vtex_c", "vpcf_c", "vsnd_c", "vmesh_c", "vanim_c"])
                .add_filter(crate::tr!("Все файлы", "All files"), &["*"])
                .pick_files()
            {
                self.attach_files(&id, paths);
            }
        }
        if pick_folder {
            if let Some(dir) = rfd::FileDialog::new().set_title(crate::tr!("Папка мода", "Mod folder")).pick_folder() {
                self.attach_files(&id, vec![dir]);
            }
        }
        if !dropped.is_empty() {
            self.attach_files(&id, dropped);
        }
        if let Some(f) = remove {
            self.remove_attached(&id, &f);
        }
        if let Some(m) = picked_donor {
            donor = m;
        }
        if needs_target {
            let current = self.config.override_targets.get(&id).cloned().unwrap_or_default();
            if donor.trim() != current {
                if donor.trim().is_empty() {
                    self.config.override_targets.remove(&id);
                } else {
                    self.config
                        .override_targets
                        .insert(id.clone(), donor.trim().to_string());
                }
                let _ = config::save_config(&self.config);
            }
        }
        if !open {
            self.files_window = None;
        }
    }

    // ------------------------------------------------------------ catalog refresh

    fn gencatalog_exe() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let candidate = exe.with_file_name("gencatalog.exe");
        candidate.is_file().then_some(candidate)
    }

    /// Re-downloads the item schema and rebuilds catalog.json in the current assets folder.
    fn start_catalog_refresh(&mut self, ctx: &egui::Context) {
        if self.catalog_job.is_some() {
            return;
        }
        let Some(exe) = Self::gencatalog_exe() else {
            self.set_status(StatusKind::Error, crate::tr!("Рядом с лаунчером нет gencatalog.exe", "gencatalog.exe is not next to the launcher"));
            return;
        };
        let out = self.assets.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("catalog-refresh".into())
            .spawn(move || {
                use std::os::windows::process::CommandExt;
                let result = std::process::Command::new(&exe)
                    .arg("--out")
                    .arg(&out)
                    .arg("--refresh")
                    .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                    .output()
                    .map_err(|e| crate::trf!("не удалось запустить gencatalog: {e}", "could not start gencatalog: {e}"))
                    .and_then(|o| {
                        let text = String::from_utf8_lossy(&o.stdout).to_string();
                        if o.status.success() {
                            Ok(text
                                .lines()
                                .rev()
                                .find(|l| l.starts_with("wrote"))
                                .unwrap_or(crate::tr!("готово", "done"))
                                .to_string())
                        } else {
                            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                            Err(if err.is_empty() { text } else { err })
                        }
                    });
                let _ = tx.send(result);
                ctx.request_repaint();
            })
            .ok();
        self.catalog_job = Some(rx);
        self.set_status(StatusKind::Warn, crate::tr!("Обновляю каталог: скачиваю свежую схему предметов…", "Updating the catalog: downloading a fresh item schema…"));
    }

    /// On first start the launcher fetches what it needs by itself: the full
    /// item catalog (gencatalog downloads the public schema) and
    /// Source2Viewer-CLI (3D preview, colourings, glows).
    fn first_start_setup(&mut self, ctx: &egui::Context) {
        if self.setup_started {
            return;
        }
        self.setup_started = true;
        if crate::preview::find_cli(&self.config.vrf_cli_path).is_none() && self.config.vrf_cli_path.trim().is_empty() {
            let (tx, rx) = std::sync::mpsc::channel();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("download-tools".into())
                .spawn(move || {
                    let _ = tx.send(crate::downloads::download_vrf_cli());
                    ctx.request_repaint();
                })
                .ok();
            self.tools_job = Some(rx);
        }
        // only the small sample catalog (or none): build the full one
        if self.catalog.skins.len() < 100 {
            self.start_catalog_refresh(ctx);
            if self.tools_job.is_some() {
                self.set_status(StatusKind::Warn, crate::tr!("Первый запуск: скачиваю каталог предметов и Source2Viewer-CLI…", "First start: downloading the item catalog and Source2Viewer-CLI…"));
            }
        } else if self.tools_job.is_some() {
            self.set_status(StatusKind::Warn, crate::tr!("Скачиваю Source2Viewer-CLI (3D-превью и эффекты)…", "Downloading Source2Viewer-CLI (3D preview and effects)…"));
        }
    }

    fn poll_tools_job(&mut self) {
        let Some(rx) = &self.tools_job else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.tools_job = None;
        match result {
            Ok(_) if self.catalog_job.is_none() => self.set_status(StatusKind::Ok, crate::tr!("Source2Viewer-CLI скачан", "Source2Viewer-CLI downloaded")),
            Ok(_) => {}
            Err(e) => self.set_status(StatusKind::Error, e),
        }
    }

    fn poll_catalog_job(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.catalog_job else { return };
        let Ok(result) = rx.try_recv() else { return };
        self.catalog_job = None;
        match result {
            Ok(_) => {
                if let Some(build) = config::dota_build_id(&self.config.dota_path) {
                    self.config.catalog_build = build;
                    let _ = config::save_config(&self.config);
                }
                self.catalog_outdated = None;
                self.reload_catalog(ctx);
            }
            Err(e) => self.set_status(StatusKind::Error, crate::trf!("Обновление каталога не удалось: {e}", "Catalog update failed: {e}")),
        }
    }

    /// Resolves a request into a subject and its model list. Items of a hero are
    /// shown on that hero (with the rest of the active preset); general items alone.
    fn preview_plan(&self, req: &PreviewRequest) -> Result<(String, Subject, Vec<String>), String> {
        let selected = self.selected_ids();
        match req {
            PreviewRequest::Item(id) => {
                let skin = self
                    .catalog
                    .skins
                    .iter()
                    .find(|s| &s.id == id)
                    .ok_or_else(|| crate::trf!("Предмет {id} не найден в каталоге", "Item {id} is not in the catalog"))?;
                let persona = catalog::is_persona_slot(&skin.slot);
                let on_hero = skin.hero_names().into_iter().find_map(|h| {
                    self.catalog
                        .outfit(h, persona, &selected, Some(id))
                        .map(|models| (h.to_string(), models))
                });
                Ok(match on_hero {
                    Some((hero, models)) => (
                        format!("{hero} – {}", skin.name),
                        Subject::Hero {
                            hero,
                            persona,
                            forced: Some(id.clone()),
                        },
                        models,
                    ),
                    None => {
                        let model = skin
                            .model
                            .clone()
                            .ok_or_else(|| crate::trf!("У «{}» нет 3D-модели в схеме", "«{}» has no 3D model in the schema", skin.name))?;
                        (skin.name.clone(), Subject::Item, vec![model])
                    }
                })
            }
            PreviewRequest::Hero { hero, persona } => {
                let models = self
                    .catalog
                    .outfit(hero, *persona, &selected, None)
                    .ok_or_else(|| crate::trf!("Нет данных о модели героя {hero}: пересоберите каталог", "No hero model data for {hero}: rebuild the catalog"))?;
                let title = if *persona {
                    crate::trf!("{hero} – персона", "{hero} – persona")
                } else {
                    hero.clone()
                };
                Ok((
                    title,
                    Subject::Hero {
                        hero: hero.clone(),
                        persona: *persona,
                        forced: None,
                    },
                    models,
                ))
            }
        }
    }

    fn open_preview(
        &mut self,
        ctx: &egui::Context,
        gl: Option<&Arc<eframe::glow::Context>>,
        req: &PreviewRequest,
    ) {
        let (title, subject, models) = match self.preview_plan(req) {
            Ok(plan) => plan,
            Err(e) => {
                self.set_status(StatusKind::Warn, e);
                return;
            }
        };
        let Some(cli) = self.vrf_cli.clone() else {
            self.set_status(
                StatusKind::Warn,
                crate::tr!("Для 3D нужен Source2Viewer-CLI: положите его в tools\\vrf-cli или укажите путь в настройках", "3D needs Source2Viewer-CLI: put it into tools\\vrf-cli or set its path in the settings"),
            );
            return;
        };
        let Some(game_dir) = vpk::game_dir_from_exe(&self.config.dota_path) else {
            self.set_status(StatusKind::Warn, crate::tr!("Не найдена установка Dota 2 (настройки → dota2.exe)", "Dota 2 installation not found (settings → dota2.exe)"));
            return;
        };
        if let Some(mut old) = self.preview.take() {
            old.destroy(gl);
        }
        self.preview = Some(Preview::new(
            ctx,
            title,
            subject,
            cli,
            game_dir.join("pak01_dir.vpk"),
            models,
        ));
    }

    /// Keeps an open hero preview in sync with the preset (items equipped or removed).
    fn sync_preview(&mut self, ctx: &egui::Context, gl: Option<&Arc<eframe::glow::Context>>) {
        let Some(p) = &self.preview else { return };
        let Subject::Hero { hero, persona, forced } = &p.subject else {
            return;
        };
        let selected = self.selected_ids();
        let Some(models) = self.catalog.outfit(hero, *persona, &selected, forced.as_deref()) else {
            return;
        };
        if let Some(p) = &mut self.preview {
            if p.models != models {
                p.reload(ctx, gl, models);
            }
        }
    }

    fn set_status(&mut self, kind: StatusKind, msg: impl Into<String>) {
        self.status = (kind, msg.into());
    }

    fn running(&self) -> bool {
        self.dota_running.load(Ordering::SeqCst)
    }

    /// Cached GSI state, refreshed at most every 2 s.
    fn game_state(&mut self) -> gsi::GameState {
        if self.game_state.0.elapsed() > Duration::from_secs(2) {
            self.game_state = (Instant::now(), gsi::query());
        }
        self.game_state.1.clone()
    }

    fn set_active(&mut self, i: usize) {
        self.active = i;
        self.config.active_profile = i;
        let _ = config::save_config(&self.config);
    }

    fn active_index(&self) -> usize {
        self.active.min(self.profiles.len() - 1)
    }

    fn selected_ids(&self) -> BTreeSet<String> {
        self.profiles[self.active_index()].skins.iter().cloned().collect()
    }

    /// Puts a whole set on (replacing the items of the same slots), or takes it
    /// off when every piece is worn already.
    fn wear_set(&mut self, name: &str, members: &[String]) {
        let idx = self.active_index();
        let catalog = &self.catalog;
        let profile = &mut self.profiles[idx];
        if members.iter().all(|m| profile.skins.contains(m)) {
            profile.skins.retain(|id| !members.contains(id));
            self.set_status(StatusKind::Ok, crate::trf!("Набор «{name}» снят", "Set “{name}” taken off"));
        } else {
            let items: Vec<&Skin> = catalog.skins.iter().filter(|s| members.contains(&s.id)).collect();
            crate::picks::wear(profile, catalog, &items);
            self.set_status(StatusKind::Ok, crate::trf!("Надет набор «{name}»: предметов {}", "Set “{name}” put on: {} items", members.len()));
        }
        self.profiles_dirty = true;
    }

    /// Adds or removes an item; adding replaces items of the same hero and slot.
    fn toggle_skin(&mut self, id: &str) {
        let idx = self.active_index();
        self.profiles_dirty = true;
        let list = &mut self.profiles[idx].skins;
        if let Some(pos) = list.iter().position(|s| s == id) {
            list.remove(pos);
            return;
        }

        let mut replaced: Vec<String> = Vec::new();
        if let Some(new_skin) = self.catalog.skins.iter().find(|s| s.id == id) {
            list.retain(|other_id| {
                let other = self.catalog.skins.iter().find(|s| &s.id == other_id);
                match other {
                    Some(o) if new_skin.conflicts_with(o) => {
                        replaced.push(o.name.clone());
                        false
                    }
                    _ => true,
                }
            });
        }
        list.push(id.to_string());

        if !replaced.is_empty() {
            self.set_status(
                StatusKind::Ok,
                crate::trf!("Заменено в том же слоте: {}", "Replaced in the same slot: {}", replaced.join(", ")),
            );
        }
    }

    /// Chosen style of an item (and of its set's worn pieces) in the active preset.
    fn set_style(&mut self, id: &str, style: u32) {
        let Some(skin) = self.catalog.skins.iter().find(|s| s.id == id) else { return };
        let idx = self.active_index();
        let worn: Vec<String> = self
            .catalog
            .skins
            .iter()
            .filter(|s| self.profiles[idx].skins.contains(&s.id))
            .map(|s| s.name.clone())
            .collect();
        let name = skin.name.clone();
        crate::styles::set(&mut self.profiles[idx].styles, &name, style, &worn);
        self.profiles_dirty = true;
        let styles = crate::styles::of(&name);
        if let Some(n) = styles.get(style as usize) {
            self.set_status(StatusKind::Ok, crate::trf!("Стиль «{}»: {n} — нажмите «Применить»", "Style of “{}”: {n} — press Apply", name));
        }
    }

    fn reload_catalog(&mut self, ctx: &egui::Context) {
        let (assets, mut catalog, error) = load_catalog_with_fallback();
        catalog.merge_custom(&self.custom);
        self.assets = assets;
        self.catalog = catalog;
        self.catalog_error = error;
        self.icons = IconLoader::new(ctx, vpk::game_dir_from_exe(&self.config.dota_path));
        match self.catalog_error.clone() {
            Some(err) => self.set_status(StatusKind::Warn, crate::trf!("Перезагрузка: {err}", "Reload: {err}")),
            None => self.set_status(
                StatusKind::Ok,
                crate::trf!("Перезагружено предметов: {}, {}", "Items reloaded: {}, {}",
                    self.catalog.skins.len(),
                    self.icons.source
                ),
            ),
        }
    }

    fn open_assets_folder(&mut self) {
        let _ = std::fs::create_dir_all(&self.assets);
        match std::process::Command::new("explorer").arg(&self.assets).spawn() {
            Ok(_) => self.set_status(
                StatusKind::Ok,
                crate::trf!("Открыта папка {}", "Opened folder {}", self.assets.display()),
            ),
            Err(e) => self.set_status(StatusKind::Error, crate::trf!("Не удалось открыть папку: {e}", "Could not open the folder: {e}")),
        }
    }

    fn save_all(&mut self) {
        // If the override folder changed, unmount the old one in gameinfo.gi
        // and mount the new one instead.
        let now = config::resolve_mod_dir(&self.config);
        if now != self.mounted_mod_dir && !self.config.dota_path.is_empty() && !self.config.memory_swap {
            let old = self.mounted_mod_dir.clone();
            if let Err(e) = crate::mount::remove(&self.config.dota_path, &old) {
                self.set_status(
                    StatusKind::Warn,
                    crate::trf!("Не удалось снять старую папку оверрайдов: {e}", "Could not remove the old overrides folder: {e}"),
                );
            }
            match crate::mount::ensure(&self.config.dota_path, &now) {
                Ok(_) => self.mounted_mod_dir = now,
                Err(e) => {
                    self.set_status(
                        StatusKind::Error,
                        crate::trf!("Монтирование оверрайдов не удалось: {e}", "Mounting the overrides failed: {e}"),
                    );
                }
            }
        }
        let profiles = config::save_profiles(&self.profiles);
        self.profiles_seen = config::profiles_modified();
        let cfg = config::save_config(&self.config);
        match (profiles, cfg) {
            (Ok(_), Ok(_)) => self.set_status(StatusKind::Ok, crate::tr!("Пресеты и настройки сохранены.", "Presets and settings saved.")),
            (Err(e), _) | (_, Err(e)) => self.set_status(StatusKind::Error, e),
        }
    }

    fn build_plan(&self, restart: bool) -> job::Plan {
        let ids = self.selected_ids();
        let all: Vec<&Skin> = self.catalog.skins.iter().filter(|s| ids.contains(&s.id)).collect();
        // as the heroes look (a hero whose persona the preset has: its persona's items); its
        // classic items' swaps stay for the menu's classic look
        let chosen = catalog::worn(all.clone());
        let menu_only = catalog::menu_only(&all, &chosen);
        // the items the account wears: their files take the preset's too
        let owned = crate::vpk::game_dir_from_exe(&self.config.dota_path).map(|d| crate::owned::equipped(&d)).unwrap_or_default();
        let (overrides, schema_picks, unsupported) =
            crate::extract::plan(&all, &self.catalog, &self.config.override_targets, &owned);
        let emblem = crate::emblem::from_preset(&chosen);
        let mut general = crate::visuals::general_items(&chosen);
        let mut hero_items = crate::visuals::hero_items(&chosen);
        crate::visuals::with_siblings(&mut hero_items, &self.catalog, &owned);
        crate::visuals::with_styles(&mut general, &mut hero_items, &self.profiles[self.active_index()].styles);
        job::Plan {
            install: Install::new(
                &chosen,
                &self.assets,
                config::resolve_mod_dir(&self.config),
                self.config.pack_vpk,
            ),
            overrides,
            schema_picks,
            unsupported,
            general,
            hero_items,
            vrf_cli: crate::preview::find_cli(&self.config.vrf_cli_path),
            emblem,
            dota_exe: self.config.dota_path.clone(),
            launch_args: self.config.launch_args.clone(),
            restart,
            memory_swap: self.config.memory_swap,
            owned,
            gems: crate::gems::chosen(&self.catalog, &self.profiles[self.active_index()].gems),
            menu_only,
        }
    }

    fn start_job(&mut self, ctx: &egui::Context, restart: bool) {
        if self.job.is_some() {
            return;
        }
        let _ = config::save_config(&self.config);
        self.job = Some(job::spawn(self.build_plan(restart), ctx.clone()));
        self.set_status(StatusKind::Warn, crate::tr!("Выполняется…", "Working…"));
    }

    /// Apply button: plain apply when Dota is closed; when it runs, restart it
    /// but only from the menu (GSI), asking first if the state is unknown.
    fn request_apply(&mut self, ctx: &egui::Context) {
        if self.config.memory_swap {
            // No restart: a running Dota gets the new table; an empty preset
            // brings the default looks back.
            self.start_job(ctx, false);
            return;
        }
        if self.selected_ids().is_empty() {
            self.set_status(StatusKind::Warn, crate::tr!("В этом пресете ничего не выбрано.", "Nothing is selected in this preset."));
            return;
        }
        if !self.running() {
            self.start_job(ctx, false);
            return;
        }
        self.game_state.0 = Instant::now() - Duration::from_secs(60);
        let state = self.game_state();
        if state.safe_to_close() {
            self.start_job(ctx, true);
        } else if state == gsi::GameState::Unknown {
            self.confirm_restart = true;
        } else {
            self.set_status(
                StatusKind::Warn,
                crate::tr!("Идёт матч: перезапуск возможен только в меню или после игры.", "A match is on: restarting is only possible in the menu or after the game."),
            );
        }
    }

    fn launch_current(&mut self, ctx: &egui::Context) {
        if self.running() {
            self.set_status(StatusKind::Warn, crate::tr!("Dota 2 уже запущена.", "Dota 2 is already running."));
            return;
        }
        if self.config.memory_swap {
            // The DLL must be injected at start; the job builds its table and launches.
            self.start_job(ctx, false);
            return;
        }
        if let Some(dir) = vpk::game_dir_from_exe(&self.config.dota_path) {
            let _ = gsi::install_cfg(&dir);
        }
        // Steam may rewrite gameinfo.gi on update; re-check before launching.
        let mod_dir = config::resolve_mod_dir(&self.config);
        match crate::mount::ensure(&self.config.dota_path, &mod_dir) {
            Ok(_) => {}
            Err(e) => {
                self.set_status(StatusKind::Error, crate::trf!("Монтирование оверрайдов: {e}", "Mounting the overrides: {e}"));
                return;
            }
        }
        let args = gsi::with_launch_flag(&self.config.launch_args);
        match dota::launch(&self.config.dota_path, &args) {
            Ok(msg) => self.set_status(StatusKind::Ok, msg),
            Err(e) => self.set_status(StatusKind::Error, crate::trf!("Не удалось запустить: {e}", "Could not start: {e}")),
        }
    }

    fn poll_job(&mut self) {
        let Some(rx) = &self.job else { return };
        let mut finished = false;
        let mut latest: Option<(StatusKind, String)> = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                job::Msg::Step(s) => latest = Some((StatusKind::Warn, s)),
                job::Msg::Done(Ok(s)) => {
                    latest = Some((StatusKind::Ok, s));
                    finished = true;
                }
                job::Msg::Done(Err(e)) => {
                    latest = Some((StatusKind::Error, e));
                    finished = true;
                }
            }
        }
        if let Some((k, s)) = latest {
            self.set_status(k, s);
        }
        if finished {
            self.job = None;
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_restart {
            return;
        }
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut go = false;
        theme::modal(ctx, "confirm_restart", 400.0, |ui| {
            dialog_title(ui, crate::tr!("Перезапустить Dota 2?", "Restart Dota 2?"));
            dialog_text(
                ui,
                crate::tr!("Не удалось узнать, идёт ли матч: Dota запущена без параметра \
                 -gamestateintegration (лаунчер добавляет его сам, когда запускает игру).", "Could not tell whether a match is on: Dota was started without -gamestateintegration (the launcher adds it itself when it starts the game)."),
                theme::TEXT_DIM,
            );
            dialog_text(ui, crate::tr!("Перезапускайте только из главного меню.", "Restart only from the main menu."), theme::ACCENT_HI);
            ui.add_space(14.0);
            dialog_buttons(ui, |ui| {
                if theme::button(ui, None, crate::tr!("Я в меню, перезапустить", "I am in the menu, restart"), egui::Vec2::ZERO, Kind::Primary).clicked() {
                    go = true;
                }
                if theme::button(ui, None, crate::tr!("Отмена", "Cancel"), egui::Vec2::ZERO, Kind::Secondary).clicked() {
                    close = true;
                }
            });
        });
        if go {
            self.confirm_restart = false;
            self.start_job(ctx, true);
        } else if close {
            self.confirm_restart = false;
        }
    }

    /// Everything the launcher changed in the game, taken back: the in-memory swap
    /// table (a running changer.dll stops swapping), the item table for empty slots
    /// with its gameinfo/signature lines, file-mode overrides with their gameinfo
    /// line, our autoexec.cfg lines and the GSI config. Presets stay.
    fn revert_all(&mut self) {
        let dota = self.config.dota_path.clone();
        let mod_dir = config::resolve_mod_dir(&self.config);
        let mut errors: Vec<String> = Vec::new();
        // an empty table: the DLL drops its swaps, glows, taunts and slot icons
        if let Err(e) = crate::inject::write_config(&[], &[], &std::collections::HashMap::new(), &std::collections::HashSet::new(), &std::collections::HashSet::new()) {
            errors.push(e);
        }
        if !dota.is_empty() {
            if let Err(e) = crate::schema_mount::remove(&dota) {
                errors.push(e);
            }
            if let Err(e) = crate::mount::remove(&dota, &mod_dir) {
                errors.push(e);
            }
        }
        if let Err(e) = apply::clear_mod(&mod_dir) {
            errors.push(e);
        }
        if let Some(game_dir) = vpk::game_dir_from_exe(&dota) {
            for r in [
                crate::autoexec::keep_ring(&game_dir, false).map(|_| ()),
                crate::autoexec::keep_weather(&game_dir, None).map(|_| ()),
                crate::autoexec::keep_combine_off(&game_dir, false).map(|_| ()),
                crate::gsi::remove_cfg(&game_dir),
            ] {
                if let Err(e) = r {
                    errors.push(e);
                }
            }
        }
        let _ = std::fs::remove_file(config::app_dir().join("schema_loaded.txt"));
        if errors.is_empty() {
            let mut msg = crate::tr!("Все изменения откатены, файлы игры в исходном виде", "Everything reverted, the game files are as Steam shipped them").to_string();
            if self.running() {
                msg.push_str(crate::tr!(" – в запущенной Dota модели вернутся со следующей загрузки, всё остальное — после перезапуска",
                    " – in the running Dota models come back on their next load, the rest after a restart"));
            }
            self.set_status(StatusKind::Ok, msg);
        } else {
            self.set_status(StatusKind::Error, crate::trf!("Откат не полный: {}", "Revert incomplete: {}", errors.join("; ")));
        }
    }

    fn revert_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_revert {
            return;
        }
        let mut close = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut go = false;
        theme::modal(ctx, "confirm_revert", 420.0, |ui| {
            dialog_title(ui, crate::tr!("Откатить все изменения?", "Revert everything?"));
            dialog_text(
                ui,
                crate::tr!(
                    "Лаунчер уберёт всё, что менял в игре: подмены моделей и эффектов (в памяти и файлами), \
                     таблицу предметов для пустых слотов и её строки в gameinfo и dota.signatures, \
                     свои строки в autoexec.cfg и конфиг GSI.",
                    "The launcher takes back everything it changed in the game: model and effect swaps (in memory \
                     and as files), the item table for empty slots with its gameinfo and dota.signatures lines, \
                     its autoexec.cfg lines and the GSI config."
                ),
                theme::TEXT_DIM,
            );
            dialog_text(ui, crate::tr!("Пресеты и настройки лаунчера останутся.", "Presets and launcher settings stay."), theme::TEXT_FAINT);
            ui.add_space(14.0);
            dialog_buttons(ui, |ui| {
                if theme::button(ui, Some(Icon::Revert), crate::tr!("Откатить", "Revert"), egui::Vec2::ZERO, Kind::Danger).clicked() {
                    go = true;
                }
                if theme::button(ui, None, crate::tr!("Отмена", "Cancel"), egui::Vec2::ZERO, Kind::Secondary).clicked() {
                    close = true;
                }
            });
        });
        if go {
            self.confirm_revert = false;
            self.revert_all();
        } else if close {
            self.confirm_revert = false;
        }
    }

    // ------------------------------------------------------------ top bar

    /// The bar at the top of the content panel: search and filters, catalog actions.
    /// Returns its bottom edge.
    fn top_bar(&mut self, ctx: &egui::Context) -> f32 {
        let mut heroes = self.catalog.heroes();
        if self.catalog.skins.iter().any(Skin::is_general) {
            heroes.insert(0, NO_HERO.to_string());
        }
        let slots = self.catalog.slots();
        let mut want_reload = false;
        let mut want_save = false;
        let mut want_open = false;
        let mut want_refresh = false;
        let refreshing = self.catalog_job.is_some();
        let outdated = self.catalog_outdated.is_some();

        let bar = egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::none()
                    .outer_margin(egui::Margin { left: 0.0, right: PANEL_GAP, top: PANEL_GAP, bottom: 0.0 })
                    .inner_margin(egui::Margin { left: 18.0, right: 12.0, top: 12.0, bottom: 12.0 }),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(theme::CONTROL_H);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // a narrow window: the catalog button keeps its icon only and the
                    // search and dropdowns give up some width
                    let narrow = ui.available_width() < 900.0;
                    if self.view == View::Settings {
                        ui.label(
                            egui::RichText::new(crate::tr!("Настройки", "Settings"))
                                .font(bold_font(16.0))
                                .color(theme::TEXT),
                        );
                    } else {
                        let picking_hero = self.view == View::Heroes && self.hero_filter.is_empty();
                        let hint = if narrow {
                            crate::tr!("Поиск", "Search")
                        } else if picking_hero {
                            crate::tr!("Поиск героя", "Search a hero")
                        } else {
                            crate::tr!("Поиск: предмет, герой, слот", "Search: item, hero, slot")
                        };
                        let (search_w, hero_w, slot_w) = if narrow { (150.0, 140.0, 120.0) } else { (260.0, 170.0, 150.0) };
                        theme::input(ui, &mut self.search, hint, search_w, Some(Icon::Search), true);

                        combo(ui, "hero_filter", crate::tr!("Все герои", "All heroes"), hero_w, &mut self.hero_filter, &heroes, |h| {
                            if h == NO_HERO {
                                crate::tr!("Без героя (общие)", "No hero (general)").to_string()
                            } else {
                                h.to_string()
                            }
                        });
                        if self.view == View::Catalog {
                            combo(ui, "slot_filter", crate::tr!("Все слоты", "All slots"), slot_w, &mut self.slot_filter, &slots, |s| {
                                catalog::slot_label(s)
                            });
                        }

                        let any_filter = !self.search.is_empty()
                            || !self.hero_filter.is_empty()
                            || !self.slot_filter.is_empty()
                            || self.rarity_filter.is_some()
                            || self.only_selected;
                        let reset = |ui: &mut egui::Ui| {
                            if narrow {
                                theme::icon_button(ui, Icon::Close, "")
                            } else {
                                theme::button(ui, None, crate::tr!("Сбросить", "Reset"), egui::Vec2::ZERO, Kind::Ghost)
                            }
                        };
                        if any_filter && reset(ui).on_hover_text(crate::tr!("Сбросить все фильтры", "Reset all filters")).clicked() {
                            self.search.clear();
                            self.hero_filter.clear();
                            self.slot_filter.clear();
                            self.rarity_filter = None;
                            self.only_selected = false;
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let hint = if outdated {
                            crate::tr!("Dota обновилась после последнего обновления каталога: скачать свежую схему предметов", "Dota has updated since the last catalog update: download a fresh item schema")
                        } else {
                            crate::tr!("Скачать свежую схему предметов и пересобрать каталог (около минуты)", "Download a fresh item schema and rebuild the catalog (about a minute)")
                        };
                        if refreshing {
                            if !narrow {
                                theme::button(ui, None, crate::tr!("Обновление…", "Updating…"), egui::Vec2::ZERO, Kind::Secondary).on_hover_text(hint);
                            }
                            ui.add(egui::Spinner::new().size(14.0).color(theme::TEXT_DIM)).on_hover_text(hint);
                        } else {
                            let label = if narrow { "" } else { crate::tr!("Обновить каталог", "Update catalog") };
                            let size = if narrow { egui::vec2(theme::CONTROL_H, 0.0) } else { egui::Vec2::ZERO };
                            let update = theme::button(ui, Some(Icon::Download), label, size, Kind::Secondary)
                                .on_hover_text(hint);
                            if outdated {
                                theme::dot(ui.painter(), update.rect.right_top() + egui::vec2(-5.0, 5.0), theme::FAVORITE);
                            }
                            if update.clicked() {
                                want_refresh = true;
                            }
                        }
                        ui.add_space(6.0);
                        if theme::icon_button(ui, Icon::Folder, crate::tr!("Открыть папку assets", "Open the assets folder")).clicked() {
                            want_open = true;
                        }
                        if theme::icon_button(ui, Icon::Save, crate::tr!("Сохранить лоадауты и настройки", "Save loadouts and settings")).clicked() {
                            want_save = true;
                        }
                        if theme::icon_button(ui, Icon::Reload, crate::tr!("Перезагрузить каталог", "Reload catalog")).clicked() {
                            want_reload = true;
                        }
                    });
                });
            });

        if want_reload {
            self.reload_catalog(ctx);
        }
        if want_save {
            self.save_all();
        }
        if want_open {
            self.open_assets_folder();
        }
        if want_refresh {
            self.start_catalog_refresh(ctx);
        }
        bar.response.rect.max.y
    }

    // ------------------------------------------------------------ side panel

    fn side_panel(&mut self, ctx: &egui::Context) {
        let mut want_apply = false;
        let mut want_launch = false;
        let selected_count = self.profiles[self.active_index()].skins.len();
        let running = self.running();
        let busy = self.job.is_some();

        egui::SidePanel::left("side")
            .exact_width(SIDE_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(theme::BG_DEEP)
                    .inner_margin(egui::Margin { left: 12.0, right: 12.0, top: 14.0, bottom: 12.0 }),
            )
            .show(ctx, |ui| {
                let full_w = ui.available_width();

                // Bottom block first, so it stays pinned while the rest scrolls.
                egui::TopBottomPanel::bottom("side_actions")
                    .frame(egui::Frame::none().inner_margin(egui::Margin {
                        left: 0.0,
                        right: 0.0,
                        top: 12.0,
                        bottom: 0.0,
                    }))
                    .show_separator_line(false)
                    .show_inside(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        let memory = self.config.memory_swap;
                        // In memory mode the preset is applied by launching, so
                        // there is no separate apply button.
                        if !memory {
                            let base = if busy {
                                crate::tr!("Выполняется…", "Working…")
                            } else if running {
                                crate::tr!("Применить и перезапустить", "Apply and restart")
                            } else {
                                crate::tr!("Применить пресет", "Apply preset")
                            };
                            let apply_label = if selected_count == 0 || busy {
                                base.to_string()
                            } else {
                                format!("{base} · {selected_count}")
                            };
                            let hint = if running {
                                crate::tr!("Dota запущена: лаунчер закроет её (только в меню), применит пресет и запустит снова через Steam", "Dota is running: the launcher will close it (menu only), apply the preset and start it again through Steam")
                            } else {
                                crate::tr!("Собрать файлы выбранных предметов в папку оверрайдов", "Put the files of the selected items into the overrides folder")
                            };
                            if theme::button(ui, Some(Icon::Check), &apply_label, egui::vec2(full_w, 38.0), Kind::Secondary)
                                .on_hover_text(hint)
                                .clicked()
                                && !busy
                            {
                                want_apply = true;
                            }
                        }
                        let (launch_icon, launch_label) = match (memory, busy, running) {
                            (true, true, _) => (None, crate::tr!("Выполняется…", "Working…")),
                            (true, false, true) => (Some(Icon::Check), crate::tr!("Применить пресет", "Apply preset")),
                            (false, _, true) => (None, crate::tr!("Dota 2 запущена", "Dota 2 is running")),
                            _ => (Some(Icon::Play), crate::tr!("Запустить Dota 2", "Launch Dota 2")),
                        };
                        let launch = theme::cta_button(ui, launch_icon, launch_label, egui::vec2(full_w, 46.0));
                        let launch = if memory {
                            launch.on_hover_text(if running {
                                crate::tr!("Без перезапуска: уже загруженные модели сменятся со следующей загрузки героя \
                                 (например, в следующем матче)", "Without a restart: models already loaded change on the hero's next load (the next match, say)")
                            } else {
                                crate::tr!("Запустить Dota с выбранным пресетом", "Launch Dota with the selected preset")
                            })
                        } else {
                            launch
                        };
                        if launch.clicked() && !busy {
                            if memory && running {
                                want_apply = true;
                            } else {
                                want_launch = true;
                            }
                        }
                    });

                brand(ui);
                ui.add_space(18.0);
                ui.spacing_mut().item_spacing.y = 2.0;
                let pages = [
                    (View::Heroes, Icon::Heroes, crate::tr!("Герои", "Heroes"), crate::tr!("Слоты героя, как в арсенале Dota", "Hero slots, as in the Dota armory")),
                    (View::Catalog, Icon::Catalog, crate::tr!("Каталог", "Catalog"), crate::tr!("Все предметы с фильтрами", "Every item, with filters")),
                    (View::Settings, Icon::Settings, crate::tr!("Настройки", "Settings"), crate::tr!("Путь к игре, режим подмены, язык", "Game path, swap mode, language")),
                ];
                for (view, icon, label, hint) in pages {
                    if theme::nav_item(ui, icon, label, self.view == view).on_hover_text(hint).clicked() {
                        if view == View::Heroes {
                            self.hero_slot = None;
                        }
                        self.view = view;
                    }
                }
                ui.add_space(20.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.profiles_section(ui, full_w);
                    });
            });

        if want_apply {
            self.request_apply(ctx);
        }
        if want_launch {
            self.launch_current(ctx);
        }
    }

    fn profiles_section(&mut self, ui: &mut egui::Ui, full_w: f32) {
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            theme::caption(ui, crate::tr!("Пресеты", "Presets"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if self.profiles.len() > 1
                    && theme::icon_button_ex(ui, Icon::Delete, 26.0, theme::TEXT_FAINT, crate::tr!("Удалить пресет", "Delete preset")).clicked()
                {
                    self.profiles.remove(self.active_index());
                    self.set_active(0);
                    self.profiles_dirty = true;
                }
                if theme::icon_button_ex(ui, Icon::Add, 26.0, theme::TEXT_FAINT, crate::tr!("Новый пресет", "New preset")).clicked() {
                    let name = crate::trf!("Пресет {}", "Preset {}", self.profiles.len() + 1);
                    self.profiles.push(Profile {
                        name,
                        skins: Vec::new(),
                        styles: Default::default(),
                        gems: Default::default(),
                    });
                    self.set_active(self.profiles.len() - 1);
                    self.profiles_dirty = true;
                }
            });
        });
        ui.add_space(4.0);

        let mut activate: Option<usize> = None;
        for (i, profile) in self.profiles.iter().enumerate() {
            if profile_row(ui, full_w, &profile.name, profile.skins.len(), i == self.active)
                .clicked()
            {
                activate = Some(i);
            }
        }
        if let Some(i) = activate {
            self.set_active(i);
        }

        ui.add_space(12.0);
        let idx = self.active_index();
        field_label(ui, crate::tr!("Название пресета", "Preset name"));
        let rename = theme::input(ui, &mut self.profiles[idx].name, crate::tr!("Название", "Name"), full_w, None, false);
        if rename.changed() {
            self.profiles_dirty = true;
        }
        if !self.profiles[idx].skins.is_empty() {
            ui.add_space(4.0);
            let clear = theme::button(ui, Some(Icon::Close), crate::tr!("Снять все предметы", "Remove all items"), egui::vec2(0.0, 30.0), Kind::Ghost)
                .on_hover_text(crate::tr!("Очистить выбор в этом пресете", "Clear the selection in this preset"));
            if clear.clicked() {
                self.profiles[idx].skins.clear();
                self.only_selected = false;
                self.profiles_dirty = true;
            }
        }
    }

    // ------------------------------------------------------------ settings page

    fn settings_central(&mut self, ctx: &egui::Context) {
        let mut revert = false;
        let mut reload = false;
        let mut save = false;
        let mut remount = false;
        egui::CentralPanel::default().frame(page_frame()).show(ctx, |ui| {
            egui::ScrollArea::vertical().id_salt("settings_page").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(760.0);
                ui.add_space(4.0);

                theme::section(
                    ui,
                    crate::tr!("Игра", "Game"),
                    crate::tr!("Где установлена Dota 2 и с какими параметрами её запускать", "Where Dota 2 is installed and what to start it with"),
                    |ui| {
                        field_label(ui, "dota2.exe");
                        ui.horizontal(|ui| {
                            let w = ui.available_width() - 96.0;
                            let edit = theme::input(ui, &mut self.config.dota_path, r"C:\...\dota2.exe", w, None, false);
                            if edit.lost_focus() {
                                reload = true; // icons come from this install
                                save = true;
                            }
                            if theme::button(ui, Some(Icon::Search), crate::tr!("Найти", "Find"), egui::vec2(88.0, 0.0), Kind::Secondary).clicked() {
                                match config::detect_dota() {
                                    Some(path) => {
                                        self.config.dota_path = path.to_string_lossy().to_string();
                                        reload = true;
                                        save = true;
                                    }
                                    None => self.set_status(
                                        StatusKind::Warn,
                                        crate::tr!("Библиотека Steam с Dota 2 не найдена.", "Steam library with Dota 2 not found."),
                                    ),
                                }
                            }
                        });
                        field_label(ui, crate::tr!("Параметры запуска", "Launch options"));
                        let w = ui.available_width();
                        if theme::input(ui, &mut self.config.launch_args, "-novid", w, None, false).lost_focus() {
                            save = true;
                        }
                    },
                );
                ui.add_space(12.0);

                theme::section(
                    ui,
                    crate::tr!("Режим подмены", "Swap mode"),
                    crate::tr!("Как лаунчер показывает предметы в игре", "How the launcher puts the items into the game"),
                    |ui| {
                        let mut file_mode = !self.config.memory_swap;
                        if theme::toggle(ui, &mut file_mode, crate::tr!("Файловый режим (без DLL)", "File mode (no DLL)"))
                            .on_hover_text(
                                crate::tr!("Вместо подмены в памяти файлы предметов кладутся в папку оверрайдов внутри игры \
                                 и подключаются через gameinfo.gi. Работают и свои файлы, Dota можно запускать из Steam.", "Instead of memory swapping, item files are put into an overrides folder inside the game and mounted through gameinfo.gi. Own files work too, Dota can be started from Steam."),
                            )
                            .changed()
                        {
                            self.config.memory_swap = !file_mode;
                            save = true;
                        }
                        ui.add_space(4.0);
                        if self.config.memory_swap {
                            let (text, color) = match crate::inject::find_dll() {
                                Some(_) => (
                                    crate::tr!("Подмена в памяти: changer.dll найдена, Dota запускается кнопкой лаунчера", "Memory mode: changer.dll found, Dota is started by the launcher button"),
                                    theme::EQUIPPED,
                                ),
                                None => (crate::tr!("Подмена в памяти: changer.dll не найдена рядом с лаунчером", "Memory mode: changer.dll not found next to the launcher"), theme::DANGER),
                            };
                            status_line(ui, text, color).on_hover_text(
                                crate::tr!("Файлы игры не трогаются: лаунчер сам запускает Dota и внедряет changer.dll, \
                                 которая подменяет модели при загрузке. Свои файлы в этом режиме не применяются. \
                                 Внедрение DLL в dota2.exe может привести к VAC-бану.", "Game files are not touched: the launcher starts Dota itself and injects changer.dll, which swaps models as they load. Own files do not apply in this mode. Injecting a DLL into dota2.exe can lead to a VAC ban."),
                            );
                            ui.label(
                                egui::RichText::new(
                                    crate::tr!("В игре: кнопка «ВЫБРАТЬ» у любого предмета или сета в Dota — надеть его", "In game: the «SELECT» button on any item or set in Dota equips it"),
                                )
                                .size(12.0)
                                .color(theme::TEXT_FAINT),
                            )
                            .on_hover_text(
                                crate::tr!("Пока открыт лаунчер. Предмет кладётся в активный пресет вместо предмета того же слота.", "While the launcher is open. The item goes into the active preset in place of the item of the same slot."),
                            );
                        } else {
                            field_label(ui, crate::tr!("Папка оверрайдов", "Overrides folder"));
                            let w = ui.available_width();
                            if theme::input(ui, &mut self.config.mod_dir, crate::tr!("пусто = <dota>\\game\\dota_skins", "empty = <dota>\\game\\dota_skins"), w, None, false)
                                .lost_focus()
                            {
                                remount = true;
                            }
                            ui.add_space(4.0);
                            if theme::toggle(ui, &mut self.config.pack_vpk, crate::tr!("Собирать в pak01_dir.vpk", "Pack into pak01_dir.vpk"))
                                .on_hover_text(crate::tr!("Иначе файлы копируются в папку оверрайдов как есть", "Otherwise files are copied into the overrides folder as they are"))
                                .changed()
                            {
                                save = true;
                            }
                        }
                    },
                );
                ui.add_space(12.0);

                theme::section(
                    ui,
                    crate::tr!("3D-превью", "3D preview"),
                    crate::tr!("Source2Viewer-CLI показывает модели и собирает цвета и эффекты предметов", "Source2Viewer-CLI shows the models and reads the items' colours and effects"),
                    |ui| {
                        field_label(ui, "Source2Viewer-CLI.exe");
                        let w = ui.available_width();
                        if theme::input(ui, &mut self.config.vrf_cli_path, crate::tr!("пусто = tools\\vrf-cli", "empty = tools\\vrf-cli"), w, None, false)
                            .lost_focus()
                        {
                            self.vrf_cli = preview::find_cli(&self.config.vrf_cli_path);
                            save = true;
                        }
                        ui.add_space(2.0);
                        let (text, color) = match &self.vrf_cli {
                            Some(_) => (crate::tr!("Найден, кнопка 3D на карточках включена", "Found, the 3D button on cards is on"), theme::EQUIPPED),
                            None => (crate::tr!("Не найден, 3D-превью недоступно", "Not found, 3D preview unavailable"), theme::TEXT_FAINT),
                        };
                        status_line(ui, text, color);
                    },
                );
                ui.add_space(12.0);

                theme::section(ui, crate::tr!("Интерфейс", "Interface"), "", |ui| {
                    field_label(ui, crate::tr!("Язык", "Language"));
                    // auto (as in Windows) / Russian / English
                    let lang_label = |code: &str| match code {
                        "ru" => "Русский",
                        "en" => "English",
                        _ => crate::tr!("Авто (как в Windows)", "Auto (as Windows)"),
                    };
                    let mut lang = self.config.language.clone();
                    theme::combo(ui, "ui_language", lang_label(&lang), 280.0, |ui| {
                        for code in ["", "ru", "en"] {
                            ui.selectable_value(&mut lang, code.to_string(), lang_label(code));
                        }
                    });
                    if lang != self.config.language {
                        self.config.language = lang;
                        crate::i18n::set(&self.config.language);
                        save = true;
                        ui.ctx().request_repaint();
                    }
                });
                ui.add_space(12.0);

                theme::section(
                    ui,
                    crate::tr!("Сброс", "Reset"),
                    crate::tr!("Вернуть файлы игры в исходный вид. Пресеты и настройки лаунчера останутся.", "Put the game files back as Steam shipped them. Presets and launcher settings stay."),
                    |ui| {
                        if theme::button(ui, Some(Icon::Revert), crate::tr!("Откатить все изменения", "Revert everything"), egui::Vec2::ZERO, Kind::Danger).clicked() {
                            revert = true;
                        }
                    },
                );
                ui.add_space(16.0);
            });
        });

        if remount {
            // a new overrides folder is mounted in place of the old one
            self.save_all();
        } else if save {
            let _ = config::save_config(&self.config);
        }
        if reload {
            self.reload_catalog(ctx);
        }
        if revert {
            self.confirm_revert = true;
        }
    }

    // ------------------------------------------------------------ status bar

    fn status_bar(&mut self, ctx: &egui::Context) {
        let color = self.status.0.color();
        let game = if self.running() {
            let gs = self.game_state();
            let c = match gs {
                gsi::GameState::InMatch(_) => theme::ACCENT_HI,
                gsi::GameState::Unknown => theme::TEXT_FAINT,
                _ => theme::EQUIPPED,
            };
            (format!("Dota 2: {}", gs.label()), c)
        } else {
            (crate::tr!("Dota 2 не запущена", "Dota 2 is not running").to_string(), theme::TEXT_FAINT)
        };
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::none()
                    .fill(theme::BG_DEEP)
                    .inner_margin(egui::Margin { left: 10.0, right: 20.0, top: 7.0, bottom: 8.0 }),
            )
            .show_separator_line(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (dot, _) = ui.allocate_exact_size(egui::vec2(10.0, 14.0), Sense::hover());
                    theme::dot(ui.painter(), dot.center(), color);
                    ui.add_space(2.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(&game.0).size(12.0).color(game.1));
                        let (d, _) = ui.allocate_exact_size(egui::vec2(8.0, 14.0), Sense::hover());
                        ui.painter().circle_filled(d.center(), 3.0, game.1);
                        ui.add_space(12.0);
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.status.1)
                                        .size(12.0)
                                        .color(theme::TEXT_DIM),
                                )
                                .truncate(),
                            );
                        });
                    });
                });
            });
    }

    // ------------------------------------------------------------ central grid

    fn central(&mut self, ctx: &egui::Context) {
        let selected = self.selected_ids();
        let query = self.search.trim().to_lowercase();
        let rarity = self.rarity_filter.clone();
        let only_selected = self.only_selected;

        let mut events: Vec<(String, CardAction)> = Vec::new();
        let mut want_reload = false;
        let mut want_open = false;
        let mut new_rarity: Option<Option<String>> = None;
        let mut toggle_only_selected = false;
        let mut toggle_only_fav = false;
        let mut new_sort: Option<SortMode> = None;
        let has_cli = self.vrf_cli.is_some();
        let cat = &self.catalog;
        let favorites = &self.config.favorites;
        let only_fav = self.only_favorites;
        let sort = self.sort;
        let active_styles = self.profiles[self.active_index()].styles.clone();

        let rarities = present_rarities(&self.catalog);
        let total = self.catalog.skins.len();
        let empty = total == 0;
        let error_text = self
            .catalog_error
            .clone()
            .unwrap_or_else(|| crate::tr!("catalog.json пуст", "catalog.json is empty").to_string());

        let hero = &self.hero_filter;
        let slot = &self.slot_filter;
        let mut visible: Vec<&Skin> = self
            .catalog
            .skins
            .iter()
            .filter(|skin| {
                (!only_selected || selected.contains(&skin.id))
                    && (!only_fav || favorites.contains(&skin.id))
                    && rarity.as_ref().map_or(true, |r| skin.rarity == *r)
                    && (slot.is_empty() || skin.slot == *slot)
                    && (hero.is_empty()
                        || (hero == NO_HERO && skin.is_general())
                        || skin.fits_hero(hero))
                    && (query.is_empty()
                        || query.split_whitespace().all(|w| skin.search.contains(w)))
            })
            .collect();
        sort_skins(&mut visible, sort);

        let assets = &self.assets;
        let icons = &mut self.icons;
        icons.begin_frame(ctx);

        egui::CentralPanel::default()
            .frame(page_frame())
            .show(ctx, |ui| {
                if empty {
                    empty_state(ui, &error_text, assets, &mut want_reload, &mut want_open);
                    return;
                }

                // ---- filter row
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if theme::chip(ui, crate::tr!("Все", "All"), rarity.is_none(), None).clicked() {
                        new_rarity = Some(None);
                    }
                    for r in &rarities {
                        let active = rarity.as_deref() == Some(r.as_str());
                        let label = catalog::rarity_label(r);
                        if theme::chip(ui, &label, active, Some(catalog::rarity_color(r))).clicked() {
                            new_rarity = Some(if active { None } else { Some(r.clone()) });
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if theme::toggle_chip(ui, Icon::Check, crate::tr!("Выбранные", "Selected"), selected.len(), only_selected, theme::EQUIPPED).clicked() {
                        toggle_only_selected = true;
                    }
                    if theme::toggle_chip(ui, Icon::StarFill, crate::tr!("Избранное", "Favorites"), favorites.len(), only_fav, theme::FAVORITE).clicked() {
                        toggle_only_fav = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(8.0);
                        if let Some(s) = sort_combo(ui, "catalog_sort", sort) {
                            new_sort = Some(s);
                        }
                        ui.label(
                            egui::RichText::new(crate::trf!("{} из {}", "{} of {}", visible.len(), total))
                                .size(12.5)
                                .color(theme::TEXT_FAINT),
                        );
                    });
                });
                ui.add_space(12.0);

                if visible.is_empty() {
                    nothing_found(ui, crate::tr!("Попробуйте другого героя, слот, редкость или запрос", "Try another hero, slot, rarity or query"));
                    return;
                }

                // ---- responsive, virtualized grid
                let available = ui.available_width() - 8.0;
                let cols = (((available + CARD_GAP) / (CARD_MIN_W + CARD_GAP)).floor() as usize)
                    .max(1);
                let card_w = (available - CARD_GAP * (cols as f32 - 1.0)) / cols as f32;
                let rows = visible.len().div_ceil(cols);
                let row_h = CARD_H + CARD_GAP;

                egui::ScrollArea::vertical().id_salt("catalog_page")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_h, rows, |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for row_index in range {
                            let start = row_index * cols;
                            let end = (start + cols).min(visible.len());
                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = CARD_GAP;
                                for skin in &visible[start..end] {
                                    let is_selected = selected.contains(&skin.id);
                                    let tex = icons.get(
                                        &skin.id,
                                        skin.icon.as_deref(),
                                        skin.preview.as_deref().map(|p| assets.join(p)),
                                    );
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

        self.handle_card_events(events);
        if toggle_only_fav {
            self.only_favorites = !self.only_favorites;
        }
        if let Some(s) = new_sort {
            self.sort = s;
        }
        if let Some(r) = new_rarity {
            self.rarity_filter = r;
        }
        if toggle_only_selected {
            self.only_selected = !self.only_selected;
        }
        if want_open {
            self.open_assets_folder();
        }
        if want_reload {
            self.reload_catalog(ctx);
        }
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.poll_job();
        self.first_start_setup(ctx);
        self.poll_catalog_job(ctx);
        self.poll_tools_job();
        if let Some(req) = self.pending_preview.take() {
            self.open_preview(ctx, frame.gl(), &req);
        }
        // the sidebar runs the window's full height; the content panel floats in
        // what is left, above the status line
        self.side_panel(ctx);
        self.status_bar(ctx);
        let free = ctx.available_rect();
        let panel = Rect::from_min_max(
            Pos2::new(free.min.x, free.min.y + PANEL_GAP),
            Pos2::new(free.max.x - PANEL_GAP, free.max.y),
        );
        let painter = ctx.layer_painter(egui::LayerId::background());
        painter.rect_filled(panel, Rounding::same(theme::R_LG), theme::BG);
        let bar_bottom = self.top_bar(ctx);
        match self.view {
            View::Catalog => self.central(ctx),
            View::Heroes => self.heroes_central(ctx),
            View::Settings => self.settings_central(ctx),
        }
        painter.hline(panel.min.x + 1.0..=panel.max.x - 1.0, bar_bottom, Stroke::new(1.0_f32, theme::BORDER));
        painter.rect_stroke(panel, Rounding::same(theme::R_LG), Stroke::new(1.0_f32, theme::BORDER));
        self.confirm_dialog(ctx);
        self.revert_dialog(ctx);
        self.files_dialog(ctx);
        if self.catalog_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        self.sync_preview(ctx, frame.gl());
        if let Some(p) = &mut self.preview {
            p.show(ctx, frame.gl());
            if !p.open {
                p.destroy(frame.gl());
                self.preview = None;
            }
        }

        // Remember the hero for the next start.
        if self.config.ui_hero != self.hero_filter {
            self.config.ui_hero = self.hero_filter.clone();
            let _ = config::save_config(&self.config);
        }

        // In-game picks change presets through profiles.json.
        if self.profiles_dirty {
            self.profiles_dirty = false;
            if let Err(e) = config::save_profiles(&self.profiles) {
                self.set_status(StatusKind::Error, e);
            }
            self.profiles_seen = config::profiles_modified();
        } else if config::profiles_modified() != self.profiles_seen {
            self.profiles_seen = config::profiles_modified();
            let fresh = config::load_profiles();
            if !fresh.is_empty() && fresh != self.profiles {
                self.profiles = fresh;
                self.set_status(StatusKind::Ok, crate::tr!("Пресет изменён из игры.", "Preset changed from the game."));
            }
        }
        if self.job.is_some() || self.running() {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = config::save_profiles(&self.profiles);
        let _ = config::save_config(&self.config);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::BG_DEEP.to_normalized_gamma_f32()
    }
}

// ---------------------------------------------------------------- pieces

/// Filter dropdown; empty `value` means "all".
fn combo(
    ui: &mut egui::Ui,
    id: &str,
    all_label: &str,
    width: f32,
    value: &mut String,
    options: &[String],
    label: impl Fn(&str) -> String,
) {
    let shown = if value.is_empty() {
        all_label.to_string()
    } else {
        label(value)
    };
    theme::combo(ui, id, shown, width, |ui| {
        ui.selectable_value(value, String::new(), all_label);
        for opt in options {
            ui.selectable_value(value, opt.clone(), label(opt));
        }
    });
}

/// Frame of a page on the content panel: inside the panel, with its padding.
fn page_frame() -> egui::Frame {
    egui::Frame::none()
        .outer_margin(egui::Margin { left: 0.0, right: PANEL_GAP, top: 0.0, bottom: 0.0 })
        .inner_margin(egui::Margin { left: 20.0, right: 12.0, top: 16.0, bottom: 0.0 })
}

/// A coloured dot and a line of text after it.
fn status_line(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    ui.horizontal(|ui| {
        let (d, _) = ui.allocate_exact_size(egui::vec2(12.0, 16.0), Sense::hover());
        ui.painter().circle_filled(d.center(), 3.5, color);
        ui.label(egui::RichText::new(text).size(12.5).color(theme::TEXT_DIM));
    })
    .response
}

fn dialog_title(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).font(bold_font(17.0)).color(theme::TEXT));
    ui.add_space(4.0);
}

fn dialog_text(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.label(egui::RichText::new(text).size(13.0).color(color));
}

/// A dialog's row of buttons, right-aligned (the first one added is rightmost).
fn dialog_buttons(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let size = egui::vec2(ui.available_width(), theme::CONTROL_H);
    ui.allocate_ui_with_layout(size, egui::Layout::right_to_left(egui::Align::Center), add);
}

/// The empty result of a search or filter.
fn nothing_found(ui: &mut egui::Ui, hint: &str) {
    ui.add_space(70.0);
    ui.vertical_centered(|ui| {
        let (r, _) = ui.allocate_exact_size(egui::vec2(48.0, 48.0), Sense::hover());
        ui.painter().rect_filled(r, Rounding::same(theme::R_LG), theme::SURFACE);
        theme::paint_icon(ui.painter(), r.center(), Icon::Search, 20.0, theme::TEXT_FAINT);
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(crate::tr!("Ничего не найдено", "Nothing found"))
                .font(bold_font(15.0))
                .color(theme::TEXT),
        );
        ui.label(egui::RichText::new(hint).size(12.5).color(theme::TEXT_FAINT));
    });
}

fn brand(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        brand_mark(ui, 32.0);
        ui.add_space(2.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            // manual letter spacing: extra_letter_spacing with Segoe UI splits the pair «DO»
            spaced_label(ui, "DOTA SKINS", bold_font(14.5), theme::TEXT, 1.4);
            ui.label(
                egui::RichText::new(crate::tr!("лаунчер косметики", "cosmetics launcher"))
                    .size(11.5)
                    .color(theme::TEXT_FAINT),
            );
        });
    });
}

/// The launcher icon, `size` points square.
fn brand_mark(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    // the launcher icon (res/icon.png), scaled once to the screen's pixel size
    let ppp = ui.ctx().pixels_per_point();
    let id = egui::Id::new("brand_icon");
    let tex = ui.ctx().data(|d| d.get_temp::<egui::TextureHandle>(id)).unwrap_or_else(|| {
        let px = (size * ppp).round().max(1.0) as u32;
        let img = image::load_from_memory(include_bytes!("../res/icon.png"))
            .map(|i| i.resize_exact(px, px, image::imageops::FilterType::Lanczos3).into_rgba8())
            .unwrap_or_default();
        let size = [img.width() as usize, img.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
        let tex = ui.ctx().load_texture("brand_icon", color, egui::TextureOptions::LINEAR);
        ui.ctx().data_mut(|d| d.insert_temp(id, tex.clone()));
        tex
    });
    egui::Image::new(egui::load::SizedTexture::new(tex.id(), rect.size()))
        .rounding(Rounding::same(size * 0.25))
        .paint_at(ui, rect);
}

/// Text with even letter spacing: each glyph laid out on its own and placed by
/// its own width, so no kerning pair can open a gap.
/// Bold face read directly for exact outline metrics (egui only exposes pixel-rounded,
/// padded glyph boxes, which makes optical spacing uneven by up to a pixel).
fn bold_outlines() -> Option<&'static ab_glyph::FontArc> {
    static FONT: std::sync::OnceLock<Option<ab_glyph::FontArc>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        // the same face the "bold" family uses
        theme::SEMIBOLD_FILES.iter().find_map(|name| {
            let bytes = std::fs::read(format!(r"C:\Windows\Fonts\{name}")).ok()?;
            ab_glyph::FontArc::try_from_vec(bytes).ok()
        })
    })
    .as_ref()
}

fn spaced_label(ui: &mut egui::Ui, text: &str, font: egui::FontId, color: Color32, spacing: f32) {
    use ab_glyph::Font as _;
    // Optical spacing: an even gap between the exact glyph outlines (from the font file)
    // plus the pair's kerning; the font's advance is not used: with it Segoe UI Bold's «D» drifts apart.
    struct G {
        galley: std::sync::Arc<egui::Galley>,
        id: Option<ab_glyph::GlyphId>,
        ink: Option<(f32, f32)>, // left/right edge of the outline from the glyph's origin, pt
        advance: f32,
    }
    let outlines = bold_outlines();
    let glyphs: Vec<G> = text
        .chars()
        .map(|c| {
            let galley = ui.fonts(|f| f.layout_no_wrap(c.to_string(), font.clone(), color));
            let first = galley.rows.first().and_then(|r| r.glyphs.first()).cloned();
            let advance = first.as_ref().map_or(galley.size().x, |g| g.advance_width);
            let id = outlines.map(|f| f.glyph_id(c));
            let exact = outlines.zip(id).and_then(|(f, id)| {
                let units = f.h_advance_unscaled(id);
                let b = f.outline(id)?.bounds;
                (units > 0.0).then(|| {
                    let k = advance / units;
                    (b.min.x * k, b.max.x * k)
                })
            });
            let ink = exact.or_else(|| {
                first
                    .filter(|g| g.uv_rect.size.x > 0.0)
                    .map(|g| (g.pos.x + g.uv_rect.offset.x, g.pos.x + g.uv_rect.offset.x + g.uv_rect.size.x))
            });
            G { galley, id, ink, advance }
        })
        .collect();
    // kerning of a pair in pt (negative for «TA», «AV»…)
    let kern = |a: &G, b: &G| -> f32 {
        match (outlines, a.id, b.id) {
            (Some(f), Some(x), Some(y)) if a.ink.is_some() && b.ink.is_some() => {
                let units = f.h_advance_unscaled(x);
                if units > 0.0 { f.kern_unscaled(x, y) * a.advance / units } else { 0.0 }
            }
            _ => 0.0,
        }
    };
    // egui lays out a lone space almost zero wide: set it ourselves
    let space_w = font.size * 0.35;
    let ppp = ui.ctx().pixels_per_point();
    // layout: x is the left edge of the next letter's outline
    let mut placed = Vec::with_capacity(glyphs.len());
    let mut x = 0.0f32;
    for (i, g) in glyphs.iter().enumerate() {
        if i > 0 {
            x += spacing + kern(&glyphs[i - 1], g);
        }
        let (left, w) = match g.ink {
            Some((l, r)) => (l, r - l),
            None => (0.0, g.advance.max(space_w)),
        };
        placed.push(x - left);
        x += w;
    }
    let height = glyphs.iter().map(|g| g.galley.size().y).fold(0.0, f32::max);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(x.max(0.0), height), Sense::hover());
    for (g, off) in glyphs.into_iter().zip(placed) {
        let px = ((rect.min.x + off) * ppp).round() / ppp;
        ui.painter().galley(egui::pos2(px, rect.min.y), g.galley, color);
    }
}

pub(crate) fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(egui::RichText::new(text).size(12.0).color(theme::TEXT_DIM));
}

/// A preset in the sidebar: a dot (lit when active), its name and item count.
pub(crate) fn profile_row(
    ui: &mut egui::Ui,
    w: f32,
    name: &str,
    count: usize,
    active: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
        let act = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("a"), active, 0.15);
        let p = ui.painter();
        let fill = theme::lerp_color(theme::SURFACE.gamma_multiply(hover), theme::SURFACE_HI, act);
        p.rect_filled(rect, Rounding::same(theme::R_MD), fill);

        let dot = Pos2::new(rect.min.x + 20.0, rect.center().y);
        if act > 0.01 {
            p.circle_filled(dot, 6.5, theme::ACCENT.gamma_multiply(0.18 * act));
        }
        p.circle_filled(dot, 3.5, theme::lerp_color(theme::BORDER_HI, theme::ACCENT_HI, act));

        let count_txt = count.to_string();
        let count_color = theme::lerp_color(theme::TEXT_FAINT, theme::TEXT_DIM, act);
        let cg = theme::galley(ui, &count_txt, FontId::proportional(12.0), count_color, f32::INFINITY);
        let count_x = rect.max.x - 12.0 - cg.size().x;
        p.galley(Pos2::new(count_x, rect.center().y - cg.size().y / 2.0), cg, count_color);

        let text_color = theme::lerp_color(theme::TEXT_DIM, theme::TEXT, act.max(hover));
        let font = if active { bold_font(13.5) } else { FontId::proportional(13.5) };
        let ng = theme::galley(ui, name, font, text_color, count_x - rect.min.x - 46.0);
        p.galley(
            Pos2::new(rect.min.x + 36.0, rect.center().y - ng.size().y / 2.0),
            ng,
            text_color,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn empty_state(
    ui: &mut egui::Ui,
    error_text: &str,
    assets: &Path,
    want_reload: &mut bool,
    want_open: &mut bool,
) {
    let searched: Vec<String> = config::assets_candidates()
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    ui.add_space(50.0);
    ui.vertical_centered(|ui| {
        egui::Frame::none()
            .fill(theme::SURFACE)
            .stroke(Stroke::new(1.0_f32, theme::BORDER))
            .rounding(Rounding::same(theme::R_LG + 2.0))
            .inner_margin(egui::Margin::same(28.0))
            .show(ui, |ui| {
                ui.set_max_width(520.0);
                ui.vertical_centered(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(52.0, 52.0), Sense::hover());
                    ui.painter().rect_filled(r, Rounding::same(theme::R_LG), theme::DANGER.gamma_multiply(0.12));
                    theme::paint_icon(ui.painter(), r.center(), Icon::Warning, 22.0, theme::DANGER);
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(crate::tr!("Каталог не загружен", "Catalog not loaded"))
                            .font(bold_font(20.0))
                            .color(theme::TEXT),
                    );
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new(error_text).color(theme::DANGER));
                    ui.add_space(14.0);
                    theme::caption(ui, crate::tr!("Где искали catalog.json", "Where catalog.json was searched"));
                    for path in &searched {
                        ui.label(
                            egui::RichText::new(path)
                                .monospace()
                                .size(11.5)
                                .color(theme::TEXT_DIM),
                        );
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(crate::trf!("Текущая папка assets: {}", "Current assets folder: {}", assets.display()))
                            .size(11.5)
                            .color(theme::TEXT_FAINT),
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        let w = 180.0;
                        let pad = (ui.available_width() - w * 2.0 - 8.0).max(0.0) / 2.0;
                        ui.add_space(pad);
                        if theme::button(ui, Some(Icon::Reload), crate::tr!("Перезагрузить", "Reload"), egui::vec2(w, 0.0), Kind::Secondary).clicked() {
                            *want_reload = true;
                        }
                        if theme::button(ui, Some(Icon::Folder), crate::tr!("Открыть папку assets", "Open the assets folder"), egui::vec2(w, 0.0), Kind::Primary).clicked() {
                            *want_open = true;
                        }
                    });
                });
            });
    });
}

#[derive(Clone, Copy, PartialEq)]
enum CardAction {
    None,
    Toggle,
    Preview,
    Favorite,
    Files,
    /// Next style of the item (`Profile::styles`).
    Style(u32),
}

/// Catalog sort orders.
#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    Default,
    Rarity,
    Name,
    Newest,
}

impl SortMode {
    const ALL: [SortMode; 4] = [SortMode::Default, SortMode::Rarity, SortMode::Newest, SortMode::Name];

    fn label(self) -> &'static str {
        match self {
            SortMode::Default => crate::tr!("По герою и слоту", "By hero and slot"),
            SortMode::Rarity => crate::tr!("Сначала редкие", "Rarest first"),
            SortMode::Name => crate::tr!("По названию", "By name"),
            SortMode::Newest => crate::tr!("Сначала новые", "Newest first"),
        }
    }
}

fn rarity_rank(r: &str) -> usize {
    RARITY_ORDER.iter().position(|x| *x == r).unwrap_or(0)
}

fn sort_skins(list: &mut [&Skin], mode: SortMode) {
    match mode {
        SortMode::Default => {}
        SortMode::Rarity => list.sort_by_key(|s| std::cmp::Reverse(rarity_rank(&s.rarity))),
        SortMode::Name => list.sort_by_cached_key(|s| s.name.to_lowercase()),
        SortMode::Newest => list.sort_by_key(|s| std::cmp::Reverse(s.index.unwrap_or(0))),
    }
}

fn sort_combo(ui: &mut egui::Ui, id: &str, current: SortMode) -> Option<SortMode> {
    let mut picked = current;
    theme::combo(ui, id, current.label(), 160.0, |ui| {
        for m in SortMode::ALL {
            ui.selectable_value(&mut picked, m, m.label());
        }
    });
    (picked != current).then_some(picked)
}

/// One item card. Clicking the card toggles it; the "3D" badge opens the preview,
/// the star toggles favourite, right click opens a menu.
fn draw_card(
    ui: &mut egui::Ui,
    skin: &Skin,
    is_selected: bool,
    is_fav: bool,
    tex: Option<&egui::TextureHandle>,
    w: f32,
    can_3d: bool,
    styles: &[String],
    style: u32,
) -> CardAction {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, CARD_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return CardAction::None;
    }
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

    let ctx = ui.ctx().clone();
    let hover = ctx.animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.12);
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), is_selected, 0.15);
    let accent = catalog::rarity_color(&skin.rarity);
    let round = theme::R_LG;
    let p = ui.painter();

    p.rect_filled(rect, Rounding::same(round), theme::lerp_color(theme::SURFACE, theme::SURFACE_HI, hover));

    // picture, inset into the card (cover-cropped, a little closer on hover)
    let pic = Rect::from_min_size(
        rect.min + egui::vec2(CARD_PAD, CARD_PAD),
        egui::vec2(w - CARD_PAD * 2.0, PREVIEW_H),
    );
    let pr = round - CARD_PAD + 2.0;
    p.rect_filled(pic, Rounding::same(pr), theme::BG);
    if let Some(tex) = tex {
        let [tw, th] = tex.size();
        let uv = zoom_uv(cover_uv(tw as f32 / th.max(1) as f32, pic.aspect_ratio()), 1.0 + 0.04 * hover);
        let tint = theme::lerp_color(Color32::from_gray(220), Color32::WHITE, hover);
        egui::Image::new(egui::load::SizedTexture::new(tex.id(), pic.size()))
            .uv(uv)
            .rounding(Rounding::same(pr))
            .tint(tint)
            .paint_at(ui, pic);
    } else {
        let label = skin
            .hero_names()
            .first()
            .map(|h| h.to_string())
            .unwrap_or_else(|| catalog::slot_label(&skin.slot));
        let initials: String = label
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(2)
            .collect();
        p.text(
            pic.center(),
            egui::Align2::CENTER_CENTER,
            if initials.is_empty() { "?".to_string() } else { initials },
            bold_font(30.0),
            accent.gamma_multiply(0.4),
        );
    }
    // the rarity's colour rising from the picture's bottom edge
    let glow = Rect::from_min_max(Pos2::new(pic.min.x, pic.max.y - 46.0), pic.max);
    theme::fade_up(
        ui,
        glow,
        Rounding { nw: 0.0, ne: 0.0, sw: pr, se: pr },
        accent.gamma_multiply(0.26 + 0.14 * hover),
    );

    // rarity pill
    let tag_txt = catalog::rarity_label(&skin.rarity);
    if !tag_txt.is_empty() {
        let g = theme::galley(ui, &tag_txt, bold_font(10.5), theme::TEXT, w * 0.6);
        let tag = Rect::from_min_size(pic.min + egui::vec2(8.0, 8.0), egui::vec2(g.size().x + 25.0, 20.0));
        p.rect_filled(tag, Rounding::same(10.0), Color32::from_black_alpha(150));
        p.circle_filled(Pos2::new(tag.min.x + 10.5, tag.center().y), 3.0, accent);
        p.galley(Pos2::new(tag.min.x + 18.0, tag.center().y - g.size().y / 2.0), g, theme::TEXT);
    }

    // selected check
    if sel > 0.01 {
        let c = Pos2::new(pic.max.x - 17.0, pic.min.y + 17.0);
        p.circle_filled(c, 11.0 * sel, theme::EQUIPPED);
        theme::paint_icon(p, c, Icon::Check, 11.0 * sel, Color32::from_rgb(0x04, 0x1c, 0x10));
    }

    // texts
    let text_x = rect.min.x + 12.0;
    let text_w = w - 24.0;
    let name_y = pic.max.y + 9.0;
    let files_w = if skin.files.is_empty() { 0.0 } else { 14.0 };
    let name = theme::galley(ui, &skin.name, bold_font(13.5), theme::TEXT, text_w - files_w);
    let name_h = name.size().y;
    p.galley(Pos2::new(text_x, name_y), name, theme::TEXT);
    if files_w > 0.0 {
        // has files to apply (catalog or attached by the user)
        p.circle_filled(Pos2::new(rect.max.x - 15.0, name_y + name_h / 2.0), 3.0, theme::EQUIPPED.gamma_multiply(0.85));
    }

    let meta_y = name_y + name_h + 1.0;
    let show_hint = is_selected || hover > 0.05;
    let (hint, hint_color) = if is_selected {
        (crate::tr!("Выбрано", "Selected"), theme::EQUIPPED)
    } else {
        (crate::tr!("Выбрать", "Select"), theme::TEXT_DIM.gamma_multiply(hover))
    };
    let hg = theme::galley(ui, hint, bold_font(11.5), hint_color, 90.0);
    let hint_w = if show_hint { hg.size().x + 10.0 } else { 0.0 };
    let meta = format!(
        "{} · {}",
        if skin.hero.is_empty() || skin.hero == "—" { crate::tr!("Общее", "General") } else { &skin.hero },
        catalog::slot_label(&skin.slot)
    );
    let mg = theme::galley(ui, &meta, FontId::proportional(12.0), theme::TEXT_FAINT, text_w - hint_w);
    let meta_h = mg.size().y;
    p.galley(Pos2::new(text_x, meta_y), mg, theme::TEXT_FAINT);
    if show_hint {
        p.galley(
            Pos2::new(rect.max.x - 12.0 - hg.size().x, meta_y + (meta_h - hg.size().y) / 2.0),
            hg,
            hint_color,
        );
    }

    // outline
    let outline = theme::lerp_color(
        theme::lerp_color(theme::BORDER, theme::BORDER_HI, hover),
        theme::EQUIPPED.gamma_multiply(0.9),
        sel,
    );
    p.rect_stroke(rect, Rounding::same(round), Stroke::new(1.0 + 0.5 * sel, outline));

    // "3D" in the picture's bottom-right corner
    let badge = Rect::from_min_size(Pos2::new(pic.max.x - 42.0, pic.max.y - 30.0), egui::vec2(34.0, 22.0));
    let pointer = ui.ctx().pointer_hover_pos();
    let on_badge = can_3d && pointer.is_some_and(|p| badge.contains(p));
    if can_3d && hover > 0.05 {
        let p = ui.painter();
        let fill = if on_badge { theme::ACCENT } else { Color32::from_black_alpha(165) };
        p.rect_filled(badge, Rounding::same(theme::R_SM), fill.gamma_multiply(hover));
        p.text(
            badge.center(),
            egui::Align2::CENTER_CENTER,
            "3D",
            bold_font(11.5),
            Color32::WHITE.gamma_multiply(hover),
        );
    }

    // favourite star in the picture's bottom-left corner
    let star = Rect::from_min_size(Pos2::new(pic.min.x + 8.0, pic.max.y - 30.0), egui::vec2(26.0, 22.0));
    let on_star = pointer.is_some_and(|p| star.contains(p));
    if is_fav || hover > 0.05 {
        let p = ui.painter();
        let a = if is_fav { 1.0 } else { hover };
        p.rect_filled(star, Rounding::same(theme::R_SM), Color32::from_black_alpha((165.0 * a) as u8));
        let (icon, color) = if is_fav {
            (Icon::StarFill, theme::FAVORITE)
        } else if on_star {
            (Icon::StarFill, theme::FAVORITE.gamma_multiply(0.85))
        } else {
            (Icon::Star, Color32::from_gray(200).gamma_multiply(a))
        };
        theme::paint_icon(p, star.center(), icon, 12.0, color);
    }

    // style switcher (items with styles): a pill under the rarity pill, a click
    // takes the next style
    let mut on_style = false;
    if styles.len() > 1 {
        let cur = (style as usize).min(styles.len() - 1);
        let txt = format!("{}/{}  {}", cur + 1, styles.len(), styles[cur]);
        let g = theme::galley(ui, &txt, bold_font(10.5), theme::TEXT, w - 50.0);
        let pill = Rect::from_min_size(pic.min + egui::vec2(8.0, 32.0), egui::vec2(g.size().x + 30.0, 20.0));
        on_style = pointer.is_some_and(|p| pill.contains(p));
        let p = ui.painter();
        p.rect(
            pill,
            Rounding::same(10.0),
            Color32::from_black_alpha(if on_style { 215 } else { 150 }),
            Stroke::new(1.0_f32, if on_style { theme::ACCENT_HI } else { Color32::TRANSPARENT }),
        );
        theme::paint_icon(p, Pos2::new(pill.min.x + 12.0, pill.center().y), Icon::Palette, 10.5, theme::TEXT_DIM);
        p.galley(Pos2::new(pill.min.x + 22.0, pill.center().y - g.size().y / 2.0), g, theme::TEXT);
    }

    let mut action = CardAction::None;
    let resp = if on_style {
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(egui::RichText::new(crate::tr!("Стиль предмета — нажмите, чтобы сменить", "Item style — click to change")).strong());
            for (i, n) in styles.iter().enumerate() {
                let mark = if i as u32 == style { "▶ " } else { "    " };
                ui.label(format!("{mark}{}. {n}", i + 1));
            }
        })
    } else if on_badge {
        resp.on_hover_text(crate::tr!("Открыть 3D-превью", "Open 3D preview"))
    } else if on_star {
        resp.on_hover_text(if is_fav { crate::tr!("Убрать из избранного", "Remove from favorites") } else { crate::tr!("В избранное", "Add to favorites") })
    } else {
        resp.on_hover_ui_at_pointer(|ui| card_tooltip(ui, skin, accent))
    };
    resp.context_menu(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.spacing_mut().button_padding = egui::vec2(10.0, 5.0);
        ui.spacing_mut().interact_size.y = 28.0;
        let fav = if is_fav { crate::tr!("Убрать из избранного", "Remove from favorites") } else { crate::tr!("В избранное", "Add to favorites") };
        if ui.button(fav).clicked() {
            action = CardAction::Favorite;
            ui.close_menu();
        }
        let files = if skin.files.is_empty() {
            crate::tr!("Свои файлы…", "Own files…").to_string()
        } else {
            crate::trf!("Свои файлы ({})…", "Own files ({})…", skin.files.len())
        };
        if ui.button(files).clicked() {
            action = CardAction::Files;
            ui.close_menu();
        }
        if can_3d && ui.button(crate::tr!("3D-превью", "3D preview")).clicked() {
            action = CardAction::Preview;
            ui.close_menu();
        }
        if styles.len() > 1 {
            ui.separator();
            for (i, n) in styles.iter().enumerate() {
                let label = format!("{}  {n}", if i as u32 == style { "◆" } else { "◇" });
                if ui.button(label).clicked() {
                    action = CardAction::Style(i as u32);
                    ui.close_menu();
                }
            }
        }
    });
    if action != CardAction::None {
        return action;
    }
    if !resp.clicked() {
        CardAction::None
    } else if on_style {
        CardAction::Style((style + 1) % styles.len() as u32)
    } else if on_badge {
        CardAction::Preview
    } else if on_star {
        CardAction::Favorite
    } else {
        CardAction::Toggle
    }
}

fn card_tooltip(ui: &mut egui::Ui, skin: &Skin, accent: Color32) {
    ui.set_max_width(300.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.label(
        egui::RichText::new(&skin.name)
            .font(bold_font(14.0))
            .color(theme::TEXT),
    );
    ui.label(
        egui::RichText::new(catalog::rarity_label(&skin.rarity))
            .font(bold_font(11.5))
            .color(accent),
    );
    ui.add_space(2.0);
    let heroes = skin.hero_names();
    let heroes = if heroes.is_empty() {
        crate::tr!("общий предмет, не для героя", "general item, not for a hero").to_string()
    } else {
        heroes.join(", ")
    };
    let row = |ui: &mut egui::Ui, k: &str, v: &str| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(k).size(12.0).color(theme::TEXT_FAINT));
            ui.label(egui::RichText::new(v).size(12.0).color(theme::TEXT_DIM));
        });
    };
    row(ui, crate::tr!("Герой:", "Hero:"), &heroes);
    row(ui, crate::tr!("Слот:", "Slot:"), &catalog::slot_label(&skin.slot));
    let files = if skin.files.is_empty() {
        crate::tr!("нет (нечего применять)", "none (nothing to apply)").to_string()
    } else {
        skin.files.len().to_string()
    };
    row(ui, crate::tr!("Файлов:", "Files:"), &files);
    row(ui, "ID:", &skin.id);
}

/// UV rect that crops an image of `img_aspect` to fill a box of `box_aspect`.
fn cover_uv(img_aspect: f32, box_aspect: f32) -> Rect {
    if img_aspect > box_aspect {
        let u = box_aspect / img_aspect;
        let off = (1.0 - u) / 2.0;
        Rect::from_min_max(Pos2::new(off, 0.0), Pos2::new(off + u, 1.0))
    } else {
        let v = img_aspect / box_aspect;
        let off = (1.0 - v) * 0.4;
        Rect::from_min_max(Pos2::new(0.0, off), Pos2::new(1.0, off + v))
    }
}

/// The middle of a UV rect, `k` times closer.
fn zoom_uv(uv: Rect, k: f32) -> Rect {
    Rect::from_center_size(uv.center(), uv.size() / k.max(0.01))
}

/// Rarities that actually occur in the catalog, in canonical order.
fn present_rarities(catalog: &Catalog) -> Vec<String> {
    let found: BTreeSet<&str> = catalog
        .skins
        .iter()
        .map(|s| s.rarity.as_str())
        .filter(|r| !r.is_empty())
        .collect();
    let mut out: Vec<String> = RARITY_ORDER
        .iter()
        .filter(|r| found.contains(**r))
        .map(|r| r.to_string())
        .collect();
    for r in found {
        if !RARITY_ORDER.contains(&r) {
            out.push(r.to_string());
        }
    }
    out
}

/// Compares the installed Dota build with the one the catalog was refreshed for.
/// Returns the new build id when Dota has been updated since. The first run just
/// records the current build.
fn check_catalog_build(cfg: &mut Config) -> Option<String> {
    let build = config::dota_build_id(&cfg.dota_path)?;
    if cfg.catalog_build.is_empty() {
        cfg.catalog_build = build;
        let _ = config::save_config(cfg);
        return None;
    }
    (cfg.catalog_build != build).then_some(build)
}

/// Tries every candidate location; if nothing exists, seeds the demo catalog
/// into %APPDATA%\dota-skins\assets so the window is never blank.
fn load_catalog_with_fallback() -> (PathBuf, Catalog, Option<String>) {
    let assets = config::assets_dir();

    match Catalog::load(&assets) {
        Ok(catalog) if !catalog.skins.is_empty() => (assets, catalog, None),
        Ok(_) => (
            assets,
            Catalog::default(),
            Some(crate::tr!("catalog.json найден, но в нём нет предметов", "catalog.json found, but it has no items").to_string()),
        ),
        Err(err) => {
            let seed_dir = config::app_dir().join("assets");
            let seed_file = seed_dir.join("catalog.json");
            let seeded = std::fs::create_dir_all(&seed_dir).is_ok()
                && std::fs::write(&seed_file, DEMO_CATALOG).is_ok();

            if !seeded {
                return (assets, Catalog::default(), Some(err));
            }

            match Catalog::load(&seed_dir) {
                Ok(catalog) => (
                    seed_dir.clone(),
                    catalog,
                    Some(crate::trf!("{err} — демо-каталог записан в {}", "{err}, demo catalog written to {}",
                        seed_file.display()
                    )),
                ),
                Err(seed_err) => (assets, Catalog::default(), Some(seed_err)),
            }
        }
    }
}

