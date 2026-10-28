//! GDI32: the little GDI the game uses. Device capabilities; bitmaps (LoadImageA from a file or
//! a resource) copied onto DirectDraw surfaces through device contexts, the DirectX SDK's
//! DDLoadBitmap/DDColorMatch way (CreateCompatibleDC, SelectObject, StretchBlt, GetPixel,
//! SetPixel); GetBitmapBits. Text and fonts do nothing.

use super::user32::{DESKTOP_H, DESKTOP_W};
use super::{Import, arg, cstr, host_path, log, mem};
use crate::image::ResId;
use crate::rt::Cpu;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// The screen's device context.
pub const SCREEN_DC: u32 = 0xdc00;
const STOCK: u32 = 0x6600;

/// A bitmap: top-down 0RGB pixels.
struct Bitmap {
    w: u32,
    h: u32,
    px: Vec<u32>,
}

/// What a device context draws on.
#[derive(Clone, Copy)]
enum Target {
    None,
    Bitmap(u32),
    Surface(u32),
}

struct Gdi {
    bitmaps: BTreeMap<u32, Bitmap>,
    dcs: BTreeMap<u32, Target>,
    next: u32,
}

static GDI: Mutex<Gdi> = Mutex::new(Gdi { bitmaps: BTreeMap::new(), dcs: BTreeMap::new(), next: 0x6700 });

fn gdi() -> MutexGuard<'static, Gdi> {
    GDI.lock().unwrap_or_else(|e| e.into_inner())
}

impl Gdi {
    fn handle(&mut self) -> u32 {
        self.next += 4;
        self.next
    }
}

/// A device context for a DirectDraw surface (IDirectDrawSurface::GetDC).
pub fn surface_dc(surface: u32) -> u32 {
    let mut g = gdi();
    let h = g.handle();
    g.dcs.insert(h, Target::Surface(surface));
    h
}

pub fn release_dc(dc: u32) {
    gdi().dcs.remove(&dc);
}

/// A pixel of what a DC draws on, as 0RGB.
fn get(g: &Gdi, dc: u32, x: i32, y: i32) -> Option<u32> {
    match g.dcs.get(&dc).copied()? {
        Target::Bitmap(b) => {
            let b = g.bitmaps.get(&b)?;
            (x >= 0 && y >= 0 && (x as u32) < b.w && (y as u32) < b.h).then(|| b.px[(y as u32 * b.w + x as u32) as usize])
        }
        Target::Surface(s) => super::ddraw::get_pixel(s, x, y),
        Target::None => None,
    }
}

fn set(g: &mut Gdi, dc: u32, x: i32, y: i32, rgb: u32) {
    match g.dcs.get(&dc).copied() {
        Some(Target::Bitmap(b)) => {
            if let Some(b) = g.bitmaps.get_mut(&b) {
                if x >= 0 && y >= 0 && (x as u32) < b.w && (y as u32) < b.h {
                    b.px[(y as u32 * b.w + x as u32) as usize] = rgb;
                }
            }
        }
        Some(Target::Surface(s)) => super::ddraw::set_pixel(s, x, y, rgb),
        _ => {}
    }
}

fn colorref(rgb: u32) -> u32 {
    (rgb & 0xff) << 16 | (rgb & 0xff00) | (rgb >> 16) & 0xff
}

// ---- bitmaps ------------------------------------------------------------------------------------

