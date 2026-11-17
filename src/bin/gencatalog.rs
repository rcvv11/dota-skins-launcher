//! Catalog generator.
//!
//! Builds `assets/catalog.json` from Valve's public item schema
//! (`items_game.txt`), resolving localized item names from `dota_english.txt`
//! and hero display names from dotaconstants.
//!
//! Usage:
//!   gencatalog --out assets --rarities immortal,arcana --previews hero
//!   gencatalog --out assets --previews market --market-limit 150

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

const ITEMS_URL: &str =
    "https://raw.githubusercontent.com/dotabuff/d2vpkr/master/dota/scripts/items/items_game.txt";
const LANG_URL: &str = "https://raw.githubusercontent.com/SteamDatabase/GameTracking-Dota2/master/game/dota/pak01_dir/resource/localization/dota_english.txt";
const HEROES_URL: &str =
    "https://raw.githubusercontent.com/odota/dotaconstants/master/build/heroes.json";
const HERO_IMG: &str = "https://cdn.cloudflare.steamstatic.com/apps/dota2/images/dota_react/heroes/{}.png";
const MARKET_ICON: &str = "https://community.cloudflare.steamstatic.com/economy/image/{}/330x192";
const MARKET_SEARCH: &str =
    "https://steamcommunity.com/market/search/render/?query={}&appid=570&norender=1&count=10&start=0";

#[derive(Serialize)]
struct OutCatalog {
    skins: Vec<OutSkin>,
    /// Hero display name -> base models and per-slot default wearables (3D preview).
    hero_models: BTreeMap<String, OutHero>,
}

#[derive(Serialize, Default)]
struct OutHero {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    persona_model: Option<String>,
    /// slot -> model of the hero's default item in that slot
    defaults: BTreeMap<String, String>,
}

const NPC_URL: &str = "https://raw.githubusercontent.com/dotabuff/d2vpkr/master/dota/scripts/npc/";

#[derive(Serialize)]
struct OutSkin {
    id: String,
    name: String,
    hero: String,
    heroes: Vec<String>,
    slot: String,
    rarity: String,
    preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// [original model, replacement] pairs this item applies when worn.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    swaps: Vec<[String; 2]>,
    /// Item definition index from the schema (higher = newer).
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<u32>,
    files: Vec<serde_json::Value>,
}

#[derive(Clone, Copy, PartialEq)]
enum PreviewMode {
    Hero,
    Market,
    None,
}

struct Args {
    out: PathBuf,
    cache: PathBuf,
    refresh: bool,
    rarities: Vec<String>,
    limit: usize,
    previews: PreviewMode,
    market_limit: usize,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Args {
            out: PathBuf::from("assets"),
            cache: default_cache_dir(),
            refresh: false,
            // empty = every rarity
            rarities: Vec::new(),
            limit: 0,
            previews: PreviewMode::Hero,
            market_limit: 150,
        };

        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--out" => args.out = PathBuf::from(it.next().ok_or("--out needs a value")?),
                "--cache" => args.cache = PathBuf::from(it.next().ok_or("--cache needs a value")?),
                "--refresh" => args.refresh = true,
                "--rarities" => {
                    let raw = it.next().ok_or("--rarities needs a value")?;
                    args.rarities = raw
                        .split(',')
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty() && s != "all")
                        .collect();
                }
                "--limit" => {
                    args.limit = it
                        .next()
                        .ok_or("--limit needs a value")?
                        .parse()
                        .map_err(|_| "--limit must be a number")?
                }
                "--previews" => {
                    args.previews = match it.next().ok_or("--previews needs a value")?.as_str() {
                        "hero" => PreviewMode::Hero,
                        "market" => PreviewMode::Market,
                        "none" => PreviewMode::None,
                        other => return Err(format!("unknown --previews value: {other}")),
                    }
                }
                "--market-limit" => {
                    args.market_limit = it
                        .next()
                        .ok_or("--market-limit needs a value")?
                        .parse()
                        .map_err(|_| "--market-limit must be a number")?
                }
                "-h" | "--help" => {
                    println!(
                        "gencatalog [--out DIR] [--cache DIR] [--refresh]\n\
                         \x20          [--rarities all|immortal,arcana,...] [--limit N]\n\
                         \x20          [--previews hero|market|none] [--market-limit N]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown flag: {other}")),
            }
        }
        Ok(args)
    }
}

