//! Taunts. A taunt item is only an activity modifier (`ACT_DOTA_TAUNT` +
//! `loser`); the game server plays it when the inventory has the taunt, so
//! nothing is swapped in files. changer.dll plays it on your own hero instead,
//! in your client only (F8 in a match): it needs the activity and the modifier,
//! and as a fallback the sequence's index in the hero model (its `ASEQ` block,
//! read with Source2Viewer-CLI). Written to the swap table as
//! `taunt|npc_dota_hero_x|sequence#index;…@modifier@ACTIVITY`.

use std::collections::HashMap;
use std::path::Path;

use crate::visuals::{HeroItem, Table};

/// A sequence of a model: name, index in the model, activity entries (activity, modifiers).
type Sequences = Vec<(String, usize, Vec<String>)>;

/// `taunt|…` lines for the preset's taunts.
pub fn config_lines(
    table: &Table,
    items: &[HeroItem],
    npcs: &HashMap<String, String>,
    units: &HashMap<String, String>,
    swaps: &HashMap<String, String>,
    cli: &Path,
    pak: &Path,
) -> Vec<String> {
    // (npc, model, activity, modifier) of every taunt in the preset
    let mut wanted: Vec<(String, String, String, String)> = Vec::new();
    for h in items.iter().filter(|h| h.slot.eq_ignore_ascii_case("taunt")) {
        let Some((activity, modifier)) = taunt_activity(&table.modifiers(&h.item).unwrap_or_default()) else {
            continue;
        };
        for hero in &h.heroes {
            let Some(npc) = npcs.get(&crate::schema::norm(hero)) else { continue };
            let Some(model) = units.get(npc) else { continue };
            // an arcana swapping the hero model: its sequences are the ones in memory
            let compiled = crate::extract::compiled_path(model).to_ascii_lowercase();
            let model = swaps.get(&compiled).map(|s| s.trim_end_matches("_c").to_string()).unwrap_or_else(|| model.clone());
            wanted.push((npc.clone(), model, activity.clone(), modifier.clone()));
        }
    }
    if wanted.is_empty() {
        return Vec::new();
    }
    let models: Vec<String> = wanted.iter().map(|w| w.1.clone()).collect();
    let sequences = sequences_cached(cli, pak, &models);
    let mut lines = Vec::new();
    for (npc, model, activity, modifier) in wanted {
        let Some(seqs) = sequences.get(&model.to_ascii_lowercase()) else { continue };
        // the DLL asks the game for the sequence of activity + modifier; the
        // index is the fallback (and none found here still leaves the game's choice)
        let names = pick(seqs, &activity, &modifier);
        let names = if names.is_empty() { "-#-1".to_string() } else { names.join(";") };
        lines.push(format!("taunt|{npc}|{names}@{modifier}@{activity}"));
    }
    lines
}

/// The taunt's activity and modifier. Nearly every taunt is `ACT_DOTA_TAUNT` +
/// one modifier; a few spell it in lower case, use `ACT_DOTA_KILLTAUNT`, or `ALL`
/// (the modifier on every activity: played as ACT_DOTA_TAUNT). With two
/// activity modifiers the taunt's own comes first.
fn taunt_activity(mods: &[crate::visuals::Modifier]) -> Option<(String, String)> {
    let acts: Vec<(String, String)> = mods
        .iter()
        .filter(|m| m.kind == "activity")
        .filter_map(|m| Some((m.asset.as_deref()?.trim().to_ascii_uppercase(), m.modifier.as_deref()?.trim().to_string())))
        .filter(|(_, modifier)| !modifier.is_empty())
        .collect();
    let rank = |a: &str| match a {
        "ACT_DOTA_TAUNT" => 0,
        a if a.contains("TAUNT") => 1,
        "ALL" => 2,
        _ => 3,
    };
    let (activity, modifier) = acts.into_iter().min_by_key(|(a, _)| rank(a))?;
    match activity.as_str() {
        "ALL" => Some(("ACT_DOTA_TAUNT".into(), modifier)),
        a if a.contains("TAUNT") => Some((activity, modifier)),
        _ => None,
    }
}

