//! DirectDraw (the old interfaces and DirectDraw 7, which Hogs of War uses through
//! DirectDrawCreateEx): surfaces in guest memory, blits, flips, and the primary surface shown
//! in the window.
//!
//! Surfaces get 16-bit 555 pixels. Windowed, the primary surface is the whole desktop and the
//! main window's part of it is shown; fullscreen it is the display mode. DDSURFACEDESC2 (124
//! bytes) is DDSURFACEDESC (108) with a larger DDSCAPS2 at the end: descriptors are written in
//! the size the caller gives.

use super::user32::{DESKTOP_H, DESKTOP_W};
use super::{Import, alloc, arg, com_new, free, guest_call, log, mem, new_cstr};
use crate::rt::Cpu;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const OK: u32 = 0;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DDERR_INVALIDPARAMS: u32 = 0x8007_0057;
const DDERR_NOTFOUND: u32 = 0x8876_00ff;
const DDERR_GENERIC: u32 = 0x8000_4005;

// DDSURFACEDESC flags
const DDSD_CAPS: u32 = 1;
const DDSD_HEIGHT: u32 = 2;
const DDSD_WIDTH: u32 = 4;
const DDSD_PITCH: u32 = 8;
const DDSD_BACKBUFFERCOUNT: u32 = 0x20;
const DDSD_LPSURFACE: u32 = 0x800;
const DDSD_PIXELFORMAT: u32 = 0x1000;
const DDSD_CKSRCBLT: u32 = 0x10000;
const DDSD_REFRESHRATE: u32 = 0x40000;
// DDSCAPS
const DDSCAPS_BACKBUFFER: u32 = 4;
const DDSCAPS_FLIP: u32 = 0x10;
const DDSCAPS_FRONTBUFFER: u32 = 0x20;
const DDSCAPS_PRIMARYSURFACE: u32 = 0x200;
const DDSCAPS_VIDEOMEMORY: u32 = 0x4000;
const DDSCAPS_VISIBLE: u32 = 0x8000;
// Blt
const DDBLT_COLORFILL: u32 = 0x400;
const DDBLT_KEYSRC: u32 = 0x8000;
const DDBLT_KEYSRCOVERRIDE: u32 = 0x10000;
const DDBLTFAST_SRCCOLORKEY: u32 = 1;
// SetColorKey
const DDCKEY_SRCBLT: u32 = 8;

struct Surface {
    w: u32,
    h: u32,
    pitch: u32,
    bpp: u32,
    /// DDPIXELFORMAT flags and the red, green, blue, alpha masks
    fmt: [u32; 5],
    pixels: u32,
    caps: u32,
    /// counts writes to the pixels (Direct3D uploads a texture or render target again after one)
    stamp: u32,
    /// the next surface in a flip chain (the back buffer of a primary)
    back: u32,
    /// source colour key: lowest and highest transparent value
    ckey: Option<(u32, u32)>,
    palette: u32,
    clipper: u32,
    refs: u32,
}

struct DirectDraw {
    surfaces: BTreeMap<u32, Surface>,
    /// palettes: 256 entries of 0RGB
    palettes: BTreeMap<u32, Vec<u32>>,
    clippers: BTreeMap<u32, u32>,
    /// display mode set by SetDisplayMode (fullscreen)
    mode: Option<(u32, u32, u32)>,
    primary: u32,
}

static DD: Mutex<DirectDraw> =
    Mutex::new(DirectDraw { surfaces: BTreeMap::new(), palettes: BTreeMap::new(), clippers: BTreeMap::new(), mode: None, primary: 0 });

fn dd() -> MutexGuard<'static, DirectDraw> {
    DD.lock().unwrap_or_else(|e| e.into_inner())
}

fn trace() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var_os("HOGS_DD_LOG").is_some())
}

const RMASK: u32 = 0x7c00;
const GMASK: u32 = 0x03e0;
const BMASK: u32 = 0x001f;

fn rect(p: u32) -> Option<[i32; 4]> {
    (p != 0).then(|| {
        let m = mem();
        [m.rd32(p) as i32, m.rd32(p + 4) as i32, m.rd32(p + 8) as i32, m.rd32(p + 12) as i32]
    })
}

/// Data1 of the GUID at `p`.
fn iid(p: u32) -> u32 {
    if p == 0 { 0 } else { mem().rd32(p) }
}

// ---- showing the primary surface ----------------------------------------------------------------

fn to_rgb(p: u16) -> u32 {
    let (r, g, b) = ((p >> 10) & 31, (p >> 5) & 31, p & 31);
    let e = |v: u16| ((v << 3) | (v >> 2)) as u32;
    e(r) << 16 | e(g) << 8 | e(b)
}

/// When the primary surface changed without being shown yet. Blits only mark it: the game
/// draws a screen in many blits, and showing each one makes the menu flicker as it builds up.
/// It is shown when the game gets its messages again (`show_pending`), at a flip, or 50 ms
/// after the first unshown change (long loads without messages).
static DIRTY: Mutex<Option<Instant>> = Mutex::new(None);

fn changed(d: &DirectDraw) {
    let first = *DIRTY.lock().unwrap().get_or_insert_with(Instant::now);
    if first.elapsed() >= Duration::from_millis(50) {
        shown();
        present(d);
    }
}

fn shown() {
    *DIRTY.lock().unwrap() = None;
}

/// Show the primary surface if it changed since it was last shown.
pub fn show_pending() {
    if DIRTY.lock().unwrap().take().is_some() {
        present(&dd());
    }
}

/// Show what the window sees of the primary surface.
fn present(d: &DirectDraw) {
    let Some(s) = d.surfaces.get(&d.primary) else { return };
    let (x0, y0, w, h) = match d.mode {
        Some(_) => (0, 0, s.w as i32, s.h as i32),
        None => {
            let r = super::user32::main_rect();
            (r[0], r[1], r[2] - r[0], r[3] - r[1])
        }
    };
    if w <= 0 || h <= 0 {
        return;
    }
    let m = mem();
    let pal = d.palettes.get(&s.palette);
    let mut out = vec![0u32; (w * h) as usize];
    for y in 0..h {
        let sy = y + y0;
        if sy < 0 || sy >= s.h as i32 {
            continue;
        }
        for x in 0..w {
            let sx = x + x0;
            if sx < 0 || sx >= s.w as i32 {
                continue;
            }
            let a = s.pixels + sy as u32 * s.pitch;
            out[(y * w + x) as usize] = if s.bpp == 8 {
                pal.map_or(0, |p| p[m.rd8(a + sx as u32) as usize])
            } else {
                to_rgb(m.rd16(a + 2 * sx as u32))
            };
        }
    }
    super::window::present(&out, w as u32, h as u32);
}

// ---- DirectDrawCreate ---------------------------------------------------------------------------

