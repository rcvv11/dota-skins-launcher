#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod apply;
mod attach;
mod autoexec;
mod card;
mod catalog;
mod config;
mod dota;
mod downloads;
mod emblem;
mod extract;
mod gamedata;
mod glow;
mod gsi;
#[macro_use]
mod i18n;
mod icons;
mod inject;
mod job;
mod mount;
mod picks;
mod preview;
mod schema;
mod schema_mount;
mod sets;
mod skins;
mod styles;
mod theme;
mod visuals;
mod vpk;
mod win;

use eframe::egui;

fn main() -> eframe::Result<()> {
    if std::env::args().any(|a| a == "--companion") {
        // старый автозапуск компаньона: его больше нет
        return Ok(());
    }

    // changer.dll внедряется в dota2.exe: лаунчер сразу работает от администратора
    if !win::is_elevated() {
        if win::relaunch_elevated() {
            return Ok(());
        }
        // запрос UAC отклонён: работаем без прав (внедрение может не сработать)
    }

    i18n::set(&config::load_config().language);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title(concat!("Dota Skins Launcher ", env!("CARGO_PKG_VERSION")))
            .with_icon(window_icon()),
        // the 3D preview needs a depth buffer
        depth_buffer: 24,
        ..Default::default()
    };

    eframe::run_native(
        "dota-skins-launcher",
        options,
        Box::new(|cc| Ok(Box::new(app::LauncherApp::new(cc)))),
    )
}

/// The launcher icon for the window title and taskbar (res/icon.png).
fn window_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../res/icon.png"))
        .map(|i| i.into_rgba8())
        .unwrap_or_default();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}