/// Sequences played for `activity` with `modifier` (the taunt's own animation),
/// as `name#index`.
fn pick(seqs: &Sequences, activity: &str, modifier: &str) -> Vec<String> {
    seqs.iter()
        .filter(|(_, _, acts)| {
            acts.iter().any(|a| a.eq_ignore_ascii_case(activity)) && acts.iter().any(|a| a.eq_ignore_ascii_case(modifier))
        })
        .map(|(name, index, _)| format!("{name}#{index}"))
        .collect()
}

/// Taunt sequences of `models`, remembered in `cache\taunt_sequences.txt`
/// (reset when the game's pak changes).
fn sequences_cached(cli: &Path, pak: &Path, models: &[String]) -> HashMap<String, Sequences> {
    let path = crate::config::app_dir().join("cache").join("taunt_sequences.txt");
    let stamp = std::fs::metadata(pak).map(|m| format!("v3|{}|{:?}", m.len(), m.modified().ok())).unwrap_or_default();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines = text.lines();
    // model TAB sequence TAB activity,activity TAB index  (a model with no taunts: model TAB TAB)
    let mut cache: HashMap<String, Sequences> = HashMap::new();
    if lines.next() == Some(stamp.as_str()) {
        for l in lines {
            let mut f = l.split('\t');
            let (Some(model), Some(seq), Some(acts)) = (f.next(), f.next(), f.next()) else { continue };
            let index = f.next().and_then(|i| i.parse().ok()).unwrap_or(0);
            let entry = cache.entry(model.to_string()).or_default();
            if !seq.is_empty() {
                entry.push((seq.to_string(), index, acts.split(',').map(String::from).collect()));
            }
        }
    }
    let mut missing: Vec<String> =
        models.iter().map(|m| m.to_ascii_lowercase()).filter(|m| !cache.contains_key(m)).collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        let Some(found) = read_sequences(cli, pak, &missing) else { return cache };
        for m in &missing {
            cache.insert(m.clone(), found.get(m).cloned().unwrap_or_default());
        }
        let mut out = format!("{stamp}\n");
        for (model, seqs) in &cache {
            if seqs.is_empty() {
                out.push_str(&format!("{model}\t\t\n"));
            }
            for (name, index, acts) in seqs {
                out.push_str(&format!("{model}\t{name}\t{}\t{index}\n", acts.join(",")));
            }
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(&path, out);
    }
    cache
}

/// Taunt sequences of the models from one Source2Viewer-CLI run. None if it could not run.
fn read_sequences(cli: &Path, pak: &Path, models: &[String]) -> Option<HashMap<String, Sequences>> {
    use std::os::windows::process::CommandExt;
    let files: Vec<String> = models.iter().map(|m| crate::extract::compiled_path(m)).collect();
    let out = std::process::Command::new(cli)
        .arg("-i")
        .arg(pak)
        .arg("-f")
        .arg(files.join(","))
        .args(["-b", "ASEQ"])
        .creation_flags(0x0800_0000)
        .output()
        .ok()?;
    Some(parse(&String::from_utf8_lossy(&out.stdout)))
}

