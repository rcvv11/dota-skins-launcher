//! Items for slots the hero has no file need the game's item table changed:
//! each empty slot's `default_item` gets a stand-in model of its own (an
//! invisible one), which changer.dll swaps for the picked item (schema.rs).
//!
//! The changed table goes into its own folder, `game/dota_skins_schema`, as a
//! one-file `pak01_dir.vpk`, registered ahead of `dota` in the search paths.
//! The search paths are written into `gameinfo_branchspecific.gi` (its keys win
//! over gameinfo.gi), and that file's hashes are appended to
//! `bin/win64/dota.signatures` after the DIGEST line: Dota checks the gameinfo
//! files against that list before online play (the "game failed the check before
//! online play" error was a gameinfo.gi changed without it).
//!
//! The table is read once at startup, so it does not depend on the preset: it
//! is written once (and after game updates), then every empty-slot pick is a
//! file swap without a restart. The file mode puts everything back as Steam
//! shipped it.

use std::path::{Path, PathBuf};

/// Our content folder, next to `dota` in `<Dota 2 beta>\game`.
const FOLDER: &str = "dota_skins_schema";
/// Written beside every line we add to a game file.
const MARKER: &str = "dota-skins-launcher";
const SIG_PREFIX: &str = r"...\..\..\dota\gameinfo_branchspecific.gi~";

struct Paths {
    folder: PathBuf,
    gameinfo: PathBuf,
    branch: PathBuf,
    signatures: PathBuf,
}

fn paths(dota_exe: &str) -> Result<Paths, String> {
    let game = crate::mount::game_root(dota_exe)
        .map(|r| r.join("game"))
        .ok_or_else(|| crate::trf!("не удалось определить папку Dota по пути {dota_exe}", "could not determine the Dota folder from the path {dota_exe}"))?;
    Ok(Paths {
        folder: game.join(FOLDER),
        gameinfo: game.join("dota").join("gameinfo.gi"),
        branch: game.join("dota").join("gameinfo_branchspecific.gi"),
        signatures: game.join("bin").join("win64").join("dota.signatures"),
    })
}

/// The invisible model every stand-in is a copy of (a game file; Monkey
/// King's transformation uses one like it).
const INVISIBLE: &str = "models/development/invisiblebox.vmdl_c";

/// Writes the changed table and registers it: every empty wearable slot of
/// `slots` (`(npc, slot)`) gets its stand-in model (schema::placeholder), the
/// default items of `no_merge` the "skip_model_combine" flag. None of it
/// depends on the preset, so it is written once (and again after a game
/// update): a pick for an empty slot is then a file swap like any other.
pub fn apply(
    dota_exe: &str,
    table: &[u8],
    slots: &std::collections::HashSet<(String, String)>,
    no_merge: &[(String, String)],
) -> Result<(), String> {
    let p = paths(dota_exe)?;
    let pak = p.folder.join("pak01_dir.vpk");
    let stamp_path = p.folder.join("stamp.txt");
    let stamp = content_stamp(table, slots, no_merge);
    let fresh = pak.is_file() && std::fs::read_to_string(&stamp_path).is_ok_and(|s| s == stamp);
    if !fresh {
        let game_dir = crate::vpk::game_dir_from_exe(dota_exe)
            .ok_or(crate::tr!("не найдена папка game/dota", "game/dota folder not found"))?;
        let invisible = crate::vpk::read_file(&game_dir, INVISIBLE)?
            .ok_or_else(|| crate::trf!("в паке игры нет {INVISIBLE}", "the game's pak has no {INVISIBLE}"))?;
        let (patched, placed) = crate::schema::with_placeholders(table, slots);
        let patched = crate::schema::skip_combine(&patched, no_merge);
        let mut files = vec![(crate::schema::SCHEMA_PATH.to_string(), patched)];
        for (npc, slot) in &placed {
            files.push((format!("{}_c", crate::schema::placeholder(npc, slot)), invisible.clone()));
        }
        std::fs::create_dir_all(&p.folder).map_err(|e| format!("{}: {e}", p.folder.display()))?;
        crate::vpk::write_vpk(&pak, &files)?;
        let _ = std::fs::write(&stamp_path, &stamp);
    }

    let gameinfo = read_latin1(&p.gameinfo)?;
    let old_branch = read_latin1(&p.branch)?;
    let block = with_folder(&search_paths(&gameinfo)?)?;
    let new_branch = insert_block(&strip_branch(&old_branch), &block)?;
    if new_branch != old_branch {
        write_latin1(&p.branch, &new_branch)?;
    }

    let old_sigs = read_latin1(&p.signatures)?;
    let line = signature_line(&to_latin1(&new_branch));
    let new_sigs = format!("{}{line}\r\n", strip_signatures(&old_sigs));
    if new_sigs != old_sigs {
        write_latin1(&p.signatures, &new_sigs)?;
    }
    Ok(())
}