fn new_ddraw(v2: bool) -> u32 {
    com_new(if v2 { DDRAW2 } else { DDRAW1 }, 0)
}

fn direct_draw_create(c: &mut Cpu) -> u32 {
    let out = arg(c, 1);
    mem().wr32(out, new_ddraw(false));
    OK
}

/// DirectDrawCreateEx(guid, out, IID_IDirectDraw7, outer)
fn direct_draw_create_ex(c: &mut Cpu) -> u32 {
    let (out, id) = (arg(c, 1), iid(arg(c, 2)));
    if id != IID_DDRAW7 {
        log!("warn: DirectDrawCreateEx({id:#010x}) not supported");
        return E_NOINTERFACE;
    }
    mem().wr32(out, com_new(DDRAW7, 0));
    OK
}

/// DirectDrawEnumerateExA(callback, context, flags): one device, the primary display.
fn direct_draw_enumerate_ex(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 0), arg(c, 1));
    static NAMES: OnceLock<(u32, u32)> = OnceLock::new();
    let &(desc, name) = NAMES.get_or_init(|| (new_cstr("Primary Display Driver"), new_cstr("display")));
    guest_call(c, cb, &[0, desc, name, ctx, 0]);
    OK
}

const IID_DDRAW7: u32 = 0x15e6_5ec0;

fn direct_draw_enumerate(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 0), arg(c, 1));
    static NAMES: OnceLock<(u32, u32)> = OnceLock::new();
    let &(desc, name) = NAMES.get_or_init(|| (new_cstr("Primary Display Driver"), new_cstr("display")));
    guest_call(c, cb, &[0, desc, name, ctx]);
    OK
}

// ---- IUnknown -----------------------------------------------------------------------------------

fn add_ref(_: &mut Cpu) -> u32 {
    1
}

fn release(_: &mut Cpu) -> u32 {
    0
}

// ---- IDirectDraw ----------------------------------------------------------------------------------

fn dd_query_interface(c: &mut Cpu) -> u32 {
    let (id, out) = (iid(arg(c, 1)), arg(c, 2));
    let obj = match id {
        0x6c14_db80 => new_ddraw(false),
        0xb3a6_f3e0 => new_ddraw(true),
        IID_DDRAW7 => com_new(DDRAW7, 0),
        super::d3d::IID_D3D7 => super::d3d::new_d3d7(),
        _ => {
            log!("warn: IDirectDraw::QueryInterface({id:#010x}) not supported");
            mem().wr32(out, 0);
            return E_NOINTERFACE;
        }
    };
    mem().wr32(out, obj);
    OK
}

fn dd_create_clipper(c: &mut Cpu) -> u32 {
    let out = arg(c, 2);
    let obj = com_new(CLIPPER, 0);
    dd().clippers.insert(obj, 0);
    mem().wr32(out, obj);
    OK
}

fn dd_create_palette(c: &mut Cpu) -> u32 {
    let (entries, out) = (arg(c, 2), arg(c, 3));
    let obj = com_new(PALETTE, 0);
    let m = mem();
    let pal = (0..256).map(|k| if entries == 0 { 0 } else { entry_rgb(m.rd32(entries + 4 * k)) }).collect();
    dd().palettes.insert(obj, pal);
    m.wr32(out, obj);
    OK
}

/// PALETTEENTRY (r, g, b, flags) as 0RGB
fn entry_rgb(e: u32) -> u32 {
    (e & 0xff) << 16 | (e & 0xff00) | (e >> 16) & 0xff
}

/// The size of a surface descriptor whose dwSize field is at `p`: DDSURFACEDESC2 or the old one.
fn desc_size(p: u32) -> u32 {
    if mem().rd32(p) == 124 { 124 } else { 108 }
}

/// 16-bit 555 RGB: the format of surfaces that do not ask for one.
const RGB555: [u32; 5] = [0x40, RMASK, GMASK, BMASK, 0];

fn new_surface(d: &mut DirectDraw, w: u32, h: u32, bpp: u32, fmt: [u32; 5], caps: u32, v7: bool) -> u32 {
    let pitch = (w * bpp / 8 + 7) & !7;
    let pixels = alloc(pitch * h);
    let obj = com_new(if v7 { SURFACE7 } else { SURFACE }, 0);
    d.surfaces.insert(obj, Surface { w, h, pitch, bpp, fmt, pixels, caps, stamp: next_stamp(), back: 0, ckey: None, palette: 0, clipper: 0, refs: 1 });
    obj
}

/// What Direct3D needs to know of a surface.
pub struct Info {
    pub w: u32,
    pub h: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub fmt: [u32; 5],
    pub pixels: u32,
    pub caps: u32,
    pub stamp: u32,
    pub ckey: Option<(u32, u32)>,
}

pub fn info(surface: u32) -> Option<Info> {
    let d = dd();
    let s = d.surfaces.get(&surface)?;
    Some(Info { w: s.w, h: s.h, pitch: s.pitch, bpp: s.bpp, fmt: s.fmt, pixels: s.pixels, caps: s.caps, stamp: s.stamp, ckey: s.ckey })
}

/// Before the pixels of `surface` are read or written here: what Direct3D drew into it is
/// written to them first.
fn sync(surface: u32) {
    super::d3d::flush(surface);
}

/// A new value for `Surface::stamp`, unique among all surfaces (a surface made where a freed
/// one was is not taken for it).
fn next_stamp() -> u32 {
    static GEN: AtomicU32 = AtomicU32::new(0);
    GEN.fetch_add(1, Ordering::Relaxed) + 1
}

fn touched(d: &mut DirectDraw, surface: u32) {
    if let Some(s) = d.surfaces.get_mut(&surface) {
        s.stamp = next_stamp();
    }
}

