//! 3D item preview.
//!
//! The item's model (`model_player` from the schema, a compiled Source 2
//! `.vmdl_c` inside the game's VPK) is exported to glTF by Source2Viewer-CLI
//! (ValveResourceFormat, MIT) into a local cache, then drawn with a small
//! OpenGL renderer inside an egui window. Game files are only read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::{self, Color32, Pos2, Rect, Rounding, Sense, Stroke};
use eframe::glow::{self, HasContext};

use crate::theme::{self, bold_font};

pub const CLI_NAME: &str = "Source2Viewer-CLI.exe";
/// Camera distance for a model normalised to radius 1 with a 35° field of view:
/// 1 / sin(17.5°) ≈ 3.33, plus a margin.
const DEFAULT_DISTANCE: f32 = 3.7;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Source2Viewer-CLI: configured path, or `tools/vrf-cli/` next to the exe or a parent.
pub fn find_cli(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.is_file().then_some(p);
    }
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent().map(Path::to_path_buf);
    for _ in 0..4 {
        let d = dir?;
        let candidate = d.join("tools").join("vrf-cli").join(CLI_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    // downloaded by the launcher on first start (downloads.rs)
    let downloaded = crate::downloads::vrf_cli_dir().join(CLI_NAME);
    downloaded.is_file().then_some(downloaded)
}

fn cache_dir() -> PathBuf {
    crate::config::app_dir().join("cache").join("models")
}

// ---------------------------------------------------------------- CPU side

struct Part {
    /// Interleaved position (3), normal (3), uv (2).
    vertices: Vec<f32>,
    indices: Vec<u32>,
    texture: Option<([usize; 2], Vec<u8>)>,
}

struct ModelData {
    parts: Vec<Part>,
    center: [f32; 3],
    radius: f32,
    triangles: usize,
}

enum Load {
    Working(&'static str),
    Ready(ModelData),
    Failed(String),
}

/// Runs Source2Viewer-CLI on the game pak; returns its message output.
fn run_cli(cli: &Path, pak: &Path, filters: &str, out_dir: &Path, extra: &[&str]) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let output = std::process::Command::new(cli)
        .arg("-i")
        .arg(pak)
        .arg("-f")
        .arg(filters)
        .arg("-o")
        .arg(format!("{}{}", out_dir.display(), std::path::MAIN_SEPARATOR))
        .args(["--gltf_export_format", "glb", "-q"])
        .args(extra)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| crate::trf!("не удалось запустить Source2Viewer-CLI: {e}", "could not start Source2Viewer-CLI: {e}"))?;
    let mut msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if msg.is_empty() {
        msg = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    Ok(msg)
}

/// Animation names stored in a .glb, read from its JSON chunk only.
fn glb_animation_names(path: &Path) -> Vec<String> {
    let Ok(data) = std::fs::read(path) else { return Vec::new() };
    if data.len() < 20 {
        return Vec::new();
    }
    let len = u32::from_le_bytes([data[12], data[13], data[14], data[15]]) as usize;
    let Some(json) = data.get(20..20 + len) else { return Vec::new() };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(json) else { return Vec::new() };
    doc.get("animations")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The plain standing animation: an "idle" without event/variant markers, shortest name.
fn pick_idle(names: &[String]) -> Option<String> {
    const SKIP: &[&str] = &[
        "injured", "rare", "debug", "effigy", "loadout", "alt", "turn", "override", "copy",
        "portrait", "taunt", "victory", "stomp", "look",
    ];
    names
        .iter()
        .filter(|n| {
            let l = n.to_lowercase();
            l.contains("idle") && !SKIP.iter().any(|s| l.contains(s))
        })
        .min_by_key(|n| n.len())
        .cloned()
}

/// Hero body exported with only its idle animation (cached), so weapons and
/// other bone-attached items end up in the hands instead of the bind pose.
/// None when no idle animation could be found; the caller falls back to bind pose.
fn posed_body(cli: &Path, pak: &Path, inner: &str, step: &dyn Fn(&'static str)) -> Option<PathBuf> {
    let root = crate::config::app_dir().join("cache");
    let rel = Path::new(inner).with_extension("glb");
    let posed_dir = root.join("posed");
    let posed = posed_dir.join(&rel);
    if posed.is_file() {
        return Some(posed);
    }
    let no_pose_marker = posed.with_extension("nopose");
    if no_pose_marker.is_file() {
        return None;
    }

    step(crate::tr!("Подбираю стойку героя (один раз)…", "Finding the hero's stance (once)…"));
    let tmp = root.join("anim_tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    run_cli(cli, pak, inner, &tmp, &["--gltf_export_animations"]).ok()?;
    let clip = pick_idle(&glb_animation_names(&tmp.join(&rel)));
    let _ = std::fs::remove_dir_all(&tmp);

    let Some(clip) = clip else {
        if let Some(parent) = no_pose_marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&no_pose_marker, b"");
        return None;
    };
    step(crate::tr!("Экспорт героя в стойке…", "Exporting the hero in the stance…"));
    run_cli(cli, pak, inner, &posed_dir, &["--gltf_textures_adapt", "--gltf_animation_list", &clip]).ok()?;
    posed.is_file().then_some(posed)
}

/// Exports the models that are not cached yet (one CLI run for all of them),
/// then parses and merges them into one scene. Runs on a worker thread.
/// With `hero`, the first model is the hero body: it is posed with its idle
/// animation and wearables are attached to its bones by name.
fn load(
    cli: &Path,
    pak: &Path,
    models: &[String],
    hero: bool,
    step: &dyn Fn(&'static str),
) -> Result<ModelData, String> {
    let out_dir = cache_dir();
    let inner: Vec<String> = models
        .iter()
        .map(|m| format!("{}_c", m.trim().trim_start_matches('/').to_lowercase()))
        .collect();
    let glb_of = |i: &String| out_dir.join(Path::new(i).with_extension("glb"));

    let body = if hero {
        inner.first().and_then(|b| posed_body(cli, pak, b, step))
    } else {
        None
    };

    step(crate::tr!("Экспорт моделей из игры…", "Exporting models from the game…"));
    let missing: Vec<&String> = inner
        .iter()
        .enumerate()
        .filter(|(n, i)| !(*n == 0 && body.is_some()) && !glb_of(i).is_file())
        .map(|(_, i)| i)
        .collect();
    let mut cli_message = String::new();
    if !missing.is_empty() {
        let filters = missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",");
        cli_message = run_cli(cli, pak, &filters, &out_dir, &["--gltf_textures_adapt"])?;
    }

    step(crate::tr!("Сборка модели…", "Building the model…"));
    let mut parts = Vec::new();
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    let mut loaded = 0usize;
    // The first model is the hero body; its bones are where the wearables'
    // same-named bones go (Source 2 "bone merge").
    let mut hero_bones: Option<HashMap<String, Mat4>> = None;
    for (n, i) in inner.iter().enumerate() {
        let glb = match (&body, n) {
            (Some(posed), 0) => posed.clone(),
            _ => glb_of(i),
        };
        if !glb.is_file() {
            continue;
        }
        let ok = if n == 0 {
            let mut bones = HashMap::new();
            let r = parse_glb(&glb, None, Some(&mut bones), &mut parts, &mut min, &mut max);
            hero_bones = Some(bones);
            r.is_ok()
        } else {
            parse_glb(&glb, hero_bones.as_ref(), None, &mut parts, &mut min, &mut max).is_ok()
        };
        if ok {
            loaded += 1;
        }
    }
    if loaded == 0 || parts.is_empty() {
        return Err(crate::trf!("модель не экспортировалась ({}){}", "the model was not exported ({}){}",
            inner.first().map(String::as_str).unwrap_or(""),
            if cli_message.is_empty() { String::new() } else { format!(": {cli_message}") }
        ));
    }
    finish(parts, min, max)
}

/// `merge_into`: bones of the model this one is worn on; `export_bones`: collect
/// this model's bone world matrices by name (for the hero body).
fn parse_glb(
    path: &Path,
    merge_into: Option<&HashMap<String, Mat4>>,
    export_bones: Option<&mut HashMap<String, Mat4>>,
    out: &mut Vec<Part>,
    min: &mut [f32; 3],
    max: &mut [f32; 3],
) -> Result<(), String> {
    let (doc, buffers, images) =
        gltf::import(path).map_err(|e| crate::trf!("не удалось прочитать glTF: {e}", "could not read glTF: {e}"))?;
    let scene = doc
        .default_scene()
        .or_else(|| doc.scenes().next())
        .ok_or(crate::tr!("в модели нет сцены", "the model has no scene"))?;
    // World matrix of every node, needed for skinning (joints can live anywhere in
    // the tree). Bones that also exist on the hero take the hero's placement;
    // their children follow from there.
    let posed = first_frame_pose(&doc, &buffers);
    let mut worlds = vec![IDENTITY; doc.nodes().len()];
    fn walk(
        node: &gltf::Node,
        parent: Mat4,
        worlds: &mut [Mat4],
        merge: Option<&HashMap<String, Mat4>>,
        posed: &HashMap<usize, Mat4>,
    ) {
        let merged = merge.and_then(|m| node.name().and_then(|n| m.get(n)));
        let w = match merged {
            Some(hero_world) => *hero_world,
            None => {
                let local = posed
                    .get(&node.index())
                    .copied()
                    .unwrap_or_else(|| flatten(node.transform().matrix()));
                mul(&parent, &local)
            }
        };
        worlds[node.index()] = w;
        for c in node.children() {
            walk(&c, w, worlds, merge, posed);
        }
    }
    for node in scene.nodes() {
        walk(&node, IDENTITY, &mut worlds, merge_into, &posed);
    }
    if let Some(bones) = export_bones {
        for node in doc.nodes() {
            if let Some(name) = node.name() {
                bones.entry(name.to_string()).or_insert(worlds[node.index()]);
            }
        }
    }
    let ctx = SceneCtx {
        buffers: &buffers,
        images: &images,
        worlds: &worlds,
    };
    for node in scene.nodes() {
        visit(&node, &ctx, out, min, max);
    }
    Ok(())
}

/// Local matrices of animated nodes at the first keyframe of the first animation
/// (the posed hero export contains just its idle animation).
fn first_frame_pose(doc: &gltf::Document, buffers: &[gltf::buffer::Data]) -> HashMap<usize, Mat4> {
    use gltf::animation::util::ReadOutputs;
    let mut out = HashMap::new();
    let Some(anim) = doc.animations().next() else {
        return out;
    };
    let mut trs: HashMap<usize, ([f32; 3], [f32; 4], [f32; 3])> = HashMap::new();
    for ch in anim.channels() {
        let node = ch.target().node();
        let entry = trs
            .entry(node.index())
            .or_insert_with(|| node.transform().decomposed());
        let reader = ch.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
        match reader.read_outputs() {
            Some(ReadOutputs::Translations(mut it)) => {
                if let Some(v) = it.next() {
                    entry.0 = v;
                }
            }
            Some(ReadOutputs::Rotations(r)) => {
                if let Some(v) = r.into_f32().next() {
                    entry.1 = v;
                }
            }
            Some(ReadOutputs::Scales(mut it)) => {
                if let Some(v) = it.next() {
                    entry.2 = v;
                }
            }
            _ => {}
        }
    }
    for (i, (t, r, s)) in trs {
        let m = gltf::scene::Transform::Decomposed {
            translation: t,
            rotation: r,
            scale: s,
        }
        .matrix();
        out.insert(i, flatten(m));
    }
    out
}

struct SceneCtx<'a> {
    buffers: &'a [gltf::buffer::Data],
    images: &'a [gltf::image::Data],
    worlds: &'a [Mat4],
}

/// `joint world × inverse bind` for every joint of a skin, in rest pose.
fn skin_matrices(skin: &gltf::Skin, ctx: &SceneCtx) -> Vec<Mat4> {
    let reader = skin.reader(|b| ctx.buffers.get(b.index()).map(|d| &d.0[..]));
    let ibms: Vec<Mat4> = reader
        .read_inverse_bind_matrices()
        .map(|it| it.map(flatten).collect())
        .unwrap_or_default();
    skin.joints()
        .enumerate()
        .map(|(i, j)| mul(&ctx.worlds[j.index()], ibms.get(i).unwrap_or(&IDENTITY)))
        .collect()
}

/// Weighted blend of joint matrices for one vertex.
fn blend(mats: &[Mat4], joints: [u16; 4], weights: [f32; 4]) -> Option<Mat4> {
    let mut out = [0.0f32; 16];
    let mut total = 0.0;
    for k in 0..4 {
        let w = weights[k];
        if w <= 0.0 {
            continue;
        }
        let m = mats.get(joints[k] as usize)?;
        for (o, v) in out.iter_mut().zip(m.iter()) {
            *o += v * w;
        }
        total += w;
    }
    (total > 1e-4).then(|| {
        if (total - 1.0).abs() > 1e-3 {
            out.iter_mut().for_each(|v| *v /= total);
        }
        out
    })
}

fn finish(out: Vec<Part>, min: [f32; 3], max: [f32; 3]) -> Result<ModelData, String> {
    if out.is_empty() {
        return Err(crate::tr!("в модели нет геометрии", "the model has no geometry").into());
    }
    let center = [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ];
    let mut radius = 0.0f32;
    for p in &out {
        for v in p.vertices.chunks_exact(8) {
            let d = ((v[0] - center[0]).powi(2) + (v[1] - center[1]).powi(2) + (v[2] - center[2]).powi(2)).sqrt();
            radius = radius.max(d);
        }
    }
    let triangles = out.iter().map(|p| p.indices.len() / 3).sum();
    Ok(ModelData {
        parts: out,
        center,
        radius: radius.max(1e-3),
        triangles,
    })
}

fn visit(
    node: &gltf::Node,
    ctx: &SceneCtx,
    out: &mut Vec<Part>,
    min: &mut [f32; 3],
    max: &mut [f32; 3],
) {
    let world = ctx.worlds[node.index()];
    let skin = node.skin().map(|s| skin_matrices(&s, ctx));
    let (buffers, images) = (ctx.buffers, ctx.images);
    if let Some(mesh) = node.mesh() {
        for prim in mesh.primitives() {
            if prim.mode() != gltf::mesh::Mode::Triangles {
                continue;
            }
            let reader = prim.reader(|b| buffers.get(b.index()).map(|d| &d.0[..]));
            // Skinned meshes ignore their node transform (glTF spec) and follow
            // the joints instead; weapons are bound to hand bones this way.
            let joints: Vec<[u16; 4]> = reader.read_joints(0).map(|j| j.into_u16().collect()).unwrap_or_default();
            let weights: Vec<[f32; 4]> = reader.read_weights(0).map(|w| w.into_f32().collect()).unwrap_or_default();
            let Some(positions) = reader.read_positions() else {
                continue;
            };
            let positions: Vec<[f32; 3]> = positions.collect();
            let normals: Vec<[f32; 3]> = reader.read_normals().map(|n| n.collect()).unwrap_or_default();
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|t| t.into_f32().collect())
                .unwrap_or_default();
            let indices: Vec<u32> = reader
                .read_indices()
                .map(|i| i.into_u32().collect())
                .unwrap_or_else(|| (0..positions.len() as u32).collect());

            let mut vertices = Vec::with_capacity(positions.len() * 8);
            for (i, p) in positions.iter().enumerate() {
                let m = match (&skin, joints.get(i), weights.get(i)) {
                    (Some(mats), Some(j), Some(w)) => blend(mats, *j, *w).unwrap_or(world),
                    _ => world,
                };
                let wp = transform_point(&m, *p);
                for k in 0..3 {
                    min[k] = min[k].min(wp[k]);
                    max[k] = max[k].max(wp[k]);
                }
                let n = normals
                    .get(i)
                    .map(|n| normalize(transform_dir(&m, *n)))
                    .unwrap_or([0.0, 1.0, 0.0]);
                let uv = uvs.get(i).copied().unwrap_or([0.0, 0.0]);
                vertices.extend_from_slice(&[wp[0], wp[1], wp[2], n[0], n[1], n[2], uv[0], uv[1]]);
            }

            let texture = prim
                .material()
                .pbr_metallic_roughness()
                .base_color_texture()
                .and_then(|info| images.get(info.texture().source().index()))
                .and_then(to_rgba);
            out.push(Part {
                vertices,
                indices,
                texture,
            });
        }
    }
    for child in node.children() {
        visit(&child, ctx, out, min, max);
    }
}

fn to_rgba(img: &gltf::image::Data) -> Option<([usize; 2], Vec<u8>)> {
    use gltf::image::Format;
    let size = [img.width as usize, img.height as usize];
    let px = match img.format {
        Format::R8G8B8A8 => img.pixels.clone(),
        Format::R8G8B8 => img
            .pixels
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        Format::R8 => img.pixels.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        _ => return None,
    };
    Some((size, px))
}

// ---------------------------------------------------------------- math (column-major)

type Mat4 = [f32; 16];
const IDENTITY: Mat4 = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

fn flatten(m: [[f32; 4]; 4]) -> Mat4 {
    let mut out = [0.0; 16];
    for c in 0..4 {
        for r in 0..4 {
            out[c * 4 + r] = m[c][r];
        }
    }
    out
}

fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [0.0; 16];
    for c in 0..4 {
        for r in 0..4 {
            out[c * 4 + r] = (0..4).map(|k| a[k * 4 + r] * b[c * 4 + k]).sum();
        }
    }
    out
}

