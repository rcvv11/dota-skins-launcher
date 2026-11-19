//! Item glows no file swap can place (the slot's default item creates no
//! particle to stand in for): the DLL creates them on the hero in a match,
//! from Dota's own UI script (Panorama `Particles` API). How each one hangs
//! on the hero comes from its particle file: the control point drivers of its
//! first configuration (CP, attach type, attachment) and the CP it takes the
//! model snapshot from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One control point of a glow: `(cp, attach type, attachment)`.
pub type Driver = (u32, String, String);

/// `(hero model, particle)` -> the particles the DLL creates on the hero.
type Parts = HashMap<(String, String), Vec<String>>;

/// The glow caches, read from their files once and kept in memory. Their locks are
/// held only to look up and to merge: the CLI runs happen outside them, so a pick
/// does not wait for the background warm-up's runs.
static DRIVERS: Mutex<Option<HashMap<String, Vec<Driver>>>> = Mutex::new(None);
static PARTS: Mutex<Option<Parts>> = Mutex::new(None);

fn cache_file(name: &str) -> PathBuf {
    crate::config::app_dir().join("cache").join(name)
}

// "2": control point 0 and world-origin offsets are read too (older entries lack them)
const DRIVERS_FILE: &str = "glow_drivers2.txt";
const PARTS_FILE: &str = "glow_parts.txt";

fn with_drivers<R>(f: impl FnOnce(&mut HashMap<String, Vec<Driver>>) -> R) -> R {
    let mut guard = DRIVERS.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(|| {
        std::fs::read_to_string(cache_file(DRIVERS_FILE))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let (p, d) = l.split_once('\t')?;
                Some((p.to_string(), parse_line(d)))
            })
            .collect()
    });
    f(cache)
}

fn with_parts<R>(f: impl FnOnce(&mut Parts) -> R) -> R {
    let mut guard = PARTS.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(|| {
        // model \t particle \t part,part,…
        std::fs::read_to_string(cache_file(PARTS_FILE))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                let (model, particle, parts) = (f.next()?, f.next()?, f.next()?);
                let parts = parts.split(',').filter(|p| !p.is_empty()).map(str::to_string).collect();
                Some(((model.to_string(), particle.to_string()), parts))
            })
            .collect()
    });
    f(cache)
}

/// Drivers of each particle, from the cache (`cache/glow_drivers2.txt`) or read
/// with Source2Viewer-CLI and added to it: a pick should not wait for the CLI
/// on every glow again.
pub fn drivers_cached(cli: &Path, pak: &Path, particles: &[&str]) -> HashMap<String, Vec<Driver>> {
    let mut missing: Vec<&str> = with_drivers(|cache| particles.iter().copied().filter(|p| !cache.contains_key(*p)).collect());
    missing.sort();
    missing.dedup();
    // a CLI start costs ~0.4 s: all missing particles in one call (in chunks:
    // the command line has a length limit)
    let mut found = HashMap::new();
    for chunk in missing.chunks(150) {
        if let Some(f) = drivers_many(cli, pak, chunk) {
            found.extend(f);
        }
    }
    with_drivers(|cache| {
        if !found.is_empty() {
            cache.extend(found);
            let mut text = String::new();
            for (p, d) in cache.iter() {
                text.push_str(&format!("{p}\t{}\n", drivers_text(d)));
            }
            let path = cache_file(DRIVERS_FILE);
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(&path, text);
        }
        particles.iter().filter_map(|p| cache.get(*p).map(|d| (p.to_string(), d.clone()))).collect()
    })
}

/// Drivers of several particles from one Source2Viewer-CLI run. None if the CLI
/// could not run.
fn drivers_many(cli: &Path, pak: &Path, particles: &[&str]) -> Option<std::collections::HashMap<String, Vec<Driver>>> {
    let texts = texts_many(cli, pak, particles)?;
    // a particle missing from the pak gets the default (as one CLI call per file did)
    Some(
        particles
            .iter()
            .map(|p| (p.to_string(), parse(texts.get(&key(p)).map(String::as_str).unwrap_or(""))))
            .collect(),
    )
}

/// Lookup key of a resource: lower case, without the compiled `_c`.
fn key(path: &str) -> String {
    path.trim_end_matches("_c").to_ascii_lowercase()
}

