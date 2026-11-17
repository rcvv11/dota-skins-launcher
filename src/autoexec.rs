//! The one line emblems need in Dota's `autoexec.cfg`.
//!
//! An emblem is drawn on the ring under your hero, and the game hides that ring
//! unless `r_draw_selected_ring` is 1. The effect itself is put on the hero by
//! `emblem.rs`; without the ring part of it would still stay hidden.
//!
//! Only our own two lines (a comment marker plus the command) are ever added or
//! removed; everything the player wrote stays untouched.

use std::path::Path;

/// The comment above our line; it is how the launcher recognises its own block.
/// Plain ASCII on purpose: game configs are read as bytes, not UTF-8.
pub const RING_MARK: &str = "// Dota Skins Launcher: emblem ring";
/// Any comment starting with this is ours (older builds wrote a translated one).
const RING_PREFIX: &str = "// Dota Skins Launcher:";
/// The command that makes the ring visible.
pub const RING_LINE: &str = "r_draw_selected_ring 1";

/// `text` with our block in it when `on`, without it otherwise.
pub fn with_ring(text: &str, on: bool) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut kept: Vec<&str> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim().starts_with(RING_PREFIX) {
            // Drop the marker and the command under it, if it is ours.
            if lines.clone().next().is_some_and(|l| l.trim() == RING_LINE) {
                lines.next();
            }
            continue;
        }
        if line.trim() == RING_LINE {
            // A line we did not write (or one whose marker was removed) is still
            // the same switch; drop it so `on`/`off` are exact.
            continue;
        }
        kept.push(line);
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    if on && !kept.iter().any(|l| l.trim() == RING_LINE) {
        kept.push(RING_MARK);
        kept.push(RING_LINE);
    }
    if kept.is_empty() {
        String::new()
    } else {
        kept.join(eol) + eol
    }
}

/// Writes the ring switch into `<Dota>/game/dota/cfg/autoexec.cfg` when `on`,
/// takes it out otherwise. Returns `true` when the file changed.
pub fn keep_ring(game_dir: &Path, on: bool) -> Result<bool, String> {
    let file = game_dir.join("cfg").join("autoexec.cfg");
    let text = match std::fs::read(&file) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        // Nothing to remove from a file that does not exist.
        Err(_) if !on => return Ok(false),
        Err(_) => String::new(),
    };
    let next = with_ring(&text, on);
    if next == text {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| crate::trf!("не удалось создать {}: {e}", "could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(&file, next).map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", file.display()))?;
    Ok(true)
}

/// Our weather line: `cl_weather <type>` turns on the weather a weather item
/// gives (its `weather_type`); the default weather has none, so there is no
/// file to swap. Its own marker (not starting with RING_PREFIX).
pub const WEATHER_MARK: &str = "// Dota Skins Launcher - weather";

/// `text` with our weather line set to `weather`, or without it for None.
pub fn with_weather(text: &str, weather: Option<u32>) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut kept: Vec<String> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() == WEATHER_MARK {
            if lines.clone().next().is_some_and(|l| l.trim().starts_with("cl_weather")) {
                lines.next();
            }
            continue;
        }
        kept.push(line.to_string());
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    if let Some(w) = weather {
        kept.push(WEATHER_MARK.to_string());
        kept.push(format!("cl_weather {w}"));
    }
    if kept.is_empty() { String::new() } else { kept.join(eol) + eol }
}

/// Writes or removes the weather line in `<Dota>/game/dota/cfg/autoexec.cfg`.
pub fn keep_weather(game_dir: &Path, weather: Option<u32>) -> Result<bool, String> {
    let file = game_dir.join("cfg").join("autoexec.cfg");
    let text = match std::fs::read(&file) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) if weather.is_none() => return Ok(false),
        Err(_) => String::new(),
    };
    let next = with_weather(&text, weather);
    if next == text {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::trf!("не удалось создать {}: {e}", "could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(&file, next).map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", file.display()))?;
    Ok(true)
}