fn dd_create_surface(c: &mut Cpu) -> u32 {
    let (desc, out) = (arg(c, 1), arg(c, 2));
    let m = mem();
    let flags = m.rd32(desc + 4);
    let caps = if flags & DDSD_CAPS != 0 { m.rd32(desc + 0x68) } else { 0 };
    let v7 = desc_size(desc) == 124;
    let mut d = dd();
    let obj = if caps & DDSCAPS_PRIMARYSURFACE != 0 {
        let (w, h, bpp) = d.mode.unwrap_or((DESKTOP_W, DESKTOP_H, 16));
        let prim = new_surface(&mut d, w, h, bpp, RGB555, caps | DDSCAPS_VISIBLE | DDSCAPS_FRONTBUFFER | DDSCAPS_VIDEOMEMORY, v7);
        let backs = if flags & DDSD_BACKBUFFERCOUNT != 0 { m.rd32(desc + 0x14) } else { 0 };
        let mut prev = prim;
        for _ in 0..backs {
            let b = new_surface(&mut d, w, h, bpp, RGB555, (caps & !(DDSCAPS_PRIMARYSURFACE | DDSCAPS_VISIBLE)) | DDSCAPS_BACKBUFFER | DDSCAPS_VIDEOMEMORY, v7);
            d.surfaces.get_mut(&prev).unwrap().back = b;
            prev = b;
        }
        if backs > 0 {
            // a flip chain is a ring
            d.surfaces.get_mut(&prev).unwrap().back = prim;
        }
        d.primary = prim;
        prim
    } else {
        let w = if flags & DDSD_WIDTH != 0 { m.rd32(desc + 0xc) } else { 0 };
        let h = if flags & DDSD_HEIGHT != 0 { m.rd32(desc + 8) } else { 0 };
        let (bpp, fmt) = if flags & DDSD_PIXELFORMAT != 0 {
            let pf = desc + 0x48;
            let fmt = [m.rd32(pf + 4), m.rd32(pf + 0x10), m.rd32(pf + 0x14), m.rd32(pf + 0x18), m.rd32(pf + 0x1c)];
            match m.rd32(pf + 0xc) {
                b @ (8 | 16 | 32) => (b, fmt),
                _ => (16, RGB555),
            }
        } else {
            (16, RGB555)
        };
        if w == 0 || h == 0 {
            log!("warn: CreateSurface without a size (flags {flags:#x}, caps {caps:#x})");
            return DDERR_INVALIDPARAMS;
        }
        let s = new_surface(&mut d, w, h, bpp, fmt, caps | DDSCAPS_VIDEOMEMORY, v7);
        if flags & DDSD_CKSRCBLT != 0 {
            d.surfaces.get_mut(&s).unwrap().ckey = Some((m.rd32(desc + 0x40), m.rd32(desc + 0x44)));
        }
        s
    };
    if trace() {
        let s = &d.surfaces[&obj];
        log!("CreateSurface {obj:#x}: {}x{}x{} format {:x?} caps {:#x}", s.w, s.h, s.bpp, s.fmt, s.caps);
    }
    m.wr32(out, obj);
    OK
}

fn mode_desc(p: u32, size: u32, w: u32, h: u32, bpp: u32) {
    let m = mem();
    m.slice(p, size as usize).fill(0);
    m.wr32(p, size);
    m.wr32(p + 4, DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_PIXELFORMAT | DDSD_REFRESHRATE);
    m.wr32(p + 8, h);
    m.wr32(p + 0xc, w);
    m.wr32(p + 0x10, w * bpp / 8);
    m.wr32(p + 0x18, 60);
    pixel_format(p + 0x48, bpp, RGB555);
}

fn pixel_format(p: u32, bpp: u32, fmt: [u32; 5]) {
    let m = mem();
    m.slice(p, 32).fill(0);
    m.wr32(p, 32);
    m.wr32(p + 0xc, bpp);
    if bpp == 8 {
        m.wr32(p + 4, 0x60); // DDPF_RGB | DDPF_PALETTEINDEXED8
    } else {
        m.wr32(p + 4, fmt[0]);
        for k in 0..4 {
            m.wr32(p + 0x10 + 4 * k, fmt[1 + k as usize]);
        }
    }
}

/// The display modes offered: the game's resolution list (index 0-9) at 16 bits.
const MODES: [(u32, u32); 11] =
    [(320, 240), (400, 300), (512, 384), (640, 400), (640, 480), (800, 600), (960, 720), (1024, 768), (1120, 840), (1152, 864), (1280, 960)];

fn enum_display_modes(c: &mut Cpu, size: u32) -> u32 {
    let (ctx, cb) = (arg(c, 3), arg(c, 4));
    let desc = alloc(size);
    for (w, h) in MODES {
        mode_desc(desc, size, w, h, 16);
        if guest_call(c, cb, &[desc, ctx]) == 0 {
            break;
        }
    }
    free(desc);
    OK
}

fn dd_enum_display_modes(c: &mut Cpu) -> u32 {
    enum_display_modes(c, 108)
}
fn dd7_enum_display_modes(c: &mut Cpu) -> u32 {
    enum_display_modes(c, 124)
}

fn caps(p: u32) {
    if p == 0 {
        return;
    }
    let m = mem();
    let size = m.rd32(p).clamp(4, 400);
    m.slice(p + 4, size as usize - 4).fill(0);
    m.wr32(p + 4, 0x40 | 0x40_0000 | 0x400_0000); // DDCAPS_BLT, COLORKEY, BLTCOLORFILL
    m.wr32(p + 0xc, 0x200); // DDCKEYCAPS_SRCBLT
    m.wr32(p + 0x3c, 16 << 20);
    m.wr32(p + 0x40, 16 << 20);
}

fn dd_get_caps(c: &mut Cpu) -> u32 {
    caps(arg(c, 1));
    caps(arg(c, 2));
    OK
}

fn dd_get_display_mode(c: &mut Cpu) -> u32 {
    let (w, h, bpp) = dd().mode.unwrap_or((DESKTOP_W, DESKTOP_H, 16));
    mode_desc(arg(c, 1), desc_size(arg(c, 1)), w, h, bpp);
    OK
}

fn dd_set_cooperative_level(c: &mut Cpu) -> u32 {
    if trace() {
        log!("SetCooperativeLevel({:#x}, {:#x})", arg(c, 1), arg(c, 2));
    }
    OK
}

fn set_mode(w: u32, h: u32, bpp: u32) -> u32 {
    if trace() {
        log!("SetDisplayMode {w}x{h}x{bpp}");
    }
    dd().mode = Some((w, h, bpp));
    OK
}

fn dd_set_display_mode(c: &mut Cpu) -> u32 {
    set_mode(arg(c, 1), arg(c, 2), arg(c, 3))
}

fn dd_restore_display_mode(_: &mut Cpu) -> u32 {
    dd().mode = None;
    OK
}

fn dd_get_monitor_frequency(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), 60);
    OK
}

fn dd_get_available_vid_mem(c: &mut Cpu) -> u32 {
    for k in [2, 3] {
        if arg(c, k) != 0 {
            mem().wr32(arg(c, k), 16 << 20);
        }
    }
    OK
}

fn dd_vblank_status(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), 1);
    OK
}

fn ok(_: &mut Cpu) -> u32 {
    OK
}

fn unsupported(_: &mut Cpu) -> u32 {
    log!("warn: unsupported DirectDraw method");
    DDERR_GENERIC
}