/// Text form (DATA block) of several resources from one Source2Viewer-CLI run
/// (its output has a `[i/n] path_c` header before each file), by [`key`];
/// a resource missing from the pak is empty. None if the CLI could not run.
fn texts_many<S: AsRef<str>>(cli: &Path, pak: &Path, files: &[S]) -> Option<std::collections::HashMap<String, String>> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let compiled: Vec<String> = files.iter().map(|p| format!("{}_c", p.as_ref().trim_end_matches("_c"))).collect();
    let out = std::process::Command::new(cli)
        .arg("-i")
        .arg(pak)
        .arg("-f")
        .arg(compiled.join(","))
        .args(["-b", "DATA"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut sections: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let t = line.trim_end();
        if t.starts_with('[') && t.contains("] ") && t.ends_with("_c") {
            let path = t.split_once("] ").map(|(_, p)| p).unwrap_or("");
            current = Some(key(path));
            continue;
        }
        if let Some(c) = &current {
            let s = sections.entry(c.clone()).or_default();
            s.push_str(line);
            s.push('\n');
        }
    }
    Some(
        files
            .iter()
            .map(|f| {
                let k = key(f.as_ref());
                let body = sections.remove(&k).unwrap_or_default();
                (k, body)
            })
            .collect(),
    )
}

/// What the DLL creates for each `(particle, hero model)`: the particle itself,
/// or, when some of its children skin a model snapshot to bones the hero's
/// skeleton lacks, its other children (with the particle's control points).
/// Such a snapshot is the item's own shape: Twilight Schism's blade outline is
/// skinned to the glaive's `blade_02_JNT`, and hung on Luna (the only entity the
/// UI script can reach) it fell onto her mount's leg. Cached in
/// `cache/glow_parts.txt`: the particle trees are read once.
pub fn parts_cached(cli: &Path, pak: &Path, glows: &[(&str, &str)]) -> Parts {
    let wanted: Vec<(String, String)> = glows.iter().map(|(p, m)| (m.to_string(), p.to_string())).collect();
    let mut missing: Vec<(String, String)> = with_parts(|cache| wanted.iter().filter(|k| !cache.contains_key(*k)).cloned().collect());
    missing.sort();
    missing.dedup();
    let found = if missing.is_empty() { Parts::new() } else { split_glows(cli, pak, &missing) };
    with_parts(|cache| {
        if !found.is_empty() {
            cache.extend(found);
            let mut text = String::new();
            for ((model, particle), parts) in cache.iter() {
                text.push_str(&format!("{model}\t{particle}\t{}\n", parts.join(",")));
            }
            let path = cache_file(PARTS_FILE);
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(&path, text);
        }
        wanted.into_iter().filter_map(|k| cache.get(&k).cloned().map(|v| (k, v))).collect()
    })
}

/// The parts known already; for the others the whole particle (what the hero
/// carried before they were worked out) while a background thread works them out
/// for the next apply. The parts only matter where the item's own model is not
/// found in the game, and a pick should not wait seconds of CLI runs for them.
pub fn parts_now(cli: &Path, pak: &Path, glows: &[(&str, &str)]) -> Parts {
    let wanted: Vec<(String, String)> = glows.iter().map(|(p, m)| (m.to_string(), p.to_string())).collect();
    let (known, missing): (Parts, Vec<(String, String)>) = with_parts(|cache| {
        let mut known = Parts::new();
        let mut missing = Vec::new();
        for k in wanted {
            match cache.get(&k) {
                Some(v) => {
                    known.insert(k, v.clone());
                }
                None => missing.push(k),
            }
        }
        (known, missing)
    });
    if missing.is_empty() {
        return known;
    }
    let mut out = known;
    for k in &missing {
        out.insert(k.clone(), vec![k.1.clone()]);
    }
    let (cli, pak) = (cli.to_path_buf(), pak.to_path_buf());
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = missing.iter().map(|(m, p)| (p.as_str(), m.as_str())).collect();
        parts_cached(&cli, &pak, &pairs);
    });
    out
}

/// Works out every catalog item's glows once in the background (their control points
/// and the parts a hero can carry), so a pick needs no CLI run for them. In chunks:
/// the caches grow (and are saved) as it goes.
pub fn warm_up(dota_exe: String, cli: PathBuf, items: Vec<crate::visuals::HeroItem>) {
    std::thread::spawn(move || warm(&dota_exe, &cli, &items));
}

