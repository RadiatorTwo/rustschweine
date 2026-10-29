//! A mission's objects (trees, buildings, crates: afDrawObj) drawn by the GPU instead of the
//! engine. The engine transforms every vertex of an object into the camera's space, lights its
//! normals and projects each triangle's corners on the CPU (recompiled x87), then appends them
//! to its page buffers. We keep each model's triangles in a vertex buffer (in the model's own
//! space, loaded again only when the model changes) and do the transform, the light and the
//! projection in a vertex shader as the engine does (0x10011810, 0x10011600, 0x10008080 and
//! 0x100085e0), with the object's matrix as uniforms.
//!
//! What the engine keeps (an object `o`, its model `m = [o+0x44]`):
//! - o+4: the object's rotation (3 x 3, rows) and position (+0x28); a point p of the model is at
//!   p * R + t in the world, the camera's matrix (0x11b8a570, the same layout) takes it on;
//!   o+0x48 the way to draw (0 normal, 1 in front of everything, 2 grey without fog,
//!   3 with animated textures 0x76/0x77); o+0x4c the alpha it fades to, o+0x50 its alpha now.
//! - m+0: vertices (16 bytes: x, y, z), m+0x10 their number; m+8: normals (16 bytes), m+0xc
//!   their number; m+4: the faces (+0x10 the number of triangles, then the triangles of 0x20
//!   bytes, then the number of quads and the quads of 0x24 bytes).
//! - A triangle: u, v bytes of its 3 corners +0, vertex indices +6, normal indices +0xc (u16),
//!   texture page +0x14 (-1: not drawn), texture +0x18, alpha +0x1c (written for each draw).
//!   A quad: 4 corners' u, v +0, vertices +8, normals +0x10, page +0x18, texture +0x1c, alpha
//!   +0x20; drawn as the triangles 0 1 2 and 2 3 0.
//! - Textures: 40-byte records at 0x11ec1180: u, v offset bytes +0, +1; +0x20 not 0: drawn
//!   after the others (the engine's second set of page buffers).
//!
//! - With bones (afDrawObj's second argument not 0: the hogs, from afDrawAnimModel): a vertex's
//!   w (+0xc) picks its matrix, [0x1002ec28 + (w + 15 [0x11cc0584]) * 4] of the 15 bones at
//!   [0x11cc0c50] (each times the camera's; the object's own matrix is not used, nor is the
//!   view clamped); all normals turn with the bone of the first normal's w. Pages 0 and 1 are
//!   drawn with the textures of page (argument >> 8) & 0xf and 1; alpha fades to [0x1002c688].
//!
//! - Way 2 (the sky): fog off while it is drawn (and on after it), not tested for visibility,
//!   the colour grey [0x1002be68] with the object's alpha, depth 0.9.
//!
//! Hogs move smoothly between the game's steps (`smooth`): their bones are kept from the last
//! two steps' pictures and blended in the pictures between (`fps::blend`).
//!
//! `HOGS_OBJECTS=engine`: all by the engine (for comparing);
//! `HOGS_OBJECTS=check`: our vertices compared with the engine's.

use super::{d3d, guest_call, log, mem};
use crate::rt::{Cpu, ESP, call_original};
use glow::HasContext;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub const DRAW_OBJ: u32 = 0x1000_d440;
/// Draws a hog: works out its bones, draws it (afDrawObj with bones), then what it holds.
pub const DRAW_ANIM_MODEL: u32 = 0x1000_da10;
/// The object's visibility (object, out: 3 shorts, its position by the camera): 0 hidden.
const VISIBLE: u32 = 0x1001_0610;
/// Drawn even when hidden (when not 0).
const ALWAYS: u32 = 0x1004_cf88;
const CAMERA: u32 = 0x11b8_a570;
/// Focal length (float) and the screen's centre (integers).
const FOCAL: u32 = 0x1013_1b1c;
const CENTRE: (u32, u32) = (0x11ab_8ca0, 0x11ab_8d30);
/// The light: direction (3 floats), ambient, strength of the direction.
const LIGHT: u32 = 0x11c9_4b98;
const AMBIENT: u32 = 0x11c9_4b70;
const DIRECT: u32 = 0x11c9_4bbc;
/// A vertex's colour: base (integers) + shade x factor (floats), red, green, blue.
const BASE: [u32; 3] = [0x1013_e6a0, 0x1013_e65c, 0x1013_e648];
const FACTOR: [u32; 3] = [0x11ab_8b08, 0x11ab_8b04, 0x11ab_8af8];
/// Way 2's grey.
const GREY: u32 = 0x1002_be68;
/// Texture surfaces by page (8 bytes each).
const PAGE_TEX: u32 = 0x11a2_5d20;
const TEXTURES: u32 = 0x11ec_1180;
/// The animated textures' frames (way 3: textures 0x76 and 0x77).
const ANIM: (u32, u32) = (0x11c9_483c, 0x11c9_4840);
/// Bones: 15 matrices (pointer), which a vertex's w picks (sets of 15), the set.
const BONES: u32 = 0x11cc_0c50;
const BONE_MAP: u32 = 0x1002_ec28;
const BONE_SET: u32 = 0x11cc_0584;
/// The alpha hogs fade to.
const BONES_ALPHA: u32 = 0x1002_c688;
/// How the fade (o+0x54 == 3) is measured, and how fast alpha follows.
const FADE_NEAR: (u32, u32) = (0x11c9_4834, 0x11c9_4830);
const FOLLOW: u32 = 0x1002_01d8;