macro_rules! ddraw_methods {
    ($set_mode_args:expr) => {
        &[
            ("IDirectDraw::QueryInterface", 3, dd_query_interface),
            ("IDirectDraw::AddRef", 1, add_ref),
            ("IDirectDraw::Release", 1, release),
            ("IDirectDraw::Compact", 1, ok),
            ("IDirectDraw::CreateClipper", 4, dd_create_clipper),
            ("IDirectDraw::CreatePalette", 5, dd_create_palette),
            ("IDirectDraw::CreateSurface", 4, dd_create_surface),
            ("IDirectDraw::DuplicateSurface", 3, unsupported),
            ("IDirectDraw::EnumDisplayModes", 5, dd_enum_display_modes),
            ("IDirectDraw::EnumSurfaces", 5, unsupported),
            ("IDirectDraw::FlipToGDISurface", 1, ok),
            ("IDirectDraw::GetCaps", 3, dd_get_caps),
            ("IDirectDraw::GetDisplayMode", 2, dd_get_display_mode),
            ("IDirectDraw::GetFourCCCodes", 3, unsupported),
            ("IDirectDraw::GetGDISurface", 2, unsupported),
            ("IDirectDraw::GetMonitorFrequency", 2, dd_get_monitor_frequency),
            ("IDirectDraw::GetScanLine", 2, unsupported),
            ("IDirectDraw::GetVerticalBlankStatus", 2, dd_vblank_status),
            ("IDirectDraw::Initialize", 2, ok),
            ("IDirectDraw::RestoreDisplayMode", 1, dd_restore_display_mode),
            ("IDirectDraw::SetCooperativeLevel", 3, dd_set_cooperative_level),
            ("IDirectDraw::SetDisplayMode", $set_mode_args, dd_set_display_mode),
            ("IDirectDraw::WaitForVerticalBlank", 3, dd_wait_for_vertical_blank),
            ("IDirectDraw2::GetAvailableVidMem", 4, dd_get_available_vid_mem),
        ]
    };
}

/// IDirectDraw and IDirectDraw2 differ in SetDisplayMode (IDirectDraw2 adds refresh rate and flags).
static DDRAW1: &[Import] = ddraw_methods!(4);
static DDRAW2: &[Import] = ddraw_methods!(6);

fn dd7_get_device_identifier(c: &mut Cpu) -> u32 {
    // DDDEVICEIDENTIFIER2: driver and description strings (512 each), versions and ids
    let p = arg(c, 1);
    mem().slice(p, 1072).fill(0);
    super::put_cstr(p, "display", 512);
    super::put_cstr(p + 512, "Primary Display Driver", 512);
    OK
}

static DDRAW7: &[Import] = &[
    ("IDirectDraw7::QueryInterface", 3, dd_query_interface),
    ("IDirectDraw7::AddRef", 1, add_ref),
    ("IDirectDraw7::Release", 1, release),
    ("IDirectDraw7::Compact", 1, ok),
    ("IDirectDraw7::CreateClipper", 4, dd_create_clipper),
    ("IDirectDraw7::CreatePalette", 5, dd_create_palette),
    ("IDirectDraw7::CreateSurface", 4, dd_create_surface),
    ("IDirectDraw7::DuplicateSurface", 3, unsupported),
    ("IDirectDraw7::EnumDisplayModes", 5, dd7_enum_display_modes),
    ("IDirectDraw7::EnumSurfaces", 5, unsupported),
    ("IDirectDraw7::FlipToGDISurface", 1, ok),
    ("IDirectDraw7::GetCaps", 3, dd_get_caps),
    ("IDirectDraw7::GetDisplayMode", 2, dd_get_display_mode),
    ("IDirectDraw7::GetFourCCCodes", 3, unsupported),
    ("IDirectDraw7::GetGDISurface", 2, unsupported),
    ("IDirectDraw7::GetMonitorFrequency", 2, dd_get_monitor_frequency),
    ("IDirectDraw7::GetScanLine", 2, unsupported),
    ("IDirectDraw7::GetVerticalBlankStatus", 2, dd_vblank_status),
    ("IDirectDraw7::Initialize", 2, ok),
    ("IDirectDraw7::RestoreDisplayMode", 1, dd_restore_display_mode),
    ("IDirectDraw7::SetCooperativeLevel", 3, dd_set_cooperative_level),
    ("IDirectDraw7::SetDisplayMode", 6, dd_set_display_mode),
    ("IDirectDraw7::WaitForVerticalBlank", 3, dd_wait_for_vertical_blank),
    ("IDirectDraw7::GetAvailableVidMem", 4, dd_get_available_vid_mem),
    ("IDirectDraw7::GetSurfaceFromDC", 3, unsupported),
    ("IDirectDraw7::RestoreAllSurfaces", 1, ok),
    ("IDirectDraw7::TestCooperativeLevel", 1, ok),
    ("IDirectDraw7::GetDeviceIdentifier", 3, dd7_get_device_identifier),
    ("IDirectDraw7::StartModeTest", 4, unsupported),
    ("IDirectDraw7::EvaluateMode", 3, unsupported),
];

// ---- IDirectDrawSurface -------------------------------------------------------------------------

fn s_query_interface(c: &mut Cpu) -> u32 {
    let (this, id, out) = (arg(c, 0), iid(arg(c, 1)), arg(c, 2));
    match id {
        // IDirectDrawSurface, 2, 3: one method table serves them all
        // (and 4, 7 for surfaces of DirectDraw 7)
        0x6c14_db81 | 0x5780_5885 | 0xda04_4e00 | 0x0b2b_8630 | 0x0667_5a80 => {
            mem().wr32(out, this);
            OK
        }
        _ => {
            log!("warn: IDirectDrawSurface::QueryInterface({id:#010x}) not supported");
            mem().wr32(out, 0);
            E_NOINTERFACE
        }
    }
}

fn s_add_ref(c: &mut Cpu) -> u32 {
    let mut d = dd();
    d.surfaces.get_mut(&arg(c, 0)).map_or(0, |s| {
        s.refs += 1;
        s.refs
    })
}

fn s_release(c: &mut Cpu) -> u32 {
    let this = arg(c, 0);
    let mut d = dd();
    let Some(s) = d.surfaces.get_mut(&this) else { return 0 };
    s.refs = s.refs.saturating_sub(1);
    let r = s.refs;
    if r == 0 {
        // a primary takes its flip chain with it
        let mut k = this;
        loop {
            let Some(s) = d.surfaces.remove(&k) else { break };
            free(s.pixels);
            k = s.back;
            if k == 0 || k == this {
                break;
            }
        }
        if d.primary == this {
            d.primary = 0;
        }
    }
    r
}