fn warm(dota_exe: &str, cli: &Path, items: &[crate::visuals::HeroItem]) {
    {
        let Some(dir) = crate::vpk::game_dir_from_exe(dota_exe) else { return };
        let Ok(data) = crate::gamedata::load(&dir) else { return };
        let pak = dir.join("pak01_dir.vpk");
        let started = std::time::Instant::now();
        let (_, glows) = crate::visuals::hero_overrides_and_glows(&data.index, items, &data.units, &data.npcs);
        let mut pairs: Vec<(String, String)> = glows
            .iter()
            .filter_map(|g| data.units.get(&g.npc).map(|m| (g.particle.clone(), m.clone())))
            .collect();
        pairs.sort();
        pairs.dedup();
        let mut particles: Vec<&str> = pairs.iter().map(|(p, _)| p.as_str()).collect();
        particles.dedup();
        drivers_cached(cli, &pak, &particles);
        for chunk in pairs.chunks(60) {
            let chunk: Vec<(&str, &str)> = chunk.iter().map(|(p, m)| (p.as_str(), m.as_str())).collect();
            parts_cached(cli, &pak, &chunk);
        }
        // the colourings too: the material groups of the item models that have them
        let mut models: Vec<String> = crate::skins::wanted(&data.index, items).into_iter().map(|(_, m, _)| m).collect();
        models.sort();
        models.dedup();
        for chunk in models.chunks(100) {
            crate::skins::material_groups_cached(cli, &pak, chunk);
        }
        crate::picks::trace(&format!(
            "warm-up: {} glows and {} coloured models of {} items in {} ms",
            pairs.len(),
            models.len(),
            items.len(),
            started.elapsed().as_millis()
        ));
    }
}

/// The parts of each missing `(hero model, particle)`, from their particle trees
/// read with Source2Viewer-CLI (none if it could not run).
fn split_glows(cli: &Path, pak: &Path, missing: &[(String, String)]) -> Parts {
    let mut out = Parts::new();
    // the particle trees and the hero models, level by level: one CLI run each
    let mut texts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut want: Vec<String> = missing.iter().flat_map(|(m, p)| [m.clone(), p.clone()]).filter(|f| !f.is_empty()).collect();
    for _depth in 0..8 {
        want.sort();
        want.dedup();
        want.retain(|w| !texts.contains_key(&key(w)));
        if want.is_empty() {
            break;
        }
        let mut found = std::collections::HashMap::new();
        for chunk in want.chunks(150) {
            match texts_many(cli, pak, chunk) {
                Some(t) => found.extend(t),
                // without the CLI nothing is split (and nothing is cached)
                None => return out,
            }
        }
        want = found.values().flat_map(|t| children(t).into_iter().map(|c| c.0).chain(snapshot(t))).collect();
        texts.extend(found);
    }
    for (model, particle) in missing {
        let bones: std::collections::HashSet<String> =
            quoted_in(texts.get(&key(model)).map(String::as_str).unwrap_or(""), "m_boneName").iter().map(|b| b.to_ascii_lowercase()).collect();
        // a hero whose skeleton could not be read keeps the whole glow
        let parts = if bones.is_empty() { vec![particle.clone()] } else { expand(particle, None, &texts, &bones, 0) };
        out.insert((model.clone(), particle.clone()), parts);
    }
    out
}