/// `[i/n] model_c` headers, then the sequence descriptions (`m_localS1SeqDescArray`):
/// `m_sName = "seq"` (its position is the sequence index) and its `m_name = "…"`
/// activity entries.
fn parse(text: &str) -> HashMap<String, Sequences> {
    let mut out: HashMap<String, Sequences> = HashMap::new();
    let mut model: Option<String> = None;
    let mut seq: Option<(String, usize, Vec<String>)> = None;
    let mut in_descs = false;
    let mut next_index = 0usize;
    let flush = |model: &Option<String>, seq: &mut Option<(String, usize, Vec<String>)>, out: &mut HashMap<String, Sequences>| {
        if let (Some(m), Some(s)) = (model, seq.take()) {
            if s.2.iter().any(|a| a.to_ascii_uppercase().contains("TAUNT")) {
                out.entry(m.clone()).or_default().push(s);
            }
        }
    };
    let quoted = |l: &str| l.split('"').nth(1).map(String::from);
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.contains("] ") && t.ends_with("_c") {
            flush(&model, &mut seq, &mut out);
            model = t.split_once("] ").map(|(_, p)| p.trim_end_matches("_c").to_ascii_lowercase());
            if let Some(m) = &model {
                out.entry(m.clone()).or_default();
            }
            in_descs = false;
        } else if t.starts_with("m_localS1SeqDescArray") {
            flush(&model, &mut seq, &mut out);
            in_descs = true;
            next_index = 0;
        } else if in_descs && t.starts_with("m_sName = ") {
            flush(&model, &mut seq, &mut out);
            seq = quoted(t).map(|n| (n, next_index, Vec::new()));
            next_index += 1;
        } else if t.starts_with("m_name = ") {
            if let (Some(s), Some(n)) = (seq.as_mut(), quoted(t)) {
                s.2.push(n);
            }
        }
    }
    flush(&model, &mut seq, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = r#"[1/1] models/heroes/juggernaut/juggernaut.vmdl_c
	m_sName = "!embedded_sequence_data!models/heroes/juggernaut/juggernaut.vmdl"
	m_localS1SeqDescArray =
	[
		{
			m_sName = "taunt_face_me"
			m_activityArray =
			[
				{
					m_name = "ACT_DOTA_TAUNT"
				},
			]
		},
		{
			m_sName = "taunt_loser_anim"
			m_activityArray =
			[
				{
					m_name = "ACT_DOTA_TAUNT"
				},
				{
					m_name = "loser"
				},
			]
		},
		{
			m_sName = "run"
			m_activityArray =
			[
				{
					m_name = "ACT_DOTA_RUN"
				},
			]
		},
"#;

    fn act(asset: &str, modifier: &str) -> crate::visuals::Modifier {
        crate::visuals::Modifier { kind: "activity".into(), asset: Some(asset.into()), modifier: Some(modifier.into()), style: None, level: None }
    }

    #[test]
    fn taunt_activity_variants() {
        assert_eq!(taunt_activity(&[act("ACT_DOTA_TAUNT", "loser")]), Some(("ACT_DOTA_TAUNT".into(), "loser".into())));
        assert_eq!(taunt_activity(&[act("act_dota_taunt", "x")]), Some(("ACT_DOTA_TAUNT".into(), "x".into())));
        assert_eq!(taunt_activity(&[act("ACT_DOTA_KILLTAUNT", "k")]), Some(("ACT_DOTA_KILLTAUNT".into(), "k".into())));
        assert_eq!(taunt_activity(&[act("ALL", "a")]), Some(("ACT_DOTA_TAUNT".into(), "a".into())));
        assert_eq!(
            taunt_activity(&[act("ACT_DOTA_RUN", "r"), act("ACT_DOTA_TAUNT", "t")]),
            Some(("ACT_DOTA_TAUNT".into(), "t".into()))
        );
        assert_eq!(taunt_activity(&[act("ACT_DOTA_RUN", "r")]), None);
    }

    #[test]
    fn taunt_sequence_by_modifier() {
        let seqs = parse(OUT);
        let jugg = &seqs["models/heroes/juggernaut/juggernaut.vmdl"];
        assert_eq!(jugg.len(), 2, "only taunt sequences are kept");
        assert_eq!(pick(jugg, "ACT_DOTA_TAUNT", "loser"), vec!["taunt_loser_anim#1".to_string()]);
        assert!(pick(jugg, "ACT_DOTA_TAUNT", "guitar").is_empty());
    }
}
