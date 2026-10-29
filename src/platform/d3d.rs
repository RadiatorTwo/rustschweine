//! Direct3D 7, as Hogs of War uses it (the EXE lists the devices, Data/_d3d.dll draws):
//! IDirect3D7 from IDirectDraw7::QueryInterface, one hardware device (HAL), IDirect3DDevice7.
//!
//! Drawing is OpenGL (`Render`): the render target (the back buffer, 16-bit 555 in guest
//! memory, where the game also blits its 2D pictures) is a colour texture with a depth buffer
//! in a framebuffer. The two are kept in step lazily: before drawing, pixels written by
//! DirectDraw since (the surface's stamp changed) are loaded into the colour texture; before
//! DirectDraw touches the surface again (`flush`), what was drawn is written back to its
//! pixels. The game draws pre-transformed vertices (XYZRHW, diffuse, specular, one texture)
//! with the fixed-function pipeline of texture stage 0 (`FS`).
//!
//! Methods that are not needed yet return D3D_OK or say "unsupported".

use super::{Import, alloc, arg, com_new, ddraw, guest_call, log, mem, new_cstr};
use crate::rt::Cpu;
use glow::HasContext;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

const OK: u32 = 0;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DDERR_INVALIDPARAMS: u32 = 0x8007_0057;

/// IID_IDirect3D7
pub const IID_D3D7: u32 = 0xf504_9e77;

/// IID_IDirect3DHALDevice {84E63DE0-46AA-11CF-816F-0000C020156E}
const HAL_GUID: [u32; 4] = [0x84e6_3de0, 0x11cf_46aa, 0xc000_6f81, 0x6e15_20c0];

fn trace() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var_os("HOGS_D3D_LOG").is_some())
}

pub fn new_d3d7() -> u32 {
    com_new(D3D7, 0)
}

fn ok(_: &mut Cpu) -> u32 {
    OK
}
fn add_ref(_: &mut Cpu) -> u32 {
    1
}
fn release(_: &mut Cpu) -> u32 {
    0
}
fn unsupported(_: &mut Cpu) -> u32 {
    log!("warn: unsupported Direct3D method");
    DDERR_INVALIDPARAMS
}

// ---- IDirect3D7 ---------------------------------------------------------------------------------

/// D3DPRIMCAPS (56 bytes): what a hardware card of 2000 offered for triangles (and lines).
fn prim_caps(p: u32) {
    let m = mem();
    let caps = [
        56,
        0x0000_00f2,     // misc: mask Z, cull none/CW/CCW
        0x0003_01b1,     // raster: dither, z-test, fog vertex/table, mipmap LOD bias, z-bias, anisotropy
        0xff,            // z compare: all
        0x1fff,          // source blend: all
        0x1fff,          // destination blend: all
        0xff,            // alpha compare: all
        0x0002_4aa8,     // shade: Gouraud colour, specular, alpha, fog
        0x0000_0145,     // texture: perspective, alpha, transparency, ...
        0x0000_3f3f,     // texture filter: point, linear, mip
        0x0000_00ff,     // texture blend (legacy)
        0x0000_001f,     // texture address: wrap, mirror, clamp, border, independent UV
        0,
        0,
    ];
    for (k, v) in caps.into_iter().enumerate() {
        m.wr32(p + 4 * k as u32, v);
    }
}

/// D3DDEVICEDESC7 (236 bytes) of our one device.
fn device_desc(p: u32) {
    let m = mem();
    m.slice(p, 236).fill(0);
    m.wr32(p, 0x0008_ffd0); // devcaps: HW rasterization, texture in video memory, DrawPrimitive2...
    prim_caps(p + 4);
    prim_caps(p + 60);
    m.wr32(p + 116, 0x500); // render bit depths 16, 32
    m.wr32(p + 120, 0x600); // z-buffer bit depths 16, 24
    m.wr32(p + 124, 1);
    m.wr32(p + 128, 1);
    m.wr32(p + 132, 2048);
    m.wr32(p + 136, 2048);
    m.wr32(p + 140, 2048);
    m.wr32(p + 144, 2048);
    m.wr32(p + 148, 16);
    for (k, v) in [-4096.0f32, -4096.0, 4096.0, 4096.0].into_iter().enumerate() {
        m.wr32(p + 152 + 4 * k as u32, v.to_bits());
    }
    m.wr32(p + 180, 0x00ff_ffff); // texture op caps
    m.wr16(p + 184, 8); // blend stages
    m.wr16(p + 186, 2); // simultaneous textures
    m.wr32(p + 188, 8); // lights
    m.wr32(p + 192, 1e10f32.to_bits()); // max vertex W
    for (k, v) in HAL_GUID.into_iter().enumerate() {
        m.wr32(p + 196 + 4 * k as u32, v);
    }
}

/// EnumDevices(callback, context): callback(description, name, D3DDEVICEDESC7*, context).
fn d3d_enum_devices(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 1), arg(c, 2));
    static DESC: OnceLock<(u32, u32, u32)> = OnceLock::new();
    let &(desc, name, dd) = DESC.get_or_init(|| {
        let dd = alloc(236);
        device_desc(dd);
        (new_cstr("Direct3D HAL"), new_cstr("Direct3D HAL"), dd)
    });
    guest_call(c, cb, &[desc, name, dd, ctx]);
    OK
}

/// EnumZBufferFormats(device GUID, callback, context): callback(DDPIXELFORMAT*, context);
/// 16 and 24 bits.
fn d3d_enum_zbuffer_formats(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 2), arg(c, 3));
    let pf = alloc(32);
    let m = mem();
    for (bits, mask) in [(16u32, 0xffffu32), (24, 0xff_ffff)] {
        m.slice(pf, 32).fill(0);
        m.wr32(pf, 32);
        m.wr32(pf + 4, 0x400); // DDPF_ZBUFFER
        m.wr32(pf + 0xc, bits);
        m.wr32(pf + 0x14, mask);
        if guest_call(c, cb, &[pf, ctx]) == 0 {
            break;
        }
    }
    super::free(pf);
    OK
}

/// CreateDevice(device GUID, render target surface, out)
fn d3d_create_device(c: &mut Cpu) -> u32 {
    let (target, out) = (arg(c, 2), arg(c, 3));
    if trace() {
        log!("CreateDevice on surface {target:#x}");
    }
    let dev = com_new(DEVICE7, 4);
    mem().wr32(dev + 4, target);
    STATE.with_borrow_mut(|st| st.target = target);
    mem().wr32(out, dev);
    OK
}

fn d3d_query_interface(c: &mut Cpu) -> u32 {
    let id = mem().rd32(arg(c, 1));
    log!("warn: IDirect3D7::QueryInterface({id:#010x}) not supported");
    mem().wr32(arg(c, 2), 0);
    E_NOINTERFACE
}

static D3D7: &[Import] = &[
    ("IDirect3D7::QueryInterface", 3, d3d_query_interface),
    ("IDirect3D7::AddRef", 1, add_ref),
    ("IDirect3D7::Release", 1, release),
    ("IDirect3D7::EnumDevices", 3, d3d_enum_devices),
    ("IDirect3D7::CreateDevice", 4, d3d_create_device),
    ("IDirect3D7::CreateVertexBuffer", 4, unsupported),
    ("IDirect3D7::EnumZBufferFormats", 4, d3d_enum_zbuffer_formats),
    ("IDirect3D7::EvictManagedTextures", 1, ok),
];

// ---- IDirect3DDevice7 -----------------------------------------------------------------------------

fn dev_get_caps(c: &mut Cpu) -> u32 {
    device_desc(arg(c, 1));
    OK
}