fn transform_point(m: &Mat4, p: [f32; 3]) -> [f32; 3] {
    [
        m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
        m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
        m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
    ]
}

fn transform_dir(m: &Mat4, d: [f32; 3]) -> [f32; 3] {
    [
        m[0] * d[0] + m[4] * d[1] + m[8] * d[2],
        m[1] * d[0] + m[5] * d[1] + m[9] * d[2],
        m[2] * d[0] + m[6] * d[1] + m[10] * d[2],
    ]
}

fn normalize(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / l, v[1] / l, v[2] / l]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn perspective(fovy: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (fovy / 2.0).tan();
    let mut m = [0.0; 16];
    m[0] = f / aspect;
    m[5] = f;
    m[10] = (far + near) / (near - far);
    m[11] = -1.0;
    m[14] = 2.0 * far * near / (near - far);
    m
}

fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> Mat4 {
    let f = normalize(sub(target, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);
    [
        s[0], u[0], -f[0], 0.0,
        s[1], u[1], -f[1], 0.0,
        s[2], u[2], -f[2], 0.0,
        -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0,
    ]
}

// ---------------------------------------------------------------- GPU side

struct GpuPart {
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    ebo: glow::Buffer,
    count: i32,
    texture: glow::Texture,
    owns_texture: bool,
}

struct Gpu {
    program: glow::Program,
    white: glow::Texture,
    parts: Vec<GpuPart>,
}

const VS: &str = r#"#version 330 core
layout(location = 0) in vec3 a_pos;
layout(location = 1) in vec3 a_nrm;
layout(location = 2) in vec2 a_uv;
uniform mat4 u_mvp;
uniform mat4 u_model;
out vec3 v_nrm;
out vec2 v_uv;
void main() {
    v_nrm = mat3(u_model) * a_nrm;
    v_uv = a_uv;
    gl_Position = u_mvp * vec4(a_pos, 1.0);
}
"#;

const FS: &str = r#"#version 330 core
in vec3 v_nrm;
in vec2 v_uv;
uniform sampler2D u_tex;
uniform vec3 u_light;
uniform vec3 u_view;
out vec4 o_color;
void main() {
    vec3 base = texture(u_tex, v_uv).rgb;
    vec3 n = normalize(v_nrm);
    if (!gl_FrontFacing) n = -n;
    float diff = max(dot(n, u_light), 0.0);
    float fill = max(dot(n, -u_light), 0.0) * 0.25;
    float rim = pow(1.0 - max(dot(n, u_view), 0.0), 3.0) * 0.35;
    vec3 c = base * (0.32 + 0.85 * diff + fill) + vec3(rim);
    o_color = vec4(c, 1.0);
}
"#;

impl Gpu {
    unsafe fn new(gl: &glow::Context, model: &ModelData) -> Result<Self, String> {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
            let sh = gl.create_shader(kind)?;
            gl.shader_source(sh, src);
            gl.compile_shader(sh);
            if !gl.get_shader_compile_status(sh) {
                return Err(gl.get_shader_info_log(sh));
            }
            gl.attach_shader(program, sh);
            shaders.push(sh);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(gl.get_program_info_log(program));
        }
        for sh in shaders {
            gl.detach_shader(program, sh);
            gl.delete_shader(sh);
        }

        let white = upload_texture(gl, [1, 1], &[255, 255, 255, 255])?;
        let mut parts = Vec::new();
        for p in &model.parts {
            let vao = gl.create_vertex_array()?;
            gl.bind_vertex_array(Some(vao));
            let vbo = gl.create_buffer()?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, as_bytes(&p.vertices), glow::STATIC_DRAW);
            let ebo = gl.create_buffer()?;
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, Some(ebo));
            gl.buffer_data_u8_slice(glow::ELEMENT_ARRAY_BUFFER, as_bytes(&p.indices), glow::STATIC_DRAW);
            let stride = 8 * 4;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 3, glow::FLOAT, false, stride, 12);
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, 24);
            gl.bind_vertex_array(None);

            let (texture, owns_texture) = match &p.texture {
                Some((size, px)) => (upload_texture(gl, *size, px)?, true),
                None => (white, false),
            };
            parts.push(GpuPart {
                vao,
                vbo,
                ebo,
                count: p.indices.len() as i32,
                texture,
                owns_texture,
            });
        }
        Ok(Self {
            program,
            white,
            parts,
        })
    }

    unsafe fn paint(&self, gl: &glow::Context, mvp: &Mat4, model: &Mat4, light: [f32; 3], view: [f32; 3]) {
        gl.enable(glow::DEPTH_TEST);
        gl.depth_func(glow::LESS);
        gl.depth_mask(true);
        gl.clear_depth_f32(1.0);
        gl.clear(glow::DEPTH_BUFFER_BIT);
        gl.disable(glow::CULL_FACE);
        gl.disable(glow::BLEND);
        gl.use_program(Some(self.program));
        gl.uniform_matrix_4_f32_slice(gl.get_uniform_location(self.program, "u_mvp").as_ref(), false, mvp);
        gl.uniform_matrix_4_f32_slice(gl.get_uniform_location(self.program, "u_model").as_ref(), false, model);
        gl.uniform_3_f32(gl.get_uniform_location(self.program, "u_light").as_ref(), light[0], light[1], light[2]);
        gl.uniform_3_f32(gl.get_uniform_location(self.program, "u_view").as_ref(), view[0], view[1], view[2]);
        gl.uniform_1_i32(gl.get_uniform_location(self.program, "u_tex").as_ref(), 0);
        gl.active_texture(glow::TEXTURE0);
        for p in &self.parts {
            gl.bind_texture(glow::TEXTURE_2D, Some(p.texture));
            gl.bind_vertex_array(Some(p.vao));
            gl.draw_elements(glow::TRIANGLES, p.count, glow::UNSIGNED_INT, 0);
        }
        gl.bind_vertex_array(None);
        gl.disable(glow::DEPTH_TEST);
    }

    unsafe fn destroy(&self, gl: &glow::Context) {
        for p in &self.parts {
            gl.delete_vertex_array(p.vao);
            gl.delete_buffer(p.vbo);
            gl.delete_buffer(p.ebo);
            if p.owns_texture {
                gl.delete_texture(p.texture);
            }
        }
        gl.delete_texture(self.white);
        gl.delete_program(self.program);
    }
}