fn ours() -> bool {
    static OURS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OURS.get_or_init(|| std::env::var("HOGS_OBJECTS").map_or(true, |v| v != "engine"))
}

/// `HOGS_OBJECTS=check`: our vertices compared with the engine's (`check`).
fn checking() -> bool {
    static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CHECK.get_or_init(|| std::env::var("HOGS_OBJECTS").is_ok_and(|v| v == "check"))
}

fn f32_at(a: u32) -> f32 {
    f32::from_bits(mem().rd32(a))
}

/// Write a word only when it changes: an extra picture (`fps.rs`) saves a page at its first
/// write, and the faces' alpha is nearly always the same.
fn set(a: u32, v: u32) {
    if mem().rd32(a) != v {
        mem().wr32(a, v);
    }
}

/// `__ftol` (crt.rs): towards zero, the low word of the 64-bit result.
fn ftol(v: f64) -> i32 {
    let i = if v.is_nan() || v >= 9.223_372_036_854_775_807e18 || v < -9.223_372_036_854_775_808e18 { i64::MIN } else { v as i64 };
    i as i32
}

thread_local! {
    /// Counts the scenes drawn: a model is checked for changes once in each.
    static SCENE: Cell<u32> = const { Cell::new(0) };
    static MODELS: RefCell<Models> = RefCell::new(Models::default());
    static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) };
}

/// A new scene is drawn (the ground is done, the objects follow).
pub fn new_scene() {
    SCENE.set(SCENE.get().wrapping_add(1));
}

pub fn draw_obj(c: &mut Cpu) {
    let m = mem();
    let esp = c.r[ESP];
    let (obj, bones) = (m.rd32(esp + 4), m.rd32(esp + 8));
    let way = m.rd32(obj + 0x48);
    // way 2 measures its fade from what is left on the stack: the engine's
    if !ours() || m.rd32(obj + 0x44) == 0 || (bones == 0 && way == 2 && m.rd32(obj + 0x54) == 3) {
        call_original(c, DRAW_OBJ);
        return;
    }
    if bones != 0 {
        smooth(m.rd32(BONES));
        set(obj + 0x4c, m.rd32(BONES_ALPHA));
        follow(obj);
        draw(obj, bones);
        if checking() {
            check(c, obj, bones);
        }
        // the engine takes its "no fog" flag from where the object's address was (its low byte)
        if obj & 0xff != 0 {
            d3d::set_render_state(28, 1);
        }
        c.r[ESP] = esp + 4;
        return;
    }
    if way == 2 {
        d3d::set_render_state(28, 0);
        follow(obj);
        draw(obj, 0);
        if checking() {
            check(c, obj, 0);
        }
        d3d::set_render_state(28, 1);
        c.r[ESP] = esp + 4;
        return;
    }
    // the visibility, with 3 shorts below our return address
    c.r[ESP] = esp - 16;
    let out = c.r[ESP];
    let visible = guest_call(c, VISIBLE, &[obj, out]);
    let (sx, sz) = (m.rd16(out) as i16 as i32, m.rd16(out + 4) as i16 as i32);
    c.r[ESP] = esp;
    if visible != 0 || m.rd32(ALWAYS) != 0 {
        fade(obj, sx, sz);
        follow(obj);
        draw(obj, 0);
        if checking() {
            check(c, obj, 0);
        }
    }
    // ret
    c.r[ESP] = esp + 4;
}

/// The bones a hog has, as the engine keeps them (15 of 12 floats: rows of three, the move).
type Bones = [[f32; 12]; 15];

/// A hog's bones in the last two steps' pictures, and the number of the last one.
struct Hog {
    prev: Bones,
    cur: Bones,
    step: u32,
}

