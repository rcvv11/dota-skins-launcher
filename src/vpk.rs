//! Minimal read-only VPK (v1/v2) reader for Dota 2 item icons.
//!
//! Panorama stores inventory icons as `panorama/images/<image_inventory>_png.vtex_c`.
//! Those resources embed a regular PNG file, so we index the directory once and
//! cut the PNG out of the resource on demand. Game files are never modified.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const SIGNATURE: u32 = 0x55AA_1234;
const DIR_ARCHIVE: u16 = 0x7FFF;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const ICON_PREFIX: &str = "panorama/images/";

struct Entry {
    archive: u16,
    offset: u32,
    length: u32,
    preload: Vec<u8>,
}

pub struct IconPak {
    root: PathBuf,
    stem: String,
    data_offset: u64,
    entries: HashMap<String, Entry>,
    handles: HashMap<u16, File>,
}

impl IconPak {
    /// `game_dir` is `<Dota>/game/dota` (the folder holding `pak01_dir.vpk`).
    pub fn open(game_dir: &Path) -> Result<Self, String> {
        let dir_path = game_dir.join("pak01_dir.vpk");
        let data = std::fs::read(&dir_path)
            .map_err(|e| format!("cannot read {}: {e}", dir_path.display()))?;

        let u32_at = |p: usize| -> Option<u32> {
            data.get(p..p + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        let u16_at = |p: usize| -> Option<u16> {
            data.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
        };

        if u32_at(0) != Some(SIGNATURE) {
            return Err("pak01_dir.vpk: bad signature".into());
        }
        let version = u32_at(4).unwrap_or(0);
        let tree_size = u32_at(8).ok_or("pak01_dir.vpk: truncated header")? as usize;
        let header = match version {
            1 => 12,
            2 => 28,
            v => return Err(format!("pak01_dir.vpk: unsupported version {v}")),
        };

        let mut pos = header;
        let tree_end = (header + tree_size).min(data.len());
        let mut entries = HashMap::new();

        let read_str = |pos: &mut usize| -> Option<String> {
            let rest = data.get(*pos..tree_end)?;
            let len = rest.iter().position(|&b| b == 0)?;
            let s = String::from_utf8_lossy(&rest[..len]).into_owned();
            *pos += len + 1;
            Some(s)
        };

        'ext: while let Some(ext) = read_str(&mut pos) {
            if ext.is_empty() {
                break;
            }
            while let Some(dir) = read_str(&mut pos) {
                if dir.is_empty() {
                    continue 'ext;
                }
                let wanted_dir = ext == "vtex_c"
                    && (dir.starts_with("panorama/images/econ")
                        || dir.starts_with("panorama/images/teamfancontent"));
                while let Some(name) = read_str(&mut pos) {
                    if name.is_empty() {
                        break;
                    }
                    let preload_len = u16_at(pos + 4).ok_or("truncated entry")? as usize;
                    let archive = u16_at(pos + 6).ok_or("truncated entry")?;
                    let offset = u32_at(pos + 8).ok_or("truncated entry")?;
                    let length = u32_at(pos + 12).ok_or("truncated entry")?;
                    pos += 18;
                    let preload = data
                        .get(pos..pos + preload_len)
                        .ok_or("truncated preload")?
                        .to_vec();
                    pos += preload_len;

                    if wanted_dir && name.ends_with("_png") {
                        entries.insert(
                            format!("{dir}/{name}"),
                            Entry {
                                archive,
                                offset,
                                length,
                                preload,
                            },
                        );
                    }
                }
            }
            break;
        }

        if entries.is_empty() {
            return Err("no econ icons found in pak01_dir.vpk".into());
        }

        Ok(Self {
            root: game_dir.to_path_buf(),
            stem: "pak01".into(),
            data_offset: (header + tree_size) as u64,
            entries,
            handles: HashMap::new(),
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// PNG bytes for an `image_inventory` path such as `econ/items/axe/foo/bar`.
    #[cfg(test)]
    pub fn icon_png(&mut self, image_inventory: &str) -> Option<Vec<u8>> {
        extract_png(&self.icon_blob(image_inventory)?)
    }

    /// Icon picture for an `image_inventory` path, whatever way the resource stores it.
    pub fn icon_image(&mut self, image_inventory: &str) -> Option<IconImage> {
        decode_vtex(&self.icon_blob(image_inventory)?)
    }

    fn icon_blob(&mut self, image_inventory: &str) -> Option<Vec<u8>> {
        let key = format!(
            "{ICON_PREFIX}{}_png",
            image_inventory.trim().trim_start_matches('/').to_lowercase()
        );
        let (archive, offset, length, preload) = {
            let e = self.entries.get(&key)?;
            (e.archive, e.offset, e.length, e.preload.clone())
        };

        let mut blob = preload;
        if length > 0 {
            let (path, start) = if archive == DIR_ARCHIVE {
                (
                    self.root.join(format!("{}_dir.vpk", self.stem)),
                    self.data_offset + offset as u64,
                )
            } else {
                (
                    self.root.join(format!("{}_{archive:03}.vpk", self.stem)),
                    offset as u64,
                )
            };
            let file = match self.handles.entry(archive) {
                std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => v.insert(File::open(path).ok()?),
            };
            file.seek(SeekFrom::Start(start)).ok()?;
            let mut buf = vec![0u8; length as usize];
            file.read_exact(&mut buf).ok()?;
            blob.extend_from_slice(&buf);
        }
        Some(blob)
    }
}

/// Reads one file from `<game_dir>/pak01_dir.vpk` by its in-pak path, e.g.
/// `models/items/axe/x/x.vmdl_c`. Loads only the directory tree into memory
/// (a few MB), never the multi-GB data section. `Ok(None)` = no such file.
pub fn read_file(game_dir: &Path, in_pak: &str) -> Result<Option<Vec<u8>>, String> {
    let want = in_pak.trim().trim_start_matches('/').to_lowercase();
    let mut found = read_matching(game_dir, |p| p == want)?;
    Ok(found.remove(&want))
}

/// Every file of the game's `pak01_dir.vpk` whose lower-cased path passes
/// `wanted`, read in one pass over the directory tree (the tree is large, so
/// reading many files one `read_file` at a time is slow). Keys are lower-cased.
pub fn read_matching(
    game_dir: &Path,
    wanted: impl Fn(&str) -> bool,
) -> Result<HashMap<String, Vec<u8>>, String> {
    let dir_path = game_dir.join("pak01_dir.vpk");
    let mut file = File::open(&dir_path).map_err(|e| format!("{}: {e}", dir_path.display()))?;
    let mut head = [0u8; 28];
    file.read_exact(&mut head)
        .map_err(|e| format!("{}: {e}", dir_path.display()))?;
    let u32_at =
        |p: usize| Some(u32::from_le_bytes([head[p], head[p + 1], head[p + 2], head[p + 3]]));
    if u32_at(0) != Some(SIGNATURE) {
        return Err(crate::trf!("{}: это не VPK", "{}: not a VPK", dir_path.display()));
    }
    let header = match u32_at(4) {
        Some(1) => 12,
        Some(2) => 28,
        v => return Err(crate::trf!("{}: неподдерживаемая версия VPK {v:?}", "{}: unsupported VPK version {v:?}", dir_path.display())),
    };
    let tree_size = u32_at(8).ok_or(crate::tr!("VPK: обрезанный заголовок", "VPK: truncated header"))? as usize;
    let data_offset = header + tree_size as u64;

    let mut tree = vec![0u8; tree_size];
    file.seek(SeekFrom::Start(header as u64)).map_err(|e| e.to_string())?;
    file.read_exact(&mut tree).map_err(|e| format!("{}: {e}", dir_path.display()))?;

    let read_str = |pos: &mut usize| -> Option<String> {
        let rest = tree.get(*pos..)?;
        let len = rest.iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&rest[..len]).into_owned();
        *pos += len + 1;
        Some(s)
    };

    let mut out = HashMap::new();
    let mut archives: HashMap<u16, File> = HashMap::new();
    let mut pos = 0usize;
    'ext: while let Some(ext) = read_str(&mut pos) {
        if ext.is_empty() {
            break;
        }
        while let Some(dir) = read_str(&mut pos) {
            if dir.is_empty() {
                continue 'ext;
            }
            while let Some(name) = read_str(&mut pos) {
                if name.is_empty() {
                    break;
                }
                let preload_len =
                    u16(&tree, pos + 4).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))? as usize;
                let archive = u16(&tree, pos + 6).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))?;
                let offset = u32(&tree, pos + 8).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))? as u64;
                let length = u32(&tree, pos + 12).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))? as usize;
                pos += 18;
                let preload = tree
                    .get(pos..pos + preload_len)
                    .ok_or(crate::tr!("VPK: обрезанные данные", "VPK: truncated data"))?;
                pos += preload_len;

                let dir_part = if dir == " " { String::new() } else { format!("{dir}/") };
                let full = format!("{dir_part}{name}.{ext}").to_lowercase();
                if !wanted(&full) {
                    continue;
                }
                let mut bytes = preload.to_vec();
                if length > 0 {
                    let mut buf = vec![0u8; length];
                    if archive == DIR_ARCHIVE {
                        file.seek(SeekFrom::Start(data_offset + offset))
                            .map_err(|e| e.to_string())?;
                        file.read_exact(&mut buf).map_err(|e| e.to_string())?;
                    } else {
                        let af = match archives.entry(archive) {
                            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                            std::collections::hash_map::Entry::Vacant(e) => {
                                let stem = dir_path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .and_then(|n| n.strip_suffix("_dir.vpk"))
                                    .ok_or(crate::tr!("VPK ссылается на архив, а файл не *_dir.vpk", "VPK refers to an archive, but the file is not *_dir.vpk"))?;
                                let ap = dir_path.with_file_name(format!("{stem}_{archive:03}.vpk"));
                                e.insert(
                                    File::open(&ap).map_err(|e| format!("{}: {e}", ap.display()))?,
                                )
                            }
                        };
                        af.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
                        af.read_exact(&mut buf).map_err(|e| e.to_string())?;
                    }
                    bytes.extend_from_slice(&buf);
                }
                out.insert(full, bytes);
            }
        }
        break;
    }
    Ok(out)
}