unsafe fn upload_texture(gl: &glow::Context, size: [usize; 2], rgba: &[u8]) -> Result<glow::Texture, String> {
    let tex = gl.create_texture()?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        size[0] as i32,
        size[1] as i32,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        Some(rgba),
    );
    gl.generate_mipmap(glow::TEXTURE_2D);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR_MIPMAP_LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::REPEAT as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::REPEAT as i32);
    gl.bind_texture(glow::TEXTURE_2D, None);
    Ok(tex)
}

fn as_bytes<T: Copy>(v: &[T]) -> &[u8] {
    // SAFETY: f32/u32 slices have no padding; reading them as bytes is fine.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

// ---------------------------------------------------------------- window

/// What a preview shows, so the launcher can rebuild it when the preset changes.
#[derive(Clone, PartialEq)]
pub enum Subject {
    /// A hero wearing the active preset; `forced` is shown in its slot regardless.
    Hero {
        hero: String,
        persona: bool,
        forced: Option<String>,
    },
    /// A lone item (general items, or no hero model data).
    Item,
}

pub struct Preview {
    pub title: String,
    pub subject: Subject,
    /// Model paths currently loaded (or loading).
    pub models: Vec<String>,
    cli: PathBuf,
    pak: PathBuf,
    state: Arc<Mutex<Load>>,
    gpu: Option<Arc<Mutex<Gpu>>>,
    model: Option<(Mat4, f32, usize)>, // normalising transform, radius, triangles
    yaw: f32,
    pitch: f32,
    distance: f32,
    auto_rotate: bool,
    last_frame: Instant,
    pub open: bool,
}

fn spawn_load(
    ctx: &egui::Context,
    cli: PathBuf,
    pak: PathBuf,
    models: Vec<String>,
    hero: bool,
) -> Arc<Mutex<Load>> {
    let state = Arc::new(Mutex::new(Load::Working(crate::tr!("Экспорт моделей из игры…", "Exporting models from the game…"))));
    let worker_state = Arc::clone(&state);
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("model-preview".into())
        .spawn(move || {
            let step = |msg: &'static str| {
                if let Ok(mut s) = worker_state.lock() {
                    *s = Load::Working(msg);
                }
                ctx.request_repaint();
            };
            let result = load(&cli, &pak, &models, hero, &step);
            if let Ok(mut s) = worker_state.lock() {
                *s = match result {
                    Ok(m) => Load::Ready(m),
                    Err(e) => Load::Failed(e),
                };
            }
            ctx.request_repaint();
        })
        .ok();
    state
}