/// The table version the running Dota read at its start (`schema_loaded.txt`).
fn loaded_path() -> std::path::PathBuf {
    crate::config::app_dir().join("schema_loaded.txt")
}

/// Called right before the launcher starts Dota: remembers the table version
/// it is going to read.
pub fn remember_loaded(dota_exe: &str) {
    let stamp = paths(dota_exe).ok().and_then(|p| std::fs::read_to_string(p.folder.join("stamp.txt")).ok());
    let _ = std::fs::write(loaded_path(), stamp.unwrap_or_default());
}

/// Whether the running Dota read the current table (its stand-ins) at its start.
pub fn loaded_current(dota_exe: &str) -> bool {
    let Ok(p) = paths(dota_exe) else { return false };
    let current = std::fs::read_to_string(p.folder.join("stamp.txt")).unwrap_or_default();
    let loaded = std::fs::read_to_string(loaded_path()).unwrap_or_default();
    !current.is_empty() && current == loaded
}

/// `restart|name\tname` for changer.dll: empty-slot items that show only
/// after a restart (the running Dota started before the table had its
/// stand-ins); their in-game button says so. None when there are none.
pub fn restart_line(names: &[String]) -> Option<String> {
    let mut pending: Vec<&str> = names.iter().map(String::as_str).collect();
    pending.sort();
    pending.dedup();
    (!pending.is_empty()).then(|| format!("restart|{}", pending.join("\t")))
}

/// What the patched table is built from: the slots, the merge flags and the
/// game's table (its size and a hash of its head and tail).
fn content_stamp(table: &[u8], slots: &std::collections::HashSet<(String, String)>, no_merge: &[(String, String)]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    table.len().hash(&mut h);
    let edge = table.len().min(1 << 20);
    table[..edge].hash(&mut h);
    table[table.len() - edge..].hash(&mut h);
    let mut slots: Vec<&(String, String)> = slots.iter().collect();
    slots.sort();
    let mut no_merge: Vec<&(String, String)> = no_merge.iter().collect();
    no_merge.sort();
    no_merge.dedup();
    format!("{slots:?}{no_merge:?}").hash(&mut h);
    format!("v2 {:016x}", h.finish())
}

/// Takes everything back: the branch file, the signature list, our folder.
pub fn remove(dota_exe: &str) -> Result<(), String> {
    let p = paths(dota_exe)?;
    if let Ok(branch) = read_latin1(&p.branch) {
        if branch.contains(MARKER) {
            write_latin1(&p.branch, &strip_branch(&branch))?;
        }
    }
    if let Ok(sigs) = read_latin1(&p.signatures) {
        let clean = strip_signatures(&sigs);
        if clean != sigs {
            write_latin1(&p.signatures, &clean)?;
        }
    }
    if p.folder.exists() {
        let _ = std::fs::remove_dir_all(&p.folder);
    }
    Ok(())
}

fn read_latin1(path: &Path) -> Result<String, String> {
    std::fs::read(path)
        .map(|b| b.iter().map(|&c| c as char).collect())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn to_latin1(text: &str) -> Vec<u8> {
    text.chars().map(|c| c as u32 as u8).collect()
}

fn write_latin1(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("launcher-tmp");
    std::fs::write(&tmp, to_latin1(text))
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Position of the `}` closing the first `{` at or after `from`.
fn block_end(text: &str, from: usize) -> Option<(usize, usize)> {
    let open = from + text[from..].find('{')?;
    let mut depth = 0;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + i));
                }
            }
            _ => {}
        }
    }
    None
}

/// The `SearchPaths { ... }` block of gameinfo.gi, as text.
fn search_paths(gameinfo: &str) -> Result<String, String> {
    let at = gameinfo.find("SearchPaths").ok_or(crate::tr!("gameinfo.gi: нет блока SearchPaths", "gameinfo.gi: no SearchPaths block"))?;
    let (_, close) = block_end(gameinfo, at).ok_or(crate::tr!("gameinfo.gi: блок SearchPaths не закрыт", "gameinfo.gi: SearchPaths block is not closed"))?;
    Ok(gameinfo[at..=close].to_string())
}

/// The block with our folder as the first Game path (the engine makes it the
/// MOD path, which items_game.txt is read from) and the first Mod path.
fn with_folder(block: &str) -> Result<String, String> {
    let mut out = Vec::new();
    let (mut game, mut modp) = (false, false);
    for line in block.split("\r\n").flat_map(|l| l.split('\n')) {
        let line = line.trim_end_matches('\r');
        let words: Vec<&str> = line.split('/').next().unwrap_or("").split_whitespace().collect();
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if !game && words == ["Game", "dota"] {
            out.push(format!("{indent}Game\t\t\t\t{FOLDER}\t\t// {MARKER}"));
            game = true;
        }
        if !modp && words == ["Mod", "dota"] {
            out.push(format!("{indent}Mod\t\t\t\t{FOLDER}\t\t// {MARKER}"));
            modp = true;
        }
        out.push(line.to_string());
    }
    if !game || !modp {
        return Err(crate::tr!("gameinfo.gi: не найдены строки «Game dota» / «Mod dota»", "gameinfo.gi: «Game dota» / «Mod dota» lines not found").into());
    }
    Ok(out.join("\r\n"))
}