fn u16(buf: &[u8], p: usize) -> Option<u16> {
    let b = buf.get(p..p + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32(buf: &[u8], p: usize) -> Option<u32> {
    let b = buf.get(p..p + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Every file inside a VPK (single-file or `*_dir.vpk` with numbered archives).
/// Used to merge a user's mod VPK into the preset's pak.
pub fn read_all(path: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let u32_at = |p: usize| data.get(p..p + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let u16_at = |p: usize| data.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if u32_at(0) != Some(SIGNATURE) {
        return Err(crate::trf!("{}: это не VPK", "{}: not a VPK", path.display()));
    }
    let header = match u32_at(4) {
        Some(1) => 12,
        Some(2) => 28,
        v => return Err(crate::trf!("{}: неподдерживаемая версия VPK {v:?}", "{}: unsupported VPK version {v:?}", path.display())),
    };
    let tree_size = u32_at(8).ok_or(crate::tr!("VPK: обрезанный заголовок", "VPK: truncated header"))? as usize;
    let tree_end = (header + tree_size).min(data.len());
    let data_offset = header + tree_size;

    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = file_name.strip_suffix("_dir.vpk").map(str::to_string);
    let archive_path = |i: u16| -> Option<PathBuf> {
        let stem = stem.as_ref()?;
        Some(path.with_file_name(format!("{stem}_{i:03}.vpk")))
    };

    let mut pos = header;
    let read_str = |pos: &mut usize| -> Option<String> {
        let rest = data.get(*pos..tree_end)?;
        let len = rest.iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&rest[..len]).into_owned();
        *pos += len + 1;
        Some(s)
    };

    let mut out = Vec::new();
    let mut handles: HashMap<u16, File> = HashMap::new();
    'ext: while let Some(ext) = read_str(&mut pos) {
        if ext.is_empty() {
            break;
        }
        while let Some(dir) = read_str(&mut pos) {
            if dir.is_empty() {
                continue 'ext;
            }
            while let Some(name) = read_str(&mut pos) {
                if name.is_empty() {
                    break;
                }
                let preload_len = u16_at(pos + 4).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))? as usize;
                let archive = u16_at(pos + 6).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))?;
                let offset = u32_at(pos + 8).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))? as usize;
                let length = u32_at(pos + 12).ok_or(crate::tr!("VPK: обрезанная запись", "VPK: truncated entry"))? as usize;
                pos += 18;
                let mut bytes = data.get(pos..pos + preload_len).ok_or(crate::tr!("VPK: обрезанные данные", "VPK: truncated data"))?.to_vec();
                pos += preload_len;
                if length > 0 {
                    if archive == DIR_ARCHIVE {
                        let start = data_offset + offset;
                        bytes.extend_from_slice(data.get(start..start + length).ok_or(crate::tr!("VPK: данные за концом файла", "VPK: data past the end of the file"))?);
                    } else {
                        let ap = archive_path(archive).ok_or(crate::tr!("VPK ссылается на архив, а файл не *_dir.vpk", "VPK refers to an archive, but the file is not *_dir.vpk"))?;
                        let file = match handles.entry(archive) {
                            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                            std::collections::hash_map::Entry::Vacant(v) => {
                                v.insert(File::open(&ap).map_err(|e| format!("{}: {e}", ap.display()))?)
                            }
                        };
                        file.seek(SeekFrom::Start(offset as u64)).map_err(|e| e.to_string())?;
                        let mut buf = vec![0u8; length];
                        file.read_exact(&mut buf).map_err(|e| e.to_string())?;
                        bytes.extend_from_slice(&buf);
                    }
                }
                let dir_part = if dir.trim() == "" { String::new() } else { format!("{dir}/") };
                let ext_part = if ext.trim() == "" { String::new() } else { format!(".{ext}") };
                out.push((format!("{dir_part}{name}{ext_part}"), bytes));
            }
        }
        break;
    }
    Ok(out)
}