thread_local! {
    /// by where their bones are (a hog's record + 0x7c)
    static HOGS: RefCell<HashMap<u32, Hog>> = RefCell::new(HashMap::new());
    /// bones a step's picture showed from the step before: the step's own put back after it
    static SHOWN: RefCell<Vec<(u32, Bones)>> = const { RefCell::new(Vec::new()) };
}

/// A hog moves farther than this in a step: a jump, not blended.
const JUMP: f32 = 1500.0;

/// Blend the hog's bones at `at` (just worked out for this picture) between the last two
/// steps: a step's own picture shows the step before (as the camera), the pictures after it
/// move on to this step's. In a step's picture the bones are put back afterwards
/// (`after_anim_model`); in the pictures between, the transaction undoes them.
fn smooth(at: u32) {
    let Some((t, step)) = super::fps::blend() else { return };
    let cur: Bones = std::array::from_fn(|b| std::array::from_fn(|i| f32_at(at + (b * 12 + i) as u32 * 4)));
    let extra = super::fps::in_extra();
    let shown = HOGS.with_borrow_mut(|hogs| {
        let h = hogs.entry(at).or_insert(Hog { prev: cur, cur, step: step.wrapping_sub(2) });
        if !extra && h.step != step {
            // a new step: the last one becomes the one before (when drawn then and near)
            let near = (9..12).all(|i| (cur[0][i] - h.cur[0][i]).abs() < JUMP);
            h.prev = if h.step == step.wrapping_sub(1) && near { h.cur } else { cur };
            h.cur = cur;
            h.step = step;
        }
        (h.step == step).then(|| blend_bones(&h.prev, &h.cur, t))
    });
    let Some(shown) = shown else { return };
    if !extra {
        SHOWN.with_borrow_mut(|s| s.push((at, cur)));
    }
    for (b, mat) in shown.iter().enumerate() {
        for (i, v) in mat.iter().enumerate() {
            set(at + (b * 12 + i) as u32 * 4, v.to_bits());
        }
    }
    // forget hogs not drawn for long (a new mission)
    HOGS.with_borrow_mut(|hogs| {
        if hogs.len() > 256 {
            hogs.retain(|_, h| step.wrapping_sub(h.step) < 25);
        }
    });
}

/// Draw a hog; in a step's picture put back the bones it showed from the step before.
pub fn draw_anim_model(c: &mut Cpu) {
    call_original(c, DRAW_ANIM_MODEL);
    SHOWN.with_borrow_mut(|s| {
        for (at, bones) in s.drain(..) {
            for (b, mat) in bones.iter().enumerate() {
                for (i, v) in mat.iter().enumerate() {
                    set(at + (b * 12 + i) as u32 * 4, v.to_bits());
                }
            }
        }
    });
}

/// Between two sets of bones: each matrix element in a straight line, then each row (a turned
/// and scaled axis) as long as in between (bones turn little in a step; a turn would otherwise
/// shrink them halfway). The engine's matrices are not pure turns: blending them as quaternions
/// changed them.
fn blend_bones(a: &Bones, b: &Bones, t: f32) -> Bones {
    if t <= 0.0 {
        return *a;
    }
    std::array::from_fn(|k| blend_matrix(&a[k], &b[k], t))
}

fn blend_matrix(a: &[f32; 12], b: &[f32; 12], t: f32) -> [f32; 12] {
    let len = |m: &[f32; 12], r: usize| (m[r * 3].powi(2) + m[r * 3 + 1].powi(2) + m[r * 3 + 2].powi(2)).sqrt();
    let mut out: [f32; 12] = std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t);
    for row in 0..3 {
        let (want, have) = (len(a, row) + (len(b, row) - len(a, row)) * t, len(&out, row));
        if have > 0.0 {
            for c in 0..3 {
                out[row * 3 + c] *= want / have;
            }
        }
    }
    out
}

/// The alpha the object fades to (afDrawObj, 0x1000d4fe...).
fn fade(obj: u32, sx: i32, sz: i32) {
    let m = mem();
    if m.rd32(obj + 0x54) == 3 && sz < m.rd32(FADE_NEAR.0) as i32 && (m.rd32(FADE_NEAR.1) as i32 as f32) < f32_at(obj + 0x2c) {
        let v = ftol(sx.unsigned_abs() as f64 / sz.unsigned_abs() as f64 * 255.0);
        set(obj + 0x4c, v.clamp(50, 255) as u32);
    }
}

