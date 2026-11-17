//! Dota 2 process control from the outside: is it running, close it politely,
//! start it again through Steam. No injection, no memory access.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::win;

const EXE: &str = "dota2.exe";
const APP_ID: &str = "570";

pub fn is_running() -> bool {
    !win::find_processes(EXE).is_empty()
}

/// Background thread that keeps a "Dota is running" flag fresh.
pub fn spawn_watcher(ctx: eframe::egui::Context) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(is_running()));
    let out = Arc::clone(&flag);
    std::thread::Builder::new()
        .name("dota-watcher".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(1500));
            let now = is_running();
            if flag.swap(now, Ordering::SeqCst) != now {
                ctx.request_repaint();
            }
        })
        .ok();
    out
}

/// Sends WM_CLOSE to Dota's windows (like pressing ✕) and waits for it to exit.
/// Never force-kills: if Dota refuses, the caller gets an error instead.
pub fn close_gracefully(timeout: Duration) -> Result<(), String> {
    let pids = win::find_processes(EXE);
    if pids.is_empty() {
        return Ok(());
    }
    let asked: usize = pids.iter().map(|&pid| win::request_close(pid)).sum();
    if asked == 0 {
        return Err(crate::tr!("не найдено окно Dota 2, закройте игру вручную", "Dota 2 window not found, close the game by hand").into());
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !is_running() {
            // let Steam notice the exit before we start the game again
            std::thread::sleep(Duration::from_secs(2));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(crate::tr!("Dota 2 не закрылась сама, закройте игру вручную и повторите", "Dota 2 did not close by itself, close the game by hand and try again").into())
}

/// steam.exe from the registry, falling back to the usual install path.
pub fn steam_exe() -> Option<PathBuf> {
    let from_registry = win::read_hkcu_string("Software\\Valve\\Steam", "SteamExe")
        .map(|s| PathBuf::from(s.replace('/', "\\")));
    from_registry
        .into_iter()
        .chain([PathBuf::from(r"C:\Program Files (x86)\Steam\steam.exe")])
        .find(|p| p.is_file())
}

/// Starts Dota through Steam (`steam.exe -applaunch 570 <args>`), so the
/// launch options set in Steam still apply; falls back to dota2.exe directly.
pub fn launch(dota_exe: &str, args: &str) -> Result<String, String> {
    let parts = split_args(args);
    if let Some(steam) = steam_exe() {
        Command::new(&steam)
            .arg("-applaunch")
            .arg(APP_ID)
            .args(&parts)
            .spawn()
            .map_err(|e| crate::trf!("не удалось запустить Steam: {e}", "could not start Steam: {e}"))?;
        return Ok(crate::trf!("Dota 2 запускается через Steam: {}", "Dota 2 is starting through Steam: {}", parts.join(" ")));
    }
    if !Path::new(dota_exe).is_file() {
        return Err(crate::trf!("Steam не найден, dota2.exe тоже: {dota_exe}", "Steam not found, dota2.exe neither: {dota_exe}"));
    }
    let child = Command::new(dota_exe)
        .args(&parts)
        .spawn()
        .map_err(|e| crate::trf!("не удалось запустить Dota 2: {e}", "could not start Dota 2: {e}"))?;
    Ok(crate::trf!("Dota 2 запущена напрямую (pid {}): {}", "Dota 2 started directly (pid {}): {}", child.id(), parts.join(" ")))
}

/// Splits a command line into arguments, honouring double quotes.
pub fn split_args(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in input.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
