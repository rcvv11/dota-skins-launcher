use crate::catalog::Skin;
use std::path::{Path, PathBuf};

/// Marker written by the launcher; only folders containing it may be wiped.
const MANIFEST: &str = "installed.txt";
const PAK_NAME: &str = "pak01_dir.vpk";

/// Everything needed to install a preset, detached from the catalog so it can
/// be handed to a background thread.
#[derive(Clone)]
pub struct Install {
    /// (destination inside the override folder, source file on disk)
    pub files: Vec<(String, PathBuf)>,
    /// Item ids, written to the manifest.
    pub ids: Vec<String>,
    pub mod_dir: PathBuf,
    pub pack_vpk: bool,
}

impl Install {
    pub fn new(skins: &[&Skin], assets: &Path, mod_dir: PathBuf, pack_vpk: bool) -> Self {
        Self {
            files: skins
                .iter()
                .flat_map(|s| s.files.iter().map(|m| (m.to.clone(), assets.join(&m.from))))
                .collect(),
            ids: skins.iter().map(|s| s.id.clone()).collect(),
            mod_dir,
            pack_vpk,
        }
    }
}

/// Replaces the previous installation with this preset. Returns files written.
pub fn install(plan: &Install) -> Result<usize, String> {
    for (_, src) in &plan.files {
        if !src.is_file() {
            return Err(crate::trf!("нет исходного файла: {}", "no source file: {}", src.display()));
        }
    }

    // Collect everything first: plain files, and whole mod VPKs (empty `to`).
    // Later entries win when two items provide the same path.
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for (to, src) in &plan.files {
        let is_vpk = src
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("vpk"));
        if to.is_empty() && is_vpk {
            entries.extend(crate::vpk::read_all(src)?);
        } else {
            let bytes = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
            entries.push((to.clone(), bytes));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<(String, Vec<u8>)> = Vec::new();
    for (path, bytes) in entries.into_iter().rev() {
        let key = path.replace('\\', "/").to_lowercase();
        if seen.insert(key) {
            unique.push((path, bytes));
        }
    }
    unique.reverse();

    clear_mod(&plan.mod_dir)?;
    std::fs::create_dir_all(&plan.mod_dir)
        .map_err(|e| crate::trf!("не удалось создать папку оверрайдов: {e}", "could not create the overrides folder: {e}"))?;

    if plan.pack_vpk {
        if !unique.is_empty() {
            crate::vpk::write_vpk(&plan.mod_dir.join(PAK_NAME), &unique)?;
        }
    } else {
        for (to, bytes) in &unique {
            let dst = plan.mod_dir.join(to);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| crate::trf!("не удалось создать {}: {e}", "could not create {}: {e}", parent.display()))?;
            }
            std::fs::write(&dst, bytes)
                .map_err(|e| crate::trf!("не удалось записать {}: {e}", "could not write {}: {e}", dst.display()))?;
        }
    }

    std::fs::write(plan.mod_dir.join(MANIFEST), plan.ids.join("\n"))
        .map_err(|e| crate::trf!("не удалось записать манифест: {e}", "could not write the manifest: {e}"))?;
    Ok(unique.len())
}

/// Removes the override folder, but only if the launcher created it
/// (manifest present) or it is empty — never an arbitrary user folder.
pub fn clear_mod(mod_dir: &Path) -> Result<(), String> {
    if !mod_dir.is_dir() {
        return Ok(());
    }
    let ours = mod_dir.join(MANIFEST).is_file();
    let empty = std::fs::read_dir(mod_dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false);
    if !ours && !empty {
        return Err(crate::trf!("{} создана не лаунчером (нет {MANIFEST}), удалять её не буду. Укажите отдельную папку в настройках.", "{} was not created by the launcher (no {MANIFEST}), I will not delete it. Set a separate folder in the settings.",
            mod_dir.display()
        ));
    }
    std::fs::remove_dir_all(mod_dir)
        .map_err(|e| crate::trf!("не удалось удалить {}: {e}", "could not delete {}: {e}", mod_dir.display()))
}