impl Preview {
    pub fn new(
        ctx: &egui::Context,
        title: String,
        subject: Subject,
        cli: PathBuf,
        pak: PathBuf,
        models: Vec<String>,
    ) -> Self {
        let hero = matches!(subject, Subject::Hero { .. });
        let state = spawn_load(ctx, cli.clone(), pak.clone(), models.clone(), hero);
        Self {
            title,
            subject,
            models,
            cli,
            pak,
            state,
            gpu: None,
            model: None,
            yaw: 0.6,
            pitch: 0.15,
            distance: DEFAULT_DISTANCE,
            auto_rotate: true,
            last_frame: Instant::now(),
            open: true,
        }
    }

    /// Loads a different set of models in place, keeping the camera.
    pub fn reload(&mut self, ctx: &egui::Context, gl: Option<&Arc<glow::Context>>, models: Vec<String>) {
        self.destroy(gl);
        self.model = None;
        let hero = matches!(self.subject, Subject::Hero { .. });
        self.state = spawn_load(ctx, self.cli.clone(), self.pak.clone(), models.clone(), hero);
        self.models = models;
    }

    /// Frees GPU resources; call before dropping the preview.
    pub fn destroy(&mut self, gl: Option<&Arc<glow::Context>>) {
        if let (Some(gpu), Some(gl)) = (self.gpu.take(), gl) {
            if let Ok(g) = gpu.lock() {
                unsafe { g.destroy(gl) };
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, gl: Option<&Arc<glow::Context>>) {
        // Upload once the worker is done (needs the GL context, i.e. the UI thread).
        if self.gpu.is_none() {
            let ready = match self.state.lock() {
                Ok(mut s) => match std::mem::replace(&mut *s, Load::Working(crate::tr!("Загрузка в видеокарту…", "Uploading to the GPU…"))) {
                    Load::Ready(m) => Some(m),
                    other => {
                        *s = other;
                        None
                    }
                },
                Err(_) => None,
            };
            if let Some(m) = ready {
                match gl.map(|gl| unsafe { Gpu::new(gl, &m) }) {
                    Some(Ok(gpu)) => {
                        let s = 1.0 / m.radius;
                        let mut t = IDENTITY;
                        t[0] = s;
                        t[5] = s;
                        t[10] = s;
                        t[12] = -m.center[0] * s;
                        t[13] = -m.center[1] * s;
                        t[14] = -m.center[2] * s;
                        self.model = Some((t, m.radius, m.triangles));
                        self.gpu = Some(Arc::new(Mutex::new(gpu)));
                    }
                    Some(Err(e)) => {
                        if let Ok(mut s) = self.state.lock() {
                            *s = Load::Failed(format!("OpenGL: {e}"));
                        }
                    }
                    None => {
                        if let Ok(mut s) = self.state.lock() {
                            *s = Load::Failed(crate::tr!("нет OpenGL-контекста", "no OpenGL context").into());
                        }
                    }
                }
            }
        }

        let dt = self.last_frame.elapsed().as_secs_f32().min(0.1);
        self.last_frame = Instant::now();

        let mut open = self.open;
        egui::Window::new(egui::RichText::new(format!("3D  ·  {}", self.title)).font(bold_font(13.5)))
            .id(egui::Id::new("model_preview"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([560.0, 500.0])
            .min_size([320.0, 300.0])
            .show(ctx, |ui| self.body(ui, dt));
        self.open = open;
    }

    fn body(&mut self, ui: &mut egui::Ui, dt: f32) {
        // Height follows the width: taking all available height would make the
        // resizable window grow without bound.
        let w = ui.available_width().max(280.0);
        let size = egui::vec2(w, (w * 0.78).clamp(240.0, 900.0));
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, Rounding::same(8.0), theme::BG_DEEP);
        p.add(theme::vgradient(
            Rect::from_min_max(rect.min, Pos2::new(rect.max.x, rect.center().y)),
            theme::ACCENT.gamma_multiply(0.06),
            Color32::TRANSPARENT,
        ));
        p.rect_stroke(rect, Rounding::same(8.0), Stroke::new(1.0_f32, theme::BORDER));

        match (&self.gpu, self.model) {
            (Some(gpu), Some((normalise, _, _))) => {
                if resp.dragged() {
                    let d = resp.drag_delta();
                    self.yaw -= d.x * 0.01;
                    self.pitch = (self.pitch + d.y * 0.01).clamp(-1.45, 1.45);
                    self.auto_rotate = false;
                }
                if resp.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        self.distance = (self.distance * (-scroll * 0.0025).exp()).clamp(1.2, 10.0);
                    }
                }
                if resp.double_clicked() {
                    self.yaw = 0.6;
                    self.pitch = 0.25;
                    self.distance = DEFAULT_DISTANCE;
                    self.auto_rotate = true;
                }
                if self.auto_rotate {
                    self.yaw += dt * 0.5;
                    ui.ctx().request_repaint();
                }

                let eye = [
                    self.distance * self.pitch.cos() * self.yaw.sin(),
                    self.distance * self.pitch.sin(),
                    self.distance * self.pitch.cos() * self.yaw.cos(),
                ];
                let view = look_at(eye, [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
                let proj = perspective(35f32.to_radians(), rect.aspect_ratio(), 0.05, 50.0);
                let mvp = mul(&proj, &mul(&view, &normalise));
                let view_dir = normalize(eye);
                let light = normalize([view_dir[0] - 0.5, view_dir[1] + 0.9, view_dir[2] + 0.3]);
                let model = normalise;
                let gpu = Arc::clone(gpu);
                let callback = egui::PaintCallback {
                    rect,
                    callback: Arc::new(eframe::egui_glow::CallbackFn::new(move |_info, painter| {
                        if let Ok(g) = gpu.lock() {
                            unsafe { g.paint(painter.gl(), &mvp, &model, light, view_dir) };
                        }
                    })),
                };
                ui.painter().add(callback);
            }
            _ => {
                let msg = match self.state.lock().ok().as_deref() {
                    Some(Load::Failed(e)) => Some(Err(e.clone())),
                    Some(Load::Working(m)) => Some(Ok(m.to_string())),
                    _ => None,
                };
                match msg {
                    Some(Err(e)) => {
                        p.text(rect.center(), egui::Align2::CENTER_CENTER, crate::tr!("Не удалось показать модель", "Could not show the model"), bold_font(14.0), theme::DANGER);
                        let g = theme::galley(ui, &e, egui::FontId::proportional(11.5), theme::TEXT_DIM, rect.width() - 30.0);
                        p.galley(Pos2::new(rect.center().x - g.size().x / 2.0, rect.center().y + 14.0), g, theme::TEXT_DIM);
                    }
                    Some(Ok(m)) => {
                        let t = ui.input(|i| i.time) as f32;
                        let c = rect.center() - egui::vec2(0.0, 14.0);
                        for k in 0..8 {
                            let a = t * 4.0 + k as f32 * std::f32::consts::TAU / 8.0;
                            let alpha = ((k as f32) / 8.0 * 200.0) as u8 + 40;
                            p.circle_filled(c + egui::vec2(a.cos(), a.sin()) * 14.0, 3.0, theme::ACCENT.gamma_multiply(alpha as f32 / 255.0));
                        }
                        p.text(rect.center() + egui::vec2(0.0, 20.0), egui::Align2::CENTER_CENTER, m, egui::FontId::proportional(12.5), theme::TEXT_DIM);
                        ui.ctx().request_repaint();
                    }
                    None => {}
                }
            }
        }

        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(crate::tr!("ЛКМ: вращать · колесо: масштаб · двойной клик: сброс", "LMB: rotate · wheel: zoom · double click: reset"))
                    .size(11.5)
                    .color(theme::TEXT_FAINT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.auto_rotate, crate::tr!("Автоповорот", "Auto-rotate"));
                if let Some((_, _, tris)) = self.model {
                    let text = if self.models.len() > 1 {
                        crate::trf!("моделей: {} · {tris} треуг.", "models: {} · {tris} tris", self.models.len())
                    } else {
                        crate::trf!("{tris} треуг.", "{tris} tris")
                    };
                    ui.label(egui::RichText::new(text).size(11.5).color(theme::TEXT_FAINT));
                }
            });
        });
    }
}