/// Items with `"skip_model_combine" "1"` (Drow's Dread Retribution crossbow:
/// own skeleton and animations) vanish when merged into the hero's combined
/// model, and the default item whose file they take is merged. Off while one
/// is in the preset.
pub const COMBINE_MARK: &str = "// Dota Skins Launcher - model combine";
pub const COMBINE_LINE: &str = "dota_combine_models 0";

/// `text` with our model-combine line when `on`, without it otherwise.
pub fn with_combine(text: &str, on: bool) -> String {
    let eol = if text.contains("
") { "
" } else { "
" };
    let mut kept: Vec<String> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() == COMBINE_MARK {
            if lines.clone().next().is_some_and(|l| l.trim() == COMBINE_LINE) {
                lines.next();
            }
            continue;
        }
        kept.push(line.to_string());
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    if on {
        kept.push(COMBINE_MARK.to_string());
        kept.push(COMBINE_LINE.to_string());
    }
    if kept.is_empty() { String::new() } else { kept.join(eol) + eol }
}

pub fn keep_combine_off(game_dir: &Path, on: bool) -> Result<bool, String> {
    let file = game_dir.join("cfg").join("autoexec.cfg");
    let text = match std::fs::read(&file) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) if !on => return Ok(false),
        Err(_) => String::new(),
    };
    let next = with_combine(&text, on);
    if next == text {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| crate::trf!("не удалось создать {}: {e}", "could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(&file, next).map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", file.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_line_to_an_empty_file() {
        assert_eq!(with_ring("", true), format!("{RING_MARK}\n{RING_LINE}\n"));
    }

    #[test]
    fn keeps_the_players_own_commands() {
        let text = "dota_show_survivors_picker 1\n";
        let out = with_ring(text, true);
        assert!(out.starts_with(text), "{out}");
        assert!(out.contains(RING_LINE));
    }

    #[test]
    fn adding_twice_changes_nothing() {
        let once = with_ring("", true);
        assert_eq!(with_ring(&once, true), once);
    }

    #[test]
    fn removing_leaves_the_rest() {
        let text = format!("voice_always_sample_mic 1\n{RING_MARK}\n{RING_LINE}\n");
        assert_eq!(with_ring(&text, false), "voice_always_sample_mic 1\n");
    }

    #[test]
    fn removing_from_an_empty_file_gives_an_empty_file() {
        let text = with_ring("", true);
        assert_eq!(with_ring(&text, false), "");
    }

    #[test]
    fn a_line_without_our_marker_is_still_removed() {
        let text = "r_draw_selected_ring 1\n";
        assert_eq!(with_ring(text, false), "");
    }

    #[test]
    fn an_old_translated_marker_is_replaced() {
        // Earlier builds wrote the comment in Russian; it must not linger.
        let old = "// Dota Skins Launcher: эмблема на кольце выбора\nr_draw_selected_ring 1\n";
        assert_eq!(
            with_ring(old, true),
            format!("{RING_MARK}\n{RING_LINE}\n")
        );
        assert_eq!(with_ring(old, false), "");
    }

    #[test]
    fn weather_line_is_set_replaced_and_removed() {
        let ring = with_ring("", true);
        let rain = with_weather(&ring, Some(2));
        assert!(rain.starts_with(&ring) && rain.contains("cl_weather 2"));
        let snow = with_weather(&rain, Some(5));
        assert!(snow.contains("cl_weather 5") && !snow.contains("cl_weather 2"));
        assert_eq!(with_weather(&snow, None), ring);
        // the ring switch keeps the weather line
        assert!(with_ring(&snow, false).contains("cl_weather 5"));
    }

    #[test]
    fn combine_line_comes_and_goes() {
        let on = with_combine("x 1
", true);
        assert_eq!(on, format!("x 1
{COMBINE_MARK}
{COMBINE_LINE}
"));
        assert_eq!(with_combine(&on, true), on);
        assert_eq!(with_combine(&on, false), "x 1
");
    }

    #[test]
    fn crlf_files_keep_crlf() {
        let out = with_ring("x 1\r\n", true);
        assert_eq!(out, format!("x 1\r\n{RING_MARK}\r\n{RING_LINE}\r\n"));
    }
}
