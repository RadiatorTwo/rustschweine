//! Drawing dialogs into an ARGB overlay, in the style of the game's menu: dark blue panels,
//! light blue outlines, pale text.

use super::font::{FIRST, GLYPHS};
use super::{Control, Dialog, Kind, OV_H, OV_W, ROW};

const PANEL: u32 = 0xf40a_1440;
const CAPTION: u32 = 0xf01c_3c9c;
const EDGE: u32 = 0xff3c_64d2;
const EDGE_HI: u32 = 0xff6a_9cff;
const FACE: u32 = 0xff0c_1e5a;
const FACE_HOT: u32 = 0xff1a_3a9a;
const FACE_DOWN: u32 = 0xff2e_58c8;
const WELL: u32 = 0xff05_0a28;
const TEXT: u32 = 0xffe8_f0ff;
const TEXT_OFF: u32 = 0xff5a_6a9a;
const MARK: u32 = 0xff7f_d0ff;
const SELECT: u32 = 0xff2e_58c8;

struct Canvas {
    px: Vec<u32>,
    /// x0 y0 x1 y1: drawing stays inside
    clip: [i32; 4],
}

impl Canvas {
    fn put(&mut self, x: i32, y: i32, c: u32) {
        let k = self.clip;
        if x >= k[0] && x < k[2] && y >= k[1] && y < k[3] {
            let p = &mut self.px[(y * OV_W + x) as usize];
            *p = over(*p, c);
        }
    }

    fn fill(&mut self, r: [i32; 4], c: u32) {
        for y in r[1]..r[3] {
            for x in r[0]..r[2] {
                self.put(x, y, c);
            }
        }
    }

    fn frame(&mut self, r: [i32; 4], c: u32) {
        self.fill([r[0], r[1], r[2], r[1] + 1], c);
        self.fill([r[0], r[3] - 1, r[2], r[3]], c);
        self.fill([r[0], r[1] + 1, r[0] + 1, r[3] - 1], c);
        self.fill([r[2] - 1, r[1] + 1, r[2], r[3] - 1], c);
    }

    /// Text with its top left at (x, y), clipped to `r`.
    fn text(&mut self, x: i32, y: i32, s: &str, c: u32, r: [i32; 4]) {
        let keep = self.clip;
        self.clip = [r[0].max(keep[0]), r[1].max(keep[1]), r[2].min(keep[2]), r[3].min(keep[3])];
        let mut x = x;
        for ch in s.chars() {
            let Some((g, l, w)) = glyph(ch) else {
                x += 3;
                continue;
            };
            for (row, bits) in g.iter().enumerate() {
                for col in l..l + w {
                    if bits >> col & 1 != 0 {
                        self.put(x + col - l, y + row as i32, c);
                    }
                }
            }
            x += w + 1;
        }
        self.clip = keep;
    }

    /// Text placed in a rectangle: `align` 0 left, 1 centred, 2 right; vertically centred.
    fn label(&mut self, r: [i32; 4], s: &str, c: u32, align: u8) {
        let w = text_width(s);
        let x = match align {
            1 => (r[0] + r[2] - w) / 2,
            2 => r[2] - w,
            _ => r[0],
        };
        self.text(x, (r[1] + r[3] - 8) / 2 + 1, s, c, r);
    }
}

/// `c` (ARGB) over `p` (ARGB): the result keeps the larger coverage.
fn over(p: u32, c: u32) -> u32 {
    let a = c >> 24;
    if a == 255 || p >> 24 == 0 {
        return c;
    }
    let mix = |sh: u32| (((c >> sh) & 0xff) * a + ((p >> sh) & 0xff) * (255 - a)) / 255 << sh;
    (a.max(p >> 24) << 24) | mix(16) | mix(8) | mix(0)
}

/// A character's glyph, its first column and width (None: blank, e.g. a space).
pub fn glyph(ch: char) -> Option<(&'static [u8; 8], i32, i32)> {
    let ch = if ch.is_ascii() { ch } else { '?' };
    let g = GLYPHS.get((ch as u8).checked_sub(FIRST)? as usize)?;
    let cols = g.iter().fold(0u8, |a, &b| a | b);
    if cols == 0 {
        return None;
    }
    let l = cols.trailing_zeros() as i32;
    let r = 7 - cols.leading_zeros() as i32;
    Some((g, l, r - l + 1))
}

/// Width of a text in pixels.
pub fn text_width(s: &str) -> i32 {
    let w: i32 = s.chars().map(|ch| glyph(ch).map_or(3, |g| g.2 + 1)).sum();
    (w - 1).max(0)
}

