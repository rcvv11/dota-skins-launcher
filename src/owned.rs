//! The items the Steam account wears in Dota: its own, bought ones.
//!
//! The game keeps a local copy of the account's inventory next to its files,
//! `game/dota/cache_<account>_1.soc` (the newest one is the account playing),
//! with where each item is equipped. Those items' files take the preset's (see
//! `extract::same_slot_items` and `visuals::with_siblings`); every other item
//! keeps its own look, so the wardrobe's cards and try-ons show it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Definition indexes of the items the account has equipped on a hero. Empty
/// when there is no cache to read.
pub fn equipped(game_dir: &Path) -> HashSet<u32> {
    newest_cache(game_dir).and_then(|p| std::fs::read(p).ok()).map(|d| parse(&d)).unwrap_or_default()
}

/// The newest `cache_*.soc` of the game folder.
fn newest_cache(game_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(game_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            name.starts_with("cache_") && name.ends_with(".soc")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(time, _)| *time)
        .map(|(_, path)| path)
}

/// Equipped on no hero (couriers, wards, terrains...): the class of general items.
const NO_HERO: u64 = 1000;

/// The file is a protobuf: field 2 the subscribed cache, whose field 4 holds its
/// object types (field 1 the type, 1 = CSOEconItem; field 2 the objects). An item:
/// field 4 its definition, field 18 where it is equipped (field 1 the class, a
/// hero id or 1000).
fn parse(data: &[u8]) -> HashSet<u32> {
    let mut out = HashSet::new();
    let Some(top) = fields(data) else { return out };
    for cache in bytes_of(&top, 2) {
        let Some(cache) = fields(cache) else { continue };
        for group in bytes_of(&cache, 4) {
            let Some(group) = fields(group) else { continue };
            if varint_of(&group, 1) != Some(1) {
                continue;
            }
            for item in bytes_of(&group, 2) {
                let Some(item) = fields(item) else { continue };
                let Some(def) = varint_of(&item, 4).and_then(|d| u32::try_from(d).ok()) else { continue };
                let on_hero = bytes_of(&item, 18)
                    .filter_map(fields)
                    .any(|eq| varint_of(&eq, 1).is_some_and(|class| class != NO_HERO));
                if on_hero {
                    out.insert(def);
                }
            }
        }
    }
    out
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// The fields of a protobuf message, or None when it does not parse as one.
fn fields(mut b: &[u8]) -> Option<Vec<(u32, Value<'_>)>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let tag = varint(&mut b)?;
        let field = u32::try_from(tag >> 3).ok()?;
        let value = match tag & 7 {
            0 => Value::Varint(varint(&mut b)?),
            1 => {
                b = b.get(8..)?;
                Value::Fixed
            }
            2 => {
                let len = usize::try_from(varint(&mut b)?).ok()?;
                let (head, rest) = (b.get(..len)?, b.get(len..)?);
                b = rest;
                Value::Bytes(head)
            }
            5 => {
                b = b.get(4..)?;
                Value::Fixed
            }
            _ => return None,
        };
        out.push((field, value));
    }
    Some(out)
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

fn bytes_of<'a>(msg: &'a [(u32, Value<'a>)], field: u32) -> impl Iterator<Item = &'a [u8]> + 'a {
    msg.iter().filter_map(move |(f, v)| match v {
        Value::Bytes(b) if *f == field => Some(*b),
        _ => None,
    })
}

fn varint_of(msg: &[(u32, Value<'_>)], field: u32) -> Option<u64> {
    msg.iter().find_map(|(f, v)| match v {
        Value::Varint(x) if *f == field => Some(*x),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_bytes(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }
    fn var(field: u32, v: u64) -> Vec<u8> {
        let mut out = varint_bytes(u64::from(field) << 3);
        out.extend(varint_bytes(v));
        out
    }
    fn msg(field: u32, body: &[u8]) -> Vec<u8> {
        let mut out = varint_bytes((u64::from(field) << 3) | 2);
        out.extend(varint_bytes(body.len() as u64));
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn items_equipped_on_heroes() {
        // an item worn by hero 1 (Anti-Mage), one by no hero (a courier), one not worn
        let worn = [var(1, 11), var(4, 4503), msg(18, &[var(1, 1), var(2, 0)].concat())].concat();
        let courier = [var(1, 12), var(4, 595), msg(18, &[var(1, 1000), var(2, 2)].concat())].concat();
        let stored = [var(1, 13), var(4, 7000)].concat();
        let items = [var(1, 1), msg(2, &worn), msg(2, &courier), msg(2, &stored)].concat();
        let other_type = [var(1, 7), msg(2, &[var(1, 5)].concat())].concat();
        let cache = [var(1, 1), var(2, 76561198000000000), msg(4, &items), msg(4, &other_type)].concat();
        let file = [var(1, 4), msg(2, &cache), var(3, 10)].concat();
        assert_eq!(parse(&file), [4503u32].into_iter().collect());
        assert!(parse(b"\xff\xff").is_empty());
    }
}
