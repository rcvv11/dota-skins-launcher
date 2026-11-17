//! Makes the game actually load the override folder.
//!
//! Dota 2 reads only what `game/dota/gameinfo.gi` lists in `SearchPaths`
//! (paths are relative to `<Dota 2 beta>\game`). This module adds a
//! `Game <folder>/` line to `SearchPaths` right before the main `Game dota`
//! line, so override files win over the game's own content, and re-installs
//! the line whenever it disappears — Steam rewrites gameinfo.gi on updates.

use std::path::{Path, PathBuf};

/// `<Dota 2 beta>` root derived from the dota2.exe path
/// (`<root>/game/bin/win64/dota2.exe`).
pub fn game_root(dota_exe: &str) -> Option<PathBuf> {
    Path::new(dota_exe).ancestors().nth(4).map(|p| p.to_path_buf())
}

/// Default override folder: `<Dota 2 beta>\game\dota_skins`.
pub fn default_mod_dir(dota_exe: &str) -> Option<PathBuf> {
    game_root(dota_exe).map(|root| root.join("game").join("dota_skins"))
}

fn gameinfo_path(dota_exe: &str) -> Result<PathBuf, String> {
    game_root(dota_exe)
        .map(|root| root.join("game").join("dota").join("gameinfo.gi"))
        .ok_or_else(|| crate::trf!("не удалось определить папку Dota по пути {dota_exe}", "could not determine the Dota folder from the path {dota_exe}"))
}

/// The `Game <path>/` line that mounts `mod_dir`, relative to `game/`.
fn entry_line(dota_exe: &str, mod_dir: &Path) -> Result<String, String> {
    let game = game_root(dota_exe)
        .map(|root| root.join("game"))
        .ok_or_else(|| crate::trf!("не удалось определить папку Dota по пути {dota_exe}", "could not determine the Dota folder from the path {dota_exe}"))?;

    let rel = match mod_dir.strip_prefix(&game) {
        Ok(rel) => rel.to_path_buf(),
        Err(_) => {
            // Retry with canonical paths: Windows is case-insensitive, the
            // lexical comparison is not.
            let game_c = game.canonicalize().unwrap_or_else(|_| game.clone());
            let mod_c = mod_dir.canonicalize().unwrap_or_else(|_| mod_dir.to_path_buf());
            mod_c
                .strip_prefix(&game_c)
                .map_err(|_| {
                    crate::trf!("папка оверрайдов {} не внутри {}, игра её не увидит. Укажите папку внутри game\\ (например game\\dota_skins)", "overrides folder {} is not inside {}, the game will not see it. Pick a folder inside game\\ (game\\dota_skins, say)",
                        mod_dir.display(),
                        game.display()
                    )
                })?
                .to_path_buf()
        }
    };

    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().replace('\\', "/"))
        .collect();
    if parts.is_empty() {
        return Err(crate::tr!("папка оверрайдов совпадает с game\\, так нельзя", "the overrides folder is game\\ itself, that is not allowed").into());
    }
    Ok(format!("Game\t\t\t\t{}/", parts.join("/")))
}