/// EnumTextureFormats(callback, context): callback(DDPIXELFORMAT*, context); 16-bit 555, 565,
/// 1555, 4444 and 32-bit 8888.
fn dev_enum_texture_formats(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 1), arg(c, 2));
    let pf = alloc(32);
    let m = mem();
    // (flags, bits, r, g, b, a): DDPF_RGB 0x40, DDPF_ALPHAPIXELS 1
    let formats: [(u32, u32, u32, u32, u32, u32); 5] = [
        (0x40, 16, 0x7c00, 0x03e0, 0x001f, 0),
        (0x40, 16, 0xf800, 0x07e0, 0x001f, 0),
        (0x41, 16, 0x7c00, 0x03e0, 0x001f, 0x8000),
        (0x41, 16, 0x0f00, 0x00f0, 0x000f, 0xf000),
        (0x41, 32, 0xff_0000, 0xff00, 0xff, 0xff00_0000),
    ];
    for (flags, bits, r, g, b, a) in formats {
        m.slice(pf, 32).fill(0);
        m.wr32(pf, 32);
        m.wr32(pf + 4, flags);
        m.wr32(pf + 0xc, bits);
        m.wr32(pf + 0x10, r);
        m.wr32(pf + 0x14, g);
        m.wr32(pf + 0x18, b);
        m.wr32(pf + 0x1c, a);
        if guest_call(c, cb, &[pf, ctx]) == 0 {
            break;
        }
    }
    super::free(pf);
    OK
}

fn dev_get_direct3d(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), new_d3d7());
    OK
}

fn dev_get_render_target(c: &mut Cpu) -> u32 {
    let t = mem().rd32(arg(c, 0) + 4);
    mem().wr32(arg(c, 1), t);
    OK
}

fn dev_set_render_target(c: &mut Cpu) -> u32 {
    flush_all();
    mem().wr32(arg(c, 0) + 4, arg(c, 1));
    STATE.with_borrow_mut(|st| st.target = arg(c, 1));
    OK
}

fn dev_validate_device(c: &mut Cpu) -> u32 {
    if arg(c, 1) != 0 {
        mem().wr32(arg(c, 1), 1);
    }
    OK
}

fn dev_clear(c: &mut Cpu) -> u32 {
    let (n, rects, flags, color, z) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), f32::from_bits(arg(c, 5)));
    let m = mem();
    let rects: Vec<[i32; 4]> = (0..n).map(|k| rects + 16 * k).map(|p| [0, 4, 8, 12].map(|o| m.rd32(p + o) as i32)).collect();
    with_render(|r, st, gl| r.clear(gl, st, &rects, flags, color, z));
    OK
}

fn dev_set_viewport(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    STATE.with_borrow_mut(|st| st.viewport = [0, 4, 8, 12].map(|o| mem().rd32(p + o)));
    OK
}

fn dev_get_viewport(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let v = STATE.with_borrow(|st| st.viewport);
    for (k, x) in v.into_iter().enumerate() {
        mem().wr32(p + 4 * k as u32, x);
    }
    mem().wr32(p + 16, 0f32.to_bits());
    mem().wr32(p + 20, 1f32.to_bits());
    OK
}

fn dev_set_render_state(c: &mut Cpu) -> u32 {
    let (k, v) = (arg(c, 1), arg(c, 2));
    let old = STATE.with_borrow_mut(|st| st.rs.get_mut(k as usize).map(|s| std::mem::replace(s, v)));
    if trace() && old.is_some_and(|o| o != v) {
        log!("render state {k} = {v:#x}");
    }
    OK
}

/// A device render state (D3DRENDERSTATETYPE) as the game set it.
pub fn render_state(k: usize) -> u32 {
    STATE.with_borrow(|st| st.rs.get(k).copied().unwrap_or(0))
}

/// Set a device render state, as SetRenderState does.
pub fn set_render_state(k: usize, v: u32) {
    STATE.with_borrow_mut(|st| {
        if let Some(s) = st.rs.get_mut(k) {
            *s = v;
        }
    });
}

/// Set a texture stage state, as SetTextureStageState does.
pub fn set_texture_stage_state(stage: usize, k: usize, v: u32) {
    STATE.with_borrow_mut(|st| {
        if let Some(s) = st.tss.get_mut(stage).and_then(|t| t.get_mut(k)) {
            *s = v;
        }
    });
}

/// Set a stage's texture (a surface), as SetTexture does.
pub fn set_texture(stage: usize, surface: u32) {
    STATE.with_borrow_mut(|st| {
        if let Some(s) = st.tex.get_mut(stage) {
            *s = surface;
        }
    });
}

fn dev_get_render_state(c: &mut Cpu) -> u32 {
    let k = arg(c, 1) as usize;
    let v = STATE.with_borrow(|st| st.rs.get(k).copied().unwrap_or(0));
    mem().wr32(arg(c, 2), v);
    OK
}

fn dev_set_texture_stage_state(c: &mut Cpu) -> u32 {
    let (stage, k, v) = (arg(c, 1) as usize, arg(c, 2) as usize, arg(c, 3));
    STATE.with_borrow_mut(|st| {
        if let Some(s) = st.tss.get_mut(stage).and_then(|t| t.get_mut(k)) {
            *s = v;
        }
    });
    OK
}

fn dev_get_texture_stage_state(c: &mut Cpu) -> u32 {
    let (stage, k) = (arg(c, 1) as usize, arg(c, 2) as usize);
    let v = STATE.with_borrow(|st| st.tss.get(stage).and_then(|t| t.get(k)).copied().unwrap_or(0));
    mem().wr32(arg(c, 3), v);
    OK
}

fn dev_set_texture(c: &mut Cpu) -> u32 {
    let (stage, t) = (arg(c, 1) as usize, arg(c, 2));
    STATE.with_borrow_mut(|st| {
        if let Some(s) = st.tex.get_mut(stage) {
            *s = t;
        }
    });
    OK
}

fn dev_get_texture(c: &mut Cpu) -> u32 {
    let stage = arg(c, 1) as usize;
    let t = STATE.with_borrow(|st| st.tex.get(stage).copied().unwrap_or(0));
    mem().wr32(arg(c, 2), t);
    OK
}

/// DrawPrimitive(type, FVF, vertices, count, flags)
fn dev_draw_primitive(c: &mut Cpu) -> u32 {
    let (kind, fvf, verts, n) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    draw(kind, fvf, verts, n, &(0..n).collect::<Vec<_>>());
    OK
}

/// DrawIndexedPrimitive(type, FVF, vertices, vertex count, 16-bit indices, index count, flags)
fn dev_draw_indexed_primitive(c: &mut Cpu) -> u32 {
    let (kind, fvf, verts, n, idx, ni) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6));
    let m = mem();
    let seq: Vec<u32> = (0..ni).map(|k| m.rd16(idx + 2 * k) as u32).filter(|&i| i < n).collect();
    draw(kind, fvf, verts, n, &seq);
    OK
}

