//! A mission's landscape drawn by the GPU instead of the engine: the engine projects every
//! corner of every visible tile on the CPU (recompiled x87) and sends the screen points. We
//! keep the tiles' corners in a vertex buffer (loaded again only when they change: craters)
//! and project them in a vertex shader the way the engine does (0x10013f20), so the depth
//! values match what the engine writes for the objects it draws after the ground.
//!
//! The engine's ground is switched off by its visibility masks (a block is skipped when its
//! word is not 0); ours is drawn where the engine has just drawn its own (after the page
//! buffers' flush at 0x10003b83, before 0x1000c040), so the order of drawing stays the same.
//! `HOGS_GROUND=engine`: the engine's ground (for comparing).
//!
//! The water (0x1000b440, before the ground) the same way: its 33 x 33 corners (0x68 bytes, as
//! the map's) at 0x11abe1a0 move along y each picture; the engine projects 4 corners and draws
//! each of the 32 x 32 quads by itself (a fan A B D C; the uv of all four corners kept in D).

use super::{d3d, mem};
use crate::rt::{Cpu, ESP, call_original};
use glow::HasContext;
use std::cell::RefCell;

/// Called by the scene drawing (0x10003380) right after the ground.
pub const AFTER_GROUND: u32 = 0x1000_c040;
/// Draws the water.
pub const WATER: u32 = 0x1000_b440;

/// The map's corners: 65 x 65 of 0x68 bytes. x, y, z floats at +0, +4, +8 (z up), the lit
/// colour +0xc, page +0x40 and tile on it +0x42 (a tile's from its first corner), u[4] +0x48,
/// v[4] +0x58 (the tile's four corners, rotation and flip applied).
const GRID: u32 = 0x11af_aa30;
const WATER_GRID: u32 = 0x11ab_e1a0;
const WATER_ROW: u32 = 33 * CORNER;
/// How far the water moves each picture, and back by when it went a whole 1024.
const WATER_STEP: (u32, u32) = (0x1002_0168, 0x1002_016c);
/// The engine's record of the device's state: a counter of changes, the state set (13: the
/// water's), the texture set.
const STATE_CHANGES: u32 = 0x11ab_945c;
const STATE_SET: u32 = 0x1002_bf4c;
const TEXTURE_SET: u32 = 0x11ab_9490;
const CORNER: u32 = 0x68;
const ROW: u32 = 65 * CORNER;
/// Texture pages: 32 records of 0xd4 bytes; +0 the texture surface, +0x10 a flag per tile
/// (2: the tile is not drawn, water shows there).
const PAGES: u32 = 0x1004_cf90;
const PAGE: u32 = 0xd4;
/// Visibility masks: 10 tables (by the camera's turn) of 33 x 33 blocks.
const MASKS: (u32, u32) = (0x11ed_0188, 0x11ed_5188);
/// 1 normally; 0 while the sky fades, when the ground is drawn another way (not by the masks).
const MASKED: u32 = 0x1002_2040;
const CAMERA: u32 = 0x1002_c524;
/// The screen's centre (integers).
const CENTRE: u32 = 0x1002_c51c;
/// The camera's rotation, rows of three as the engine keeps them.
const ROT: [[u32; 3]; 3] = [
    [0x11ab_d964, 0x11ab_d970, 0x11ab_e188],
    [0x11ab_d968, 0x11ab_e178, 0x11ab_e18c],
    [0x11ab_d96c, 0x11ab_e180, 0x11ab_e190],
];