/// A decoded or still-encoded icon.
pub enum IconImage {
    /// PNG/JPEG file bytes, decode with the `image` crate.
    Encoded(Vec<u8>),
    /// Straight RGBA8 pixels.
    Rgba { width: usize, height: usize, pixels: Vec<u8> },
}

// Source 2 texture formats we can read (VTEX_FORMAT_*).
const FMT_DXT1: u8 = 1;
const FMT_DXT5: u8 = 2;
const FMT_RGBA8888: u8 = 4;
const FMT_JPEG_RGBA8888: u8 = 15;
const FMT_PNG_RGBA8888: u8 = 16;
const FMT_JPEG_DXT5: u8 = 17;
const FMT_PNG_DXT5: u8 = 18;

/// Reads a compiled `.vtex_c`: embedded PNG/JPEG, or raw DXT1/DXT5/RGBA8888 pixels
/// (largest mip). Returns None for formats we do not decode (BC7, HDR, ...).
fn decode_vtex(blob: &[u8]) -> Option<IconImage> {
    let u16_at = |p: usize| blob.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |p: usize| blob.get(p..p + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));

    // Resource header: file size, header version, version, block offset, block count.
    let data_start = u32_at(0)? as usize;
    let block_table = 8 + u32_at(8)? as usize;
    let block_count = u32_at(12)? as usize;
    let data_block = (0..block_count.min(32)).find_map(|i| {
        let entry = block_table + i * 12;
        (blob.get(entry..entry + 4)? == b"DATA").then(|| entry + 4 + u32_at(entry + 4).unwrap_or(0) as usize)
    })?;

    let width = u16_at(data_block + 20)? as usize;
    let height = u16_at(data_block + 22)? as usize;
    let format = *blob.get(data_block + 26)?;
    let mips = (*blob.get(data_block + 27)?).max(1) as usize;

    match format {
        FMT_PNG_RGBA8888 | FMT_PNG_DXT5 => extract_png(blob).map(IconImage::Encoded),
        FMT_JPEG_RGBA8888 | FMT_JPEG_DXT5 => extract_jpeg(blob).map(IconImage::Encoded),
        FMT_DXT1 | FMT_DXT5 | FMT_RGBA8888 => {
            if width == 0 || height == 0 || width > 4096 || height > 4096 {
                return None;
            }
            let size_of = |w: usize, h: usize| match format {
                FMT_RGBA8888 => w * h * 4,
                FMT_DXT1 => w.div_ceil(4) * h.div_ceil(4) * 8,
                _ => w.div_ceil(4) * h.div_ceil(4) * 16,
            };
            // Mips are stored smallest first, so the full-size image comes last.
            let total: usize = (0..mips)
                .map(|m| size_of((width >> m).max(1), (height >> m).max(1)))
                .sum();
            let pixels = blob.get(data_start..data_start + total)?;
            let top = &pixels[total - size_of(width, height)..];
            let mut rgba = match format {
                FMT_RGBA8888 => top.to_vec(),
                FMT_DXT1 => decode_bc(top, width, height, false),
                _ => decode_bc(top, width, height, true),
            };
            // Icons are padded to a power of two: prefer the stored display size,
            // else drop transparent rows (checked before YCoCg makes alpha opaque).
            let shown = display_height(blob, data_block).filter(|&h| h > 0 && h <= height);
            let shown = shown.unwrap_or_else(|| visible_height(&rgba, width, height));
            // Many UI icons are "YCoCg DXT5"; the texture compiler notes it in the header.
            if format == FMT_DXT5 && find(&blob[..data_start.min(blob.len())], b"YCoCg").is_some() {
                ycocg_to_rgb(&mut rgba);
            }
            let height = shown;
            Some(IconImage::Rgba {
                width,
                height,
                pixels: rgba[..width * height * 4].to_vec(),
            })
        }
        _ => None,
    }
}