/// Clip a copy of w x h from (sx, sy) to (dx, dy) to both surfaces.
fn clip(dst: &Surface, src: Option<&Surface>, d: &mut [i32; 4], s: &mut [i32; 4]) {
    // d, s: x0 y0 x1 y1; a copy without stretching keeps both the same size
    for k in 0..2 {
        let lo = -d[k].min(s[k]).min(0);
        d[k] += lo;
        s[k] += lo;
        let dmax = if k == 0 { dst.w } else { dst.h } as i32;
        let over = (d[k + 2] - dmax).max(0);
        d[k + 2] -= over;
        s[k + 2] -= over;
        if let Some(src) = src {
            let smax = if k == 0 { src.w } else { src.h } as i32;
            let over = (s[k + 2] - smax).max(0);
            d[k + 2] -= over;
            s[k + 2] -= over;
        }
    }
}

fn fill(s: &Surface, r: [i32; 4], color: u32) {
    let x0 = r[0].clamp(0, s.w as i32) as u32;
    let x1 = r[2].clamp(0, s.w as i32) as u32;
    let y0 = r[1].clamp(0, s.h as i32) as u32;
    let y1 = r[3].clamp(0, s.h as i32) as u32;
    for y in y0..y1 {
        let row = s.pixels + y * s.pitch;
        for x in x0..x1 {
            put(row, x, s.bpp, color);
        }
    }
}

/// Pixel x of a row at `bpp` bits.
fn get(row: u32, x: u32, bpp: u32) -> u32 {
    let m = mem();
    match bpp {
        8 => m.rd8(row + x) as u32,
        16 => m.rd16(row + 2 * x) as u32,
        _ => m.rd32(row + 4 * x),
    }
}

fn put(row: u32, x: u32, bpp: u32, v: u32) {
    let m = mem();
    match bpp {
        8 => m.wr8(row + x, v as u8),
        16 => m.wr16(row + 2 * x, v as u16),
        _ => m.wr32(row + 4 * x, v),
    }
}

/// Copy src rectangle `sr` to dst rectangle `dr` (stretched if the sizes differ), skipping
/// source pixels inside the colour key range.
fn copy(dst: &Surface, src: &Surface, dr: [i32; 4], sr: [i32; 4], key: Option<(u32, u32)>) {
    let m = mem();
    let (dw, dh, sw, sh) = (dr[2] - dr[0], dr[3] - dr[1], sr[2] - sr[0], sr[3] - sr[1]);
    if dw <= 0 || dh <= 0 || sw <= 0 || sh <= 0 {
        return;
    }
    let bytes = (src.bpp / 8) as i32;
    // a plain copy row by row (source and destination may be the same surface)
    if dw == sw && dh == sh && key.is_none() && src.bpp == dst.bpp {
        let order: Vec<i32> = if dst.pixels == src.pixels && dr[1] > sr[1] { (0..dh).rev().collect() } else { (0..dh).collect() };
        for y in order {
            let s = src.pixels + (sr[1] + y) as u32 * src.pitch + (sr[0] * bytes) as u32;
            let d = dst.pixels + (dr[1] + y) as u32 * dst.pitch + (dr[0] * bytes) as u32;
            let row = m.slice(s, (sw * bytes) as usize).to_vec();
            m.slice(d, row.len()).copy_from_slice(&row);
        }
        return;
    }
    for y in 0..dh {
        let sy = sr[1] + y * sh / dh;
        if sy < 0 || sy >= src.h as i32 || dr[1] + y < 0 || dr[1] + y >= dst.h as i32 {
            continue;
        }
        for x in 0..dw {
            let sx = sr[0] + x * sw / dw;
            let dx = dr[0] + x;
            if sx < 0 || sx >= src.w as i32 || dx < 0 || dx >= dst.w as i32 {
                continue;
            }
            let v = get(src.pixels + sy as u32 * src.pitch, sx as u32, src.bpp);
            if key.is_some_and(|(lo, hi)| v >= lo && v <= hi) {
                continue;
            }
            put(dst.pixels + (dr[1] + y) as u32 * dst.pitch, dx as u32, dst.bpp, v);
        }
    }
}

fn s_blt(c: &mut Cpu) -> u32 {
    let (this, dr, src, sr, flags, fx) = (arg(c, 0), rect(arg(c, 1)), arg(c, 2), rect(arg(c, 3)), arg(c, 4), arg(c, 5));
    sync(this);
    sync(src);
    let mut d = dd();
    let Some(dst) = d.surfaces.get(&this) else { return DDERR_INVALIDPARAMS };
    if trace() {
        log!("Blt {this:#x} {dr:?} <- {src:#x} {sr:?} flags {flags:#x}");
    }
    let full = |s: &Surface| [0, 0, s.w as i32, s.h as i32];
    if flags & DDBLT_COLORFILL != 0 {
        let color = if fx != 0 { mem().rd32(fx + 0x50) } else { 0 };
        if trace() && fx != 0 {
            log!("  fill {color:#x} (DDBLTFX size {}, at 0x48: {:#x})", mem().rd32(fx), mem().rd32(fx + 0x48));
        }
        fill(dst, dr.unwrap_or(full(dst)), color);
    } else {
        let Some(s) = d.surfaces.get(&src) else { return DDERR_INVALIDPARAMS };
        let key = if flags & DDBLT_KEYSRCOVERRIDE != 0 && fx != 0 {
            Some((mem().rd32(fx + 0x5c), mem().rd32(fx + 0x60)))
        } else if flags & DDBLT_KEYSRC != 0 {
            s.ckey
        } else {
            None
        };
        let (mut a, mut b) = (dr.unwrap_or(full(dst)), sr.unwrap_or(full(s)));
        if a[2] - a[0] == b[2] - b[0] && a[3] - a[1] == b[3] - b[1] {
            clip(dst, Some(s), &mut a, &mut b);
        }
        copy(dst, s, a, b, key);
    }
    touched(&mut d, this);
    if this == d.primary {
        changed(&d);
    }
    OK
}

fn s_blt_fast(c: &mut Cpu) -> u32 {
    let (this, x, y, src, sr, flags) = (arg(c, 0), arg(c, 1) as i32, arg(c, 2) as i32, arg(c, 3), rect(arg(c, 4)), arg(c, 5));
    sync(this);
    sync(src);
    let mut d = dd();
    let (Some(dst), Some(s)) = (d.surfaces.get(&this), d.surfaces.get(&src)) else { return DDERR_INVALIDPARAMS };
    if trace() {
        log!("BltFast {this:#x} ({x},{y}) <- {src:#x} {sr:?} flags {flags:#x}");
    }
    let mut b = sr.unwrap_or([0, 0, s.w as i32, s.h as i32]);
    let mut a = [x, y, x + b[2] - b[0], y + b[3] - b[1]];
    clip(dst, Some(s), &mut a, &mut b);
    copy(dst, s, a, b, if flags & DDBLTFAST_SRCCOLORKEY != 0 { s.ckey } else { None });
    touched(&mut d, this);
    if this == d.primary {
        changed(&d);
    }
    OK
}