/// A DIB (BITMAPINFOHEADER, colour table, bits) as top-down 0RGB pixels.
fn parse_dib(d: &[u8]) -> Option<Bitmap> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?) as u32);
    let u32_at = |o: usize| Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?));
    let hsize = u32_at(0)? as usize;
    let w = u32_at(4)? as i32;
    let h = u32_at(8)? as i32;
    let bpp = u16_at(14)?;
    let comp = u32_at(16)?;
    let used = u32_at(32)?;
    if w <= 0 || h == 0 || !(comp == 0 || comp == 3) {
        return None;
    }
    let colors = if bpp <= 8 { if used != 0 { used } else { 1 << bpp } } else { 0 };
    let masks = if comp == 3 { 12 } else { 0 };
    let table = hsize + masks;
    let bits = table + 4 * colors as usize;
    let pal: Vec<u32> = (0..colors).map(|k| u32_at(table + 4 * k as usize).unwrap_or(0) & 0xff_ffff).collect();
    let (rows, top_down) = (h.unsigned_abs(), h < 0);
    let stride = ((w as u32 * bpp + 31) / 32 * 4) as usize;
    let mut px = vec![0; (w as u32 * rows) as usize];
    for y in 0..rows {
        let row = bits + stride * if top_down { y } else { rows - 1 - y } as usize;
        for x in 0..w as u32 {
            let v = match bpp {
                1 | 4 | 8 => {
                    let bit = x as usize * bpp as usize;
                    let byte = *d.get(row + bit / 8)? as u32;
                    let idx = (byte >> (8 - bpp - (bit % 8) as u32)) & ((1 << bpp) - 1);
                    pal.get(idx as usize).copied().unwrap_or(0)
                }
                16 => {
                    let p = u16_at(row + 2 * x as usize)?;
                    let (r, g, b) = if comp == 3 && u32_at(hsize + 4)? == 0x7e0 { (p >> 11, (p >> 5) & 63, p & 31) } else { (p >> 10 & 31, (p >> 5 & 31) << 1, p & 31) };
                    (r << 3 | r >> 2) << 16 | (g << 2 | g >> 4) << 8 | (b << 3 | b >> 2)
                }
                24 => {
                    let o = row + 3 * x as usize;
                    let b = d.get(o..o + 3)?;
                    b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16
                }
                32 => u32_at(row + 4 * x as usize)? & 0xff_ffff,
                _ => return None,
            };
            px[(y * w as u32 + x) as usize] = v;
        }
    }
    Some(Bitmap { w: w as u32, h: rows, px })
}

const LR_LOADFROMFILE: u32 = 0x10;

/// LoadImageA(module, name, type, cx, cy, flags): bitmaps only (IMAGE_BITMAP), from a .bmp file
/// or an RT_BITMAP resource of the EXE or the engine DLL.
fn load_image(c: &mut Cpu) -> u32 {
    let (module, name, ty, flags) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 5));
    if ty != 0 {
        log!("warn: LoadImageA type {ty} not supported");
        return 0;
    }
    let dib = if flags & LR_LOADFROMFILE != 0 {
        let path = cstr(name);
        match std::fs::read(host_path(&path)) {
            Ok(f) if f.len() > 14 && f.starts_with(b"BM") => f[14..].to_vec(),
            _ => {
                if super::trace_api() {
                    log!("LoadImageA {path}: not found");
                }
                return 0;
            }
        }
    } else {
        let base = if module == 0 { super::IMAGE_BASE } else { module };
        match super::resource_in(base, ResId::Id(2), ResId::from_guest(name)) {
            Some((a, n)) => mem().slice(a, n as usize).to_vec(),
            None => return 0,
        }
    };
    let Some(b) = parse_dib(&dib) else {
        log!("warn: LoadImageA: a bitmap we cannot read");
        return 0;
    };
    let mut g = gdi();
    let h = g.handle();
    g.bitmaps.insert(h, b);
    h
}

/// GetObjectA on a bitmap: BITMAP (24 bytes), as a 16-bit device bitmap.
fn get_object(c: &mut Cpu) -> u32 {
    let (h, n, p) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let g = gdi();
    let Some(b) = g.bitmaps.get(&h) else { return 0 };
    if p == 0 {
        return 24;
    }
    let m = mem();
    let v = [0, b.w, b.h, b.w * 2, 1 | 16 << 16, 0];
    for (k, x) in v.into_iter().enumerate().take(n as usize / 4) {
        m.wr32(p + 4 * k as u32, x);
    }
    n.min(24)
}

/// GetBitmapBits(bitmap, bytes, buffer): the bits of a 16-bit (555) device bitmap, rows of
/// whole words.
fn get_bitmap_bits(c: &mut Cpu) -> u32 {
    let (h, n, p) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let g = gdi();
    let Some(b) = g.bitmaps.get(&h) else { return 0 };
    let mut out = Vec::with_capacity((b.w * b.h * 2) as usize);
    for &v in &b.px {
        let p = (v >> 19 & 31) << 10 | (v >> 11 & 31) << 5 | (v >> 3 & 31);
        out.extend((p as u16).to_le_bytes());
    }
    let k = out.len().min(n as usize);
    mem().slice(p, k).copy_from_slice(&out[..k]);
    k as u32
}

fn delete_object(c: &mut Cpu) -> u32 {
    gdi().bitmaps.remove(&arg(c, 0));
    1
}

// ---- device contexts ----------------------------------------------------------------------------