/// The parts of `particle` that can hang on a hero with these bones (see
/// [`parts_cached`]): none when it skins a snapshot to a bone the hero lacks;
/// itself when all its children can; else its children's parts. A parent that
/// draws or holds a snapshot of its own stays whole (its children need it).
fn expand(
    particle: &str,
    inherited_snapshot: Option<&str>,
    texts: &std::collections::HashMap<String, String>,
    bones: &std::collections::HashSet<String>,
    depth: u32,
) -> Vec<String> {
    let text = texts.get(&key(particle)).map(String::as_str).unwrap_or("");
    let own_snapshot = snapshot(text);
    let snap = own_snapshot.as_deref().or(inherited_snapshot);
    if skins_snapshot(text) {
        if let Some(s) = snap {
            let snap_text = texts.get(&key(s)).map(String::as_str).unwrap_or("");
            if quoted_in(snap_text, "string_list").iter().any(|b| !bones.contains(&b.to_ascii_lowercase())) {
                return Vec::new();
            }
        }
    }
    let kids = children(text);
    if kids.is_empty() || depth >= 8 {
        return vec![particle.to_string()];
    }
    let parts: Vec<Vec<String>> = kids.iter().map(|(c, _)| expand(c, snap, texts, bones, depth + 1)).collect();
    if parts.iter().zip(&kids).all(|(p, (c, _))| p.len() == 1 && &p[0] == c) {
        return vec![particle.to_string()];
    }
    let all_gone = parts.iter().all(Vec::is_empty);
    if draws(text) || (own_snapshot.is_some() && !all_gone) {
        return vec![particle.to_string()];
    }
    // created on its own, a child plays at once: end-cap and disabled ones are left out
    parts.into_iter().zip(&kids).filter(|(_, (_, skip))| !skip).flat_map(|(p, _)| p).collect()
}

/// `m_Children` of a particle: `(path, end-cap or disabled)`.
fn children(text: &str) -> Vec<(String, bool)> {
    let Some(start) = text.find("m_Children") else { return Vec::new() };
    let body = &text[start..];
    let body = &body[..matching_close(body).unwrap_or(body.len())];
    body.split('}')
        .filter_map(|entry| {
            let path = resource(&field(entry, "m_ChildRef")?);
            let skip = field(entry, "m_bEndCap").as_deref() == Some("true") || field(entry, "m_bDisableChild").as_deref() == Some("true");
            (!path.is_empty()).then_some((path, skip))
        })
        .collect()
}

/// The snapshot file a particle sets (`m_hSnapshot`).
fn snapshot(text: &str) -> Option<String> {
    field(text, "m_hSnapshot").map(|v| resource(&v)).filter(|p| !p.is_empty())
}

/// A particle placing its particles on a snapshot skinned to the bones of the
/// control point's entity.
fn skins_snapshot(text: &str) -> bool {
    ["C_INIT_InitSkinnedPositionFromCPSnapshot", "C_OP_SnapshotRigidSkinToBones", "C_OP_SnapshotSkinToBones"].iter().any(|c| text.contains(c))
}

/// A particle with renderers of its own.
fn draws(text: &str) -> bool {
    let Some(start) = text.find("m_Renderers") else { return false };
    let body = &text[start..];
    body[..matching_close(body).unwrap_or(body.len())].contains("_class")
}

/// `resource:"path"` (quotes partly trimmed already) -> `path`.
fn resource(value: &str) -> String {
    value.trim().trim_start_matches("resource_name:").trim_start_matches("resource:").trim_matches('"').to_string()
}