/// Its alpha now follows a sixth of the way (0x1000d571).
fn follow(obj: u32) {
    let m = mem();
    let (to, now) = (m.rd32(obj + 0x4c) as i32, m.rd32(obj + 0x50) as i32);
    let k = f64::from_bits(m.rd32(FOLLOW) as u64 | (m.rd32(FOLLOW + 4) as u64) << 32);
    set(obj + 0x50, ftol(to.wrapping_sub(now) as f64 * k + now as f64) as u32);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
    /// the vertex's w: its bone
    bone: f32,
}

/// A model's triangles in the vertex buffer.
struct Model {
    /// the engine's data it was made from (`source`)
    source: Vec<u8>,
    checked: u32,
    /// (page + 32 for the second set, first vertex, count) in drawing order
    ranges: Vec<(u32, i32, i32)>,
}

#[derive(Default)]
struct Models {
    /// by model, and the animated textures' frames (way 3; !0 otherwise)
    by: HashMap<(u32, u32, u32), Model>,
    /// all models' vertices; loaded into the vertex buffer when `dirty`
    verts: Vec<Vertex>,
    dirty: bool,
}

/// The faces of model `md` (`m+4`): triangles then quads as (address, corners).
fn faces(md: u32) -> impl Iterator<Item = (u32, u32)> {
    let m = mem();
    let list = m.rd32(md + 4);
    let tris = (m.rd32(list + 0x10) as i32).max(0) as u32;
    let quads_at = list + 0x14 + tris * 0x20;
    let quads = (m.rd32(quads_at) as i32).max(0) as u32;
    (0..tris).map(move |i| (list + 0x14 + i * 0x20, 3)).chain((0..quads).map(move |i| (quads_at + 4 + i * 0x24, 4)))
}

/// A face's texture page, its texture and where its alpha is.
fn face_fields(f: u32, n: u32) -> (u32, u32, u32) {
    let m = mem();
    let at = if n == 3 { 0x14 } else { 0x18 };
    (m.rd32(f + at), m.rd32(f + at + 4), f + at + 8)
}

/// The texture whose u, v offsets a face takes (way 3: the animated ones' frames).
fn shown_texture(tex: u32, anim: (u32, u32)) -> u32 {
    match tex {
        0x76 if anim.0 != !0 => anim.0.wrapping_add(0x78),
        0x77 if anim.1 != !0 => anim.1.wrapping_add(0x7f),
        t => t,
    }
}