fn default_cache_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("dota-skins")
        .join("cache")
}

fn main() {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), String> {
    std::fs::create_dir_all(&args.out).map_err(|e| format!("cannot create output dir: {e}"))?;
    std::fs::create_dir_all(&args.cache).map_err(|e| format!("cannot create cache dir: {e}"))?;

    println!("=== schema ===");
    let items_text = fetch_text(ITEMS_URL, &args.cache.join("items_game.txt"), args.refresh)?;
    let drafts = parse_items(&items_text);
    println!("parsed item blocks: {}", drafts.len());
    let with_name = drafts.iter().filter(|d| d.name.is_some()).count();
    let with_heroes = drafts.iter().filter(|d| !d.hero_list().is_empty()).count();
    let with_slot = drafts.iter().filter(|d| d.item_slot.is_some()).count();
    let with_rarity = drafts.iter().filter(|d| d.item_rarity.is_some()).count();
    println!(
        "  fields: name={with_name} used_by_heroes={with_heroes} item_slot={with_slot} item_rarity={with_rarity}"
    );
    if let Some(sample) = drafts.iter().find(|d| d.item_rarity.is_some()) {
        println!("  sample: {:?}", sample);
    }

    println!("=== localization ===");
    let lang_text = fetch_text(LANG_URL, &args.cache.join("dota_english.txt"), args.refresh)?;
    let tokens = parse_tokens(&lang_text);
    println!("localization tokens: {}", tokens.len());

    println!("=== heroes ===");
    let heroes_text = fetch_text(HEROES_URL, &args.cache.join("heroes.json"), args.refresh)?;
    let (hero_names, hero_slugs) = parse_heroes(&heroes_text)?;
    println!("heroes: {}", hero_names.len());

    println!("=== hero models ===");
    let hero_models = collect_hero_models(args, &hero_names, &drafts)?;
    println!(
        "hero models: {} (with persona: {})",
        hero_models.len(),
        hero_models.values().filter(|h| h.persona_model.is_some()).count()
    );

    // --- filter ---
    let mut skins: Vec<OutSkin> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();

    for draft in &drafts {
        if !is_cosmetic(draft, &args.rarities) {
            continue;
        }
        let internal = draft.name.as_deref().unwrap_or("");
        let raw_name = draft.item_name.as_deref().unwrap_or("");
        let display = localize(&tokens, raw_name).unwrap_or_else(|| prettify(internal));
        if display.is_empty() {
            continue;
        }

        let id = if internal.is_empty() {
            format!("item_{}", skins.len())
        } else {
            internal.to_string()
        };
        if seen.insert(id.clone(), ()).is_some() {
            continue;
        }

        let hero_list = draft.hero_list();
        let heroes = hero_display_list(&hero_names, &hero_list);
        // Map/HUD/courier items have no hero: leave the label empty.
        let hero = if heroes.is_empty() {
            String::new()
        } else {
            hero_display(&hero_names, &hero_list)
        };
        // Items without an explicit rarity are "common" in the schema.
        let rarity = draft
            .item_rarity
            .as_deref()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or("common")
            .to_lowercase();
        skins.push(OutSkin {
            id,
            name: display,
            hero,
            heroes,
            slot: draft.item_slot.clone().unwrap_or_default(),
            rarity,
            preview: None,
            icon: draft
                .image_inventory
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            model: draft
                .model_player
                .as_deref()
                .map(str::trim)
                .filter(|s| s.ends_with(".vmdl"))
                .map(str::to_string),
            swaps: draft
                .model_swaps
                .iter()
                .filter_map(|(a, m)| {
                    let from = if a.starts_with("npc_dota_hero_") {
                        let name = hero_names.get(a)?;
                        hero_models.get(name)?.model.clone()
                    } else {
                        a.clone()
                    };
                    Some([from, m.clone()])
                })
                .collect(),
            index: draft.block.as_deref().and_then(|b| b.parse().ok()),
            files: Vec::new(),
        });
    }

    // Hero items first (by hero), then general items grouped by slot.
    skins.sort_by(|a, b| {
        (a.hero.is_empty(), &a.hero, &a.slot, &a.name).cmp(&(b.hero.is_empty(), &b.hero, &b.slot, &b.name))
    });

    if args.limit > 0 && skins.len() > args.limit {
        skins.truncate(args.limit);
    }
    println!("cosmetics after filter: {}", skins.len());

    // --- previews ---
    if args.previews != PreviewMode::None && !skins.is_empty() {
        attach_previews(args, &mut skins, &hero_slugs, &drafts, &tokens)?;
    }

    let catalog = OutCatalog { skins, hero_models };
    let json = serde_json::to_string_pretty(&catalog).map_err(|e| e.to_string())?;
    let target = args.out.join("catalog.json");
    std::fs::write(&target, json).map_err(|e| format!("cannot write catalog: {e}"))?;

    println!(
        "wrote {} entries -> {}",
        catalog.skins.len(),
        target.display()
    );
    Ok(())
}