static DEVICE7: &[Import] = &[
    ("IDirect3DDevice7::QueryInterface", 3, unsupported),
    ("IDirect3DDevice7::AddRef", 1, add_ref),
    ("IDirect3DDevice7::Release", 1, release),
    ("IDirect3DDevice7::GetCaps", 2, dev_get_caps),
    ("IDirect3DDevice7::EnumTextureFormats", 3, dev_enum_texture_formats),
    ("IDirect3DDevice7::BeginScene", 1, ok),
    ("IDirect3DDevice7::EndScene", 1, dev_end_scene),
    ("IDirect3DDevice7::GetDirect3D", 2, dev_get_direct3d),
    ("IDirect3DDevice7::SetRenderTarget", 3, dev_set_render_target),
    ("IDirect3DDevice7::GetRenderTarget", 2, dev_get_render_target),
    ("IDirect3DDevice7::Clear", 7, dev_clear),
    ("IDirect3DDevice7::SetTransform", 3, ok),
    ("IDirect3DDevice7::GetTransform", 3, unsupported),
    ("IDirect3DDevice7::SetViewport", 2, dev_set_viewport),
    ("IDirect3DDevice7::MultiplyTransform", 3, unsupported),
    ("IDirect3DDevice7::GetViewport", 2, dev_get_viewport),
    ("IDirect3DDevice7::SetMaterial", 2, ok),
    ("IDirect3DDevice7::GetMaterial", 2, unsupported),
    ("IDirect3DDevice7::SetLight", 3, ok),
    ("IDirect3DDevice7::GetLight", 3, unsupported),
    ("IDirect3DDevice7::SetRenderState", 3, dev_set_render_state),
    ("IDirect3DDevice7::GetRenderState", 3, dev_get_render_state),
    ("IDirect3DDevice7::BeginStateBlock", 1, unsupported),
    ("IDirect3DDevice7::EndStateBlock", 2, unsupported),
    ("IDirect3DDevice7::PreLoad", 2, ok),
    ("IDirect3DDevice7::DrawPrimitive", 6, dev_draw_primitive),
    ("IDirect3DDevice7::DrawIndexedPrimitive", 8, dev_draw_indexed_primitive),
    ("IDirect3DDevice7::SetClipStatus", 2, ok),
    ("IDirect3DDevice7::GetClipStatus", 2, unsupported),
    ("IDirect3DDevice7::DrawPrimitiveStrided", 6, unsupported),
    ("IDirect3DDevice7::DrawIndexedPrimitiveStrided", 8, unsupported),
    ("IDirect3DDevice7::DrawPrimitiveVB", 6, unsupported),
    ("IDirect3DDevice7::DrawIndexedPrimitiveVB", 8, unsupported),
    ("IDirect3DDevice7::ComputeSphereVisibility", 6, unsupported),
    ("IDirect3DDevice7::GetTexture", 3, dev_get_texture),
    ("IDirect3DDevice7::SetTexture", 3, dev_set_texture),
    ("IDirect3DDevice7::GetTextureStageState", 4, dev_get_texture_stage_state),
    ("IDirect3DDevice7::SetTextureStageState", 4, dev_set_texture_stage_state),
    ("IDirect3DDevice7::ValidateDevice", 2, dev_validate_device),
    ("IDirect3DDevice7::ApplyStateBlock", 2, unsupported),
    ("IDirect3DDevice7::CaptureStateBlock", 2, unsupported),
    ("IDirect3DDevice7::DeleteStateBlock", 2, unsupported),
    ("IDirect3DDevice7::CreateStateBlock", 3, unsupported),
    ("IDirect3DDevice7::Load", 6, unsupported),
    ("IDirect3DDevice7::LightEnable", 3, ok),
    ("IDirect3DDevice7::GetLightEnable", 3, unsupported),
    ("IDirect3DDevice7::SetClipPlane", 3, unsupported),
    ("IDirect3DDevice7::GetClipPlane", 3, unsupported),
    ("IDirect3DDevice7::GetInfo", 4, unsupported),
];

// ---- drawing ------------------------------------------------------------------------------------

// render states (D3DRENDERSTATETYPE)
const RS_ZENABLE: usize = 7;
const RS_SHADEMODE: usize = 9;
const RS_ZWRITEENABLE: usize = 14;
const RS_ALPHATESTENABLE: usize = 15;
const RS_SRCBLEND: usize = 19;
const RS_DESTBLEND: usize = 20;
const RS_CULLMODE: usize = 22;
const RS_ZFUNC: usize = 23;
const RS_ALPHAREF: usize = 24;
const RS_ALPHAFUNC: usize = 25;
const RS_ALPHABLENDENABLE: usize = 27;
const RS_FOGENABLE: usize = 28;
const RS_SPECULARENABLE: usize = 29;
const RS_FOGCOLOR: usize = 34;
const RS_FOGTABLEMODE: usize = 35;
const RS_FOGTABLESTART: usize = 36;
const RS_FOGTABLEEND: usize = 37;
const RS_FOGTABLEDENSITY: usize = 38;
const RS_COLORKEYENABLE: usize = 41;
const RS_TEXTUREFACTOR: usize = 60;
// texture stage states (D3DTEXTURESTAGESTATETYPE)
const TSS_COLOROP: usize = 1;
const TSS_COLORARG1: usize = 2;
const TSS_COLORARG2: usize = 3;
const TSS_ALPHAOP: usize = 4;
const TSS_ALPHAARG1: usize = 5;
const TSS_ALPHAARG2: usize = 6;
const TSS_ADDRESSU: usize = 13;
const TSS_ADDRESSV: usize = 14;
const TSS_MAGFILTER: usize = 16;
const TSS_MINFILTER: usize = 17;

/// What the device was told: render states, texture stage states, textures, viewport.
struct State {
    rs: [u32; 256],
    tss: [[u32; 32]; 8],
    tex: [u32; 8],
    /// x, y, width, height
    viewport: [u32; 4],
    target: u32,
}

impl State {
    fn new() -> State {
        let mut rs = [0; 256];
        for (k, v) in [
            (4, 1),            // texture perspective
            (8, 3),            // fill solid
            (RS_SHADEMODE, 2), // Gouraud
            (RS_ZWRITEENABLE, 1),
            (RS_SRCBLEND, 2),  // one
            (RS_DESTBLEND, 1), // zero
            (RS_CULLMODE, 3),  // counter-clockwise
            (RS_ZFUNC, 4),     // less or equal
            (RS_ALPHAFUNC, 8), // always
            (136, 1),          // clipping
            (137, 1),          // lighting
            (RS_TEXTUREFACTOR, 0xffff_ffff),
        ] {
            rs[k] = v;
        }
        let mut tss = [[0; 32]; 8];
        for (k, t) in tss.iter_mut().enumerate() {
            // stage 0 modulates the texture with the diffuse colour; the others are off
            t[TSS_COLOROP] = if k == 0 { 4 } else { 1 };
            t[TSS_COLORARG1] = 2;
            t[TSS_COLORARG2] = 1;
            t[TSS_ALPHAOP] = if k == 0 { 2 } else { 1 };
            t[TSS_ALPHAARG1] = 2;
            t[TSS_ALPHAARG2] = 1;
            t[TSS_ADDRESSU] = 1;
            t[TSS_ADDRESSV] = 1;
            t[TSS_MAGFILTER] = 1;
            t[TSS_MINFILTER] = 1;
            t[18] = 1;
        }
        State { rs, tss, tex: [0; 8], viewport: [0, 0, 640, 480], target: 0 }
    }
}

/// A vertex as we draw it: x, y, z, rhw, u, v, diffuse and specular (ARGB words).
#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    pos: [f32; 4],
    uv: [f32; 2],
    diffuse: u32,
    specular: u32,
}

const VS: &str = r#"
layout(location = 0) in vec4 pos;
layout(location = 1) in vec2 tc;
layout(location = 2) in vec4 dif;
layout(location = 3) in vec4 spe;
uniform vec2 size;
out vec4 diffuse;
out vec4 specular;
out vec2 uv;
out float eye;
void main() {
    // screen pixels (centres at whole numbers in Direct3D) to clip space, top row at the top;
    // w = 1 / rhw gives perspective-correct interpolation. A negative rhw is a corner behind
    // the camera (the engine's 2D polys in the world, afDraw2dPolyIn3dWorld: x and y projected
    // by the same depth, so x * w stays a straight line): GL cuts it off at w = 0, where with
    // w = 1 its far-off x, y stretched the triangle over the picture (the dark stripes)
    float w = pos.w != 0.0 ? 1.0 / pos.w : 1.0;
    vec2 p = (pos.xy + 0.5) / size * 2.0 - 1.0;
    gl_Position = vec4(p.x, -p.y, pos.z * 2.0 - 1.0, 1.0) * w;
    eye = w;
    // ARGB words: their bytes are B G R A
    diffuse = dif.bgra;
    specular = spe.bgra;
    uv = tc;
}
"#;

/// Texture stage 0 of the fixed-function pipeline: colour and alpha each from an operation
/// (D3DTOP) on two arguments (D3DTA), then specular, alpha test and fog: vertex fog (the
/// specular alpha) or table fog by the distance from the eye (w = 1 / rhw; the game's fog
/// start and end are distances, not depths).
const FS: &str = r#"
uniform sampler2D tex;
uniform int textured;
uniform ivec3 color_op;
uniform ivec3 alpha_op;
uniform vec4 factor;
uniform int alpha_func;
uniform float alpha_ref;
uniform int add_specular;
uniform int fog;
uniform vec3 fog_color;
uniform vec3 fog_table;
in vec4 diffuse;
in vec4 specular;
in vec2 uv;
in float eye;
out vec4 color;