/// What a model's triangles are made from, as bytes: its counts, vertices, normals, faces
/// (without their alpha) and the textures' offsets and sets.
fn source(md: u32, anim: (u32, u32), out: &mut Vec<u8>) {
    let m = mem();
    out.clear();
    let (nv, nn) = (m.rd32(md + 0x10).min(0x10000), m.rd32(md + 0xc).min(0x10000));
    for w in [nv, nn, m.rd32(md), m.rd32(md + 8), m.rd32(md + 4)] {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.extend_from_slice(m.slice(m.rd32(md), (nv * 16) as usize));
    out.extend_from_slice(m.slice(m.rd32(md + 8), (nn * 16) as usize));
    for (f, n) in faces(md) {
        let (_, tex, alpha) = face_fields(f, n);
        out.extend_from_slice(m.slice(f, (alpha - f) as usize));
        let shown = shown_texture(tex, anim);
        out.extend_from_slice(m.slice(TEXTURES + shown.wrapping_mul(40), 2));
        out.extend_from_slice(&m.rd32(TEXTURES + tex.wrapping_mul(40) + 0x20).to_le_bytes());
    }
}

/// A face's triangles (`f`, `n` corners) as the engine's: a quad as 0 1 2, 2 3 0.
fn build_face(f: u32, n: u32, md: u32, anim: (u32, u32), out: &mut Vec<Vertex>) {
    let m = mem();
    let (nv, nn) = (m.rd32(md + 0x10).min(0x10000), m.rd32(md + 0xc).min(0x10000));
    let (vs, ns) = (m.rd32(md), m.rd32(md + 8));
    let shown = TEXTURES + shown_texture(face_fields(f, n).1, anim).wrapping_mul(40);
    let (vi, ni) = if n == 3 { (6, 0xc) } else { (8, 0x10) };
    let corner = |k: u32| {
        let v = (m.rd16(f + vi + 2 * k) as u32).min(nv.saturating_sub(1));
        let nr = (m.rd16(f + ni + 2 * k) as u32).min(nn.saturating_sub(1));
        let p = vs + v * 16;
        let q = ns + nr * 16;
        // as the engine: 1/512 + (byte + offset) / 256
        let uv = |b: u32| 0.001953125 + (m.rd8(f + 2 * k + b) as u32 + m.rd8(shown + b) as u32) as f32 * 0.00390625;
        Vertex {
            pos: [f32_at(p), f32_at(p + 4), f32_at(p + 8)],
            normal: [f32_at(q), f32_at(q + 4), f32_at(q + 8)],
            uv: [uv(0), uv(1)],
            bone: f32_at(p + 12),
        }
    };
    if n == 3 {
        out.extend([corner(0), corner(1), corner(2)]);
    } else {
        out.extend([corner(0), corner(1), corner(2), corner(2), corner(3), corner(0)]);
    }
}

/// Make a model's triangles: by set (the second drawn after the first), then by page.
fn build(md: u32, anim: (u32, u32), verts: &mut Vec<Vertex>) -> Vec<(u32, i32, i32)> {
    let m = mem();
    let mut by: Vec<Vec<Vertex>> = vec![Vec::new(); 64];
    for (f, n) in faces(md) {
        let (page, tex, _) = face_fields(f, n);
        if page >= 32 {
            continue;
        }
        let late = m.rd32(TEXTURES + tex.wrapping_mul(40) + 0x20) != 0;
        build_face(f, n, md, anim, &mut by[page as usize + 32 * late as usize]);
    }
    let mut ranges = Vec::new();
    for (k, list) in by.into_iter().enumerate() {
        if !list.is_empty() {
            ranges.push((k as u32, verts.len() as i32, list.len() as i32));
            verts.extend(list);
        }
    }
    ranges
}

/// The ranges of model `md`'s triangles, made or checked first.
fn model(md: u32, anim: (u32, u32)) -> Vec<(u32, i32, i32)> {
    thread_local! {
        static BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    let scene = SCENE.get();
    MODELS.with_borrow_mut(|ms| {
        let key = (md, anim.0, anim.1);
        if let Some(model) = ms.by.get_mut(&key)
            && model.checked == scene
        {
            return model.ranges.clone();
        }
        BUF.with_borrow_mut(|buf| {
            source(md, anim, buf);
            if let Some(model) = ms.by.get_mut(&key)
                && model.source == *buf
            {
                model.checked = scene;
                return model.ranges.clone();
            }
            // new or changed: its triangles go after the others' (all made again when too many)
            if ms.verts.len() > 1 << 21 {
                ms.verts.clear();
                ms.by.clear();
            }
            let ranges = build(md, anim, &mut ms.verts);
            ms.dirty = true;
            ms.by.insert(key, Model { source: buf.clone(), checked: scene, ranges: ranges.clone() });
            ranges
        })
    })
}

const VS: &str = r#"
layout(location = 0) in vec3 pos;
layout(location = 1) in vec3 normal;
layout(location = 2) in vec2 tc;
layout(location = 3) in float bone;
uniform vec2 size;
// the object's and camera's matrices as one (columns of the engine's rows, and the move); with
// bones 32 of them, picked by the vertex's bone
uniform vec3 view[128];
// the object's rotation, for the normals
uniform vec3 turn[3];
uniform vec3 light;
// ambient, strength of the light's direction, focal length
uniform vec3 shade;
uniform vec2 centre;
uniform vec3 base;
uniform vec3 tint;
// alpha, way (1: in front of everything), bones
uniform vec3 how;
out vec4 diffuse;
out vec4 specular;
out vec2 uv;
out float eye;
void main() {
    // into the camera's space (0x10011810): no nearer than 1.527e-5; with bones (0x10011710)
    // as it is (the engine leaves out triangles behind the camera, GL cuts them). Behind the
    // camera not as the engine: it put those corners on the near plane, so a tree the camera
    // walks past was drawn huge over the whole picture (dark stripes); kept behind, GL cuts them
    int k = how.z != 0.0 ? clamp(int(bone), 0, 31) * 4 : 0;
    vec3 v = vec3(dot(pos, view[k]), dot(pos, view[k + 1]), dot(pos, view[k + 2])) + view[k + 3];
    float z = how.z != 0.0 || v.z < 0.0 ? v.z : max(v.z, 1.527e-5);
    // projected as 0x10008080 does: focal / z, rhw 8 / z, depth z * 2e-6
    vec2 s = centre + vec2(v.x, -v.y) * (shade.z / z);
    float depth = how.y == 1.0 ? 0.0 : how.y == 2.0 ? 0.9 : z * 2e-6;
    float w = z / 8.0;
    vec2 p = (s + 0.5) / size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, depth * 2.0 - 1.0, 1.0) * w;
    eye = w;
    // the light (0x10011600): ambient, the direction's part when facing it, at most 2031616,
    // / 8192; the colour base + shade / 255 x factor, each towards zero
    vec3 n = vec3(dot(normal, turn[0]), dot(normal, turn[1]), dot(normal, turn[2]));
    float d = dot(n, light);
    float l = d > 0.0 ? shade.x + d * shade.y : shade.x;
    l = min(l, 2031616.0) * (1.0 / 8192.0);
    vec3 c = clamp(trunc(base + l * (1.0 / 255.0) * tint), 0.0, 255.0);
    diffuse = vec4(c / 255.0, how.x / 255.0);
    specular = vec4(1.0);
    uv = tc;
}
"#;

struct Gpu {
    prog: d3d::OwnProgram,
    u: [d3d::Uniform; 8],
    vao: glow::VertexArray,
    vbo: glow::Buffer,
}

fn gpu(gl: &glow::Context) -> Gpu {
    unsafe {
        let prog = d3d::own_program(gl, VS);
        let u = ["view", "turn", "light", "shade", "centre", "base", "tint", "how"].map(|n| d3d::Uniform::new(gl, &prog, n));
        let vao = gl.create_vertex_array().expect("GL vertex array");
        let vbo = gl.create_buffer().expect("GL buffer");
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let stride = size_of::<Vertex>() as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 3, glow::FLOAT, false, stride, 12);
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_f32(2, 2, glow::FLOAT, false, stride, 24);
        gl.enable_vertex_attrib_array(3);
        gl.vertex_attrib_pointer_f32(3, 1, glow::FLOAT, false, stride, 32);
        gl.bind_vertex_array(None);
        Gpu { prog, u, vao, vbo }
    }
}

