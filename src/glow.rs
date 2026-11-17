//! Item glows no file swap can place (the slot's default item creates no
//! particle to stand in for): the DLL creates them on the hero in a match,
//! from Dota's own UI script (Panorama `Particles` API). How each one hangs
//! on the hero comes from its particle file: the control point drivers of its
//! first configuration (CP, attach type, attachment) and the CP it takes the
//! model snapshot from.

use std::path::Path;

/// One control point of a glow: `(cp, attach type, attachment)`.
pub type Driver = (u32, String, String);

/// Drivers of each particle, from the cache (`cache/glow_drivers.txt`) or read
/// with Source2Viewer-CLI and added to it: a pick should not wait for the CLI
/// on every glow again.
pub fn drivers_cached(cli: &Path, pak: &Path, particles: &[&str]) -> std::collections::HashMap<String, Vec<Driver>> {
    // one writer at a time (a pick and the background warm-up)
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = crate::config::app_dir().join("cache").join("glow_drivers.txt");
    let mut cache: std::collections::HashMap<String, Vec<Driver>> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let (p, d) = l.split_once('\t')?;
            Some((p.to_string(), parse_line(d)))
        })
        .collect();
    let mut missing: Vec<&str> = particles.iter().copied().filter(|p| !cache.contains_key(*p)).collect();
    missing.sort();
    missing.dedup();
    let mut changed = false;
    // a CLI start costs ~0.4 s: all missing particles in one call (in chunks:
    // the command line has a length limit)
    for chunk in missing.chunks(150) {
        if let Some(found) = drivers_many(cli, pak, chunk) {
            cache.extend(found);
            changed = true;
        }
    }
    if changed {
        let mut text = String::new();
        for (p, d) in &cache {
            text.push_str(&format!("{p}\t{}\n", drivers_text(d)));
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(&path, text);
    }
    cache
}

/// Drivers of several particles from one Source2Viewer-CLI run (its output has
/// a `[i/n] path_c` header before each file). None if the CLI could not run.
fn drivers_many(cli: &Path, pak: &Path, particles: &[&str]) -> Option<std::collections::HashMap<String, Vec<Driver>>> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let files: Vec<String> = particles.iter().map(|p| format!("{}_c", p.trim_end_matches("_c"))).collect();
    let out = std::process::Command::new(cli)
        .arg("-i")
        .arg(pak)
        .arg("-f")
        .arg(files.join(","))
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
            current = Some(path.trim_end_matches("_c").to_ascii_lowercase());
            continue;
        }
        if let Some(c) = &current {
            let s = sections.entry(c.clone()).or_default();
            s.push_str(line);
            s.push('\n');
        }
    }
    // a particle missing from the pak gets the default (as one CLI call per file did)
    Some(
        particles
            .iter()
            .map(|p| {
                let body = sections.get(&p.trim_end_matches("_c").to_ascii_lowercase()).map(String::as_str).unwrap_or("");
                (p.to_string(), parse(body))
            })
            .collect(),
    )
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
pub fn parse(text: &str) -> Vec<Driver> {
    let mut out: Vec<Driver> = Vec::new();
    if let Some(start) = text.find("m_controlPointConfigurations") {
        let rest = &text[start..];
        // the first configuration's drivers only
        if let Some(d) = rest.find("m_drivers") {
            let body = &rest[d..];
            let end = matching_close(body).unwrap_or(body.len());
            let body = &body[..end];
            for entry in body.split('}').filter(|e| e.contains("m_iAttachType")) {
                let cp = field(entry, "m_iControlPoint").and_then(|v| v.parse().ok()).unwrap_or(out.len() as u32);
                let kind = field(entry, "m_iAttachType").unwrap_or_else(|| "PATTACH_ABSORIGIN_FOLLOW".into());
                let attachment = field(entry, "m_attachmentName").unwrap_or_default();
                out.push((cp, kind, attachment));
            }
        }
    }
    if let Some(cp) = field(text, "m_nSnapshotControlPoint").and_then(|v| v.parse::<u32>().ok()) {
        if !out.iter().any(|d| d.0 == cp) {
            out.push((cp, "PATTACH_ABSORIGIN_FOLLOW".into(), String::new()));
        }
    }
    if out.is_empty() {
        out.push((0, "PATTACH_ABSORIGIN_FOLLOW".into(), String::new()));
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

/// Config line for the DLL: `glow|npc|particle|cp:type:attachment;...`.
pub fn config_line(npc: &str, particle: &str, drivers: &[Driver]) -> String {
    format!("glow|{npc}|{particle}|{}", drivers_text(drivers))
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
            config_line("npc_dota_hero_alchemist", "particles/a.vpcf", &d),
            "glow|npc_dota_hero_alchemist|particles/a.vpcf|0:PATTACH_POINT_FOLLOW:attach_weapon_l;7:PATTACH_ABSORIGIN_FOLLOW:"
        );
    }
}

/// Every catalog item against the real game: which glows are left to the DLL,
/// and whether the hero model has the attachments they hang on.
/// `cargo test glow_coverage -- --ignored --nocapture`
#[cfg(test)]
mod coverage {
    use std::collections::{BTreeMap, HashMap, HashSet};