vec4 arg(int a, vec4 t) {
    int s = a & 15;
    vec4 v = s == 2 ? t : s == 3 ? factor : s == 4 ? specular : diffuse;
    if ((a & 0x20) != 0) v = v.aaaa;
    if ((a & 0x10) != 0) v = 1.0 - v;
    return v;
}

vec4 op(ivec3 o, vec4 t) {
    vec4 a = arg(o.y, t);
    vec4 b = arg(o.z, t);
    switch (o.x) {
    case 2: return a;
    case 3: return b;
    case 4: return a * b;
    case 5: return 2.0 * a * b;
    case 6: return 4.0 * a * b;
    case 7: return a + b;
    case 8: return a + b - 0.5;
    case 9: return 2.0 * (a + b - 0.5);
    case 10: return a - b;
    case 11: return a + b - a * b;
    case 12: return mix(b, a, diffuse.a);
    case 13: return mix(b, a, t.a);
    case 14: return mix(b, a, factor.a);
    case 15: return a + b * (1.0 - t.a);
    case 16: return mix(b, a, diffuse.a);
    default: return diffuse;
    }
}

bool passes(float a) {
    float v = floor(a * 255.0 + 0.5);
    switch (alpha_func) {
    case 1: return false;
    case 2: return v < alpha_ref;
    case 3: return v == alpha_ref;
    case 4: return v <= alpha_ref;
    case 5: return v > alpha_ref;
    case 6: return v != alpha_ref;
    case 7: return v >= alpha_ref;
    default: return true;
    }
}

void main() {
    vec4 t = textured != 0 ? texture(tex, uv) : vec4(1.0);
    vec4 c = clamp(vec4(op(color_op, t).rgb, op(alpha_op, t).a), 0.0, 1.0);
    if (add_specular != 0) c.rgb = min(c.rgb + specular.rgb, 1.0);
    if (!passes(c.a)) discard;
    // fog: 1 vertex, 2 exponential, 3 exponential squared, 4 linear (start, end, density)
    float f = 1.0;
    if (fog == 1) f = specular.a;
    else if (fog == 2) f = exp(-fog_table.z * eye);
    else if (fog == 3) f = exp(-pow(fog_table.z * eye, 2.0));
    else if (fog == 4) f = (fog_table.y - eye) / (fog_table.y - fog_table.x);
    c.rgb = mix(fog_color, c.rgb, clamp(f, 0.0, 1.0));
    color = c;
}
"#;

#[derive(Clone, Copy)]
struct Uniforms {
    size: Option<glow::UniformLocation>,
    tex: Option<glow::UniformLocation>,
    textured: Option<glow::UniformLocation>,
    color_op: Option<glow::UniformLocation>,
    alpha_op: Option<glow::UniformLocation>,
    factor: Option<glow::UniformLocation>,
    alpha_func: Option<glow::UniformLocation>,
    alpha_ref: Option<glow::UniformLocation>,
    add_specular: Option<glow::UniformLocation>,
    fog: Option<glow::UniformLocation>,
    fog_color: Option<glow::UniformLocation>,
    fog_table: Option<glow::UniformLocation>,
}

impl Uniforms {
    fn of(gl: &glow::Context, prog: glow::Program) -> Uniforms {
        let l = |n: &str| unsafe { gl.get_uniform_location(prog, n) };
        Uniforms {
            size: l("size"),
            tex: l("tex"),
            textured: l("textured"),
            color_op: l("color_op"),
            alpha_op: l("alpha_op"),
            factor: l("factor"),
            alpha_func: l("alpha_func"),
            alpha_ref: l("alpha_ref"),
            add_specular: l("add_specular"),
            fog: l("fog"),
            fog_color: l("fog_color"),
            fog_table: l("fog_table"),
        }
    }
}

/// A texture surface as a GL texture, and the stamp of the pixels it was made from.
struct Tex {
    tex: glow::Texture,
    stamp: u32,
}

/// The GL side of the device: the render target's framebuffer, and the textures.
struct Render {
    target: u32,
    size: (u32, u32),
    fbo: glow::Framebuffer,
    color: glow::Texture,
    depth: glow::Renderbuffer,
    /// the stamp of the target's pixels that the colour texture holds
    stamp: u32,
    /// drawn into since the pixels were last written
    ahead: bool,
    prog: glow::Program,
    u: Uniforms,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    textures: HashMap<u32, Tex>,
    /// sources of DirectDraw blits drawn by GL (`blit`), by surface and colour key
    blits: HashMap<(u32, Option<(u32, u32)>), Tex>,
    /// the program and state GL is set up for (`apply`), and whether the framebuffer is bound:
    /// set again only when they change (each GL call costs the driver's thread)
    applied: Option<(glow::Program, Pipe)>,
    bound: bool,
    cache: Cache,
    /// draws not sent to GL yet: consecutive ones with the same state are drawn as one
    /// (a mission picture has ~1500 small ones)
    batch: Option<(Pipe, Vec<Vertex>)>,
}

/// The state a draw depends on: drawn together when equal.
#[derive(Clone, Copy, PartialEq)]
struct Pipe {
    mode: u32,
    tex: Option<glow::Texture>,
    rs: [u32; 64],
    ts: [u32; 32],
    viewport: [u32; 4],
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::new());
    static RENDER: RefCell<Option<Render>> = const { RefCell::new(None) };
}

/// Run `f` with the GL side, its framebuffer bound and loaded with the render target's pixels.
/// Nothing without a window or a render target.
fn with_render<R>(f: impl FnOnce(&mut Render, &State, &glow::Context) -> R) -> Option<R> {
    let target = STATE.with_borrow(|st| st.target);
    let info = ddraw::info(target)?;
    super::window::with_gl(|g| {
        let gl = &g.gl;
        RENDER.with_borrow_mut(|r| {
            let r = r.get_or_insert_with(|| Render::new(gl));
            r.bind(gl, target, &info);
            STATE.with_borrow(|st| f(r, st, gl))
        })
    })
}

/// Write what was drawn into `surface` to its pixels (before DirectDraw reads or writes them).
pub fn flush(surface: u32) {
    if surface == 0 {
        return;
    }
    let ahead = RENDER.with_borrow(|r| r.as_ref().is_some_and(|r| r.ahead && r.target == surface));
    if ahead && let Some(info) = ddraw::info(surface) {
        super::window::with_gl(|g| RENDER.with_borrow_mut(|r| r.as_mut().unwrap().write_back(&g.gl, &info)));
    }
}

/// Draw what waits (before a flip shows the picture, and at the end of a scene).
pub fn finish() {
    super::window::with_gl(|g| {
        RENDER.with_borrow_mut(|r| {
            if let Some(r) = r {
                r.flush_batch(&g.gl);
            }
        });
    });
}

fn dev_end_scene(_: &mut Cpu) -> u32 {
    finish();
    OK
}

/// Whether `surface` is the render target and has drawing not written to its pixels.
pub fn ahead(surface: u32) -> bool {
    surface != 0 && RENDER.with_borrow(|r| r.as_ref().is_some_and(|r| r.ahead && r.target == surface))
}

/// A flip of the render target that was drawn into (`ahead`): show the colour texture in the
/// window. The target's pixels changed under it (the flip swaps them): it takes their new
/// stamp and keeps its colour (Direct3D's pictures cover all of it; the pixels get the
/// picture written when DirectDraw touches them).
pub fn show(stamp: u32) {
    let shown = RENDER.with_borrow_mut(|r| {
        r.as_mut().map(|r| {
            r.stamp = stamp;
            (r.color, r.size)
        })
    });
    if let Some((tex, (w, h))) = shown {
        super::window::show_texture(tex, w, h, 1.0);
    }
}