/// Visible height from the texture's extra data (type 3: u16 width at +2, height at +4).
fn display_height(blob: &[u8], data_block: usize) -> Option<usize> {
    let u16_at = |p: usize| blob.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |p: usize| blob.get(p..p + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let field = data_block + 32;
    let table = field + u32_at(field)? as usize;
    let count = u32_at(field + 4)? as usize;
    (0..count.min(16)).find_map(|i| {
        let e = table + i * 12;
        if u32_at(e)? != 3 {
            return None;
        }
        let start = e + 4 + u32_at(e + 4)? as usize;
        u16_at(start + 4).map(|h| h as usize)
    })
}

/// Scaled YCoCg stored as R = Co, G = Cg, B = scale, A = Y (J.M.P. van Waveren, 2007).
/// The result is opaque.
fn ycocg_to_rgb(rgba: &mut [u8]) {
    for p in rgba.chunks_exact_mut(4) {
        let scale = p[2] as f32 / 8.0 + 1.0;
        let co = (p[0] as f32 - 128.0) / 255.0 / scale;
        let cg = (p[1] as f32 - 128.0) / 255.0 / scale;
        let y = p[3] as f32 / 255.0;
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        p[0] = to_u8(y + co - cg);
        p[1] = to_u8(y + cg);
        p[2] = to_u8(y - co - cg);
        p[3] = 255;
    }
}

/// Icons are padded to a power of two; drop fully transparent rows at the bottom.
fn visible_height(rgba: &[u8], width: usize, height: usize) -> usize {
    (1..=height)
        .rev()
        .find(|&y| rgba[(y - 1) * width * 4..y * width * 4].chunks_exact(4).any(|p| p[3] > 8))
        .unwrap_or(height)
}

fn extract_jpeg(blob: &[u8]) -> Option<Vec<u8>> {
    let start = find(blob, &[0xFF, 0xD8, 0xFF])?;
    let tail = &blob[start..];
    let end = tail.windows(2).rposition(|w| w == [0xFF, 0xD9]).map(|i| i + 2).unwrap_or(tail.len());
    Some(tail[..end].to_vec())
}

/// RGB565 -> RGB888
fn rgb565(c: u16) -> [u8; 3] {
    let r = ((c >> 11) & 31) as u32;
    let g = ((c >> 5) & 63) as u32;
    let b = (c & 31) as u32;
    [(r * 255 / 31) as u8, (g * 255 / 63) as u8, (b * 255 / 31) as u8]
}

/// Decodes DXT1 (BC1) or DXT5 (BC3) blocks into RGBA8.
fn decode_bc(data: &[u8], width: usize, height: usize, dxt5: bool) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    let block_size = if dxt5 { 16 } else { 8 };
    let bw = width.div_ceil(4);
    for (bi, block) in data.chunks_exact(block_size).enumerate() {
        let (bx, by) = (bi % bw * 4, bi / bw * 4);
        if by >= height {
            break;
        }
        let (alpha, color) = if dxt5 { block.split_at(8) } else { (&block[..0], block) };

        // alpha palette (BC3)
        let mut alphas = [255u8; 16];
        if dxt5 {
            let (a0, a1) = (alpha[0] as u32, alpha[1] as u32);
            let mut pal = [0u32; 8];
            pal[0] = a0;
            pal[1] = a1;
            if a0 > a1 {
                for i in 1..7 {
                    pal[i + 1] = ((7 - i as u32) * a0 + i as u32 * a1) / 7;
                }
            } else {
                for i in 1..5 {
                    pal[i + 1] = ((5 - i as u32) * a0 + i as u32 * a1) / 5;
                }
                pal[6] = 0;
                pal[7] = 255;
            }
            let bits = alpha[2..8].iter().rev().fold(0u64, |acc, &b| (acc << 8) | b as u64);
            for (i, a) in alphas.iter_mut().enumerate() {
                *a = pal[((bits >> (3 * i)) & 7) as usize] as u8;
            }
        }

        // colour palette (BC1)
        let c0 = u16::from_le_bytes([color[0], color[1]]);
        let c1 = u16::from_le_bytes([color[2], color[3]]);
        let (p0, p1) = (rgb565(c0), rgb565(c1));
        let mix = |a: u8, b: u8, wa: u32, wb: u32| ((a as u32 * wa + b as u32 * wb) / (wa + wb)) as u8;
        let mut pal = [[0u8; 4]; 4];
        pal[0] = [p0[0], p0[1], p0[2], 255];
        pal[1] = [p1[0], p1[1], p1[2], 255];
        if c0 > c1 || dxt5 {
            pal[2] = [mix(p0[0], p1[0], 2, 1), mix(p0[1], p1[1], 2, 1), mix(p0[2], p1[2], 2, 1), 255];
            pal[3] = [mix(p0[0], p1[0], 1, 2), mix(p0[1], p1[1], 1, 2), mix(p0[2], p1[2], 1, 2), 255];
        } else {
            pal[2] = [mix(p0[0], p1[0], 1, 1), mix(p0[1], p1[1], 1, 1), mix(p0[2], p1[2], 1, 1), 255];
            pal[3] = [0, 0, 0, 0];
        }
        let idx = u32::from_le_bytes([color[4], color[5], color[6], color[7]]);

        for py in 0..4 {
            for px in 0..4 {
                let (x, y) = (bx + px, by + py);
                if x >= width || y >= height {
                    continue;
                }
                let i = py * 4 + px;
                let mut p = pal[((idx >> (2 * i)) & 3) as usize];
                if dxt5 {
                    p[3] = alphas[i];
                }
                let o = (y * width + x) * 4;
                out[o..o + 4].copy_from_slice(&p);
            }
        }
    }
    out
}