/// The water's corners move along y; the quads by the GPU (0x1000b440).
pub fn water(c: &mut Cpu) {
    if !ours() {
        call_original(c, WATER);
        return;
    }
    let m = mem();
    // the device's state for it, as the engine sets it (and records that it did)
    if m.rd32(STATE_SET) != 13 {
        m.wr32(STATE_CHANGES, m.rd32(STATE_CHANGES).wrapping_add(1));
        d3d::set_render_state(9, 2);
        d3d::set_render_state(27, 1);
        d3d::set_texture_stage_state(0, 4, 4);
        m.wr32(STATE_SET, 13);
    }
    let tex = m.rd32(PAGES);
    if m.rd32(TEXTURE_SET) != tex {
        d3d::set_texture(0, tex);
        m.wr32(TEXTURE_SET, tex);
    }
    // the inner corners move; back by a step when at a whole 1024 (an extra picture would
    // only have it undone: it moves them where it reads them)
    let (step, back) = (f32_at(WATER_STEP.0), f32_at(WATER_STEP.1));
    let moved = |a: u32| {
        let v = f32_at(a) - step;
        let t = v as f64 as i64 as i32;
        if t.unsigned_abs() & 0x3ff == 0 { v - back } else { v }
    };
    let extra = super::fps::in_extra();
    if !extra {
        for col in 1..32 {
            for row in 1..32 {
                let a = WATER_GRID + row * WATER_ROW + col * CORNER + 4;
                m.wr32(a, moved(a).to_bits());
            }
        }
    }
    let inner = |p: u32| {
        let k = (p - WATER_GRID) / CORNER;
        let (row, col) = (k / 33, k % 33);
        (1..32).contains(&row) && (1..32).contains(&col)
    };
    let mut verts = Vec::with_capacity(32 * 32 * 6);
    for row in 0..32 {
        for col in 0..32 {
            let a = WATER_GRID + row * WATER_ROW + col * CORNER;
            let (b, cc, d) = (a + CORNER, a + WATER_ROW, a + WATER_ROW + CORNER);
            // blue, half seen; green where the corner is marked
            let color = if m.rd8(d + 0x24) & 0x1f != 0 { 0x8000_ff00 } else { 0x8084_84ff };
            let v = |p: u32, k: u32| Vertex {
                pos: [f32_at(p), if extra && inner(p) { moved(p + 4) } else { f32_at(p + 4) }, f32_at(p + 8)],
                uv: [f32_at(d + 0x48 + k * 4), f32_at(d + 0x58 + k * 4)],
                color,
            };
            let (va, vb, vd, vc) = (v(a, 1), v(b, 2), v(d, 3), v(cc, 0));
            verts.extend([va, vb, vd, va, vd, vc]);
        }
    }
    draw_water(tex, &verts);
    // ret
    c.r[ESP] += 4;
}

fn ours() -> bool {
    static OURS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OURS.get_or_init(|| std::env::var("HOGS_GROUND").map_or(true, |v| v != "engine"))
}

pub fn after_ground(c: &mut Cpu) {
    super::objects::new_scene();
    if ours() && mem().rd32(MASKED) == 1 {
        hide_engine_ground();
        draw();
    }
    call_original(c, AFTER_GROUND);
}

/// Mark every block invisible to the engine (from the next picture on).
fn hide_engine_ground() {
    let masks = mem().slice(MASKS.0, (MASKS.1 - MASKS.0) as usize);
    if masks.iter().any(|&b| b != 0xff) {
        masks.fill(0xff);
    }
}

const VS: &str = r#"
layout(location = 0) in vec3 pos;
layout(location = 1) in vec2 tc;
layout(location = 2) in vec4 dif;
uniform vec2 size;
uniform vec3 cam;
uniform vec3 rot[3];
uniform vec2 centre;
out vec4 diffuse;
out vec4 specular;
out vec2 uv;
out float eye;
void main() {
    // the engine's projection (0x10013f20): depth d along the view; in front of 937 1/d,
    // nearer a straight line touching it there (no near plane)
    vec3 q = pos - cam;
    float d = dot(rot[2], q);
    float inv = d > 937.0 ? 1.0 / d : 0.0021344717 - d * 1.138992379e-6;
    vec2 s = centre + vec2(dot(rot[0], q), -dot(rot[1], q)) * inv;
    // behind the camera (d < 0) the straight line goes on and gives points on the screen with
    // the nearest depth (ground behind the camera drawn over everything): in front of the near
    // plane instead, so GL cuts them off. The engine drew no ground there (its blocks' masks)
    float z = d < 0.0 ? d * 2e-6 : max(d * 2e-6, 1.527e-5);
    // as the device's vertices: rhw = 8 inv, pixel centres at whole numbers
    float w = 1.0 / (8.0 * inv);
    vec2 p = (s + 0.5) / size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, z * 2.0 - 1.0, 1.0) * w;
    eye = w;
    diffuse = dif.bgra;
    specular = vec4(1.0);
    uv = tc;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct Vertex {
    pos: [f32; 3],
    uv: [f32; 2],
    color: u32,
}

/// The GL side: the program, the vertex buffer and what it holds (by page).
struct Gpu {
    prog: d3d::OwnProgram,
    cam: d3d::Uniform,
    rot: d3d::Uniform,
    centre: d3d::Uniform,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    /// the water's, loaded each picture
    water: (glow::VertexArray, glow::Buffer),
    verts: Vec<Vertex>,
    /// texture surface, first vertex, count
    pages: Vec<(u32, i32, i32)>,
}

thread_local! {
    static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) };
}

fn f32_at(a: u32) -> f32 {
    f32::from_bits(mem().rd32(a))
}