/// A DirectDraw blit onto the render target while it holds drawing not written to its pixels
/// (a mission's text and HUD, glyph by glyph after the 3D): drawn by GL into the picture,
/// instead of reading the picture back, copying on the CPU and loading it again (reading back
/// waits for the GPU to finish). False when the target is not ahead.
pub fn blit(dst: u32, src: u32, dr: [i32; 4], sr: [i32; 4], key: Option<(u32, u32)>) -> bool {
    if src == dst || !ahead(dst) {
        return false;
    }
    let Some(info) = ddraw::info(src).filter(|i| matches!(i.bpp, 16 | 32)) else { return false };
    super::window::with_gl(|g| RENDER.with_borrow_mut(|r| r.as_mut().unwrap().blit(&g.gl, src, &info, dr, sr, key))).is_some()
}

/// A vertex shader of ours with the device's fragment shader (`own_draw`). Its outputs are
/// the device's (`diffuse`, `specular`, `uv`, `eye`); `size` is the target's size.
pub struct OwnProgram {
    pub prog: glow::Program,
    u: Uniforms,
}

/// A uniform of an own program (`own_program`) that remembers its value and is sent again
/// only when that changes (the program is ours alone: nothing else sets it).
pub struct Uniform {
    loc: Option<glow::UniformLocation>,
    last: Vec<f32>,
}

impl Uniform {
    pub fn new(gl: &glow::Context, p: &OwnProgram, name: &str) -> Uniform {
        Uniform { loc: unsafe { gl.get_uniform_location(p.prog, name) }, last: Vec::new() }
    }

    /// Set it (vec2s or vec3s by `n`) when `v` differs from what it has; the program must be
    /// in use (after `Own::apply`).
    pub fn set(&mut self, gl: &glow::Context, n: usize, v: &[f32]) {
        if self.last == v {
            return;
        }
        unsafe {
            match n {
                2 => gl.uniform_2_f32_slice(self.loc.as_ref(), v),
                _ => gl.uniform_3_f32_slice(self.loc.as_ref(), v),
            }
        }
        self.last.clear();
        self.last.extend_from_slice(v);
    }
}

pub fn own_program(gl: &glow::Context, vs: &str) -> OwnProgram {
    let prog = super::gl::compile(gl, vs, FS);
    OwnProgram { prog, u: Uniforms::of(gl, prog) }
}

/// What a draw of our own gets (`own_draw`).
pub struct Own<'a> {
    pub gl: &'a glow::Context,
    r: &'a mut Render,
    st: &'a State,
}

impl Own<'_> {
    /// Set GL up as a device draw with the device's current states would be, by our program,
    /// with `surface` as the texture (0: none).
    pub fn apply(&mut self, p: &OwnProgram, surface: u32) {
        let tex = if surface != 0 { self.r.texture(self.gl, surface) } else { None };
        let mut rs = [0; 64];
        rs.copy_from_slice(&self.st.rs[..64]);
        let pipe = Pipe { mode: glow::TRIANGLES, tex, rs, ts: self.st.tss[0], viewport: self.st.viewport };
        self.r.apply(self.gl, p.prog, &p.u, &pipe);
    }
}

/// Draw something of our own into the render target, in order with the device's draws.
pub fn own_draw(f: impl FnOnce(&mut Own)) {
    with_render(|r, st, gl| {
        r.flush_batch(gl);
        f(&mut Own { gl, r, st });
        r.ahead = true;
    });
}

fn flush_all() {
    if let Some(t) = RENDER.with_borrow(|r| r.as_ref().map(|r| r.target)) {
        flush(t);
    }
}

/// A channel of a pixel by its mask, as 0...255 (255 without a mask).
fn channel(v: u32, mask: u32) -> u32 {
    if mask == 0 {
        return 255;
    }
    let max = mask >> mask.trailing_zeros();
    ((v & mask) >> mask.trailing_zeros()) * 255 / max
}

/// A blit source's pixels as RGBA bytes, top row first: opaque, transparent inside the colour
/// key range.
fn keyed(info: &ddraw::Info, key: Option<(u32, u32)>) -> Vec<u32> {
    let m = mem();
    let [_, r, g, b, _] = info.fmt;
    let mut out = Vec::with_capacity((info.w * info.h) as usize);
    for y in 0..info.h {
        let row = info.pixels + y * info.pitch;
        for x in 0..info.w {
            let v = if info.bpp == 16 { m.rd16(row + 2 * x) as u32 } else { m.rd32(row + 4 * x) };
            let a = if key.is_some_and(|(lo, hi)| v >= lo && v <= hi) { 0 } else { 0xff00_0000 };
            out.push(channel(v, r) | channel(v, g) << 8 | channel(v, b) << 16 | a);
        }
    }
    out
}

/// A surface's pixels as RGBA bytes (in little-endian words), top or bottom row first.
fn rgba(info: &ddraw::Info, bottom_first: bool) -> Vec<u32> {
    let m = mem();
    let [flags, r, g, b, a] = info.fmt;
    let a = if flags & 1 != 0 { a } else { 0 };
    let mut out = Vec::with_capacity((info.w * info.h) as usize);
    for k in 0..info.h {
        let y = if bottom_first { info.h - 1 - k } else { k };
        let row = info.pixels + y * info.pitch;
        for x in 0..info.w {
            let v = match info.bpp {
                16 => m.rd16(row + 2 * x) as u32,
                32 => m.rd32(row + 4 * x),
                _ => 0,
            };
            out.push(channel(v, r) | channel(v, g) << 8 | channel(v, b) << 16 | channel(v, a) << 24);
        }
    }
    out
}