/// Non-empty quoted strings of the `key = [ … ]` list (bone names of a model's
/// skeleton, of a snapshot's skinning).
fn quoted_in(text: &str, key: &str) -> Vec<String> {
    let Some(start) = text.find(&format!("{key} =")) else { return Vec::new() };
    let body = &text[start..];
    let body = &body[..matching_close(body).unwrap_or(body.len())];
    body.split('"').skip(1).step_by(2).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

fn drivers_text(drivers: &[Driver]) -> String {
    drivers.iter().map(|(cp, kind, att)| format!("{cp}:{kind}:{att}")).collect::<Vec<_>>().join(";")
}

fn parse_line(text: &str) -> Vec<Driver> {
    text.split(';')
        .filter_map(|d| {
            let mut f = d.splitn(3, ':');
            Some((f.next()?.parse().ok()?, f.next()?.to_string(), f.next().unwrap_or("").to_string()))
        })
        .collect()
}

/// Drivers from a particle's text form, plus the snapshot CP on the entity.
/// A world-origin control point is a value, not a place: its "attachment" is the
/// offset ("1,0,0"), set as it is (attaching it to the hero made a model snapshot
/// taken from that point trace the hero's whole model). A driver with no attach
/// type is the entity's origin (`self_with_wearables` for control point 0); the
/// game's `parent` stub is left out. Control point 0 is on the entity when the
/// file does not place it (else it stayed at the world origin).
pub fn parse(text: &str) -> Vec<Driver> {
    let mut out: Vec<Driver> = Vec::new();
    if let Some(start) = text.find("m_controlPointConfigurations") {
        let rest = &text[start..];
        // the first configuration's drivers only
        if let Some(d) = rest.find("m_drivers") {
            let body = &rest[d..];
            let end = matching_close(body).unwrap_or(body.len());
            let body = &body[..end];
            for entry in body.split('}') {
                let entity = field(entry, "m_entityName");
                let kind = field(entry, "m_iAttachType");
                if kind.is_none() && (entity.is_none() || entity.as_deref() == Some("parent")) {
                    continue;
                }
                let cp = field(entry, "m_iControlPoint").and_then(|v| v.parse().ok()).unwrap_or(out.len() as u32);
                let kind = kind.unwrap_or_else(|| "PATTACH_ABSORIGIN_FOLLOW".into());
                let attachment = if kind == "PATTACH_WORLDORIGIN" {
                    field(entry, "m_vecOffset")
                        .map(|v| v.trim_matches(|c| c == '[' || c == ']' || c == ' ').replace(' ', ""))
                        .unwrap_or_default()
                } else {
                    field(entry, "m_attachmentName").unwrap_or_default()
                };
                out.push((cp, kind, attachment));
            }
        }
    }
    if let Some(cp) = field(text, "m_nSnapshotControlPoint").and_then(|v| v.parse::<u32>().ok()) {
        if !out.iter().any(|d| d.0 == cp) {
            out.push((cp, "PATTACH_ABSORIGIN_FOLLOW".into(), String::new()));
        }
    }
    if !out.iter().any(|d| d.0 == 0) {
        out.insert(0, (0, "PATTACH_ABSORIGIN_FOLLOW".into(), String::new()));
    }
    out
}

/// Value of `key = value` (quotes dropped) on the first line that has it.
fn field(text: &str, key: &str) -> Option<String> {
    let at = text.find(&format!("{key} ="))?;
    let line = text[at + key.len() + 2..].lines().next()?.trim();
    Some(line.trim_matches('"').to_string())
}

/// Position of the `]` closing the first `[` in `text`.
fn matching_close(text: &str) -> Option<usize> {
    let open = text.find('[')?;
    let mut depth = 0;
    for (i, c) in text[open..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Config line for the DLL: `glow|npc|particle|cp:type:attachment;...|item model|part,part`.
/// The DLL hangs the particle on the item's own model in the game (the wearable
/// whose model the swap table turns into `item`); where that is not found, it
/// creates `parts` on the hero (see [`parts_cached`]).
pub fn config_line(npc: &str, particle: &str, drivers: &[Driver], item: Option<&str>, parts: &[String]) -> String {
    format!("glow|{npc}|{particle}|{}|{}|{}", drivers_text(drivers), item.unwrap_or(""), parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AURELIAN: &str = r#"{
	m_nSnapshotControlPoint = 7
	m_controlPointConfigurations =
	[
		{
			m_name = "preview"
			m_drivers =
			[
				{
					m_iAttachType = "PATTACH_POINT_FOLLOW"
					m_entityName = "parent"
					m_attachmentName = "attach_weapon_l"
				},
			]
			m_previewState =
			{
				m_previewModel = "models/items/alchemist/twin_blades_aurelian/twin_blades_aurelian.vmdl"
			}
		},
	]
}"#;

    const SCHISM: &str = r#"{
	m_nSnapshotControlPoint = 6
	m_controlPointConfigurations =
	[
		{
			m_name = "preview"
			m_drivers =
			[
				{
					m_entityName = "self_with_wearables"
				},
				{
					m_iControlPoint = 1
					m_iAttachType = "PATTACH_POINT_FOLLOW"
					m_attachmentName = "attach_weapon_center"
					m_entityName = "self"
				},
				{
					m_iControlPoint = 6
					m_iAttachType = "PATTACH_WORLDORIGIN"
					m_vecOffset = [ 1.0, 0.0, 0.0 ]
					m_entityName = "self"
				},
				{
					m_iControlPoint = 10
					m_entityName = "parent"
				},
			]
		},
	]
}"#;

    #[test]
    fn children_skinned_to_bones_the_hero_lacks_are_left_out() {
        let texts: std::collections::HashMap<String, String> = [
            (
                "p/root.vpcf",
                "{\n\tm_Children =\n\t[\n\t\t{\n\t\t\tm_ChildRef = resource:\"p/trace.vpcf\"\n\t\t},\n\t\t{\n\t\t\tm_ChildRef = resource:\"p/rays.vpcf\"\n\t\t},\n\t\t{\n\t\t\tm_ChildRef = resource:\"p/end.vpcf\"\n\t\t\tm_bEndCap = true\n\t\t},\n\t]\n\tm_nMaxParticles = 0\n}",
            ),
            (
                "p/trace.vpcf",
                "{\n\tm_hSnapshot = resource:\"p/blade.vsnap\"\n\tm_Children =\n\t[\n\t\t{\n\t\t\tm_ChildRef = resource:\"p/trace_glow.vpcf\"\n\t\t},\n\t]\n}",
            ),
            (
                "p/trace_glow.vpcf",
                "{\n\tm_Renderers =\n\t[\n\t\t{\n\t\t\t_class = \"C_OP_RenderSprites\"\n\t\t},\n\t]\n\tm_Initializers =\n\t[\n\t\t{\n\t\t\t_class = \"C_INIT_InitSkinnedPositionFromCPSnapshot\"\n\t\t},\n\t]\n}",
            ),
            ("p/rays.vpcf", "{\n\tm_Renderers =\n\t[\n\t\t{\n\t\t\t_class = \"C_OP_RenderTrails\"\n\t\t},\n\t]\n}"),
            ("p/end.vpcf", "{\n\tm_Renderers =\n\t[\n\t\t{\n\t\t\t_class = \"C_OP_RenderSprites\"\n\t\t},\n\t]\n}"),
            ("p/blade.vsnap", "{\n\tstring_list =\n\t[\n\t\t\"\",\n\t\t\"blade_02_JNT\",\n\t]\n}"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let hero = |b: &[&str]| b.iter().map(|s| s.to_string()).collect::<std::collections::HashSet<_>>();
        // Luna has no blade bone: the outline goes, the rays stay, the end cap is not played at once
        assert_eq!(expand("p/root.vpcf", None, &texts, &hero(&["root", "spine2_1"]), 0), vec!["p/rays.vpcf".to_string()]);
        // a skeleton with the bone keeps the glow whole
        assert_eq!(expand("p/root.vpcf", None, &texts, &hero(&["root", "blade_02_jnt"]), 0), vec!["p/root.vpcf".to_string()]);
        assert_eq!(quoted_in(&texts["p/blade.vsnap"], "string_list"), vec!["blade_02_JNT".to_string()]);
    }

    #[test]
    fn world_origin_points_are_values_and_cp0_is_on_the_entity() {
        assert_eq!(
            parse(SCHISM),
            vec![
                (0, "PATTACH_ABSORIGIN_FOLLOW".to_string(), String::new()),
                (1, "PATTACH_POINT_FOLLOW".to_string(), "attach_weapon_center".to_string()),
                (6, "PATTACH_WORLDORIGIN".to_string(), "1.0,0.0,0.0".to_string()),
            ]
        );
    }

    #[test]
    fn drivers_of_the_aurelian_glow() {
        let d = parse(AURELIAN);
        assert_eq!(
            d,
            vec![
                (0, "PATTACH_POINT_FOLLOW".to_string(), "attach_weapon_l".to_string()),
                (7, "PATTACH_ABSORIGIN_FOLLOW".to_string(), String::new()),
            ]
        );
        assert_eq!(parse_line(&drivers_text(&d)), d);
        assert_eq!(
            config_line("npc_dota_hero_alchemist", "particles/a.vpcf", &d, Some("models/items/alchemist/axe.vmdl"), &["particles/a.vpcf".to_string()]),
            "glow|npc_dota_hero_alchemist|particles/a.vpcf|0:PATTACH_POINT_FOLLOW:attach_weapon_l;7:PATTACH_ABSORIGIN_FOLLOW:|models/items/alchemist/axe.vmdl|particles/a.vpcf"
        );
        // no item model, nothing left for the hero
        assert_eq!(config_line("npc", "p.vpcf", &d[..1], None, &[]), "glow|npc|p.vpcf|0:PATTACH_POINT_FOLLOW:attach_weapon_l||");
    }
}