// ---------------------------------------------------------------- filtering

/// Equippable slots that are not tied to a hero (map, HUD, courier, ...).
/// Chests, tools, stickers, league passes etc. use slot "none" and stay out.
const WORLD_SLOTS: &[&str] = &[
    "terrain",
    "weather",
    "emblem",
    "courier",
    "ward",
    "hud_skin",
    "announcer",
    "mega_kills",
    "music",
    "cursor_pack",
    "loading_screen",
    "versus_screen",
    "heroic_statue",
    "pet_effigy",
    "teleport_effect",
    "streak_effect",
    "map_effect",
    "multikill_banner",
    "radiant_creeps",
    "dire_creeps",
    "radiant_siege_creeps",
    "dire_siege_creeps",
    "radiant_towers",
    "dire_towers",
    "roshan",
    "tormentor",
];

fn is_cosmetic(d: &Draft, rarities: &[String]) -> bool {
    let hero_bound = d.hero_list().starts_with("npc_dota_hero");
    // "none" is the bundle prefab's slot: sets, not wearable items.
    let slot = d.item_slot.as_deref().unwrap_or("").trim();
    if slot.is_empty() || slot == "none" {
        return false;
    }
    if !hero_bound && !WORLD_SLOTS.contains(&slot) {
        return false;
    }
    // "Default Terrain", "Default Weather": the stock look, nothing to choose.
    if !hero_bound && d.name.as_deref().is_some_and(|n| n.starts_with("Default ")) {
        return false;
    }
    if d.prefab.as_deref().is_some_and(|p| p.split_whitespace().any(|x| x == "bundle")) {
        return false;
    }
    if d.prefab.as_deref() == Some("default_item") {
        return false;
    }
    if !rarities.is_empty() {
        let rarity = d.item_rarity.as_deref().unwrap_or("").to_lowercase();
        if !rarities.iter().any(|r| r == &rarity) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------- previews

fn attach_previews(
    args: &Args,
    skins: &mut [OutSkin],
    hero_slugs: &HashMap<String, String>,
    drafts: &[Draft],
    tokens: &HashMap<String, String>,
) -> Result<(), String> {
    let preview_dir = args.out.join("previews");
    std::fs::create_dir_all(&preview_dir).map_err(|e| format!("cannot create previews dir: {e}"))?;

    // Hero portraits are the guaranteed fallback and cache well across runs.
    let mut hero_cache: HashMap<String, String> = HashMap::new();
    let mut downloaded = 0usize;
    let mut market_errors = 0usize;
    let mut market_stopped = false;

    let by_id: HashMap<&str, &Draft> = drafts
        .iter()
        .filter_map(|d| d.name.as_deref().map(|n| (n, d)))
        .collect();

    for (index, skin) in skins.iter_mut().enumerate() {
        let hero_key = by_id
            .get(skin.id.as_str())
            .map(|d| d.hero_list())
            .unwrap_or_default();
        let slug = hero_key
            .split(',')
            .map(|s| s.trim())
            .find(|s| s.starts_with("npc_dota_hero"))
            .and_then(|h| hero_slugs.get(h))
            .cloned();

        let mut chosen: Option<String> = None;

        if args.previews == PreviewMode::Market && index < args.market_limit && !market_stopped {
            let file = format!("item_{}.png", sanitize(&skin.id));
            let path = preview_dir.join(&file);
            if path.is_file() {
                chosen = Some(format!("previews/{file}"));
            } else {
                match market_icon_hash(&skin.name) {
                    Ok(Some(hash)) => {
                        let url = MARKET_ICON.replace("{}", &hash);
                        if download_to(&url, &path).is_ok() {
                            chosen = Some(format!("previews/{file}"));
                            downloaded += 1;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        market_errors += 1;
                        if market_errors <= 3 {
                            eprintln!("  market lookup failed for '{}': {err}", skin.name);
                        }
                        if market_errors >= 8 {
                            market_stopped = true;
                            println!("  Steam is rate-limiting (429); using hero portraits from here on");
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }

        if chosen.is_none() {
            if let Some(slug) = slug {
                let file = format!("hero_{slug}.png");
                if !hero_cache.contains_key(&slug) {
                    let path = preview_dir.join(&file);
                    let url = HERO_IMG.replace("{}", &slug);
                    if download_to(&url, &path).is_ok() {
                        hero_cache.insert(slug.clone(), format!("previews/{file}"));
                        downloaded += 1;
                    }
                }
                chosen = hero_cache.get(&slug).cloned();
            }
        }

        skin.preview = chosen;

        if args.previews == PreviewMode::Market && index < args.market_limit && index % 25 == 0 {
            println!(
                "  previews {}/{} (market icons so far: {downloaded})",
                index,
                args.market_limit
            );
        }
    }

    let _ = tokens;
    println!("downloaded preview files: {downloaded} (market errors: {market_errors})");
    Ok(())
}

fn market_icon_hash(name: &str) -> Result<Option<String>, String> {
    let url = MARKET_SEARCH.replace("{}", &urlencode(name));
    let text = http_text(&url)?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("market json ({}): {e}", &text[..text.len().min(80)]))?;
    let results = json
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or("market response without results")?;

    let target = norm(name);
    let mut fallback: Option<String> = None;

    for entry in results {
        let Some(hash_name) = entry.get("hash_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(icon) = entry
            .get("asset_description")
            .and_then(|d| d.get("icon_url"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        if icon.is_empty() {
            continue;
        }
        let candidate = norm(hash_name);
        if candidate == target {
            return Ok(Some(icon.to_string()));
        }
        if fallback.is_none() && !target.is_empty() && candidate.contains(&target) {
            fallback = Some(icon.to_string());
        }
    }
    Ok(fallback)
}

fn download_to(url: &str, path: &Path) -> Result<(), String> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > 0 {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    // curl.exe first: Steam's limiter rejects some TLS fingerprints outright.
    if let Ok(output) = std::process::Command::new("curl.exe")
        .args(["-s", "-L", "--max-time", "60", "-o"])
        .arg(path)
        .arg(url)
        .output()
    {
        if output.status.success() {
            if std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
                return Ok(());
            }
        }
    }

    let mut buf = Vec::new();
    ureq::get(url)
        .timeout(Duration::from_secs(60))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {url}: {e}"))?;
    std::fs::write(path, &buf).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Text GET: curl.exe first, ureq as the portable fallback.
fn http_text(url: &str) -> Result<String, String> {
    if let Ok(output) = std::process::Command::new("curl.exe")
        .args([
            "-s",
            "-L",
            "--max-time",
            "30",
            "-A",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
            url,
        ])
        .output()
    {
        if output.status.success() && !output.stdout.is_empty() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
    }

    ureq::get(url)
        .set("User-Agent", "dota-skins-launcher/0.1")
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .into_string()
        .map_err(|e| format!("body {url}: {e}"))
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn urlencode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn norm(input: &str) -> String {
    let mut s = input.to_lowercase();
    for prefix in [
        "inscribed ",
        "genuine ",
        "frozen ",
        "exalted ",
        "elder ",
        "ascendant ",
        "corrupted ",
        "auspicious ",
        "hexed ",
        "cursed ",
        "unusual ",
        "heroic ",
        "autographed ",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    if let Some((head, _)) = s.split_once(" - ") {
        s = head.to_string();
    }
    s.trim().to_string()
}

// ---------------------------------------------------------------- download

/// Base model, persona model and default wearables of every hero.
/// Hero scripts live in one file per hero (scripts/npc/heroes/*.txt), cached locally.
fn collect_hero_models(
    args: &Args,
    hero_names: &HashMap<String, String>,
    drafts: &[Draft],
) -> Result<BTreeMap<String, OutHero>, String> {
    let index = fetch_text(
        &format!("{NPC_URL}npc_heroes.txt"),
        &args.cache.join("npc_heroes.txt"),
        args.refresh,
    )?;
    let files: Vec<&str> = index
        .lines()
        .filter_map(|l| l.trim().strip_prefix("#base"))
        .filter_map(|rest| rest.trim().strip_prefix('"')?.strip_suffix('"'))
        .filter(|f| f.starts_with("heroes/npc_dota_hero_") && !f.ends_with("_base.txt"))
        .collect();

    let mut out: BTreeMap<String, OutHero> = BTreeMap::new();
    let mut by_npc: HashMap<String, String> = HashMap::new();
    let mut downloaded = 0usize;
    for file in &files {
        let cache_path = args.cache.join(file.replace('/', "_"));
        let fresh = args.refresh || !cache_path.is_file();
        let text = match fetch_quiet(&format!("{NPC_URL}{file}"), &cache_path, args.refresh) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  {file}: {e}");
                continue;
            }
        };
        if fresh {
            downloaded += 1;
        }
        let Some((npc, model, persona)) = parse_hero_models(&text) else {
            continue;
        };
        let name = hero_names
            .get(&npc)
            .cloned()
            .unwrap_or_else(|| npc.trim_start_matches("npc_dota_hero_").replace('_', " "));
        by_npc.insert(npc, name.clone());
        out.insert(
            name,
            OutHero {
                model,
                persona_model: persona,
                defaults: BTreeMap::new(),
            },
        );
    }
    println!("hero script files: {} (downloaded now: {downloaded})", files.len());

    for d in drafts {
        let is_default = d
            .prefab
            .as_deref()
            .is_some_and(|p| p.split_whitespace().any(|x| x == "default_item"));
        let (Some(slot), Some(model)) = (d.item_slot.as_deref(), d.model_player.as_deref()) else {
            continue;
        };
        if !is_default || !model.ends_with(".vmdl") {
            continue;
        }
        for npc in d.hero_list().split(',').map(str::trim) {
            if let Some(hero) = by_npc.get(npc).and_then(|n| out.get_mut(n)) {
                hero.defaults
                    .entry(slot.to_string())
                    .or_insert_with(|| model.to_string());
            }
        }
    }
    Ok(out)
}

/// (npc name, base model, persona model) from one hero script file.
fn parse_hero_models(text: &str) -> Option<(String, String, Option<String>)> {
    let mut lexer = Lexer::new(text);
    let mut stack: Vec<String> = Vec::new();
    let mut npc: Option<String> = None;
    let mut model: Option<String> = None;
    let mut persona: Option<String> = None;
    while let Some(token) = lexer.next() {
        match token {
            Tok::Str(key) => match lexer.next() {
                Some(Tok::Str(value)) => {
                    if !key.eq_ignore_ascii_case("Model") || !value.ends_with(".vmdl") {
                        continue;
                    }
                    if stack.len() == 2 {
                        npc = Some(stack[1].clone());
                        model.get_or_insert_with(|| value.to_string());
                    } else if stack.len() > 2
                        && persona.is_none()
                        && stack.iter().any(|k| k.to_ascii_lowercase().contains("persona"))
                    {
                        persona = Some(value.to_string());
                    }
                }
                Some(Tok::Open) => stack.push(key.to_string()),
                Some(Tok::Close) | None => break,
            },
            Tok::Open => stack.push(String::from("?")),
            Tok::Close => {
                stack.pop();
            }
        }
    }
    Some((npc?, model?, persona))
}

/// Like `fetch_text`, without progress output (used for many small files).
fn fetch_quiet(url: &str, cache_path: &Path, refresh: bool) -> Result<String, String> {
    if !refresh {
        if let Ok(text) = std::fs::read_to_string(cache_path) {
            if !text.is_empty() {
                return Ok(text);
            }
        }
    }
    let text = ureq::get(url)
        .timeout(Duration::from_secs(60))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .into_string()
        .map_err(|e| format!("read {url}: {e}"))?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(cache_path, &text).map_err(|e| format!("cache write: {e}"))?;
    Ok(text)
}

fn fetch_text(url: &str, cache_path: &Path, refresh: bool) -> Result<String, String> {
    if !refresh {
        if let Ok(bytes) = std::fs::read(cache_path) {
            if bytes.len() > 1024 {
                println!("cache hit: {} ({:.1} MB)", cache_path.display(), bytes.len() as f64 / 1e6);
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    println!("downloading {url}");
    let mut buf = Vec::new();
    ureq::get(url)
        .timeout(Duration::from_secs(300))
        .call()
        .map_err(|e| format!("GET {url}: {e}"))?
        .into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {url}: {e}"))?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(cache_path, &buf).map_err(|e| format!("cache write: {e}"))?;
    println!("cached {:.1} MB", buf.len() as f64 / 1e6);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

// ---------------------------------------------------------------- VDF lexer

enum Tok<'a> {
    Str(&'a str),
    Open,
    Close,
}

struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        let bytes = text.as_bytes();
        let pos = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
        Self { bytes, pos }
    }

    fn skip_ws(&mut self) {
        loop {
            while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            if self.pos + 1 < self.bytes.len()
                && self.bytes[self.pos] == b'/'
                && self.bytes[self.pos + 1] == b'/'
            {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
            } else {
                return;
            }
        }
    }

    fn next(&mut self) -> Option<Tok<'a>> {
        self.skip_ws();
        if self.pos >= self.bytes.len() {
            return None;
        }
        match self.bytes[self.pos] {
            b'{' => {
                self.pos += 1;
                Some(Tok::Open)
            }
            b'}' => {
                self.pos += 1;
                Some(Tok::Close)
            }
            b'"' => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    match self.bytes[self.pos] {
                        b'\\' => self.pos += 2,
                        b'"' => break,
                        _ => self.pos += 1,
                    }
                }
                let slice = &self.bytes[start..self.pos.min(self.bytes.len())];
                self.pos += 1;
                std::str::from_utf8(slice).ok().map(Tok::Str)
            }
            _ => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    let c = self.bytes[self.pos];
                    if c.is_ascii_whitespace() || c == b'{' || c == b'}' || c == b'"' {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    self.pos += 1;
                    return self.next();
                }
                let slice = &self.bytes[start..self.pos];
                std::str::from_utf8(slice).ok().map(Tok::Str)
            }
        }
    }
}

// ---------------------------------------------------------------- parsers

#[derive(Debug, Default, Clone)]
struct Draft {
    /// Key of the block in the schema (item index or prefab name).
    block: Option<String>,
    name: Option<String>,
    prefab: Option<String>,
    item_name: Option<String>,
    used_by_heroes: Option<String>,
    heroes: Vec<String>,
    item_slot: Option<String>,
    item_rarity: Option<String>,
    image_inventory: Option<String>,
    model_player: Option<String>,
    item_quality: Option<String>,
    /// visuals/asset_modifier blocks of type "model": (asset, replacement)
    model_swaps: Vec<(String, String)>,
    /// asset_modifier block being read: (type, asset, modifier)
    pending_modifier: (String, String, String),
}

impl Draft {
    fn modifier_field(&mut self, key: &str, value: &str) {
        match key {
            "type" => self.pending_modifier.0 = value.to_string(),
            "asset" => self.pending_modifier.1 = value.to_string(),
            "modifier" => self.pending_modifier.2 = value.to_string(),
            _ => {}
        }
    }

    fn modifier_done(&mut self) {
        let (kind, asset, modifier) = std::mem::take(&mut self.pending_modifier);
        if !modifier.ends_with(".vmdl") {
            return;
        }
        // "model": one model replaced by another (e.g. a refit of another slot);
        // "entity_model" on the hero itself: a new body (arcanas). The npc name is
        // resolved to the hero's base model when the catalog is written.
        let keep = (kind == "model" && asset.ends_with(".vmdl"))
            || (kind == "entity_model" && asset.starts_with("npc_dota_hero_"));
        if keep {
            self.model_swaps.push((asset, modifier));
        }
    }

    /// Heroes come either as a legacy string or as a nested `used_by_heroes` block.
    fn hero_list(&self) -> String {
        if !self.heroes.is_empty() {
            return self.heroes.join(",");
        }
        self.used_by_heroes.clone().unwrap_or_default()
    }

    fn add_hero(&mut self, hero: &str) {
        let hero = hero.trim();
        if hero.starts_with("npc_dota_hero") && !self.heroes.iter().any(|h| h == hero) {
            self.heroes.push(hero.to_string());
        }
    }

    fn set(&mut self, key: &str, value: &str) {
        let target = match key {
            "name" => &mut self.name,
            "prefab" => &mut self.prefab,
            "item_name" => &mut self.item_name,
            "used_by_heroes" => &mut self.used_by_heroes,
            "item_slot" => &mut self.item_slot,
            "item_rarity" => &mut self.item_rarity,
            "image_inventory" => &mut self.image_inventory,
            "model_player" => &mut self.model_player,
            "item_quality" => &mut self.item_quality,
            _ => return,
        };
        let slot = target.get_or_insert_with(String::new);
        slot.clear();
        slot.push_str(value);
    }
}

/// Parses `items_game/items` and fills missing `item_slot` / `item_rarity`
/// from the item's prefab (e.g. plain wearables default to the weapon slot).
fn parse_items(text: &str) -> Vec<Draft> {
    let mut items = parse_section(text, "items");
    let prefabs: HashMap<String, Draft> = parse_section(text, "prefabs")
        .into_iter()
        .filter_map(|d| d.block.clone().map(|k| (k, d)))
        .collect();

    for item in &mut items {
        let Some(prefab) = item.prefab.clone() else {
            continue;
        };
        for name in prefab.split_whitespace() {
            let Some(base) = prefabs.get(name) else {
                continue;
            };
            if item.item_slot.is_none() {
                item.item_slot = base.item_slot.clone();
            }
            if item.item_rarity.is_none() {
                item.item_rarity = base.item_rarity.clone();
            }
        }
    }
    items
}

/// items_game/items/<id>/visuals/asset_modifier*
fn is_modifier_block(stack: &[String]) -> bool {
    stack.len() == 5 && stack[3] == "visuals" && stack[4].starts_with("asset_modifier")
}

fn parse_section(text: &str, section: &str) -> Vec<Draft> {
    let mut lexer = Lexer::new(text);
    let mut stack: Vec<String> = Vec::with_capacity(8);
    let mut draft: Option<Draft> = None;
    let mut out: Vec<Draft> = Vec::new();

    let in_items = |stack: &[String]| {
        stack.len() >= 2 && stack[0] == "items_game" && stack[1] == section
    };

    while let Some(token) = lexer.next() {
        match token {
            Tok::Str(key) => match lexer.next() {
                Some(Tok::Str(value)) => {
                    if stack.len() == 3 && in_items(&stack) {
                        if let Some(d) = draft.as_mut() {
                            d.set(key, value);
                        }
                    } else if stack.len() == 4 && in_items(&stack) && stack[3] == "used_by_heroes" {
                        if let Some(d) = draft.as_mut() {
                            d.add_hero(key);
                        }
                    } else if is_modifier_block(&stack) && in_items(&stack) {
                        if let Some(d) = draft.as_mut() {
                            d.modifier_field(key, value);
                        }
                    }
                }
                Some(Tok::Open) => {
                    stack.push(key.to_string());
                    if stack.len() == 3 && in_items(&stack) {
                        draft = Some(Draft {
                            block: Some(key.to_string()),
                            ..Draft::default()
                        });
                    }
                }
                Some(Tok::Close) | None => break,
            },
            Tok::Open => stack.push(String::from("?")),
            Tok::Close => {
                if stack.len() == 3 && in_items(&stack) {
                    if let Some(d) = draft.take() {
                        out.push(d);
                    }
                } else if is_modifier_block(&stack) && in_items(&stack) {
                    if let Some(d) = draft.as_mut() {
                        d.modifier_done();
                    }
                }
                stack.pop();
            }
        }
    }
    out
}

fn parse_tokens(text: &str) -> HashMap<String, String> {
    let mut lexer = Lexer::new(text);
    let mut stack: Vec<String> = Vec::with_capacity(4);
    let mut out: HashMap<String, String> = HashMap::new();

    while let Some(token) = lexer.next() {
        match token {
            Tok::Str(key) => match lexer.next() {
                Some(Tok::Str(value)) => {
                    if stack.len() == 2 && stack[0] == "lang" && stack[1] == "Tokens" {
                        out.insert(key.trim_start_matches('#').to_lowercase(), value.to_string());
                    }
                }
                Some(Tok::Open) => stack.push(key.to_string()),
                Some(Tok::Close) | None => break,
            },
            Tok::Open => stack.push(String::from("?")),
            Tok::Close => {
                stack.pop();
            }
        }
    }
    out
}

fn parse_heroes(
    text: &str,
) -> Result<(HashMap<String, String>, HashMap<String, String>), String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("heroes.json: {e}"))?;
    let mut names = HashMap::new();
    let mut slugs = HashMap::new();

    if let Some(map) = json.as_object() {
        for value in map.values() {
            let npc = value.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let localized = value
                .get("localized_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if npc.is_empty() {
                continue;
            }
            if !localized.is_empty() {
                names.insert(npc.to_string(), localized.to_string());
            }
            if let Some(img) = value.get("img").and_then(|v| v.as_str()) {
                let file = img.rsplit('/').next().unwrap_or("").split('?').next().unwrap_or("");
                let slug = file.trim_end_matches(".png");
                if !slug.is_empty() {
                    slugs.insert(npc.to_string(), slug.to_string());
                }
            }
        }
    }
    Ok((names, slugs))
}

fn localize(tokens: &HashMap<String, String>, raw: &str) -> Option<String> {
    let key = raw.trim().trim_start_matches('#').to_lowercase();
    if key.is_empty() {
        return None;
    }
    tokens.get(&key).cloned().filter(|s| !s.trim().is_empty())
}

fn prettify(internal: &str) -> String {
    internal.replace('_', " ")
}

fn hero_display(names: &HashMap<String, String>, used: &str) -> String {
    let mut list = hero_display_list(names, used);
    match list.len() {
        0 => "—".to_string(),
        1 => list.remove(0),
        n => format!("{} +{}", list[0], n - 1),
    }
}

fn hero_display_list(names: &HashMap<String, String>, used: &str) -> Vec<String> {
    let mut list: Vec<String> = used
        .split(',')
        .map(|s| s.trim())
        .filter(|s| s.starts_with("npc_dota_hero"))
        .map(|npc| {
            names
                .get(npc)
                .cloned()
                .unwrap_or_else(|| npc.trim_start_matches("npc_dota_hero_").replace('_', " "))
        })
        .collect();
    list.sort();
    list.dedup();
    list
}