/// Normalizes a gameinfo line for comparison: collapses whitespace, lowercases.
fn norm(line: &str) -> String {
    line.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether `SearchPaths` already contains our entry (`Game <folder>` with or
/// without the trailing slash).
fn contains_entry(text: &str, folder: &str) -> bool {
    let folder = folder.trim_end_matches('/').to_lowercase();
    let mut inside = false;
    for line in text.lines() {
        let n = norm(line);
        if !inside {
            if n == "searchpaths" || n.starts_with("searchpaths ") {
                inside = true;
            }
            continue;
        }
        if n == "}" {
            break;
        }
        if let Some(rest) = n.strip_prefix("game ") {
            if rest.trim().trim_end_matches('/') == folder {
                return true;
            }
        }
    }
    false
}

/// Inserts the entry right before the `Game dota` line inside `SearchPaths`.
/// Language paths stay ahead (gameinfo says they must be mounted first).
fn insert_entry(text: &str, entry: &str) -> Result<String, String> {
    let mut lines: Vec<&str> = text.lines().collect();
    let searchpaths = lines
        .iter()
        .position(|l| norm(l) == "searchpaths")
        .ok_or(crate::tr!("в gameinfo.gi нет строки SearchPaths", "gameinfo.gi has no SearchPaths line"))?;
    let opening = lines[searchpaths..]
        .iter()
        .position(|l| norm(l) == "{")
        .ok_or(crate::tr!("в gameinfo.gi нет { после SearchPaths", "gameinfo.gi has no { after SearchPaths"))?
        + searchpaths;
    let game_dota = lines[opening + 1..]
        .iter()
        .position(|l| {
            let n = norm(l);
            n == "game dota" || n == "game dota/"
        })
        .map(|i| i + opening + 1)
        .ok_or(crate::tr!("в gameinfo.gi не нашлась строка \"Game dota\" — проверьте файл вручную", "no \"Game dota\" line found in gameinfo.gi, check the file by hand"))?;
    lines.insert(game_dota, entry);
    Ok(join_like(text, &lines))
}

/// `lines` joined with the line ending `text` uses (Valve ships gameinfo.gi
/// with CRLF; rewriting it with LF made the file differ from the original in
/// every line, not just ours).
fn join_like(text: &str, lines: &[&str]) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = lines.join(eol);
    if text.ends_with('\n') {
        out.push_str(eol);
    }
    out
}

/// Adds the entry if missing. Idempotent; keeps a one-time backup next to the
/// file. Returns a short human message.
pub fn ensure(dota_exe: &str, mod_dir: &Path) -> Result<String, String> {
    let path = gameinfo_path(dota_exe)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| crate::trf!("не удалось прочитать {}: {e}", "could not read {}: {e}", path.display()))?;
    let entry = entry_line(dota_exe, mod_dir)?;
    let folder: String = mod_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if contains_entry(&text, &folder) {
        return Ok(crate::tr!("папка оверрайдов уже смонтирована в gameinfo.gi", "the overrides folder is already mounted in gameinfo.gi").into());
    }
    let backup = path.with_extension("gi.launcher-backup");
    if !backup.is_file() {
        std::fs::copy(&path, &backup)
            .map_err(|e| crate::trf!("не удалось сделать резервную копию {}: {e}", "could not back up {}: {e}", backup.display()))?;
    }
    let updated = insert_entry(&text, &entry)?;
    std::fs::write(&path, updated)
        .map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", path.display()))?;
    Ok(crate::trf!("добавлена строка \"{entry}\" в {}", "added the line \"{entry}\" to {}", path.display()))
}

/// Removes the entry (used when the user uninstalls or changes the folder).
pub fn remove(dota_exe: &str, mod_dir: &Path) -> Result<(), String> {
    let path = gameinfo_path(dota_exe)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| crate::trf!("не удалось прочитать {}: {e}", "could not read {}: {e}", path.display()))?;
    let folder: String = mod_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !contains_entry(&text, &folder) {
        return Ok(());
    }
    let entry = entry_line(dota_exe, mod_dir)?;
    let filtered: Vec<&str> = text.lines().filter(|l| norm(l) != norm(&entry)).collect();
    let out = join_like(&text, &filtered);
    // Put Valve's own bytes back when nothing but our line differs from the
    // backup taken before the first mount (it also keeps the original CRLF).
    let backup = path.with_extension("gi.launcher-backup");
    let restored = std::fs::read(&backup)
        .ok()
        .filter(|b| String::from_utf8_lossy(b).lines().eq(out.lines()));
    let bytes = restored.unwrap_or_else(|| out.into_bytes());
    std::fs::write(&path, bytes).map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", path.display()))
}