/// The object's matrix times the camera's (0x10010910), in doubles rounded as the engine's
/// floats: rows of three, then the move.
fn combined(obj: [f32; 12], cam: [f32; 12]) -> [f32; 12] {
    let (a, b) = (obj.map(|v| v as f64), cam.map(|v| v as f64));
    let mut out = [0.0; 12];
    for r in 0..4 {
        for c in 0..3 {
            let v = a[r * 3] * b[c] + a[r * 3 + 1] * b[3 + c] + a[r * 3 + 2] * b[6 + c];
            out[r * 3 + c] = (if r == 3 { v + b[9 + c] } else { v }) as f32;
        }
    }
    out
}

/// An object's uniforms (see the vertex shader).
struct Look {
    view: Vec<[f32; 3]>,
    turn: [[f32; 3]; 3],
    light: [f32; 3],
    shade: [f32; 3],
    centre: [f32; 2],
    base: [f32; 3],
    tint: [f32; 3],
    how: [f32; 3],
}

impl Look {
    fn of(obj: u32, bones: u32) -> Look {
        let m = mem();
        let alpha = m.rd32(obj + 0x50);
        let way = m.rd32(obj + 0x48);
        let rd12 = |a: u32| std::array::from_fn::<f32, 12, _>(|i| f32_at(a + 4 * i as u32));
        let cols = |c: [f32; 12]| [[c[0], c[3], c[6]], [c[1], c[4], c[7]], [c[2], c[5], c[8]], [c[9], c[10], c[11]]];
        let cam = rd12(CAMERA);
        let (view, rot) = if bones != 0 {
            let base = m.rd32(BONES);
            let set = m.rd32(BONE_SET).wrapping_mul(15);
            let view = (0..32)
                .flat_map(|k: u32| {
                    let b = m.rd32(BONE_MAP + k.wrapping_add(set).wrapping_mul(4)).min(14);
                    cols(combined(rd12(base + b * 0x30), cam))
                })
                .collect();
            let md = m.rd32(obj + 0x44);
            let first = (ftol(f32_at(m.rd32(md + 8) + 12) as f64) as u32).min(14);
            (view, rd12(base + first * 0x30))
        } else {
            let rot = rd12(obj + 4);
            (cols(combined(rot, cam)).to_vec(), rot)
        };
        Look {
            view,
            turn: [[rot[0], rot[3], rot[6]], [rot[1], rot[4], rot[7]], [rot[2], rot[5], rot[8]]],
            light: [f32_at(LIGHT), f32_at(LIGHT + 4), f32_at(LIGHT + 8)],
            shade: [f32_at(AMBIENT), f32_at(DIRECT), f32_at(FOCAL)],
            centre: [
                m.rd32(CENTRE.0).wrapping_sub(alpha >> 20) as i32 as f32,
                m.rd32(CENTRE.1).wrapping_add((alpha as i32 >> 8 & 0xfff) as u32) as i32 as f32,
            ],
            // way 2: grey, unlit
            base: if way == 2 { [m.rd32(GREY) as i32 as f32; 3] } else { BASE.map(|a| m.rd32(a) as i32 as f32) },
            tint: if way == 2 { [0.0; 3] } else { FACTOR.map(f32_at) },
            how: [(alpha & 0xff) as f32, way as f32, (bones != 0) as u32 as f32],
        }
    }