static CARET: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// The text caret blinks as Windows' does: shown and hidden for 530 ms each.
pub fn caret_phase() -> u64 {
    let mut c = CARET.lock().unwrap();
    c.get_or_insert_with(std::time::Instant::now).elapsed().as_millis() as u64 / 530
}

/// After a key or a click into the text the caret shows at once and blinks from there.
pub fn caret_reset() {
    *CARET.lock().unwrap() = Some(std::time::Instant::now());
}

/// Text that can be typed, in `r` (its well): when it has the focus, selected as a whole
/// (highlighted) or not, and the blinking caret after it.
fn typed_text(cv: &mut Canvas, r: [i32; 4], text: &str, focus: bool, whole: bool, fg: u32) {
    let t = [r[0] + 2, r[1], r[2] - 2, r[3]];
    let w = text_width(text);
    let y = (r[1] + r[3] - 8) / 2;
    if focus && whole && !text.is_empty() {
        cv.fill([t[0] - 1, y - 1, t[0] + w + 1, y + 10], SELECT);
    }
    cv.label(t, text, fg, 0);
    if focus && caret_phase() % 2 == 0 {
        let x = (t[0] + w + if text.is_empty() { 0 } else { 2 }).min(t[2] - 1);
        cv.fill([x, y - 1, x + 1, y + 10], TEXT);
    }
}

fn inset(r: [i32; 4], d: i32) -> [i32; 4] {
    [r[0] + d, r[1] + d, r[2] - d, r[3] - d]
}

/// Text drawn for `&`-marked mnemonics: without the `&`.
fn plain(s: &str) -> String {
    s.replace("&&", "\u{1}").replace('&', "").replace('\u{1}', "&")
}

fn control(cv: &mut Canvas, d: &Dialog, k: usize) {
    let c: &Control = &d.controls[k];
    if !c.visible {
        return;
    }
    let r = c.rect;
    let fg = if c.enabled { TEXT } else { TEXT_OFF };
    let text = plain(&c.text);
    let hot = d.hot == Some(k) && c.interactive();
    match &c.kind {
        Kind::Push { default } => {
            let face = if d.pressed == Some(k) && hot {
                FACE_DOWN
            } else if hot {
                FACE_HOT
            } else {
                FACE
            };
            cv.fill(inset(r, 1), face);
            cv.frame(r, if *default || hot { EDGE_HI } else { EDGE });
            if *default {
                cv.frame(inset(r, 1), EDGE);
            }
            cv.label(inset(r, 2), &text, fg, 1);
        }
        Kind::Check { .. } | Kind::Radio { .. } => {
            let s = 9;
            let y = (r[1] + r[3] - s) / 2;
            let b = [r[0], y, r[0] + s, y + s];
            cv.fill(b, WELL);
            cv.frame(b, if hot { EDGE_HI } else { EDGE });
            if c.check != 0 {
                if matches!(c.kind, Kind::Radio { .. }) {
                    cv.fill([b[0] + 3, b[1] + 2, b[2] - 3, b[3] - 2], MARK);
                    cv.fill([b[0] + 2, b[1] + 3, b[2] - 2, b[3] - 3], MARK);
                } else {
                    cv.fill(inset(b, 2), MARK);
                }
            }
            cv.label([r[0] + s + 4, r[1], r[2], r[3]], &text, fg, 0);
        }
        Kind::Group => {
            let y = r[1] + 4;
            cv.frame([r[0], y, r[2], r[3]], EDGE);
            if !text.is_empty() {
                let w = text_width(&text);
                cv.fill([r[0] + 6, r[1], r[0] + 10 + w, r[1] + 9], 0xff0a_1440);
                cv.text(r[0] + 8, r[1], &text, fg, r);
            }
        }
        Kind::Static => {
            let boxed = c.style & 0x0080_0000 != 0 || c.style & 0x1000 != 0;
            let mut t = r;
            if boxed {
                cv.fill(r, if c.inverted { MARK } else if hot { FACE_HOT } else { WELL });
                cv.frame(r, if hot { EDGE_HI } else { EDGE });
                t = [r[0] + 3, r[1], r[2] - 3, r[3]];
            }
            let align = match c.style & 0x1f {
                1 => 1,
                2 => 2,
                _ => 0,
            };
            let fg = if c.inverted { WELL } else { fg };
            if c.style & 0x200 != 0 || boxed {
                cv.label(t, &text, fg, align);
            } else {
                // top-aligned, one line per line of text
                for (n, line) in text.lines().enumerate() {
                    let y = t[1] + n as i32 * 9;
                    cv.label([t[0], y, t[2], y + 9], line, fg, align);
                }
            }
        }
        Kind::Edit { .. } => {
            cv.fill(r, WELL);
            cv.frame(r, if d.focus == Some(k) { EDGE_HI } else { EDGE });
            typed_text(cv, [r[0] + 1, r[1], r[2] - 1, r[3]], &c.text, d.focus == Some(k), false, fg);
        }
        Kind::List { items, sel, top } => {
            cv.fill(r, WELL);
            cv.frame(r, EDGE);
            rows(cv, inset(r, 1), items, *sel, *top, fg);
        }
        Kind::Combo { items, sel, edit, whole, .. } => {
            let f = c.face();
            cv.fill(inset(f, 1), if hot { FACE_HOT } else { FACE });
            cv.frame(f, if hot || d.focus == Some(k) { EDGE_HI } else { EDGE });
            if *edit {
                // the text that can be typed: in a well, selected as a whole or with a caret
                let w = [f[0] + 2, f[1] + 2, f[2] - 14, f[3] - 2];
                cv.fill(w, WELL);
                typed_text(cv, w, &c.text, d.focus == Some(k), *whole, fg);
            } else {
                let t = items.get(*sel as usize).map_or("", |t| t.0.as_str());
                cv.label([f[0] + 4, f[1], f[2] - 14, f[3]], t, fg, 0);
            }
            // the arrow
            let (ax, ay) = (f[2] - 10, (f[1] + f[3]) / 2 - 2);
            for i in 0..4 {
                cv.fill([ax + i, ay + i, ax + 7 - i, ay + i + 1], fg);
            }
        }
        Kind::Track { lo, hi, pos } => {
            let y = (r[1] + r[3]) / 2;
            cv.fill([r[0] + 5, y - 1, r[2] - 5, y + 1], EDGE);
            let w = (r[2] - r[0] - 10).max(1);
            let x = r[0] + 5 + (pos - lo) * w / (hi - lo).max(1);
            let t = [x - 3, r[1] + 2, x + 4, r[3] - 2];
            cv.fill(t, if hot { FACE_DOWN } else { FACE_HOT });
            cv.frame(t, EDGE_HI);
        }
        Kind::Other => {}
    }
}

