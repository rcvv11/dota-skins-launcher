//! Attaching the user's own files (models, materials, a whole mod .vpk) to an item.
//! Files are copied into %APPDATA%\dota-skins\custom\<item>\, so the originals can
//! be moved or deleted later; the mapping lives in custom_files.json.

use std::path::{Path, PathBuf};

use crate::catalog::FileMap;
use crate::config;

/// Top-level game folders; a path is placed in the game from the first of these.
const GAME_ROOTS: &[&str] = &[
    "models", "materials", "particles", "sounds", "soundevents", "panorama", "scripts",
    "resource", "maps", "characters",
];

pub struct Outcome {
    pub added: Vec<FileMap>,
    pub errors: Vec<String>,
}

/// Where a file goes inside the game, or None if that cannot be told.
/// `.vpk` -> "" (all of its files); a lone `.vmdl_c` -> the item's own model.
pub fn game_path(file: &Path, item_model: Option<&str>) -> Option<String> {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if ext == "vpk" {
        return Some(String::new());
    }
    let parts: Vec<String> = file
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_lowercase()))
        .collect();
    if let Some(i) = parts.iter().position(|p| GAME_ROOTS.contains(&p.as_str())) {
        return Some(parts[i..].join("/"));
    }
    if ext == "vmdl_c" {
        if let Some(model) = item_model {
            return Some(format!("{}_c", model.trim().trim_start_matches('/').to_lowercase()));
        }
    }
    None
}

/// Copies `paths` (files or folders) for `item_id` and returns the new mappings.
pub fn attach(item_id: &str, item_model: Option<&str>, paths: &[PathBuf]) -> Outcome {
    let mut out = Outcome {
        added: Vec::new(),
        errors: Vec::new(),
    };
    let dest_root = config::custom_dir(item_id);
    for path in paths {
        if path.is_dir() {
            for file in walk(path) {
                let rel = file.strip_prefix(path).unwrap_or(&file).to_path_buf();
                // inside a picked folder, "models/..." relative paths count too
                let target = game_path(&file, item_model).or_else(|| game_path(&rel, None));
                add_one(&file, target, &dest_root, &mut out);
            }
        } else {
            add_one(path, game_path(path, item_model), &dest_root, &mut out);
        }
    }
    out
}

fn add_one(src: &Path, target: Option<String>, dest_root: &Path, out: &mut Outcome) {
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    let Some(target) = target else {
        out.errors.push(crate::trf!("{name}: непонятно, куда в игре его класть (нужен путь вида models/…, materials/… или .vpk)", "{name}: unclear where it goes in the game (needs a path like models/…, materials/… or .vpk)"
        ));
        return;
    };
    let is_vpk = target.is_empty();
    let dest = if is_vpk {
        dest_root.join("vpk").join(&name)
    } else {
        dest_root.join("files").join(&target)
    };
    if let Some(parent) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            out.errors.push(format!("{name}: {e}"));
            return;
        }
    }
    if let Err(e) = std::fs::copy(src, &dest) {
        out.errors.push(format!("{name}: {e}"));
        return;
    }
    // a split VPK keeps its data in numbered archives next to *_dir.vpk
    if is_vpk {
        if let Some(stem) = name.strip_suffix("_dir.vpk") {
            for i in 0..1000u16 {
                let part = src.with_file_name(format!("{stem}_{i:03}.vpk"));
                if !part.is_file() {
                    break;
                }
                if let Some(file) = part.file_name() {
                    let _ = std::fs::copy(&part, dest.with_file_name(file));
                }
            }
        }
    }
    out.added.push(FileMap {
        from: dest.to_string_lossy().into_owned(),
        to: target,
    });
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Deletes the stored copy of an attached file (only inside our custom folder).
pub fn remove_copy(file: &FileMap) {
    let p = PathBuf::from(&file.from);
    if p.starts_with(config::app_dir().join("custom")) {
        let _ = std::fs::remove_file(&p);
    }
}

/// Whether a mapping is one of the user's attachments (not from catalog.json).
pub fn is_custom(file: &FileMap) -> bool {
    PathBuf::from(&file.from).starts_with(config::app_dir().join("custom"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_paths() {
        assert_eq!(
            game_path(Path::new(r"D:\mods\hook\models\items\pudge\hook.vmdl_c"), None).as_deref(),
            Some("models/items/pudge/hook.vmdl_c")
        );
        assert_eq!(
            game_path(Path::new(r"D:\stuff\Materials\Foo.vmat_c"), None).as_deref(),
            Some("materials/foo.vmat_c")
        );
        assert_eq!(game_path(Path::new(r"D:\mods\pack_dir.vpk"), None).as_deref(), Some(""));
        assert_eq!(
            game_path(Path::new(r"D:\new_hook.vmdl_c"), Some("models/items/pudge/x.vmdl")).as_deref(),
            Some("models/items/pudge/x.vmdl_c")
        );
        assert!(game_path(Path::new(r"D:\random.png"), None).is_none());
    }
}