/// The branch file with the block at the end of its FileSystem section.
fn insert_block(branch: &str, block: &str) -> Result<String, String> {
    let at = branch.find("FileSystem").ok_or(crate::tr!("gameinfo_branchspecific.gi: нет блока FileSystem", "gameinfo_branchspecific.gi: no FileSystem block"))?;
    let (_, close) = block_end(branch, at).ok_or(crate::tr!("gameinfo_branchspecific.gi: блок FileSystem не закрыт", "gameinfo_branchspecific.gi: FileSystem block is not closed"))?;
    let head = branch[..close].trim_end_matches([' ', '\t']);
    let body: Vec<String> = block
        .split("\r\n")
        .map(|l| if l.trim().is_empty() { String::new() } else { format!("\t\t{}", l.trim()) })
        .collect();
    Ok(format!("{head}\t\t// {MARKER}\r\n{}\r\n\t{}", body.join("\r\n"), &branch[close..]))
}

/// The branch file without our block, as Steam shipped it: everything from our
/// leading marker line to the end of our SearchPaths block comes out, and the
/// FileSystem section gets back its "\r\n\t" before the closing brace.
fn strip_branch(branch: &str) -> String {
    let lead = format!("\t\t// {MARKER}\r\n");
    let Some(start) = branch.find(&lead) else { return branch.to_string() };
    let Some(sp) = branch[start..].find("SearchPaths").map(|o| start + o) else { return branch.to_string() };
    let Some((_, close)) = block_end(branch, sp) else { return branch.to_string() };
    let rest = branch[close + 1..].trim_start_matches(['\r', '\n', ' ', '\t']);
    format!("{}\r\n\t{rest}", branch[..start].trim_end_matches(['\r', '\n', ' ', '\t']))
}

/// The signature list without our lines (they come after the DIGEST line).
fn strip_signatures(text: &str) -> String {
    let lines: Vec<&str> = text.split("\r\n").collect();
    let Some(digest) = lines.iter().position(|l| l.starts_with("DIGEST:")) else { return text.to_string() };
    let mut kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| *i <= digest || !l.starts_with(SIG_PREFIX))
        .map(|(_, l)| *l)
        .collect();
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    kept.join("\r\n") + "\r\n"
}

/// `...\gameinfo_branchspecific.gi~SHA1:<hex>;CRC:<hex>` (CRC little-endian).
fn signature_line(file: &[u8]) -> String {
    let sha: String = sha1(file).iter().map(|b| format!("{b:02X}")).collect();
    let crc: String = crate::vpk::crc32(file).to_le_bytes().iter().map(|b| format!("{b:02X}")).collect();
    format!("{SIG_PREFIX}SHA1:{sha};CRC:{crc}")
}

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRANCH: &str = "\"GameInfo\"\r\n{\r\n\tgame \t\t\"Dota 2\"\r\n\r\n\tFileSystem\r\n\t{\r\n\t\tSteamAppId\t\t\t\t570\r\n\t}\r\n}\r\n";
    const GAMEINFO: &str = "\"GameInfo\"\r\n{\r\n\tFileSystem\r\n\t{\r\n\t\tSearchPaths\r\n\t\t{\r\n\t\t\tGame_Language\t\tdota_*LANGUAGE*\r\n\t\t\tGame\t\t\t\tdota\r\n\t\t\tGame\t\t\t\tcore\r\n\t\t\tMod\t\t\t\t\tdota\r\n\t\t}\r\n\t}\r\n}\r\n";

    #[test]
    fn sha1_known_values() {
        let hex = |b: [u8; 20]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(sha1(&[b'a'; 1000])), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
    }

    #[test]
    fn branch_patch_goes_back_byte_for_byte() {
        let block = with_folder(&search_paths(GAMEINFO).unwrap()).unwrap();
        assert!(block.contains("Game\t\t\t\tdota_skins_schema"));
        assert!(block.find("dota_skins_schema").unwrap() < block.find("Game\t\t\t\tdota\r").unwrap());
        let patched = insert_block(BRANCH, &block).unwrap();
        assert!(patched.contains("SearchPaths"));
        assert_eq!(strip_branch(&patched), BRANCH);
        // applying twice keeps one block
        let twice = insert_block(&strip_branch(&patched), &block).unwrap();
        assert_eq!(twice, patched);
    }

    #[test]
    fn signature_lines_come_off_again() {
        let sigs = "a~SHA1:1;CRC:2\r\nDIGEST:FF\r\n";
        let ours = format!("{sigs}{}\r\n", signature_line(b"x"));
        assert!(ours.contains("gameinfo_branchspecific.gi~SHA1:"));
        assert_eq!(strip_signatures(&ours), sigs);
    }

}