/// Rows of a list from `top`, the selected one highlighted.
fn rows(cv: &mut Canvas, r: [i32; 4], items: &[(String, u32)], sel: i32, top: i32, fg: u32) {
    for (n, (t, _)) in items.iter().enumerate().skip(top.max(0) as usize) {
        let y = r[1] + 1 + (n as i32 - top) * ROW;
        if y + ROW > r[3] {
            break;
        }
        let row = [r[0], y, r[2], y + ROW];
        if n as i32 == sel {
            cv.fill(row, SELECT);
        }
        cv.label([r[0] + 3, y, r[2] - 2, y + ROW], t, fg, 0);
    }
}

fn dialog(cv: &mut Canvas, d: &Dialog, above: bool) {
    let r = d.rect;
    // a dialog over another one hides it completely
    cv.fill(r, if above { PANEL | 0xff00_0000 } else { PANEL });
    cv.frame(r, EDGE_HI);
    cv.frame(inset(r, 1), EDGE);
    if d.caption {
        let bar = [r[0] + 2, r[1] + 2, r[2] - 2, r[1] + 14];
        cv.fill(bar, CAPTION);
        cv.label([bar[0] + 4, bar[1], bar[2], bar[3]], &d.title, TEXT, 0);
    }
    for k in 0..d.controls.len() {
        control(cv, d, k);
    }
    // open combo lists go over everything
    for c in &d.controls {
        if let (Some(lr), Kind::Combo { items, sel, top, .. }) = (c.drop_rect(), &c.kind) {
            cv.fill(lr, 0xff05_0a28);
            cv.frame(lr, EDGE_HI);
            rows(cv, inset(lr, 1), items, *sel, *top, TEXT);
        }
    }
}

/// All open dialogs, the topmost last, as ARGB pixels of OV_W x OV_H.
pub fn render(dialogs: &[Dialog]) -> Vec<u32> {
    let mut cv = Canvas { px: vec![0; (OV_W * OV_H) as usize], clip: [0, 0, OV_W, OV_H] };
    for (k, d) in dialogs.iter().enumerate() {
        dialog(&mut cv, d, k > 0);
    }
    cv.px
}