/// Cuts the embedded PNG (signature .. IEND chunk) out of a resource blob.
fn extract_png(blob: &[u8]) -> Option<Vec<u8>> {
    let start = find(blob, PNG_MAGIC)?;
    let tail = &blob[start..];
    let end = find(tail, b"IEND").map(|i| i + 8).unwrap_or(tail.len());
    Some(tail[..end.min(tail.len())].to_vec())
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `<Dota>/game/dota` derived from the configured `dota2.exe` path.
pub fn game_dir_from_exe(dota_exe: &str) -> Option<PathBuf> {
    let exe = Path::new(dota_exe);
    // dota2.exe lives in <Dota>/game/bin/win64
    let game = exe.parent()?.parent()?.parent()?;
    let dir = game.join("dota");
    dir.join("pak01_dir.vpk").is_file().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_png_between_magic_and_iend() {
        let mut blob = b"vtex-header".to_vec();
        blob.extend_from_slice(PNG_MAGIC);
        blob.extend_from_slice(b"chunks....IEND\xae\x42\x60\x82trailing");
        let png = extract_png(&blob).unwrap();
        assert!(png.starts_with(PNG_MAGIC));
        assert!(png.ends_with(b"IEND\xae\x42\x60\x82"));
    }

    #[test]
    fn no_png_means_none() {
        assert!(extract_png(b"no image here").is_none());
    }

    /// Manual check against a real install:
    /// DOTA_GAME_DIR=<Dota>/game/dota ICON_OUT=<dir> cargo test -- --ignored real_icons
    #[test]
    #[ignore]
    fn real_icons() {
        let dir = std::env::var("DOTA_GAME_DIR").expect("DOTA_GAME_DIR");
        let out = PathBuf::from(std::env::var("ICON_OUT").expect("ICON_OUT"));
        let mut pak = IconPak::open(Path::new(&dir)).unwrap();
        for icon in [
            "econ/tools/aghs_emblem_2021",
            "econ/creeps/lane_creeps/creep_dc_radiant/creep_dc_radiant",
            "econ/terrain/desert_terrain",
        ] {
            let name = icon.rsplit('/').next().unwrap();
            match pak.icon_image(icon).expect(icon) {
                IconImage::Rgba { width, height, pixels } => {
                    image::save_buffer(out.join(format!("{name}.png")), &pixels, width as u32, height as u32, image::ColorType::Rgba8).unwrap();
                    println!("{name}: {width}x{height} decoded");
                }
                IconImage::Encoded(b) => {
                    std::fs::write(out.join(format!("{name}.bin")), b).unwrap();
                    println!("{name}: embedded image");
                }
            }
        }
    }

    /// DOTA_GAME_DIR=... CATALOG=<catalog.json> cargo test -- --ignored icon_coverage --nocapture
    #[test]
    #[ignore]
    fn icon_coverage() {
        let dir = std::env::var("DOTA_GAME_DIR").expect("DOTA_GAME_DIR");
        let cat: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(std::env::var("CATALOG").unwrap()).unwrap()).unwrap();
        let mut pak = IconPak::open(Path::new(&dir)).unwrap();
        let (mut ok, mut missing, mut undecodable) = (0, 0, Vec::new());
        for s in cat["skins"].as_array().unwrap() {
            let Some(icon) = s["icon"].as_str() else { continue };
            if pak.icon_blob(icon).is_none() {
                missing += 1;
            } else if pak.icon_image(icon).is_some() {
                ok += 1;
            } else {
                undecodable.push(icon.to_string());
            }
        }
        println!("decoded {ok}, not in pak {missing}, undecodable {}", undecodable.len());
        println!("examples: {:?}", &undecodable[..undecodable.len().min(5)]);
    }

    #[test]
    fn ycocg_pixel_matches_reference() {
        // value taken from a real icon; Source2Viewer decodes it as (141, 142, 91)
        let mut px = [230u8, 182, 24, 129];
        ycocg_to_rgb(&mut px);
        assert!((px[0] as i32 - 141).abs() <= 1);
        assert!((px[1] as i32 - 142).abs() <= 1);
        assert!((px[2] as i32 - 91).abs() <= 1);
        assert_eq!(px[3], 255);
    }

    #[test]
    fn dxt1_solid_block() {
        // c0 = pure red (565: 0xF800), c1 = black, all indices 0 -> every pixel red
        let block = [0x00, 0xF8, 0x00, 0x00, 0, 0, 0, 0];
        let rgba = decode_bc(&block, 4, 4, false);
        assert!(rgba.chunks_exact(4).all(|p| p == [255, 0, 0, 255]));
    }

    #[test]
    fn dxt5_alpha_and_trim() {
        // alpha0 = 0, alpha1 = 0 -> fully transparent block; colour irrelevant
        let mut data = vec![0u8; 16 * 2]; // 4x8 image = two blocks stacked
        // first (top) block opaque: alpha0 = 255, indices 0
        data[0] = 255;
        data[1] = 255;
        let rgba = decode_bc(&data, 4, 8, true);
        assert_eq!(rgba[3], 255);
        assert_eq!(rgba[(7 * 4) * 4 + 3], 0);
        assert_eq!(visible_height(&rgba, 4, 8), 4);
    }
}