/// Old builds wrote overrides to `<root>\dota_skins`, which the game never
/// mounts. Move such an install into `<root>\game\dota_skins` when the target
/// is free and the source folder belongs to the launcher (or is empty).
pub fn migrate_legacy_dir(dota_exe: &str, new_mod_dir: &Path) -> Option<String> {
    let root = game_root(dota_exe)?;
    let old = root.join("dota_skins");
    if !old.is_dir() || old == new_mod_dir {
        return None;
    }
    if std::fs::read_dir(new_mod_dir).is_ok() {
        return None; // never overwrite an existing installation
    }
    let ours = old.join("installed.txt").is_file();
    let empty = std::fs::read_dir(&old)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if !ours && !empty {
        return None;
    }
    let parent = new_mod_dir.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    std::fs::rename(&old, new_mod_dir).ok()?;
    Some(crate::trf!("перенесены оверрайды: {} → {}", "moved the overrides: {} → {}",
        old.display(),
        new_mod_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed copy of the SearchPaths block from the current gameinfo.gi.
    const SAMPLE: &str = r#"	SearchPaths
	{
		// language paths
		Game_AudioLanguage	dota_*LANGUAGE*
		Game_LowViolence	dota_lv

		Game				dota
		Game				core

		Mod					dota

		Write				dota

		AddonRoot_AudioLanguage	dota_*LANGUAGE*_addons
		AddonRoot			dota_addons
		PublicContent		dota_core
		PublicContent		core
	}
"#;

    fn sample_entry() -> String {
        "Game\t\t\t\tdota_skins/".into()
    }

    #[test]
    fn insert_goes_before_game_dota() {
        let out = insert_entry(SAMPLE, &sample_entry()).unwrap();
        let sp = out.find("SearchPaths").unwrap();
        let ours = out.find("Game\t\t\t\tdota_skins/").unwrap();
        let dota = out.find("Game\t\t\t\tdota\n").unwrap_or(out.find("Game\t\t\t\tdota").unwrap());
        let core = out.find("Game\t\t\t\tcore").unwrap();
        assert!(sp < ours, "SearchPaths должен идти раньше вставки");
        assert!(ours < dota, "наша строка должна стоять перед Game dota");
        assert!(dota < core, "Game dota должен остаться перед Game core");
        assert!(out.contains("Game_AudioLanguage"), "языковые пути не потеряны");
    }

    #[test]
    fn insert_is_idempotent_via_contains() {
        let once = insert_entry(SAMPLE, &sample_entry()).unwrap();
        assert!(contains_entry(&once, "dota_skins"));
        let mut twice = once.clone();
        if !contains_entry(&twice, "dota_skins") {
            twice = insert_entry(&twice, &sample_entry()).unwrap();
        }
        assert_eq!(twice.matches("dota_skins").count(), once.matches("dota_skins").count());
    }

    #[test]
    fn contains_without_trailing_slash() {
        // `Game dota_skins/` written by us vs `Game dota_skins` written by hand
        let text = "SearchPaths\n{\n\tGame\tdota_skins/\n\tGame\tdota\n}\n";
        assert!(contains_entry(text, "dota_skins"));
        let text2 = "SearchPaths\n{\n\tGame\tdota_skins\n\tGame\tdota\n}\n";
        assert!(contains_entry(text2, "dota_skins"));
        // "dota_skins" must not be matched for folder "dota"
        let text3 = "SearchPaths\n{\n\tGame\tdota_skins\n}\n";
        assert!(!contains_entry(text3, "dota"));
    }

    #[test]
    fn crlf_is_kept() {
        let text = "SearchPaths\r\n{\r\n\tGame\tdota\r\n}\r\n";
        let out = insert_entry(text, "Game\t\t\t\tdota_skins/").unwrap();
        assert_eq!(out, "SearchPaths\r\n{\r\nGame\t\t\t\tdota_skins/\r\n\tGame\tdota\r\n}\r\n");
        assert_eq!(join_like(text, &text.lines().collect::<Vec<_>>()), text);
    }

    #[test]
    fn remove_deletes_only_our_line() {
        let once = insert_entry(SAMPLE, &sample_entry()).unwrap();
        let filtered: Vec<&str> = once
            .lines()
            .filter(|l| norm(l) != norm("Game\t\t\t\tdota_skins/"))
            .collect();
        let out = filtered.join("\n");
        assert!(!out.contains("dota_skins"));
        assert!(out.contains("Game\t\t\t\tdota"));
    }

    #[test]
    fn nested_folder_gets_relative_entry() {
        // mod_dir: <root>/game/custom/skins -> Game custom/skins/
        let root = std::env::temp_dir().join(format!("mount_test_{}", std::process::id()));
        let game = root.join("game");
        let mod_dir = game.join("custom").join("skins");
        let dota_exe = game.join("bin").join("win64").join("dota2.exe");
        let _ = std::fs::create_dir_all(&mod_dir);
        let line = entry_line(&dota_exe.to_string_lossy(), &mod_dir).unwrap();
        assert_eq!(line, "Game\t\t\t\tcustom/skins/");
        assert!(line.ends_with("custom/skins/"), "путь относительный и со слэшем");
        let _ = std::fs::remove_dir_all(&root);
    }
}