/// The display's refresh: a flip waits for it (the front end has no clock of its own and
/// relied on this; its animations ran at 60 frames a second on a 60 Hz monitor). Missions
/// step at 25 a second by their own clock (40 ms, `[[0x537f24] + 0x314]`) and draw pictures
/// between the steps (`fps.rs`): they do not wait.
const REFRESH: Duration = Duration::from_nanos(1_000_000_000 / 60);

/// Wait until a refresh has passed since the last flip.
fn vertical_blank() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = *last {
        let due = t + REFRESH;
        // sleep most of it, then spin: sleeping is only exact to a millisecond or so
        if let Some(d) = due.checked_duration_since(Instant::now()).and_then(|d| d.checked_sub(Duration::from_millis(2))) {
            std::thread::sleep(d);
        }
        while Instant::now() < due {
            std::hint::spin_loop();
        }
        // a late frame starts a new rhythm instead of hurrying the next ones
        *last = Some(if Instant::now() - due > REFRESH { Instant::now() } else { due });
    } else {
        *last = Some(Instant::now());
    }
}

fn dd_wait_for_vertical_blank(_: &mut Cpu) -> u32 {
    vertical_blank();
    OK
}

fn s_flip(c: &mut Cpu) -> u32 {
    let this = arg(c, 0);
    // missions draw as often as they can (or as `fps.rs` caps them)
    if !super::fps::active() {
        vertical_blank();
    }
    super::fps::stats();
    let back = dd().surfaces.get(&this).map_or(0, |s| s.back);
    // a picture Direct3D drew (all of a mission's) is shown from the GPU as it is, not read
    // back into the surface first: the surface's pixels stay behind until something reads them
    super::d3d::finish();
    let drawn = super::d3d::ahead(back);
    if !drawn {
        sync(this);
        sync(back);
    }
    let mut d = dd();
    if back == 0 || !d.surfaces.contains_key(&back) {
        return DDERR_INVALIDPARAMS;
    }
    touched(&mut d, this);
    touched(&mut d, back);
    // the front surface shows what the back buffer held, and the back buffer gets the old front
    let a = d.surfaces[&this].pixels;
    let b = d.surfaces[&back].pixels;
    d.surfaces.get_mut(&this).unwrap().pixels = b;
    d.surfaces.get_mut(&back).unwrap().pixels = a;
    shown();
    if drawn {
        let stamp = d.surfaces[&back].stamp;
        drop(d);
        super::d3d::show(stamp);
    } else {
        present(&d);
    }
    OK
}

fn s_get_attached_surface(c: &mut Cpu) -> u32 {
    let (this, caps, out) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let want = mem().rd32(caps);
    let mut d = dd();
    let back = d.surfaces.get(&this).map_or(0, |s| s.back);
    match d.surfaces.get_mut(&back) {
        Some(b) if want & (DDSCAPS_BACKBUFFER | DDSCAPS_FLIP) != 0 => {
            b.refs += 1;
            mem().wr32(out, back);
            OK
        }
        _ => {
            log!("warn: GetAttachedSurface(caps {want:#x}) found nothing");
            DDERR_NOTFOUND
        }
    }
}

fn s_get_caps(c: &mut Cpu) -> u32 {
    let caps = dd().surfaces.get(&arg(c, 0)).map_or(0, |s| s.caps);
    mem().wr32(arg(c, 1), caps);
    OK
}

/// GetCaps of IDirectDrawSurface7: DDSCAPS2 (16 bytes)
fn s7_get_caps(c: &mut Cpu) -> u32 {
    mem().slice(arg(c, 1), 16).fill(0);
    s_get_caps(c)
}

fn s7_get_dd_interface(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), com_new(DDRAW7, 0));
    OK
}

fn surface_desc(s: &Surface, p: u32, pixels: u32) {
    let m = mem();
    let size = desc_size(p);
    m.slice(p, size as usize).fill(0);
    m.wr32(p, size);
    let mut flags = DDSD_CAPS | DDSD_WIDTH | DDSD_HEIGHT | DDSD_PITCH | DDSD_PIXELFORMAT | DDSD_LPSURFACE;
    if s.back != 0 && s.caps & DDSCAPS_PRIMARYSURFACE != 0 {
        flags |= DDSD_BACKBUFFERCOUNT;
        m.wr32(p + 0x14, 1);
    }
    if let Some((lo, hi)) = s.ckey {
        flags |= DDSD_CKSRCBLT;
        m.wr32(p + 0x40, lo);
        m.wr32(p + 0x44, hi);
    }
    m.wr32(p + 4, flags);
    m.wr32(p + 8, s.h);
    m.wr32(p + 0xc, s.w);
    m.wr32(p + 0x10, s.pitch);
    m.wr32(p + 0x24, pixels);
    pixel_format(p + 0x48, s.bpp, s.fmt);
    m.wr32(p + 0x68, s.caps);
}

fn s_get_surface_desc(c: &mut Cpu) -> u32 {
    let d = dd();
    let Some(s) = d.surfaces.get(&arg(c, 0)) else { return DDERR_INVALIDPARAMS };
    surface_desc(s, arg(c, 1), s.pixels);
    OK
}

fn s_get_pixel_format(c: &mut Cpu) -> u32 {
    let (bpp, fmt) = dd().surfaces.get(&arg(c, 0)).map_or((16, RGB555), |s| (s.bpp, s.fmt));
    pixel_format(arg(c, 1), bpp, fmt);
    OK
}

fn s_lock(c: &mut Cpu) -> u32 {
    let (this, r, desc) = (arg(c, 0), rect(arg(c, 1)), arg(c, 2));
    sync(this);
    let d = dd();
    let Some(s) = d.surfaces.get(&this) else { return DDERR_INVALIDPARAMS };
    let at = r.map_or(s.pixels, |r| s.pixels + r[1] as u32 * s.pitch + r[0] as u32 * s.bpp / 8);
    surface_desc(s, desc, at);
    OK
}

fn s_unlock(c: &mut Cpu) -> u32 {
    let mut d = dd();
    touched(&mut d, arg(c, 0));
    if arg(c, 0) == d.primary {
        changed(&d);
    }
    OK
}

fn s_set_color_key(c: &mut Cpu) -> u32 {
    let (this, flags, key) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let mut d = dd();
    let Some(s) = d.surfaces.get_mut(&this) else { return DDERR_INVALIDPARAMS };
    if flags & DDCKEY_SRCBLT != 0 {
        s.ckey = (key != 0).then(|| (mem().rd32(key), mem().rd32(key + 4)));
    }
    OK
}

fn s_get_color_key(c: &mut Cpu) -> u32 {
    let d = dd();
    match d.surfaces.get(&arg(c, 0)).and_then(|s| s.ckey) {
        Some((lo, hi)) => {
            mem().wr32(arg(c, 2), lo);
            mem().wr32(arg(c, 2) + 4, hi);
            OK
        }
        None => 0x8876_0186, // DDERR_NOCOLORKEY
    }
}

