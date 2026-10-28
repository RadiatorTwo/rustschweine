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

use super::{d3d, mem};
use crate::rt::{Cpu, call_original};
use glow::HasContext;
use std::cell::RefCell;

/// Called by the scene drawing (0x10003380) right after the ground.
pub const AFTER_GROUND: u32 = 0x1000_c040;

/// The map's corners: 65 x 65 of 0x68 bytes. x, y, z floats at +0, +4, +8 (z up), the lit
/// colour +0xc, page +0x40 and tile on it +0x42 (a tile's from its first corner), u[4] +0x48,
/// v[4] +0x58 (the tile's four corners, rotation and flip applied).
const GRID: u32 = 0x11af_aa30;
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

fn ours() -> bool {
    static OURS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OURS.get_or_init(|| std::env::var("HOGS_GROUND").map_or(true, |v| v != "engine"))
}

pub fn after_ground(c: &mut Cpu) {
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
    float z = max(d * 2e-6, 1.527e-5);
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
    cam: Option<glow::UniformLocation>,
    rot: Option<glow::UniformLocation>,
    centre: Option<glow::UniformLocation>,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
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

fn draw() {
    let (verts, pages) = tiles();
    d3d::own_draw(|own| {
        let gl = own.gl;
        GPU.with_borrow_mut(|g| {
            let g = g.get_or_insert_with(|| unsafe {
                let prog = d3d::own_program(gl, VS);
                let l = |n: &str| gl.get_uniform_location(prog.prog, n);
                let (cam, rot, centre) = (l("cam"), l("rot"), l("centre"));
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
                Gpu { prog, cam, rot, centre, vao, vbo, verts: Vec::new(), pages: Vec::new() }
            });
            unsafe {
                // loaded again only when the map changed (craters)
                if g.verts != verts {
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(g.vbo));
                    let bytes = std::slice::from_raw_parts(verts.as_ptr() as *const u8, size_of_val(&verts[..]));
                    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
                    g.verts = verts;
                }
                g.pages = pages;
                let m = mem();
                let rot: Vec<f32> = ROT.iter().flatten().map(|&a| f32_at(a)).collect();
                for &(surface, first, count) in &g.pages {
                    own.apply(&g.prog, surface);
                    gl.uniform_3_f32(g.cam.as_ref(), f32_at(CAMERA), f32_at(CAMERA + 4), f32_at(CAMERA + 8));
                    gl.uniform_3_f32_slice(g.rot.as_ref(), &rot);
                    gl.uniform_2_f32(g.centre.as_ref(), m.rd32(CENTRE) as i32 as f32, m.rd32(CENTRE + 4) as i32 as f32);
                    gl.bind_vertex_array(Some(g.vao));
                    gl.draw_arrays(glow::TRIANGLES, first, count);
                }
                gl.bind_vertex_array(None);
            }
        });
    });
}