/// The tiles' triangles, by page.
fn tiles() -> (Vec<Vertex>, Vec<(u32, i32, i32)>) {
    let m = mem();
    let mut by_page: Vec<Vec<Vertex>> = vec![Vec::new(); 32];
    for row in 0..64 {
        for col in 0..64 {
            let a = GRID + row * ROW + col * CORNER;
            let (page, tile) = (m.rd16(a + 0x40) as i16 as i32, m.rd16(a + 0x42) as i16 as i32);
            if !(0..32).contains(&page) || !(0..49).contains(&tile) {
                continue;
            }
            if m.rd32(PAGES + page as u32 * PAGE + 0x10 + tile as u32 * 4) == 2 {
                continue;
            }
            // corner k of the tile's uv: A (row, col) 3, B (row, col + 1) 0, C (row + 1, col) 2,
            // D (row + 1, col + 1) 1; the engine's triangles D C A, A B D
            let v = |p: u32, k: u32| Vertex {
                pos: [f32_at(p), f32_at(p + 4), f32_at(p + 8)],
                uv: [f32_at(a + 0x48 + k * 4), f32_at(a + 0x58 + k * 4)],
                color: m.rd32(p + 0xc),
            };
            let (b, c, d) = (a + CORNER, a + ROW, a + ROW + CORNER);
            by_page[page as usize].extend([v(d, 1), v(c, 2), v(a, 3), v(a, 3), v(b, 0), v(d, 1)]);
        }
    }
    let mut verts = Vec::new();
    let mut pages = Vec::new();
    for (p, v) in by_page.into_iter().enumerate() {
        if !v.is_empty() {
            pages.push((m.rd32(PAGES + p as u32 * PAGE), verts.len() as i32, v.len() as i32));
            verts.extend(v);
        }
    }
    (verts, pages)
}

/// A vertex array of `Vertex`es and its buffer.
fn vertex_array(gl: &glow::Context) -> (glow::VertexArray, glow::Buffer) {
    unsafe {
        let vao = gl.create_vertex_array().expect("GL vertex array");
        let vbo = gl.create_buffer().expect("GL buffer");
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let stride = size_of::<Vertex>() as i32;
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 3, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 12);
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_f32(2, 4, glow::UNSIGNED_BYTE, true, stride, 20);
        gl.bind_vertex_array(None);
        (vao, vbo)
    }
}

fn gpu(gl: &glow::Context) -> Gpu {
    let prog = d3d::own_program(gl, VS);
    let l = |n: &str| d3d::Uniform::new(gl, &prog, n);
    let (cam, rot, centre) = (l("cam"), l("rot"), l("centre"));
    let (vao, vbo) = vertex_array(gl);
    Gpu { prog, cam, rot, centre, vao, vbo, water: vertex_array(gl), verts: Vec::new(), pages: Vec::new() }
}

/// The camera's uniforms (set after `apply`: the program is in use).
fn camera(gl: &glow::Context, g: &mut Gpu) {
    let m = mem();
    let rot: Vec<f32> = ROT.iter().flatten().map(|&a| f32_at(a)).collect();
    g.cam.set(gl, 3, &[f32_at(CAMERA), f32_at(CAMERA + 4), f32_at(CAMERA + 8)]);
    g.rot.set(gl, 3, &rot);
    g.centre.set(gl, 2, &[m.rd32(CENTRE) as i32 as f32, m.rd32(CENTRE + 4) as i32 as f32]);
}

fn draw_water(tex: u32, verts: &[Vertex]) {
    d3d::own_draw(|own| {
        let gl = own.gl;
        GPU.with_borrow_mut(|g| {
            let g = g.get_or_insert_with(|| gpu(gl));
            own.apply(&g.prog, tex);
            camera(gl, g);
            unsafe {
                gl.bind_vertex_array(Some(g.water.0));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(g.water.1));
                let bytes = std::slice::from_raw_parts(verts.as_ptr() as *const u8, size_of_val(verts));
                gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
                gl.draw_arrays(glow::TRIANGLES, 0, verts.len() as i32);
                gl.bind_vertex_array(None);
            }
        });
    });
}

fn draw() {
    let (verts, pages) = tiles();
    d3d::own_draw(|own| {
        let gl = own.gl;
        GPU.with_borrow_mut(|g| {
            let g = g.get_or_insert_with(|| gpu(gl));
            unsafe {
                // loaded again only when the map changed (craters)
                if g.verts != verts {
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(g.vbo));
                    let bytes = std::slice::from_raw_parts(verts.as_ptr() as *const u8, size_of_val(&verts[..]));
                    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
                    g.verts = verts;
                }
                g.pages = pages;
                for (surface, first, count) in g.pages.clone() {
                    own.apply(&g.prog, surface);
                    camera(gl, g);
                    gl.bind_vertex_array(Some(g.vao));
                    gl.draw_arrays(glow::TRIANGLES, first, count);
                }
                gl.bind_vertex_array(None);
            }
        });
    });
}