    /// The vertex shader on the CPU (`check`): screen x, y, depth, rhw, colour.
    fn vertex(&self, v: &Vertex) -> ([f32; 4], u32) {
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let p = v.pos;
        let k = if self.how[2] != 0.0 { (v.bone as i32).clamp(0, 31) as usize * 4 } else { 0 };
        let view = &self.view[k..k + 4];
        let w = [dot(p, view[0]) + view[3][0], dot(p, view[1]) + view[3][1], dot(p, view[2]) + view[3][2]];
        let z = if self.how[2] != 0.0 { w[2] } else { w[2].max(1.527e-5) };
        let k = self.shade[2] / z;
        let depth = if self.how[1] == 1.0 { 0.0 } else if self.how[1] == 2.0 { 0.9 } else { z * 2e-6 };
        let n = [dot(v.normal, self.turn[0]), dot(v.normal, self.turn[1]), dot(v.normal, self.turn[2])];
        let d = dot(n, self.light);
        let l = if d > 0.0 { self.shade[0] + d * self.shade[1] } else { self.shade[0] };
        let l = l.min(2031616.0) * (1.0 / 8192.0);
        let ch = |i: usize| (self.base[i] + l * (1.0 / 255.0) * self.tint[i]).trunc().clamp(0.0, 255.0) as u32;
        let colour = (self.how[0] as u32) << 24 | ch(0) << 16 | ch(1) << 8 | ch(2);
        ([self.centre[0] + w[0] * k, self.centre[1] - w[1] * k, depth, 8.0 / z], colour)
    }
}

/// Our vertices of each face against the engine's (its functions run on the same face; their
/// output taken back out of the page buffers): the worst differences, logged now and then.
fn check(c: &mut Cpu, obj: u32, bones: u32) {
    thread_local! {
        static WORST: RefCell<(u32, [f32; 5], [u32; 4])> = const { RefCell::new((0, [0.0; 5], [0; 4])) };
    }
    let m = mem();
    let md = m.rd32(obj + 0x44);
    let way = m.rd32(obj + 0x48);
    let anim = if way == 3 { (m.rd32(ANIM.0), m.rd32(ANIM.1)) } else { (!0, !0) };
    if bones != 0 {
        guest_call(c, 0x1001_14e0, &[m.rd32(BONES), m.rd32(md + 0xc), m.rd32(md + 8)]);
        guest_call(c, 0x1001_1710, &[obj + 4, m.rd32(md + 0x10), m.rd32(md)]);
    } else {
        guest_call(c, 0x1001_1600, &[obj + 4, m.rd32(md + 0xc), m.rd32(md + 8)]);
        guest_call(c, 0x1001_1810, &[obj + 4, m.rd32(md + 0x10), m.rd32(md)]);
    }
    let look = Look::of(obj, bones);
    let mut ours = Vec::new();
    for (f, n) in faces(md) {
        let (page, tex, _) = face_fields(f, n);
        if page >= 32 {
            continue;
        }
        let late = m.rd32(TEXTURES + tex.wrapping_mul(40) + 0x20) != 0;
        let (count, buf) = if late { (0x11ab_8cb0 + page * 4, 0x11a2_6520 + page * 0x4000) } else { (0x1013_e440 + page * 4, 0x1013_ed08 + page * 0xc_7380) };
        let before = m.rd32(count);
        guest_call(c, if n == 3 { 0x1000_8080 } else { 0x1000_85e0 }, &[f, 0x11c7_eee8, way]);
        // left out (behind the camera, with bones)
        if m.rd32(count) == before {
            continue;
        }
        m.wr32(count, before);
        ours.clear();
        build_face(f, n, md, anim, &mut ours);
        WORST.with_borrow_mut(|(faces, w, cw)| {
            for (k, v) in ours.iter().enumerate() {
                let e = buf + (before + k as u32) * 32;
                let (p, colour) = look.vertex(v);
                let theirs = [f32_at(e), f32_at(e + 4), f32_at(e + 8), f32_at(e + 12)];
                for i in 0..4 {
                    let d = if i < 2 { if theirs[i].abs() < 2000.0 { (p[i] - theirs[i]).abs() } else { 0.0 } } else { ((p[i] - theirs[i]) / theirs[i].abs().max(1e-30)).abs() };
                    w[i] = w[i].max(d);
                }
                w[4] = w[4].max((v.uv[0] - f32_at(e + 24)).abs().max((v.uv[1] - f32_at(e + 28)).abs()));
                let tc = m.rd32(e + 16);
                for (i, s) in [24, 16, 8, 0].into_iter().enumerate() {
                    cw[i] = cw[i].max(((colour >> s) & 0xff).abs_diff((tc >> s) & 0xff));
                }
            }
            *faces += 1;
            if *faces % 20000 == 0 {
                log!("objects check: {faces} faces; worst x {} y {} pixels (within 2000), depth {:e} rhw {:e} (relative), uv {}, colour argb {:?}", w[0], w[1], w[2], w[3], w[4], cw);
            }
        });
    }
}