fn s_set_palette(c: &mut Cpu) -> u32 {
    let mut d = dd();
    if let Some(s) = d.surfaces.get_mut(&arg(c, 0)) {
        s.palette = arg(c, 1);
    }
    OK
}

fn s_get_palette(c: &mut Cpu) -> u32 {
    let p = dd().surfaces.get(&arg(c, 0)).map_or(0, |s| s.palette);
    mem().wr32(arg(c, 1), p);
    if p == 0 { 0x8876_0203 } else { OK } // DDERR_NOPALETTEATTACHED
}

fn s_set_clipper(c: &mut Cpu) -> u32 {
    let mut d = dd();
    if let Some(s) = d.surfaces.get_mut(&arg(c, 0)) {
        s.clipper = arg(c, 1);
    }
    OK
}

fn s_get_clipper(c: &mut Cpu) -> u32 {
    let p = dd().surfaces.get(&arg(c, 0)).map_or(0, |s| s.clipper);
    mem().wr32(arg(c, 1), p);
    OK
}

fn s_get_dc(c: &mut Cpu) -> u32 {
    sync(arg(c, 0));
    mem().wr32(arg(c, 1), super::gdi32::surface_dc(arg(c, 0)));
    OK
}

fn s_release_dc(c: &mut Cpu) -> u32 {
    super::gdi32::release_dc(arg(c, 1));
    let mut d = dd();
    touched(&mut d, arg(c, 0));
    if arg(c, 0) == d.primary {
        changed(&d);
    }
    OK
}

/// A surface's pixel as 0RGB (for GDI on its DC).
pub fn get_pixel(surface: u32, x: i32, y: i32) -> Option<u32> {
    let d = dd();
    let s = d.surfaces.get(&surface)?;
    if x < 0 || y < 0 || x as u32 >= s.w || y as u32 >= s.h {
        return None;
    }
    let a = s.pixels + y as u32 * s.pitch;
    Some(if s.bpp == 8 {
        d.palettes.get(&s.palette).map_or(0, |p| p[mem().rd8(a + x as u32) as usize])
    } else {
        to_rgb(mem().rd16(a + 2 * x as u32))
    })
}

/// Set a surface's pixel from 0RGB (8-bit surfaces: not supported).
pub fn set_pixel(surface: u32, x: i32, y: i32, rgb: u32) {
    let d = dd();
    let Some(s) = d.surfaces.get(&surface) else { return };
    if x < 0 || y < 0 || x as u32 >= s.w || y as u32 >= s.h || s.bpp != 16 {
        return;
    }
    let p = (rgb >> 19 & 31) << 10 | (rgb >> 11 & 31) << 5 | (rgb >> 3 & 31);
    mem().wr16(s.pixels + y as u32 * s.pitch + 2 * x as u32, p as u16);
}

fn s_get_dd_interface(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), new_ddraw(false));
    OK
}

static SURFACE: &[Import] = &[
    ("IDirectDrawSurface::QueryInterface", 3, s_query_interface),
    ("IDirectDrawSurface::AddRef", 1, s_add_ref),
    ("IDirectDrawSurface::Release", 1, s_release),
    ("IDirectDrawSurface::AddAttachedSurface", 2, unsupported),
    ("IDirectDrawSurface::AddOverlayDirtyRect", 2, unsupported),
    ("IDirectDrawSurface::Blt", 6, s_blt),
    ("IDirectDrawSurface::BltBatch", 4, unsupported),
    ("IDirectDrawSurface::BltFast", 6, s_blt_fast),
    ("IDirectDrawSurface::DeleteAttachedSurface", 3, ok),
    ("IDirectDrawSurface::EnumAttachedSurfaces", 3, unsupported),
    ("IDirectDrawSurface::EnumOverlayZOrders", 4, unsupported),
    ("IDirectDrawSurface::Flip", 3, s_flip),
    ("IDirectDrawSurface::GetAttachedSurface", 3, s_get_attached_surface),
    ("IDirectDrawSurface::GetBltStatus", 2, ok),
    ("IDirectDrawSurface::GetCaps", 2, s_get_caps),
    ("IDirectDrawSurface::GetClipper", 2, s_get_clipper),
    ("IDirectDrawSurface::GetColorKey", 3, s_get_color_key),
    ("IDirectDrawSurface::GetDC", 2, s_get_dc),
    ("IDirectDrawSurface::GetFlipStatus", 2, ok),
    ("IDirectDrawSurface::GetOverlayPosition", 3, unsupported),
    ("IDirectDrawSurface::GetPalette", 2, s_get_palette),
    ("IDirectDrawSurface::GetPixelFormat", 2, s_get_pixel_format),
    ("IDirectDrawSurface::GetSurfaceDesc", 2, s_get_surface_desc),
    ("IDirectDrawSurface::Initialize", 3, ok),
    ("IDirectDrawSurface::IsLost", 1, ok),
    ("IDirectDrawSurface::Lock", 5, s_lock),
    ("IDirectDrawSurface::ReleaseDC", 2, s_release_dc),
    ("IDirectDrawSurface::Restore", 1, ok),
    ("IDirectDrawSurface::SetClipper", 2, s_set_clipper),
    ("IDirectDrawSurface::SetColorKey", 3, s_set_color_key),
    ("IDirectDrawSurface::SetOverlayPosition", 3, unsupported),
    ("IDirectDrawSurface::SetPalette", 2, s_set_palette),
    ("IDirectDrawSurface::Unlock", 2, s_unlock),
    ("IDirectDrawSurface::UpdateOverlay", 6, unsupported),
    ("IDirectDrawSurface::UpdateOverlayDisplay", 2, unsupported),
    ("IDirectDrawSurface::UpdateOverlayZOrder", 3, unsupported),
    ("IDirectDrawSurface2::GetDDInterface", 2, s_get_dd_interface),
    ("IDirectDrawSurface2::PageLock", 2, ok),
    ("IDirectDrawSurface2::PageUnlock", 2, ok),
    ("IDirectDrawSurface3::SetSurfaceDesc", 3, unsupported),
];