    #[test]
    #[ignore]
    fn glow_coverage() {
        let exe = r"C:\Program Files (x86)\Steam\steamapps\common\dota 2 beta\game\bin\win64\dota2.exe";
        let game = crate::vpk::game_dir_from_exe(exe).unwrap();
        let pak = game.join("pak01_dir.vpk");
        let cli = std::path::PathBuf::from(r"tools\vrf-cli\Source2Viewer-CLI.exe");
        let text = crate::vpk::read_file(&game, crate::schema::SCHEMA_PATH).unwrap().unwrap();
        let table = crate::visuals::Table::new(&text);
        let npcs = crate::schema::hero_npcs(&text);
        let units = crate::visuals::all_unit_models(&game).unwrap();
        let catalog = crate::catalog::Catalog::load(std::path::Path::new("dist/assets")).unwrap();
        let chosen: Vec<&crate::catalog::Skin> = catalog.skins.iter().filter(|s| !s.is_general()).collect();
        let items = crate::visuals::hero_items(&chosen);
        let mut glows: Vec<(String, String, String)> = Vec::new(); // item, npc, particle
        for h in &items {
            let (_, g) = crate::visuals::hero_overrides_and_glows(&table, std::slice::from_ref(h), &HashMap::new(), &npcs);
            for x in g {
                glows.push((h.item.name.clone(), x.npc, x.particle));
            }
        }
        let with: HashSet<&str> = glows.iter().map(|g| g.0.as_str()).collect();
        println!("hero items {}, with DLL glows {}, glows {}", items.len(), with.len(), glows.len());
        let particles: Vec<&str> = glows.iter().map(|g| g.2.as_str()).collect::<HashSet<_>>().into_iter().collect();
        let drivers = super::drivers_cached(&cli, &pak, &particles);
        // hero model attachments
        let mut attach: HashMap<String, HashSet<String>> = HashMap::new();
        let heroes: HashSet<&str> = glows.iter().map(|g| g.1.as_str()).collect();
        for npc in heroes {
            let Some(model) = units.get(npc) else { continue };
            let out = std::process::Command::new(&cli)
                .arg("-i").arg(&pak).arg("-f").arg(format!("{}_c", model)).args(["-b", "MDAT"])
                .output().unwrap();
            let t = String::from_utf8_lossy(&out.stdout).to_string();
            let set: HashSet<String> = t
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .filter(|w| w.starts_with("attach_"))
                .map(str::to_string)
                .collect();
            attach.insert(npc.to_string(), set);
        }
        let (mut ok, mut missing, mut none) = (0, 0, 0);
        let mut missing_items: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (item, npc, p) in &glows {
            let d = drivers.get(p).cloned().unwrap_or_default();
            let need: Vec<&str> = d.iter().map(|x| x.2.as_str()).filter(|a| !a.is_empty()).collect();
            if need.is_empty() {
                none += 1;
                continue;
            }
            let have = attach.get(npc).cloned().unwrap_or_default();
            let lack: Vec<&str> = need.iter().copied().filter(|a| !have.contains(*a)).collect();
            if lack.is_empty() {
                ok += 1;
            } else {
                missing += 1;
                missing_items.entry(item.clone()).or_default().push(format!("{} [{}]", p.rsplit('/').next().unwrap(), lack.join(",")));
            }
        }
        println!("attachments on hero model: ok {ok}, missing {missing}, no attachment (origin) {none}");
        let filter = std::env::var("GLOW_HERO").unwrap_or_default();
        for (i, v) in missing_items.iter().filter(|(i, _)| filter.is_empty() || glows.iter().any(|g| &g.0 == *i && g.1.contains(&filter))).take(40) {
            println!("  {i}: {}", v.join("; "));
        }
    }
}

#[cfg(test)]
mod batch_real {
    /// The batch read gives what the per-file reads cached before.
    #[test]
    #[ignore]
    fn batch_matches_cache() {
        let cli = std::path::Path::new("tools/vrf-cli/Source2Viewer-CLI.exe");
        let pak = std::path::Path::new(r"C:\Program Files (x86)\Steam\steamapps\common\dota 2 beta\game\dota\pak01_dir.vpk");
        let text = std::fs::read_to_string(crate::config::app_dir().join("cache").join("glow_drivers.txt")).unwrap();
        let cached: Vec<(String, Vec<super::Driver>)> = text
            .lines()
            .take(120)
            .filter_map(|l| l.split_once('\t').map(|(p, d)| (p.to_string(), super::parse_line(d))))
            .collect();
        let names: Vec<&str> = cached.iter().map(|(p, _)| p.as_str()).collect();
        let t = std::time::Instant::now();
        let got = super::drivers_many(cli, pak, &names).unwrap();
        println!("{} particles in {} ms", names.len(), t.elapsed().as_millis());
        let mut bad = 0;
        for (p, d) in &cached {
            if got.get(p) != Some(d) {
                bad += 1;
                println!("differs: {p}: cached {d:?} batch {:?}", got.get(p));
            }
        }
        assert_eq!(bad, 0);
    }
}
