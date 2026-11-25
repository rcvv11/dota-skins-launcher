//! Effects that never end by themselves.
//!
//! The game lets some effects go as soon as it makes them, counting on them to end
//! on their own: Caustic Finale's burst on a hit (`sandking_caustic_finale_debuff`,
//! emitting for 0.1 s). An item's effect in its place that never ends by itself
//! (The Barren Vector of the Crimson Witness's debuff, which keeps dripping until the
//! game stops it) then stays on the unit for good: nothing stops it. So a swap of an
//! effect that ends by itself for one that does not is left out. Effects the game
//! stops itself (a modifier's own effect, Caustic Finale's `orb_of_venom`) never end
//! by themselves either way, and their swaps stay.
//!
//! An effect never ends by itself when it, or one of its children, emits without end:
//! a continuous emitter without an emission duration, or a maintain emitter. An effect
//! surely ends by itself when something in it emits, and everything that emits does so
//! for a while only, its particles decay, and nothing waits for the game to stop it (an
//! end cap) or starts it again. An empty effect (a hero's blank placeholder an item
//! fills, Windranger's focus fire) is neither: the game stops it itself.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::extract::Req;
use crate::skins::Groups;

/// From Source2Viewer-CLI's `-b DATA` output: per effect, its children and its own
/// flags (`[children, [endless, emits, ends]]`, each "1" or "0"): it emits without end,
/// it emits at all, what it emits ends by itself.
pub fn parse_effects(output: &str) -> Groups {
    let mut out = Groups::new();
    let mut current: Option<(String, Vec<String>, String)> = None;
    let finish = |c: Option<(String, Vec<String>, String)>, out: &mut Groups| {
        if let Some((name, children, body)) = c {
            let flag = |b: bool| if b { "1" } else { "0" }.to_string();
            let flags = vec![flag(emits_without_end(&body)), flag(emits(&body)), flag(ends_by_itself(&body))];
            out.insert(name, vec![children, flags]);
        }
    };
    for line in output.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.contains("] ") && t.ends_with("_c") {
            finish(current.take(), &mut out);
            let path = t.split_once("] ").map(|(_, p)| p).unwrap_or("");
            current = Some((path.trim_end_matches("_c").to_ascii_lowercase(), Vec::new(), String::new()));
            continue;
        }
        let Some((_, children, body)) = current.as_mut() else { continue };
        if let Some(rest) = t.strip_prefix("m_ChildRef = resource:\"") {
            if let Some(child) = rest.split('"').next().filter(|c| c.ends_with(".vpcf")) {
                children.push(child.to_ascii_lowercase());
            }
        }
        body.push_str(t);
        body.push('\n');
    }
    finish(current.take(), &mut out);
    out
}