fn create_compatible_dc(_: &mut Cpu) -> u32 {
    let mut g = gdi();
    let h = g.handle();
    g.dcs.insert(h, Target::None);
    h
}

fn delete_dc(c: &mut Cpu) -> u32 {
    gdi().dcs.remove(&arg(c, 0));
    1
}

/// SelectObject: a bitmap into a memory DC (the one it held before is returned); anything else
/// is accepted and does nothing.
fn select_object(c: &mut Cpu) -> u32 {
    let (dc, obj) = (arg(c, 0), arg(c, 1));
    let mut g = gdi();
    if !g.bitmaps.contains_key(&obj) {
        return STOCK;
    }
    match g.dcs.get_mut(&dc) {
        Some(t) => match std::mem::replace(t, Target::Bitmap(obj)) {
            Target::Bitmap(old) => old,
            _ => STOCK,
        },
        None => 0,
    }
}

/// StretchBlt(dst, x, y, w, h, src, x, y, w, h, rop): a copy (SRCCOPY), stretched.
fn stretch_blt(c: &mut Cpu) -> u32 {
    let a: Vec<i32> = (0..11).map(|k| arg(c, k) as i32).collect();
    let (dst, dx, dy, dw, dh, src, sx, sy, sw, sh) = (a[0] as u32, a[1], a[2], a[3], a[4], a[5] as u32, a[6], a[7], a[8], a[9]);
    if dw <= 0 || dh <= 0 || sw <= 0 || sh <= 0 {
        return 1;
    }
    let mut g = gdi();
    for y in 0..dh {
        for x in 0..dw {
            if let Some(v) = get(&g, src, sx + x * sw / dw, sy + y * sh / dh) {
                set(&mut g, dst, dx + x, dy + y, v);
            }
        }
    }
    1
}

fn get_pixel(c: &mut Cpu) -> u32 {
    let g = gdi();
    get(&g, arg(c, 0), arg(c, 1) as i32, arg(c, 2) as i32).map_or(0xffff_ffff, colorref)
}

fn set_pixel(c: &mut Cpu) -> u32 {
    let rgb = colorref(arg(c, 3));
    set(&mut gdi(), arg(c, 0), arg(c, 1) as i32, arg(c, 2) as i32, rgb);
    arg(c, 3)
}

// ---- the rest -----------------------------------------------------------------------------------

fn get_device_caps(c: &mut Cpu) -> u32 {
    match arg(c, 1) {
        8 => DESKTOP_W,  // HORZRES
        10 => DESKTOP_H, // VERTRES
        12 => 16,        // BITSPIXEL
        14 => 1,         // PLANES
        88 | 90 => 96,   // LOGPIXELSX/Y
        116 => 60,       // VREFRESH
        _ => 0,
    }
}

fn get_text_metrics(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let m = mem();
    m.slice(p, 56).fill(0);
    // height, ascent, descent, internal and external leading, average and maximum width, weight
    for (k, v) in [16u32, 13, 3, 0, 0, 8, 16, 400].into_iter().enumerate() {
        m.wr32(p + 4 * k as u32, v);
    }
    1
}

fn handle(_: &mut Cpu) -> u32 {
    STOCK
}
fn ret0(_: &mut Cpu) -> u32 {
    0
}
fn ret1(_: &mut Cpu) -> u32 {
    1
}

pub const IMPORTS: &[Import] = &[
    ("GetDeviceCaps", 2, get_device_caps),
    ("GetTextMetricsA", 2, get_text_metrics),
    ("GetStockObject", 1, handle),
    ("CreateFontA", 14, handle),
    ("SelectObject", 2, select_object),
    ("DeleteObject", 1, delete_object),
    ("TextOutA", 5, ret1),
    ("SetBkColor", 2, ret0),
    ("SetBkMode", 2, ret1),
    ("SetTextColor", 2, ret0),
    ("GetObjectA", 3, get_object),
    ("GetBitmapBits", 3, get_bitmap_bits),
    ("CreateCompatibleDC", 1, create_compatible_dc),
    ("DeleteDC", 1, delete_dc),
    ("StretchBlt", 11, stretch_blt),
    ("GetPixel", 3, get_pixel),
    ("SetPixel", 4, set_pixel),
];

/// USER32 functions that belong with the bitmaps.
pub const USER_IMPORTS: &[Import] = &[("LoadImageA", 6, load_image)];