impl Render {
    fn new(gl: &glow::Context) -> Render {
        let prog = super::gl::compile(gl, VS, FS);
        let u = Uniforms::of(gl, prog);
        unsafe {
            let vao = gl.create_vertex_array().expect("GL vertex array");
            let vbo = gl.create_buffer().expect("GL buffer");
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            let stride = size_of::<Vertex>() as i32;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 4, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 16);
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 4, glow::UNSIGNED_BYTE, true, stride, 24);
            gl.enable_vertex_attrib_array(3);
            gl.vertex_attrib_pointer_f32(3, 4, glow::UNSIGNED_BYTE, true, stride, 28);
            gl.bind_vertex_array(None);
            Render {
                target: 0,
                size: (0, 0),
                fbo: gl.create_framebuffer().expect("GL framebuffer"),
                color: super::gl::texture(gl, false),
                depth: gl.create_renderbuffer().expect("GL renderbuffer"),
                stamp: 0,
                ahead: false,
                prog,
                u,
                vao,
                vbo,
                textures: HashMap::new(),
                blits: HashMap::new(),
                applied: None,
                bound: false,
                cache: Cache::default(),
                batch: None,
            }
        }
    }

    /// Bind the framebuffer for `target`: sized to it, holding its pixels.
    fn bind(&mut self, gl: &glow::Context, target: u32, info: &ddraw::Info) {
        if target != self.target || self.size != (info.w, info.h) || self.stamp != info.stamp {
            self.flush_batch(gl);
        }
        if target != self.target || self.size != (info.w, info.h) {
            if self.ahead {
                log!("warn: Direct3D: render target changed with drawing not written back");
            }
            self.target = target;
            self.size = (info.w, info.h);
            self.stamp = 0;
            self.ahead = false;
            self.applied = None;
            self.bound = false;
            // the scissor and depth mask set below
            self.cache = Cache::default();
            unsafe {
                gl.bind_renderbuffer(glow::RENDERBUFFER, Some(self.depth));
                gl.renderbuffer_storage(glow::RENDERBUFFER, glow::DEPTH_COMPONENT24, info.w as i32, info.h as i32);
                super::gl::upload(gl, self.color, &vec![0; (info.w * info.h) as usize], info.w, info.h);
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(self.color), 0);
                gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::RENDERBUFFER, Some(self.depth));
                gl.disable(glow::SCISSOR_TEST);
                gl.depth_mask(true);
                gl.clear_depth_f32(1.0);
                gl.clear(glow::DEPTH_BUFFER_BIT);
            }
        }
        self.lost();
        if !self.bound {
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.viewport(0, 0, info.w as i32, info.h as i32);
            }
            self.bound = true;
        }
        if self.stamp != info.stamp {
            // DirectDraw wrote the pixels since: they replace the colour, the depth stays
            if self.ahead {
                log!("warn: Direct3D: drawing lost to a DirectDraw write");
            }
            // the framebuffer's first row is the bottom of the picture
            super::gl::upload(gl, self.color, &rgba(info, true), info.w, info.h);
            self.stamp = info.stamp;
            self.ahead = false;
        }
    }

    /// GL's state was changed by someone else (the window's drawing, a texture loaded): set
    /// everything again.
    fn lost(&mut self) {
        if super::gl::TOUCHED.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.applied = None;
            self.bound = false;
            self.cache = Cache::default();
        }
    }

    /// Write the colour texture to the target's pixels.
    fn write_back(&mut self, gl: &glow::Context, info: &ddraw::Info) {
        self.flush_batch(gl);
        let (w, h) = (info.w, info.h);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut buf)));
        }
        let [_, rm, gm, bm, _] = info.fmt;
        let pack = |c: u8, mask: u32| {
            let max = mask >> mask.trailing_zeros();
            ((c as u32 * max + 127) / 255) << mask.trailing_zeros()
        };
        let m = mem();
        for y in 0..h {
            let row = info.pixels + (h - 1 - y) * info.pitch;
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let v = pack(buf[i], rm) | pack(buf[i + 1], gm) | pack(buf[i + 2], bm);
                match info.bpp {
                    16 => m.wr16(row + 2 * x, v as u16),
                    32 => m.wr32(row + 4 * x, v),
                    _ => {}
                }
            }
        }
        self.ahead = false;
    }

    /// Draw only into a rectangle (x, y, width, height; y down).
    fn scissor(&mut self, gl: &glow::Context, r @ [x, y, w, h]: [u32; 4]) {
        if self.cache.scissor == Some(r) {
            return;
        }
        unsafe {
            if self.cache.scissor.is_none() {
                gl.enable(glow::SCISSOR_TEST);
            }
            gl.scissor(x as i32, self.size.1 as i32 - (y + h) as i32, w as i32, h as i32);
        }
        self.cache.scissor = Some(r);
    }

    fn clear(&mut self, gl: &glow::Context, st: &State, rects: &[[i32; 4]], flags: u32, color: u32, z: f32) {
        self.flush_batch(gl);
        // sets the colour and depth masks and the scissor
        self.applied = None;
        self.cache.depth_mask = None;
        let mut mask = 0;
        unsafe {
            if flags & 1 != 0 {
                let ch = |s: u32| (color >> s & 0xff) as f32 / 255.0;
                gl.clear_color(ch(16), ch(8), ch(0), ch(24));
                gl.color_mask(true, true, true, true);
                mask |= glow::COLOR_BUFFER_BIT;
                self.ahead = true;
            }
            if flags & 2 != 0 {
                gl.clear_depth_f32(z);
                gl.depth_mask(true);
                mask |= glow::DEPTH_BUFFER_BIT;
            }
            if mask == 0 {
                return;
            }
            if rects.is_empty() {
                self.scissor(gl, st.viewport);
                gl.clear(mask);
            }
            for &[x0, y0, x1, y1] in rects {
                self.scissor(gl, [x0.max(0) as u32, y0.max(0) as u32, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32]);
                gl.clear(mask);
            }
        }
    }

    /// The GL texture of a texture surface, loaded again when its pixels changed.
    fn texture(&mut self, gl: &glow::Context, surface: u32) -> Option<glow::Texture> {
        let info = ddraw::info(surface)?;
        if self.textures.get(&surface).is_none_or(|t| t.stamp != info.stamp) {
            // waiting draws may use the texture's old pixels
            self.flush_batch(gl);
        }
        let t = self.textures.entry(surface).or_insert_with(|| Tex { tex: super::gl::texture(gl, false), stamp: 0 });
        if t.stamp != info.stamp {
            if info.bpp == 8 {
                log!("warn: Direct3D: palette textures are not supported");
            }
            // v = 0 is the top row, GL's t = 0 the first row
            let px = rgba(&info, false);
            super::gl::upload(gl, t.tex, &px, info.w, info.h);
            t.stamp = info.stamp;
            dump(surface, &px, info.w, info.h);
        }
        Some(t.tex)
    }

    /// Queue a blit of `src`'s rectangle `sr` to the rectangle `dr` (`blit`): a textured quad
    /// whose colour-keyed texels fail the alpha test.
    fn blit(&mut self, gl: &glow::Context, src: u32, info: &ddraw::Info, dr: [i32; 4], sr: [i32; 4], key: Option<(u32, u32)>) {
        if self.blits.get(&(src, key)).is_none_or(|t| t.stamp != info.stamp) {
            self.flush_batch(gl);
        }
        let t = self.blits.entry((src, key)).or_insert_with(|| Tex { tex: super::gl::texture(gl, false), stamp: 0 });
        if t.stamp != info.stamp {
            super::gl::upload(gl, t.tex, &keyed(info, key), info.w, info.h);
            t.stamp = info.stamp;
        }
        let tex = Some(t.tex);
        let mut rs = [0; 64];
        rs[RS_ALPHATESTENABLE] = 1;
        rs[RS_ALPHAFUNC] = 5; // greater than 0
        rs[RS_CULLMODE] = 1;
        let mut ts = [0; 32];
        ts[TSS_COLOROP] = 2; // the texture
        ts[TSS_COLORARG1] = 2;
        ts[TSS_ALPHAOP] = 2;
        ts[TSS_ALPHAARG1] = 2;
        ts[TSS_ADDRESSU] = 3;
        ts[TSS_ADDRESSV] = 3;
        ts[TSS_MAGFILTER] = 1;
        ts[TSS_MINFILTER] = 1;
        let pipe = Pipe { mode: glow::TRIANGLES, tex, rs, ts, viewport: [0, 0, self.size.0, self.size.1] };
        // pixel edges are half a pixel before the centres
        let (x0, y0, x1, y1) = (dr[0] as f32 - 0.5, dr[1] as f32 - 0.5, dr[2] as f32 - 0.5, dr[3] as f32 - 0.5);
        let (w, h) = (info.w as f32, info.h as f32);
        let (u0, v0, u1, v1) = (sr[0] as f32 / w, sr[1] as f32 / h, sr[2] as f32 / w, sr[3] as f32 / h);
        let v = |x, y, u, v| Vertex { pos: [x, y, 0.0, 1.0], uv: [u, v], diffuse: !0, specular: 0 };
        let quad = [v(x0, y0, u0, v0), v(x1, y0, u1, v0), v(x0, y1, u0, v1), v(x1, y0, u1, v0), v(x1, y1, u1, v1), v(x0, y1, u0, v1)];
        match &mut self.batch {
            Some((p, v)) if *p == pipe => v.extend_from_slice(&quad),
            _ => {
                self.flush_batch(gl);
                self.batch = Some((pipe, quad.to_vec()));
            }
        }
    }

    /// Queue a draw: it joins the waiting ones when its state is theirs.
    fn draw(&mut self, gl: &glow::Context, st: &State, mode: u32, verts: &[Vertex]) {
        let tex = if st.tex[0] != 0 { self.texture(gl, st.tex[0]) } else { None };
        let mut rs = [0; 64];
        rs.copy_from_slice(&st.rs[..64]);
        let pipe = Pipe { mode, tex, rs, ts: st.tss[0], viewport: st.viewport };
        match &mut self.batch {
            Some((p, v)) if *p == pipe => v.extend_from_slice(verts),
            _ => {
                self.flush_batch(gl);
                self.batch = Some((pipe, verts.to_vec()));
            }
        }
        self.ahead = true;
    }

    /// Draw the waiting draws.
    fn flush_batch(&mut self, gl: &glow::Context) {
        let Some((pipe, verts)) = self.batch.take() else { return };
        let u = self.u;
        self.apply(gl, self.prog, &u, &pipe);
        unsafe {
            gl.bind_vertex_array(Some(self.vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(verts.as_ptr() as *const u8, size_of_val(&verts[..]));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            gl.draw_arrays(pipe.mode, 0, verts.len() as i32);
            gl.bind_vertex_array(None);
        }
    }

    /// Set GL up to draw into the target with `pipe`'s state, by `prog` (a vertex shader with
    /// the device's fragment shader; `u` its uniforms). Only what differs from what GL has is
    /// sent (`Cache`): draws of the device, the ground and the objects take turns, and each GL
    /// call costs the driver's thread.
    fn apply(&mut self, gl: &glow::Context, prog: glow::Program, u: &Uniforms, pipe: &Pipe) {
        self.lost();
        if self.bound && self.applied == Some((prog, *pipe)) {
            return;
        }
        self.applied = Some((prog, *pipe));
        let c = &mut self.cache;
        let (rs, ts, tex) = (&pipe.rs, &pipe.ts, pipe.tex);
        unsafe {
            if !self.bound {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
                gl.viewport(0, 0, self.size.0 as i32, self.size.1 as i32);
                self.bound = true;
            }
            if c.program != Some(prog) {
                gl.use_program(Some(prog));
                c.program = Some(prog);
            }
            if let Some(t) = tex {
                if c.tex != Some(t) {
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(t));
                    c.tex = Some(t);
                }
                let filter = |f: u32| if f >= 2 { glow::LINEAR } else { glow::NEAREST } as i32;
                let wrap = |a: u32| match a {
                    2 => glow::MIRRORED_REPEAT,
                    3 | 4 => glow::CLAMP_TO_EDGE,
                    _ => glow::REPEAT,
                } as i32;
                let want = [filter(ts[TSS_MAGFILTER]), filter(ts[TSS_MINFILTER]), wrap(ts[TSS_ADDRESSU]), wrap(ts[TSS_ADDRESSV])];
                let have = c.tex_params.entry(t).or_insert([-1; 4]);
                for (k, name) in [glow::TEXTURE_MAG_FILTER, glow::TEXTURE_MIN_FILTER, glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T].into_iter().enumerate() {
                    if have[k] != want[k] {
                        gl.tex_parameter_i32(glow::TEXTURE_2D, name, want[k]);
                        have[k] = want[k];
                    }
                }
            }
            let argb = |v: u32| [(v >> 16 & 0xff) as f32 / 255.0, (v >> 8 & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0, (v >> 24) as f32 / 255.0];
            let ff = |k: usize| f32::from_bits(rs[k]);
            let fog = match (rs[RS_FOGENABLE], rs[RS_FOGTABLEMODE]) {
                (0, _) => 0,
                (_, t @ 1..=3) => t as i32 + 1,
                _ => 1,
            };
            let fc = argb(rs[RS_FOGCOLOR]);
            let want = FsUniforms {
                size: [self.size.0 as f32, self.size.1 as f32],
                textured: tex.is_some() as i32,
                color_op: [ts[TSS_COLOROP] as i32, ts[TSS_COLORARG1] as i32, ts[TSS_COLORARG2] as i32],
                alpha_op: [ts[TSS_ALPHAOP] as i32, ts[TSS_ALPHAARG1] as i32, ts[TSS_ALPHAARG2] as i32],
                factor: argb(rs[RS_TEXTUREFACTOR]),
                alpha_func: if rs[RS_ALPHATESTENABLE] != 0 { rs[RS_ALPHAFUNC] as i32 } else { 8 },
                alpha_ref: (rs[RS_ALPHAREF] & 0xff) as f32,
                add_specular: (rs[RS_SPECULARENABLE] != 0) as i32,
                fog,
                fog_table: [ff(RS_FOGTABLESTART), ff(RS_FOGTABLEEND), ff(RS_FOGTABLEDENSITY)],
                fog_color: [fc[0], fc[1], fc[2]],
            };
            let have = c.fs.entry(prog).or_insert_with(|| {
                gl.uniform_1_i32(u.tex.as_ref(), 0);
                FsUniforms::unknown()
            });
            if have.size != want.size {
                gl.uniform_2_f32(u.size.as_ref(), want.size[0], want.size[1]);
            }
            if have.textured != want.textured {
                gl.uniform_1_i32(u.textured.as_ref(), want.textured);
            }
            if have.color_op != want.color_op {
                gl.uniform_3_i32_slice(u.color_op.as_ref(), &want.color_op);
            }
            if have.alpha_op != want.alpha_op {
                gl.uniform_3_i32_slice(u.alpha_op.as_ref(), &want.alpha_op);
            }
            if have.factor != want.factor {
                gl.uniform_4_f32_slice(u.factor.as_ref(), &want.factor);
            }
            if have.alpha_func != want.alpha_func {
                gl.uniform_1_i32(u.alpha_func.as_ref(), want.alpha_func);
            }
            if have.alpha_ref != want.alpha_ref {
                gl.uniform_1_f32(u.alpha_ref.as_ref(), want.alpha_ref);
            }
            if have.add_specular != want.add_specular {
                gl.uniform_1_i32(u.add_specular.as_ref(), want.add_specular);
            }
            if have.fog != want.fog {
                gl.uniform_1_i32(u.fog.as_ref(), want.fog);
            }
            if have.fog_table != want.fog_table {
                gl.uniform_3_f32_slice(u.fog_table.as_ref(), &want.fog_table);
            }
            if have.fog_color != want.fog_color {
                gl.uniform_3_f32_slice(u.fog_color.as_ref(), &want.fog_color);
            }
            *have = want;

            let blend = (rs[RS_ALPHABLENDENABLE] != 0).then(|| match rs[RS_SRCBLEND] {
                // the "both" factors set the destination's as well
                12 => (glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA),
                13 => (glow::ONE_MINUS_SRC_ALPHA, glow::SRC_ALPHA),
                s => (blend(s), blend(rs[RS_DESTBLEND])),
            });
            if c.blend != Some(blend) {
                match blend {
                    Some((s, d)) => {
                        if c.blend.is_none_or(|b| b.is_none()) {
                            gl.enable(glow::BLEND);
                        }
                        gl.blend_func(s, d);
                    }
                    None => gl.disable(glow::BLEND),
                }
                c.blend = Some(blend);
            }
            let depth = (rs[RS_ZENABLE] != 0).then(|| compare(rs[RS_ZFUNC]));
            if c.depth != Some(depth) {
                match depth {
                    Some(f) => {
                        if c.depth.is_none_or(|d| d.is_none()) {
                            gl.enable(glow::DEPTH_TEST);
                        }
                        gl.depth_func(f);
                    }
                    None => gl.disable(glow::DEPTH_TEST),
                }
                c.depth = Some(depth);
            }
            let mask = rs[RS_ZWRITEENABLE] != 0 && rs[RS_ZENABLE] != 0;
            if c.depth_mask != Some(mask) {
                gl.depth_mask(mask);
                c.depth_mask = Some(mask);
            }
            // both name the order as seen on the screen; GL's front faces are counter-clockwise
            let cull = match rs[RS_CULLMODE] {
                2 => Some(glow::BACK),
                3 => Some(glow::FRONT),
                _ => None,
            };
            if c.cull != Some(cull) {
                match cull {
                    Some(f) => {
                        gl.enable(glow::CULL_FACE);
                        gl.cull_face(f);
                    }
                    None => gl.disable(glow::CULL_FACE),
                }
                c.cull = Some(cull);
            }
            if !c.color_mask {
                gl.color_mask(true, true, true, true);
                c.color_mask = true;
            }
        }
        self.scissor(gl, pipe.viewport);
    }
}

/// The GL state `apply` set, so that it sets only what changes (None: not known).
#[derive(Default)]
struct Cache {
    program: Option<glow::Program>,
    tex: Option<glow::Texture>,
    /// per texture: magnification and minification filter, wrap s and t
    tex_params: HashMap<glow::Texture, [i32; 4]>,
    /// per program: the fragment shader's uniforms
    fs: HashMap<glow::Program, FsUniforms>,
    blend: Option<Option<(u32, u32)>>,
    depth: Option<Option<u32>>,
    depth_mask: Option<bool>,
    cull: Option<Option<u32>>,
    color_mask: bool,
    scissor: Option<[u32; 4]>,
}

/// The values of the device's fragment shader's uniforms.
#[derive(Clone, Copy, PartialEq)]
struct FsUniforms {
    size: [f32; 2],
    textured: i32,
    color_op: [i32; 3],
    alpha_op: [i32; 3],
    factor: [f32; 4],
    alpha_func: i32,
    alpha_ref: f32,
    add_specular: i32,
    fog: i32,
    fog_table: [f32; 3],
    fog_color: [f32; 3],
}

impl FsUniforms {
    /// Values no state has: everything is sent the first time.
    fn unknown() -> FsUniforms {
        FsUniforms {
            size: [-1.0; 2],
            textured: -1,
            color_op: [-1; 3],
            alpha_op: [-1; 3],
            factor: [-1.0; 4],
            alpha_func: -1,
            alpha_ref: -1.0,
            add_specular: -1,
            fog: -1,
            fog_table: [f32::NAN; 3],
            fog_color: [-1.0; 3],
        }
    }
}

/// `HOGS_D3D_DUMP=folder`: each texture as it is loaded, as `<surface>-<n>.png` (transparent
/// texels magenta).
fn dump(surface: u32, px: &[u32], w: u32, h: u32) {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    let Some(dir) = DIR.get_or_init(|| std::env::var("HOGS_D3D_DUMP").ok()) else { return };
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let rows: Vec<u32> = px
        .iter()
        .map(|&p| if p >> 24 == 0 { 0xff00ff } else { (p & 0xff) << 16 | (p & 0xff00) | (p >> 16 & 0xff) })
        .collect();
    let _ = std::fs::write(format!("{dir}/{surface:08x}-{n:04}.png"), super::window::png(&rows, w, h));
}

/// D3DBLEND as a GL blend factor.
fn blend(b: u32) -> u32 {
    match b {
        1 => glow::ZERO,
        3 => glow::SRC_COLOR,
        4 => glow::ONE_MINUS_SRC_COLOR,
        5 => glow::SRC_ALPHA,
        6 => glow::ONE_MINUS_SRC_ALPHA,
        7 => glow::DST_ALPHA,
        8 => glow::ONE_MINUS_DST_ALPHA,
        9 => glow::DST_COLOR,
        10 => glow::ONE_MINUS_DST_COLOR,
        11 => glow::SRC_ALPHA_SATURATE,
        _ => glow::ONE,
    }
}

/// D3DCMPFUNC as a GL comparison.
fn compare(f: u32) -> u32 {
    match f {
        1 => glow::NEVER,
        2 => glow::LESS,
        3 => glow::EQUAL,
        4 => glow::LEQUAL,
        5 => glow::GREATER,
        6 => glow::NOTEQUAL,
        7 => glow::GEQUAL,
        _ => glow::ALWAYS,
    }
}

/// Draw `seq` (indices into `n` vertices of format `fvf` at `verts`) as primitives of `kind`
/// (D3DPRIMITIVETYPE). Strips and fans become lists, so that flat shading takes the colour of
/// each triangle's first vertex, as Direct3D does.
fn draw(kind: u32, fvf: u32, verts: u32, n: u32, seq: &[u32]) {
    if fvf & 0xe != 4 {
        static SAID: OnceLock<()> = OnceLock::new();
        if SAID.set(()).is_ok() {
            log!("warn: Direct3D: vertices of FVF {fvf:#x} (not pre-transformed) are not supported");
        }
        return;
    }
    // the layout: XYZRHW, [normal], [reserved], [diffuse], [specular], texture coordinates
    let mut off = 16;
    if fvf & 0x10 != 0 {
        off += 12;
    }
    if fvf & 0x20 != 0 {
        off += 4;
    }
    let mut field = |present: bool| {
        let at = present.then_some(off);
        off += 4 * present as u32;
        at
    };
    let diffuse = field(fvf & 0x40 != 0);
    let specular = field(fvf & 0x80 != 0);
    let texs = (fvf >> 8) & 0xf;
    let tc = (texs > 0).then_some(off);
    let stride = off + 8 * texs;
    let m = mem();
    let f = |a: u32| f32::from_bits(m.rd32(a));
    let vertex = |i: u32| {
        let a = verts + i.min(n.saturating_sub(1)) * stride;
        Vertex {
            pos: [f(a), f(a + 4), f(a + 8), f(a + 12)],
            uv: tc.map_or([0.0; 2], |o| [f(a + o), f(a + o + 4)]),
            diffuse: diffuse.map_or(0xffff_ffff, |o| m.rd32(a + o)),
            specular: specular.map_or(0xff00_0000, |o| m.rd32(a + o)),
        }
    };
    let len = seq.len();
    let order: Vec<u32> = match kind {
        4 => seq[..len - len % 3].to_vec(),
        5 => (0..len.saturating_sub(2)).flat_map(|i| if i % 2 == 0 { [i, i + 1, i + 2] } else { [i, i + 2, i + 1] }).map(|i| seq[i]).collect(),
        6 => (1..len.saturating_sub(1)).flat_map(|i| [i, i + 1, 0]).map(|i| seq[i]).collect(),
        2 => seq[..len - len % 2].to_vec(),
        3 => (0..len.saturating_sub(1)).flat_map(|i| [i, i + 1]).map(|i| seq[i]).collect(),
        1 => seq.to_vec(),
        _ => {
            log!("warn: Direct3D: primitive type {kind} is not supported");
            return;
        }
    };
    let mut vs: Vec<Vertex> = order.into_iter().map(vertex).collect();
    let (mode, per) = match kind {
        1 => (glow::POINTS, 1),
        2 | 3 => (glow::LINES, 2),
        _ => (glow::TRIANGLES, 3),
    };
    let (flat, unsupported) = STATE.with_borrow(|st| {
        (st.rs[RS_SHADEMODE] == 1, (st.rs[RS_COLORKEYENABLE], st.tss[1][TSS_COLOROP]))
    });
    if flat {
        for p in vs.chunks_mut(per) {
            let (d, s) = (p[0].diffuse, p[0].specular);
            for v in p {
                v.diffuse = d;
                v.specular = s;
            }
        }
    }
    if unsupported != (0, 1) {
        static SAID: OnceLock<()> = OnceLock::new();
        if SAID.set(()).is_ok() {
            log!("warn: Direct3D: colour keys or texture stage 1 are not supported yet {unsupported:?}");
        }
    }
    // HOGS_D3D_LOG=<texture surface, hex>: only the draws with that texture, all vertices
    static ONLY: OnceLock<Option<u32>> = OnceLock::new();
    let only = *ONLY.get_or_init(|| std::env::var("HOGS_D3D_LOG").ok().and_then(|v| u32::from_str_radix(&v, 16).ok()));
    let st = STATE.with_borrow(|st| (st.tex[0], st.tss[0][TSS_COLOROP], st.tss[0][TSS_ALPHAOP], st.rs[RS_ALPHABLENDENABLE], st.rs[RS_SRCBLEND], st.rs[RS_DESTBLEND]));
    if trace() && only.is_none_or(|t| t == st.0) {
        log!("draw {kind} fvf {fvf:#x} {} vertices, texture/ops/blend {st:x?}", vs.len());
        for v in vs.iter().take(if only.is_some() { usize::MAX } else { 3 }) {
            log!("  {:?} {:?} {:08x} {:08x}", v.pos, v.uv, v.diffuse, v.specular);
        }
    }
    if !vs.is_empty() {
        with_render(|r, st, gl| r.draw(gl, st, mode, &vs));
    }
}