/// Whether one effect's own operators emit without end.
fn emits_without_end(body: &str) -> bool {
    if body.contains("_class = \"C_OP_MaintainEmitter\"") {
        return true;
    }
    // each continuous emitter: up to the next operator
    let mut rest = body;
    while let Some(at) = rest.find("_class = \"C_OP_ContinuousEmitter\"") {
        let after = &rest[at + 1..];
        let block = &after[..after.find("_class = ").unwrap_or(after.len())];
        if !has_duration(block) {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether one effect emits particles at all.
fn emits(body: &str) -> bool {
    body.lines().any(|l| {
        let l = l.trim();
        l.starts_with("_class = \"C_OP_") && l.ends_with("Emitter\"")
    })
}

/// Whether what one effect emits ends by itself: every emitter for a while only, its
/// particles decay, no end cap waits for the game, nothing starts it again.
fn ends_by_itself(body: &str) -> bool {
    if !emits(body) {
        return true;
    }
    !emits_without_end(body)
        && body.contains("_class = \"C_OP_Decay\"")
        && !body.contains("_class = \"C_OP_EndCap")
        && !body.contains("_class = \"C_OP_Restart")
        && !body.lines().any(|l| l.trim().strip_prefix("m_flMaxRecreationTime = ").and_then(|v| v.trim().parse::<f32>().ok()).is_some_and(|v| v != 0.0))
}

/// A positive `m_flEmissionDuration` (a value that is not a plain number counts as one).
fn has_duration(block: &str) -> bool {
    let Some(at) = block.find("m_flEmissionDuration") else { return false };
    let tail = &block[at..];
    let value = tail
        .lines()
        .take(6)
        .find_map(|l| l.trim().strip_prefix("m_flLiteralValue = "))
        .and_then(|v| v.trim().parse::<f32>().ok());
    let literal = tail.lines().take(4).any(|l| l.contains("PF_TYPE_LITERAL"));
    !literal || value.is_some_and(|v| v > 0.0)
}

/// How an effect lives, its children counted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Life {
    /// Something in it emits without end.
    pub endless: bool,
    /// Something in it emits, and all of it ends by itself.
    pub ends: bool,
}

/// How effects (`.vpcf`) live, their children read too. An effect not found counts as
/// neither endless nor surely ending.
pub fn lives(cli: &Path, pak: &Path, effects: &[String]) -> HashMap<String, Life> {
    // per effect: its own flags (endless, emits, ends) and its children
    let mut info: HashMap<String, ([bool; 3], Vec<String>)> = HashMap::new();
    let mut known: HashSet<String> = HashSet::new();
    let mut want: Vec<String> = effects.iter().map(|e| e.trim().to_ascii_lowercase()).collect();
    for _ in 0..8 {
        let mut missing: Vec<String> = want.iter().filter(|e| !info.contains_key(*e)).cloned().collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            break;
        }
        let got = crate::skins::data_cached(cli, pak, &missing, "effect_life2.txt", parse_effects);
        let mut next = Vec::new();
        for e in missing {
            let entry = got.get(&e).map(|g| {
                let f = g.get(1).cloned().unwrap_or_default();
                let on = |i: usize| f.get(i).map(String::as_str) == Some("1");
                ([on(0), on(1), on(2)], g.first().cloned().unwrap_or_default())
            });
            if entry.is_some() {
                known.insert(e.clone());
            }
            let (flags, children) = entry.unwrap_or(([false, false, false], Vec::new()));
            next.extend(children.iter().cloned());
            info.insert(e, (flags, children));
        }
        want = next;
    }
    // over the tree: something emits without end; something emits; all of it known and ending
    fn walk(e: &str, info: &HashMap<String, ([bool; 3], Vec<String>)>, known: &HashSet<String>, seen: &mut HashSet<String>) -> (bool, bool, bool) {
        if !seen.insert(e.to_string()) {
            return (false, false, true);
        }
        let Some((f, children)) = info.get(e) else { return (false, false, false) };
        let mut acc = (f[0], f[1], known.contains(e) && f[2]);
        for c in children {
            let (endless, emits, ends) = walk(c, info, known, seen);
            acc = (acc.0 || endless, acc.1 || emits, acc.2 && ends);
        }
        acc
    }
    effects
        .iter()
        .map(|e| {
            let key = e.trim().to_ascii_lowercase();
            let (endless, emits, ends) = walk(&key, &info, &known, &mut HashSet::new());
            (key, Life { endless, ends: emits && ends && !endless })
        })
        .collect()
}

/// Drops the effect swaps that would put an effect which never ends by itself in
/// place of one that does. Returns the swaps left out (for the log).
pub fn drop_endless_swaps(cli: &Path, pak: &Path, reqs: &mut Vec<Req>) -> Vec<Req> {
    let plain = |p: &str| p.strip_suffix("_c").unwrap_or(p).to_ascii_lowercase();
    let effects: Vec<String> = reqs
        .iter()
        .filter(|r| r.src.ends_with(".vpcf_c") && r.dst.ends_with(".vpcf_c"))
        .flat_map(|r| [plain(&r.src), plain(&r.dst)])
        .collect();
    if effects.is_empty() {
        return Vec::new();
    }
    let life = lives(cli, pak, &effects);
    let unsafe_swap = |r: &Req| {
        r.src.ends_with(".vpcf_c")
            && r.dst.ends_with(".vpcf_c")
            && life.get(&plain(&r.src)).is_some_and(|l| l.endless)
            && life.get(&plain(&r.dst)).is_some_and(|l| l.ends)
    };
    let dropped: Vec<Req> = reqs.iter().filter(|r| unsafe_swap(r)).cloned().collect();
    reqs.retain(|r| !unsafe_swap(r));
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"[1/3] particles/units/heroes/hero_sandking/sandking_caustic_finale_debuff.vpcf_c
--- Data for block "DATA" ---
{
	m_Operators =
	[
		{
			_class = "C_OP_Decay"
		},
	]
	m_Emitters =
	[
		{
			_class = "C_OP_ContinuousEmitter"
			m_flEmissionDuration =
			{
				m_nType = "PF_TYPE_LITERAL"
				m_flLiteralValue = 0.1
			}
			m_flEmitRate =
			{
				m_nType = "PF_TYPE_LITERAL"
				m_flLiteralValue = 400.0
			}
		},
	]
	m_Children =
	[
		{
			m_ChildRef = resource:"particles/units/heroes/hero_sandking/sandking_caustic_finale_debuff_glow.vpcf"
		},
	]
}
[2/3] particles/econ/items/sand_king/crimson_debuff.vpcf_c
{
	m_nMaxParticles = 0
	m_Children =
	[
		{
			m_ChildRef = resource:"particles/econ/items/sand_king/crimson_debuff_drip.vpcf"
		},
	]
}
[3/3] particles/econ/items/sand_king/crimson_debuff_drip.vpcf_c
{
	m_Emitters =
	[
		{
			_class = "C_OP_ContinuousEmitter"
			m_flEmitRate =
			{
				m_nType = "PF_TYPE_LITERAL"
				m_flLiteralValue = 40.0
			}
		},
	]
}
"#;

    #[test]
    fn emitting_without_end_is_seen_through_children() {
        let g = parse_effects(DUMP);
        let base = &g["particles/units/heroes/hero_sandking/sandking_caustic_finale_debuff.vpcf"];
        assert_eq!(base[0], vec!["particles/units/heroes/hero_sandking/sandking_caustic_finale_debuff_glow.vpcf"]);
        // flags: endless, emits, ends
        assert_eq!(base[1], vec!["0", "1", "1"]);
        assert_eq!(g["particles/econ/items/sand_king/crimson_debuff.vpcf"][1], vec!["0", "0", "1"]);
        // no decay of its own: not surely ending either
        assert_eq!(g["particles/econ/items/sand_king/crimson_debuff_drip.vpcf"][1], vec!["1", "1", "0"]);
        // an empty placeholder (Windranger's focus fire) emits nothing
        assert!(!emits("_class = \"CParticleSystemDefinition\"\nm_nMaxParticles = 0\n"));
        // an end cap waits for the game to stop it
        assert!(!ends_by_itself("_class = \"C_OP_InstantaneousEmitter\"\n_class = \"C_OP_Decay\"\n_class = \"C_OP_EndCapTimedDecay\"\n"));
        assert!(ends_by_itself("_class = \"C_OP_InstantaneousEmitter\"\n_class = \"C_OP_Decay\"\n"));
        // a duration that is not a plain number counts as an end
        assert!(has_duration("m_flEmissionDuration =\n{\nm_nType = \"PF_TYPE_RANDOM_UNIFORM\"\n}\n"));
        assert!(!has_duration("m_flEmissionDuration =\n{\nm_nType = \"PF_TYPE_LITERAL\"\nm_flLiteralValue = 0.0\n}\n"));
        assert!(emits_without_end("_class = \"C_OP_MaintainEmitter\"\n"));
    }
}