fn draw(obj: u32, bones: u32) {
    let m = mem();
    let md = m.rd32(obj + 0x44);
    let way = m.rd32(obj + 0x48);
    let alpha = m.rd32(obj + 0x50);
    // each face gets the object's alpha, as the engine writes it
    for (f, n) in faces(md) {
        set(face_fields(f, n).2, alpha);
    }
    let anim = if way == 3 { (m.rd32(ANIM.0), m.rd32(ANIM.1)) } else { (!0, !0) };
    let ranges = model(md, anim);
    if ranges.is_empty() {
        return;
    }
    let look = Look::of(obj, bones);
    // with bones the first set's page 0 shows the page the argument names
    let page = |k: u32| if bones != 0 && k == 0 { bones >> 8 & 0xf } else { k % 32 };
    let pages: Vec<u32> = ranges.iter().map(|r| m.rd32(PAGE_TEX + page(r.0) * 8)).collect();
    d3d::own_draw(|own| {
        let gl = own.gl;
        GPU.with_borrow_mut(|g| {
            let g = g.get_or_insert_with(|| gpu(gl));
            unsafe {
                MODELS.with_borrow_mut(|ms| {
                    if ms.dirty {
                        gl.bind_buffer(glow::ARRAY_BUFFER, Some(g.vbo));
                        let bytes = std::slice::from_raw_parts(ms.verts.as_ptr() as *const u8, size_of_val(&ms.verts[..]));
                        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
                        ms.dirty = false;
                    }
                });
                gl.bind_vertex_array(Some(g.vao));
                for (k, &(_, first, count)) in ranges.iter().enumerate() {
                    own.apply(&g.prog, pages[k]);
                    if k == 0 {
                        let (u, l) = (&mut g.u, &look);
                        u[0].set(gl, 3, l.view.as_flattened());
                        u[1].set(gl, 3, l.turn.as_flattened());
                        u[2].set(gl, 3, &l.light);
                        u[3].set(gl, 3, &l.shade);
                        u[4].set(gl, 2, &l.centre);
                        u[5].set(gl, 3, &l.base);
                        u[6].set(gl, 3, &l.tint);
                        u[7].set(gl, 3, &l.how);
                    }
                    gl.draw_arrays(glow::TRIANGLES, first, count);
                }
                gl.bind_vertex_array(None);
            }
        });
    });
    if d3d::render_state(9) == 1 {
        static SAID: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        if SAID.set(()).is_ok() {
            log!("warn: objects: flat shading is not done by our objects");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn about z by `angle`, scaled (rows of three, then the move).
    fn turned(angle: f32, scale: f32, at: [f32; 3]) -> [f32; 12] {
        let (s, c) = angle.sin_cos();
        let r = [c, -s, 0.0, s, c, 0.0, 0.0, 0.0, 1.0];
        std::array::from_fn(|i| if i < 9 { r[i] * scale } else { at[i - 9] })
    }

    #[test]
    fn bones_blend_halfway() {
        let h = blend_matrix(&turned(0.0, 2.0, [0.0; 3]), &turned(1.0, 2.0, [10.0, 0.0, 4.0]), 0.5);
        let want = turned(0.5, 2.0, [5.0, 0.0, 2.0]);
        for i in 0..12 {
            assert!((h[i] - want[i]).abs() < 1e-5, "{h:?} {want:?}");
        }
    }

    #[test]
    fn bones_blend_ends() {
        let (a, b) = (turned(0.3, 1.0, [1.0, 2.0, 3.0]), turned(0.6, 1.5, [4.0, 5.0, 6.0]));
        assert_eq!(blend_bones(&[a; 15], &[b; 15], 0.0)[3], a);
        let end = blend_matrix(&a, &b, 1.0);
        for i in 0..12 {
            assert!((end[i] - b[i]).abs() < 1e-5);
        }
    }
}