/// IDirectDrawSurface7: the same as the old ones, with DDSCAPS2 and DDSURFACEDESC2, and more
/// methods at the end.
static SURFACE7: &[Import] = &[
    ("IDirectDrawSurface7::QueryInterface", 3, s_query_interface),
    ("IDirectDrawSurface7::AddRef", 1, s_add_ref),
    ("IDirectDrawSurface7::Release", 1, s_release),
    ("IDirectDrawSurface7::AddAttachedSurface", 2, ok),
    ("IDirectDrawSurface7::AddOverlayDirtyRect", 2, unsupported),
    ("IDirectDrawSurface7::Blt", 6, s_blt),
    ("IDirectDrawSurface7::BltBatch", 4, unsupported),
    ("IDirectDrawSurface7::BltFast", 6, s_blt_fast),
    ("IDirectDrawSurface7::DeleteAttachedSurface", 3, ok),
    ("IDirectDrawSurface7::EnumAttachedSurfaces", 3, unsupported),
    ("IDirectDrawSurface7::EnumOverlayZOrders", 4, unsupported),
    ("IDirectDrawSurface7::Flip", 3, s_flip),
    ("IDirectDrawSurface7::GetAttachedSurface", 3, s_get_attached_surface),
    ("IDirectDrawSurface7::GetBltStatus", 2, ok),
    ("IDirectDrawSurface7::GetCaps", 2, s7_get_caps),
    ("IDirectDrawSurface7::GetClipper", 2, s_get_clipper),
    ("IDirectDrawSurface7::GetColorKey", 3, s_get_color_key),
    ("IDirectDrawSurface7::GetDC", 2, s_get_dc),
    ("IDirectDrawSurface7::GetFlipStatus", 2, ok),
    ("IDirectDrawSurface7::GetOverlayPosition", 3, unsupported),
    ("IDirectDrawSurface7::GetPalette", 2, s_get_palette),
    ("IDirectDrawSurface7::GetPixelFormat", 2, s_get_pixel_format),
    ("IDirectDrawSurface7::GetSurfaceDesc", 2, s_get_surface_desc),
    ("IDirectDrawSurface7::Initialize", 3, ok),
    ("IDirectDrawSurface7::IsLost", 1, ok),
    ("IDirectDrawSurface7::Lock", 5, s_lock),
    ("IDirectDrawSurface7::ReleaseDC", 2, s_release_dc),
    ("IDirectDrawSurface7::Restore", 1, ok),
    ("IDirectDrawSurface7::SetClipper", 2, s_set_clipper),
    ("IDirectDrawSurface7::SetColorKey", 3, s_set_color_key),
    ("IDirectDrawSurface7::SetOverlayPosition", 3, unsupported),
    ("IDirectDrawSurface7::SetPalette", 2, s_set_palette),
    ("IDirectDrawSurface7::Unlock", 2, s_unlock),
    ("IDirectDrawSurface7::UpdateOverlay", 6, unsupported),
    ("IDirectDrawSurface7::UpdateOverlayDisplay", 2, unsupported),
    ("IDirectDrawSurface7::UpdateOverlayZOrder", 3, unsupported),
    ("IDirectDrawSurface7::GetDDInterface", 2, s7_get_dd_interface),
    ("IDirectDrawSurface7::PageLock", 2, ok),
    ("IDirectDrawSurface7::PageUnlock", 2, ok),
    ("IDirectDrawSurface7::SetSurfaceDesc", 3, unsupported),
    ("IDirectDrawSurface7::SetPrivateData", 5, unsupported),
    ("IDirectDrawSurface7::GetPrivateData", 4, unsupported),
    ("IDirectDrawSurface7::FreePrivateData", 2, unsupported),
    ("IDirectDrawSurface7::GetUniquenessValue", 2, unsupported),
    ("IDirectDrawSurface7::ChangeUniquenessValue", 1, ok),
    ("IDirectDrawSurface7::SetPriority", 2, ok),
    ("IDirectDrawSurface7::GetPriority", 2, unsupported),
    ("IDirectDrawSurface7::SetLOD", 2, ok),
    ("IDirectDrawSurface7::GetLOD", 2, unsupported),
];

// ---- IDirectDrawPalette, IDirectDrawClipper --------------------------------------------------------

fn pal_get_entries(c: &mut Cpu) -> u32 {
    let (this, first, n, out) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 4));
    let d = dd();
    let Some(p) = d.palettes.get(&this) else { return DDERR_INVALIDPARAMS };
    for k in 0..n.min(256u32.saturating_sub(first)) {
        mem().wr32(out + 4 * k, entry_rgb(p[(first + k) as usize]));
    }
    OK
}

fn pal_set_entries(c: &mut Cpu) -> u32 {
    let (this, first, n, src) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 4));
    let mut d = dd();
    let Some(p) = d.palettes.get_mut(&this) else { return DDERR_INVALIDPARAMS };
    for k in 0..n.min(256u32.saturating_sub(first)) {
        p[(first + k) as usize] = entry_rgb(mem().rd32(src + 4 * k));
    }
    let prim = d.surfaces.get(&d.primary).is_some_and(|s| s.palette == this);
    if prim {
        changed(&d);
    }
    OK
}

fn pal_get_caps(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), 4 | 0x40); // DDPCAPS_8BIT | DDPCAPS_ALLOW256
    OK
}

static PALETTE: &[Import] = &[
    ("IDirectDrawPalette::QueryInterface", 3, unsupported),
    ("IDirectDrawPalette::AddRef", 1, add_ref),
    ("IDirectDrawPalette::Release", 1, release),
    ("IDirectDrawPalette::GetCaps", 2, pal_get_caps),
    ("IDirectDrawPalette::GetEntries", 5, pal_get_entries),
    ("IDirectDrawPalette::Initialize", 4, ok),
    ("IDirectDrawPalette::SetEntries", 5, pal_set_entries),
];

fn clip_set_hwnd(c: &mut Cpu) -> u32 {
    dd().clippers.insert(arg(c, 0), arg(c, 2));
    OK
}

fn clip_get_hwnd(c: &mut Cpu) -> u32 {
    let h = dd().clippers.get(&arg(c, 0)).copied().unwrap_or(0);
    mem().wr32(arg(c, 1), h);
    OK
}

fn clip_list_changed(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), 0);
    OK
}

static CLIPPER: &[Import] = &[
    ("IDirectDrawClipper::QueryInterface", 3, unsupported),
    ("IDirectDrawClipper::AddRef", 1, add_ref),
    ("IDirectDrawClipper::Release", 1, release),
    ("IDirectDrawClipper::GetClipList", 4, unsupported),
    ("IDirectDrawClipper::GetHWnd", 2, clip_get_hwnd),
    ("IDirectDrawClipper::Initialize", 3, ok),
    ("IDirectDrawClipper::IsClipListChanged", 2, clip_list_changed),
    ("IDirectDrawClipper::SetClipList", 3, unsupported),
    ("IDirectDrawClipper::SetHWnd", 3, clip_set_hwnd),
];

pub const IMPORTS: &[Import] = &[
    ("DirectDrawCreate", 3, direct_draw_create),
    ("DirectDrawEnumerateA", 2, direct_draw_enumerate),
    ("DirectDrawCreateEx", 4, direct_draw_create_ex),
    ("DirectDrawEnumerateExA", 3, direct_draw_enumerate_ex),
];