// ---------------------------------------------------------------- writer

/// Writes a single-file VPK v2 (`<name>_dir.vpk` with all data inline).
/// `files` are (path inside the pak, bytes); paths use '/' and are lower-cased.
/// Source 2 mounts the v2 layout (28-byte header); v1 also loads, but v2 is
/// what the engine itself writes, so stick to it.
pub fn write_vpk(path: &Path, files: &[(String, Vec<u8>)]) -> Result<(), String> {
    use std::collections::BTreeMap;

    // ext -> dir -> [(name, file index)]
    let mut tree: BTreeMap<String, BTreeMap<String, Vec<(String, usize)>>> = BTreeMap::new();
    for (i, (p, _)) in files.iter().enumerate() {
        let norm = p.replace('\\', "/").trim_start_matches('/').to_lowercase();
        let (dir, file) = match norm.rsplit_once('/') {
            Some((d, f)) => (d.to_string(), f.to_string()),
            None => (" ".to_string(), norm.clone()),
        };
        let (name, ext) = match file.rsplit_once('.') {
            Some((n, e)) => (n.to_string(), e.to_string()),
            None => (file.clone(), " ".to_string()),
        };
        if name.is_empty() {
            return Err(format!("bad path in pak: {p}"));
        }
        tree.entry(ext).or_default().entry(dir).or_default().push((name, i));
    }

    let mut dir_bytes: Vec<u8> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let put_str = |buf: &mut Vec<u8>, s: &str| {
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    };
    for (ext, dirs) in &tree {
        put_str(&mut dir_bytes, ext);
        for (dir, entries) in dirs {
            put_str(&mut dir_bytes, dir);
            for (name, idx) in entries {
                let bytes = &files[*idx].1;
                put_str(&mut dir_bytes, name);
                dir_bytes.extend_from_slice(&crc32(bytes).to_le_bytes());
                dir_bytes.extend_from_slice(&0u16.to_le_bytes()); // preload bytes
                dir_bytes.extend_from_slice(&DIR_ARCHIVE.to_le_bytes());
                dir_bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
                dir_bytes.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                dir_bytes.extend_from_slice(&0xFFFFu16.to_le_bytes());
                data.extend_from_slice(bytes);
            }
            dir_bytes.push(0);
        }
        dir_bytes.push(0);
    }
    dir_bytes.push(0);

    let mut out = Vec::with_capacity(28 + dir_bytes.len() + data.len());
    out.extend_from_slice(&SIGNATURE.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes()); // version 2
    out.extend_from_slice(&(dir_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // file data section size
    out.extend_from_slice(&0u32.to_le_bytes()); // archive MD5 section (absent)
    out.extend_from_slice(&0u32.to_le_bytes()); // other MD5 section (absent)
    out.extend_from_slice(&0u32.to_le_bytes()); // signature section (absent)
    out.extend_from_slice(&dir_bytes);
    out.extend_from_slice(&data);
    std::fs::write(path, out).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn crc32_known_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn written_pak_is_readable() {
        let dir = std::env::temp_dir().join(format!("vpk_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut png = PNG_MAGIC.to_vec();
        png.extend_from_slice(b"dataIEND\xae\x42\x60\x82");
        let mut vtex = b"header".to_vec();
        vtex.extend_from_slice(&png);
        write_vpk(
            &dir.join("pak01_dir.vpk"),
            &[
                ("panorama/images/econ/items/test/item_png.vtex_c".into(), vtex),
                ("models/heroes/x/cape.vmdl_c".into(), b"model".to_vec()),
            ],
        )
        .unwrap();
        let mut pak = IconPak::open(&dir).unwrap();
        assert_eq!(pak.len(), 1);
        assert_eq!(pak.icon_png("econ/items/test/item").unwrap(), png);
        let mut all = read_all(&dir.join("pak01_dir.vpk")).unwrap();
        all.sort();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "models/heroes/x/cape.vmdl_c");
        assert_eq!(all[0].1, b"model");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Diagnostic against the real game pak: run with
    /// `cargo test read_file_real_pak -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn read_file_real_pak() {
        let dota_exe = std::env::var("DOTA_EXE").unwrap_or_else(|_| {
            r"C:\Program Files (x86)\Steam\steamapps\common\dota 2 beta\game\bin\win64\dota2.exe"
                .into()
        });
        let Some(game_dir) = game_dir_from_exe(&dota_exe) else {
            panic!("dota2.exe не найден: {dota_exe}");
        };
        for path in [
            "models/heroes/antimage/antimage_weapon.vmdl_c",
            "models/heroes/axe/axe.vmdl_c",
            "models/items/axe/ti8_axe_violent_prisoner_of_war_armor/ti8_axe_violent_prisoner_of_war_armor.vmdl_c",
        ] {
            match read_file(&game_dir, path) {
                Ok(Some(bytes)) => println!("{path}: {} байт", bytes.len()),
                Ok(None) => println!("{path}: НЕТ В VPK"),
                Err(e) => println!("{path}: ошибка {e}"),
            }
        }
    }
}
