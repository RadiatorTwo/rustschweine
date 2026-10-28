//! The game's dialog boxes (Preferences, controller and audio settings, the pause bar, load and
//! save, the quit box): its own RT_DIALOG resources, driven by its own dialog procedures, drawn
//! by us in the game's style as an overlay over its frames.

pub mod draw;
pub mod font;
pub mod template;

use super::user32::{self, Msg};
use super::{Import, ResId, arg, cstr, guest_call, log, mem, put_cstr, resource};
use crate::rt::Cpu;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use template::{Class, Item, Template};

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("HOGS_DLG_LOG").is_some())
}

/// Our own controls' ids start here.
pub const FIRST_OURS: u32 = 0x7e00;
/// The overlay dialogs are drawn into (stretched over the frame like the game's 640x480 menus).
pub const OV_W: i32 = 640;
pub const OV_H: i32 = 480;

const WM_SETTEXT: u32 = 0x0c;
const WM_GETTEXT: u32 = 0x0d;
const WM_GETTEXTLENGTH: u32 = 0x0e;
const WM_INITDIALOG: u32 = 0x110;
const WM_COMMAND: u32 = 0x111;
const WM_HSCROLL: u32 = 0x114;
const WM_VSCROLL: u32 = 0x115;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_DISABLED: u32 = 0x0800_0000;
const WS_GROUP: u32 = 0x0002_0000;
const WS_CAPTION: u32 = 0x00c0_0000;
const SS_NOTIFY: u32 = 0x100;
const ERR: u32 = 0xffff_ffff;

#[derive(Clone, Debug)]
pub enum Kind {
    Push { default: bool },
    Check { auto: bool },
    Radio { auto: bool },
    Group,
    Static,
    Edit { limit: u32 },
    List { items: Vec<(String, u32)>, sel: i32, top: i32 },
    /// a drop-down list; `drop` is the height of the open list. With `edit` (CBS_DROPDOWN) its
    /// text (the control's) can be typed; `whole`: the text is selected, a key replaces it.
    Combo { items: Vec<(String, u32)>, sel: i32, open: bool, top: i32, drop: i32, edit: bool, whole: bool },
    Track { lo: i32, hi: i32, pos: i32 },
    Other,
}

#[derive(Clone, Debug)]
pub struct Control {
    pub hwnd: u32,
    pub id: u32,
    pub kind: Kind,
    pub text: String,
    pub style: u32,
    /// x0 y0 x1 y1 in overlay pixels
    pub rect: [i32; 4],
    pub enabled: bool,
    pub visible: bool,
    pub check: u32,
    /// one of our own controls (see `extend`)
    pub ours: bool,
    /// shown inverted (the game's InvertRect)
    pub inverted: bool,
}

#[derive(Clone, Debug)]
pub struct Dialog {
    pub hwnd: u32,
    /// the template's resource id (0 for a message box)
    pub template: u32,
    proc_: u32,
    pub title: String,
    pub caption: bool,
    pub rect: [i32; 4],
    /// dialog units to pixels
    pub scale: (f32, f32),
    pub controls: Vec<Control>,
    pub result: Option<u32>,
    pub focus: Option<usize>,
    pub pressed: Option<usize>,
    pub hot: Option<usize>,
    drag: Option<usize>,
}

/// The open dialogs, the topmost last.
struct Stack {
    dialogs: Vec<Dialog>,
    serial: u32,
    dirty: bool,
}

static STACK: Mutex<Stack> = Mutex::new(Stack { dialogs: Vec::new(), serial: 0, dirty: false });

fn stack() -> MutexGuard<'static, Stack> {
    STACK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A message for the dialog procedure, sent once the state is unlocked.
#[derive(Clone, Copy)]
struct Notify {
    dialog: u32,
    msg: u32,
    wp: u32,
    lp: u32,
}

impl Control {
    pub fn interactive(&self) -> bool {
        self.visible
            && self.enabled
            && match self.kind {
                Kind::Group | Kind::Other => false,
                Kind::Static => self.style & SS_NOTIFY != 0,
                _ => true,
            }
    }

    /// The closed part of a combo box.
    pub fn face(&self) -> [i32; 4] {
        match self.kind {
            Kind::Combo { drop, .. } => [self.rect[0], self.rect[1], self.rect[2], self.rect[3] - drop],
            _ => self.rect,
        }
    }

    /// The open list of a combo box.
    pub fn drop_rect(&self) -> Option<[i32; 4]> {
        match self.kind {
            Kind::Combo { open: true, drop, .. } => Some([self.rect[0], self.rect[3] - drop, self.rect[2], self.rect[3]]),
            _ => None,
        }
    }

    /// A combo box shows its selected item (selected as a whole when it can be typed).
    fn sync_combo(&mut self) {
        if let Kind::Combo { items, sel, whole, .. } = &mut self.kind {
            if let Some(t) = items.get(*sel as usize) {
                self.text = t.0.clone();
                *whole = true;
            }
        }
    }

    /// A combo box whose text can be typed.
    fn typed_combo(&self) -> bool {
        matches!(self.kind, Kind::Combo { edit: true, .. })
    }

    fn items(&mut self) -> Option<(&mut Vec<(String, u32)>, &mut i32)> {
        match &mut self.kind {
            Kind::List { items, sel, .. } | Kind::Combo { items, sel, .. } => Some((items, sel)),
            _ => None,
        }
    }
}

pub const ROW: i32 = 11;

fn inside(r: [i32; 4], x: i32, y: i32) -> bool {
    x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
}

fn kind_of(i: &Item, h: i32) -> (Kind, i32) {
    let k = match &i.class {
        Class::Button => match i.style & 0xf {
            1 => Kind::Push { default: true },
            2 | 5 | 6 => Kind::Check { auto: false },
            3 => Kind::Check { auto: true },
            4 => Kind::Radio { auto: false },
            9 => Kind::Radio { auto: true },
            7 => Kind::Group,
            _ => Kind::Push { default: false },
        },
        Class::Static => Kind::Static,
        Class::Edit => Kind::Edit { limit: 0x7fff },
        Class::ListBox => Kind::List { items: Vec::new(), sel: -1, top: 0 },
        // the template's height includes the open list
        Class::ComboBox => Kind::Combo {
            items: Vec::new(),
            sel: -1,
            open: false,
            top: 0,
            drop: (h - 14).max(4 * ROW),
            edit: i.style & 3 == 2,
            whole: false,
        },
        Class::Other(n) if n.eq_ignore_ascii_case("msctls_trackbar32") => Kind::Track { lo: 0, hi: 100, pos: 0 },
        _ => Kind::Other,
    };
    (k, h)
}

impl Dialog {
    fn new(hwnd: u32, template: u32, proc_: u32, t: &Template) -> Dialog {
        let caption = t.style & WS_CAPTION == WS_CAPTION && !t.title.is_empty();
        let cap_h = if caption { 14 } else { 0 };
        // MS Sans Serif 8: 6 x 13 pixel dialog base units; shrink to fit the overlay
        let (mut sx, mut sy) = (1.5f32, 13.0 / 8.0);
        let (w, h) = (t.rect[2] as f32 * sx, t.rect[3] as f32 * sy + cap_h as f32);
        let fit = ((OV_W - 16) as f32 / w).min((OV_H - 16) as f32 / h).min(1.0);
        sx *= fit;
        sy *= fit;
        let (w, h) = ((t.rect[2] as f32 * sx) as i32, (t.rect[3] as f32 * sy) as i32 + cap_h);
        let (x0, y0) = ((OV_W - w) / 2, (OV_H - h) / 2);
        let controls = t
            .items
            .iter()
            .enumerate()
            .map(|(k, i)| {
                let px = |v: i32, s: f32| (v as f32 * s).round() as i32;
                let (cx, cy) = (x0 + px(i.rect[0], sx), y0 + cap_h + px(i.rect[1], sy));
                let (kind, h) = kind_of(i, px(i.rect[3], sy));
                Control {
                    hwnd: hwnd + 1 + k as u32,
                    id: i.id,
                    kind,
                    text: i.text.clone(),
                    style: i.style,
                    rect: [cx, cy, cx + px(i.rect[2], sx), cy + h],
                    enabled: i.style & WS_DISABLED == 0,
                    visible: i.style & WS_VISIBLE != 0,
                    check: 0,
                    ours: i.id >= FIRST_OURS && i.id < FIRST_OURS + 0x100,
                    inverted: false,
                }
            })
            .collect();
        Dialog {
            hwnd,
            template,
            proc_,
            title: t.title.clone(),
            caption,
            rect: [x0, y0, x0 + w, y0 + h],
            scale: (sx, sy),
            controls,
            result: None,
            focus: None,
            pressed: None,
            hot: None,
            drag: None,
        }
    }

    pub fn find(&self, id: u32) -> Option<usize> {
        self.controls.iter().position(|c| c.id == id)
    }

    pub fn control(&mut self, id: u32) -> Option<&mut Control> {
        self.controls.iter_mut().find(|c| c.id == id)
    }

    /// Dialog units (relative to the client area) to overlay pixels.
    pub fn du(&self, r: [i32; 4]) -> [i32; 4] {
        let cap = if self.caption { 14 } else { 0 };
        let x = self.rect[0] + (r[0] as f32 * self.scale.0).round() as i32;
        let y = self.rect[1] + cap + (r[1] as f32 * self.scale.1).round() as i32;
        [x, y, x + (r[2] as f32 * self.scale.0).round() as i32, y + (r[3] as f32 * self.scale.1).round() as i32]
    }

    /// The control under a point: an open combo list first.
    fn hit(&self, x: i32, y: i32) -> Option<usize> {
        if let Some(k) = self.controls.iter().position(|c| c.drop_rect().is_some_and(|r| inside(r, x, y))) {
            return Some(k);
        }
        // later controls are drawn on top
        self.controls.iter().rposition(|c| c.interactive() && inside(c.face(), x, y))
    }

    fn command(&self, k: usize, code: u32) -> Notify {
        let c = &self.controls[k];
        Notify { dialog: self.hwnd, msg: WM_COMMAND, wp: c.id & 0xffff | code << 16, lp: c.hwnd }
    }

    /// Activate a control as a click does.
    fn click(&mut self, k: usize, out: &mut Vec<Notify>) {
        let style = self.controls[k].style;
        match self.controls[k].kind {
            Kind::Check { auto: true } => self.controls[k].check ^= 1,
            Kind::Radio { auto: true } => {
                // the group: from the control with WS_GROUP at or before it to the next one
                let start = (0..=k).rev().find(|&i| self.controls[i].style & WS_GROUP != 0).unwrap_or(0);
                let end = (k + 1..self.controls.len()).find(|&i| self.controls[i].style & WS_GROUP != 0).unwrap_or(self.controls.len());
                for i in start..end {
                    if matches!(self.controls[i].kind, Kind::Radio { .. }) {
                        self.controls[i].check = (i == k) as u32;
                    }
                }
            }
            Kind::Static if style & SS_NOTIFY == 0 => return,
            Kind::Push { .. } | Kind::Check { .. } | Kind::Radio { .. } | Kind::Static => {}
            _ => return,
        }
        let c = &self.controls[k];
        if self.proc_ == 0 {
            // a message box: any button ends it
            self.result = Some(c.id);
        } else {
            out.push(self.command(k, 0));
        }
    }

    /// A push button by what it does: the default one, or one that cancels.
    fn button(&self, default: bool) -> Option<usize> {
        self.controls.iter().position(|c| {
            c.visible
                && c.enabled
                && match c.kind {
                    Kind::Push { default: d } if default => d,
                    Kind::Push { .. } => {
                        let t = c.text.trim().to_ascii_lowercase();
                        // Esc: the pause bar's "Resume Game" too (the original's
                        // buttons ignored Esc)
                        c.id == 2 || t == "cancel" || t.starts_with("resume") || t == "no"
                    }
                    _ => false,
                }
        })
    }

    fn set_track(&mut self, k: usize, x: i32) {
        let r = self.controls[k].rect;
        if let Kind::Track { lo, hi, pos } = &mut self.controls[k].kind {
            let w = (r[2] - r[0] - 10).max(1);
            *pos = *lo + ((x - r[0] - 5).clamp(0, w) * (*hi - *lo) + w / 2) / w;
        }
    }

    fn track_notify(&self, k: usize, code: u32) -> Notify {
        let c = &self.controls[k];
        let pos = if let Kind::Track { pos, .. } = c.kind { pos } else { 0 };
        let msg = if c.style & 2 != 0 { WM_VSCROLL } else { WM_HSCROLL };
        Notify { dialog: self.hwnd, msg, wp: code | (pos as u32) << 16, lp: c.hwnd }
    }

    /// Mouse and keyboard input for the topmost dialog.
    fn input(&mut self, m: &Msg, out: &mut Vec<Notify>) {
        if matches!(m.msg, 0x100 | 0x102 | 0x201) {
            draw::caret_reset();
        }
        let (fw, fh) = super::window::frame_size();
        let (sx, sw) = super::window::overlay_span(fw, fh);
        let x = ((m.lp & 0xffff) as i16 as i32 - sx as i32) * OV_W / sw.max(1) as i32;
        let y = (m.lp >> 16) as i16 as i32 * OV_H / fh.max(1) as i32;
        match m.msg {
            0x200 => {
                self.hot = self.hit(x, y);
                if let Some(k) = self.drag {
                    self.set_track(k, x);
                    out.push(self.track_notify(k, 5)); // TB_THUMBTRACK
                }
            }
            0x201 => {
                // an open combo list takes the click
                if let Some(k) = self.controls.iter().position(|c| c.drop_rect().is_some()) {
                    let r = self.controls[k].drop_rect().unwrap();
                    if let Kind::Combo { open, sel, top, items, .. } = &mut self.controls[k].kind {
                        *open = false;
                        let row = *top + (y - r[1] - 2) / ROW;
                        if inside(r, x, y) && row >= 0 && (row as usize) < items.len() {
                            *sel = row;
                            self.controls[k].sync_combo();
                            out.push(self.command(k, 1)); // CBN_SELCHANGE
                        }
                    }
                    out.push(self.command(k, 8)); // CBN_CLOSEUP
                    self.pressed = None;
                    return;
                }
                let Some(k) = self.hit(x, y) else { return };
                self.pressed = Some(k);
                let r = self.controls[k].rect;
                let face = self.controls[k].face();
                match &mut self.controls[k].kind {
                    // a combo box that can be typed: its text takes the focus, all selected
                    Kind::Combo { edit: true, whole, .. } if x < face[2] - 14 => {
                        *whole = true;
                        self.focus = Some(k);
                    }
                    Kind::Combo { open, top, sel, items, drop, .. } => {
                        *open = true;
                        // a little above the selection, but no empty rows at the end
                        let rows = (*drop - 4) / ROW;
                        *top = (*sel - 2).clamp(0, (items.len() as i32 - rows).max(0));
                        out.push(self.command(k, 7)); // CBN_DROPDOWN
                    }
                    Kind::Track { .. } => {
                        self.drag = Some(k);
                        self.set_track(k, x);
                        out.push(self.track_notify(k, 5));
                    }
                    Kind::List { items, sel, top } => {
                        let row = *top + (y - r[1] - 2) / ROW;
                        if row >= 0 && (row as usize) < items.len() {
                            *sel = row;
                            out.push(self.command(k, 1)); // LBN_SELCHANGE
                        }
                        self.focus = Some(k);
                    }
                    Kind::Edit { .. } => self.focus = Some(k),
                    _ => {}
                }
            }
            0x202 => {
                if let Some(k) = self.drag.take() {
                    out.push(self.track_notify(k, 4)); // TB_THUMBPOSITION
                    out.push(self.track_notify(k, 8)); // TB_ENDTRACK
                } else if let Some(k) = self.pressed.take() {
                    if self.hit(x, y) == Some(k) {
                        self.click(k, out);
                    }
                }
            }
            0x20a => {
                let lines = -((m.wp >> 16) as i16 as i32) / 120;
                let target = self.controls.iter().position(|c| c.drop_rect().is_some()).or_else(|| self.hit(x, y));
                if let Some(k) = target {
                    let rows = match self.controls[k].kind {
                        Kind::Combo { drop, .. } => (drop - 4) / ROW,
                        _ => (self.controls[k].rect[3] - self.controls[k].rect[1] - 4) / ROW,
                    };
                    if let Kind::List { items, top, .. } | Kind::Combo { items, top, .. } = &mut self.controls[k].kind {
                        *top = (*top + lines * 3).clamp(0, (items.len() as i32 - rows).max(0));
                    }
                }
            }
            0x100 => match m.wp {
                0x1b => {
                    if let Some(k) = self.button(false) {
                        self.click(k, out);
                    } else if self.proc_ == 0 {
                        self.result = Some(2);
                    } else {
                        out.push(Notify { dialog: self.hwnd, msg: WM_COMMAND, wp: 2, lp: 0 });
                    }
                }
                0x0d => {
                    if let Some(k) = self.button(true) {
                        self.click(k, out);
                    } else if self.proc_ == 0 {
                        self.result = self.controls.iter().find(|c| matches!(c.kind, Kind::Push { .. })).map(|c| c.id);
                    } else {
                        out.push(Notify { dialog: self.hwnd, msg: WM_COMMAND, wp: 1, lp: 0 });
                    }
                }
                0x08 => {
                    if let Some(k) = self.focus.filter(|&k| matches!(self.controls[k].kind, Kind::Edit { .. })) {
                        self.controls[k].text.pop();
                        out.push(self.command(k, 0x300)); // EN_CHANGE
                    } else if let Some(k) = self.focus.filter(|&k| self.controls[k].typed_combo()) {
                        let c = &mut self.controls[k];
                        if let Kind::Combo { whole, .. } = &mut c.kind {
                            if std::mem::take(whole) {
                                c.text.clear();
                            } else {
                                c.text.pop();
                            }
                        }
                        out.push(self.command(k, 5)); // CBN_EDITCHANGE
                    }
                }
                0x26 | 0x28 => {
                    let d = if m.wp == 0x26 { -1 } else { 1 };
                    // (an empty list has nothing to select)
                    if let Some(k) = self.focus.filter(|&k| matches!(&self.controls[k].kind, Kind::List { items, .. } if !items.is_empty())) {
                        if let Kind::List { items, sel, .. } = &mut self.controls[k].kind {
                            *sel = (*sel + d).clamp(0, items.len() as i32 - 1);
                        }
                        out.push(self.command(k, 1));
                    }
                }
                _ => {}
            },
            0x102 => {
                let ch = m.wp as u8;
                if let Some(k) = self.focus.filter(|&k| matches!(self.controls[k].kind, Kind::Edit { .. })) {
                    if let Kind::Edit { limit } = self.controls[k].kind {
                        if ch >= 32 && ch != 127 && (self.controls[k].text.len() as u32) < limit {
                            self.controls[k].text.push(ch as char);
                            out.push(self.command(k, 0x300));
                        }
                    }
                } else if let Some(k) = self.focus.filter(|&k| self.controls[k].typed_combo()) {
                    let c = &mut self.controls[k];
                    if let Kind::Combo { whole, .. } = &mut c.kind {
                        if ch >= 32 && ch != 127 {
                            if std::mem::take(whole) {
                                c.text.clear();
                            }
                            c.text.push(ch as char);
                            out.push(self.command(k, 5)); // CBN_EDITCHANGE
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// What the game's messages do to a control; returns the result.
fn control_message(d: &mut Dialog, k: usize, msg: u32, wp: u32, lp: u32, out: &mut Vec<Notify>) -> u32 {
    let m = mem();
    let text_of = |p: u32| if p == 0 { String::new() } else { cstr(p) };
    // copy a string to a guest buffer of `n` bytes (n = 0: unknown size)
    let give = |p: u32, s: &str, n: u32| put_cstr(p, s, if n == 0 { s.len() as u32 + 1 } else { n });
    let c = &mut d.controls[k];
    match msg {
        WM_SETTEXT => {
            c.text = text_of(lp);
            // the control repaints: an InvertRect over it is gone
            c.inverted = false;
            1
        }
        WM_GETTEXT => give(lp, &c.text, wp),
        WM_GETTEXTLENGTH => c.text.len() as u32,
        0x30 | 0x31 => 0, // WM_SETFONT, WM_GETFONT
        // buttons
        0xf0 => c.check,
        0xf1 => {
            c.check = wp;
            0
        }
        0xf2 => c.check,
        0xf3 | 0xf4 => 0,
        0xf5 => {
            d.click(k, out);
            0
        }
        // trackbars
        0x400..=0x41f if matches!(c.kind, Kind::Track { .. }) => {
            let Kind::Track { lo, hi, pos } = &mut c.kind else { unreachable!() };
            match msg {
                0x400 => *pos as u32,
                0x401 => *lo as u32,
                0x402 => *hi as u32,
                0x405 => {
                    *pos = (lp as i32).clamp(*lo, *hi);
                    0
                }
                0x406 => {
                    *lo = (lp & 0xffff) as i16 as i32;
                    *hi = (lp >> 16) as i16 as i32;
                    *pos = (*pos).clamp(*lo, *hi);
                    0
                }
                0x407 => {
                    *lo = lp as i32;
                    0
                }
                0x408 => {
                    *hi = lp as i32;
                    0
                }
                _ => 0,
            }
        }
        // edit
        0xc5 => {
            if let Kind::Edit { limit } = &mut c.kind {
                *limit = if wp == 0 { 0x7fff } else { wp };
            }
            0
        }
        // list and combo boxes share their messages, at different numbers
        _ => {
            let combo = matches!(c.kind, Kind::Combo { .. });
            let op = match (combo, msg) {
                (true, 0x143) | (false, 0x180) => "add",
                (true, 0x144) | (false, 0x182) => "delete",
                (true, 0x146) | (false, 0x18b) => "count",
                (true, 0x147) | (false, 0x188) => "getsel",
                (true, 0x148) | (false, 0x189) => "gettext",
                (true, 0x149) | (false, 0x18a) => "textlen",
                (true, 0x14a) | (false, 0x181) => "insert",
                (true, 0x14b) | (false, 0x184) => "reset",
                (true, 0x14c) | (false, 0x18f) => "find",
                (true, 0x14d) | (false, 0x18c) => "select",
                (true, 0x14e) | (false, 0x186) => "setsel",
                (true, 0x150) | (false, 0x199) => "getdata",
                (true, 0x151) | (false, 0x19a) => "setdata",
                (true, 0x158) | (false, 0x1a2) => "findexact",
                (true, 0x14f) => "dropdown",
                (true, 0x157) => "dropped",
                (false, 0x197) => "settop",
                (false, 0x18e) => "gettop",
                _ => "",
            };
            if op.is_empty() {
                if super::trace_api() {
                    log!("dialog: message {msg:#x} to control {} not handled", c.id);
                }
                return 0;
            }
            if let Kind::Combo { open, .. } = &mut c.kind {
                match op {
                    "dropdown" => {
                        *open = wp != 0;
                        return 1;
                    }
                    "dropped" => return *open as u32,
                    _ => {}
                }
            }
            if let Kind::List { top, .. } = &mut c.kind {
                match op {
                    "settop" => {
                        *top = wp as i32;
                        return 0;
                    }
                    "gettop" => return *top as u32,
                    _ => {}
                }
            }
            let Some((items, sel)) = c.items() else { return 0 };
            let get = |i: u32| items.get(i as usize);
            let find = |items: &Vec<(String, u32)>, start: u32, s: &str, exact: bool| -> u32 {
                let n = items.len();
                let from = if start == ERR { 0 } else { start as usize + 1 };
                (0..n)
                    .map(|k| (from + k) % n.max(1))
                    .find(|&k| {
                        let t = items[k].0.to_ascii_lowercase();
                        let s = s.to_ascii_lowercase();
                        if exact { t == s } else { t.starts_with(&s) }
                    })
                    .map_or(ERR, |k| k as u32)
            };
            let r = match op {
                "add" => {
                    items.push((text_of(lp), 0));
                    items.len() as u32 - 1
                }
                "insert" => {
                    let at = if wp == ERR { items.len() } else { (wp as usize).min(items.len()) };
                    items.insert(at, (text_of(lp), 0));
                    at as u32
                }
                "delete" => {
                    if (wp as usize) < items.len() {
                        items.remove(wp as usize);
                        if *sel >= items.len() as i32 {
                            *sel = -1;
                        }
                        items.len() as u32
                    } else {
                        ERR
                    }
                }
                "count" => items.len() as u32,
                "getsel" => *sel as u32,
                "setsel" => {
                    if (wp as usize) < items.len() {
                        *sel = wp as i32;
                        wp
                    } else {
                        *sel = -1;
                        ERR
                    }
                }
                "gettext" => get(wp).map_or(ERR, |t| give(lp, &t.0.clone(), 0)),
                "textlen" => get(wp).map_or(ERR, |t| t.0.len() as u32),
                "reset" => {
                    items.clear();
                    *sel = -1;
                    0
                }
                "find" | "findexact" => find(items, wp, &text_of(lp), op == "findexact"),
                "select" => {
                    let k = find(items, wp, &text_of(lp), false);
                    if k != ERR {
                        *sel = k as i32;
                    }
                    k
                }
                "getdata" => get(wp).map_or(ERR, |t| t.1),
                "setdata" => match items.get_mut(wp as usize) {
                    Some(t) => {
                        t.1 = lp;
                        1
                    }
                    None => ERR,
                },
                _ => {
                    let _ = m;
                    0
                }
            };
            if matches!(op, "setsel" | "select") {
                c.sync_combo();
            }
            r
        }
    }
}

// ---- running dialogs ------------------------------------------------------------------------------

/// Send the collected messages to the dialog procedures (with the state unlocked).
fn deliver(c: &mut Cpu, out: Vec<Notify>) {
    for n in out {
        if trace() {
            log!("dialog {:#x}: message {:#x} {:#x} {:#x}", n.dialog, n.msg, n.wp, n.lp);
        }
        let proc_ = stack().dialogs.iter().find(|d| d.hwnd == n.dialog).map(|d| d.proc_);
        if let Some(p) = proc_.filter(|&p| p != 0) {
            guest_call(c, p, &[n.dialog, n.msg, n.wp, n.lp]);
        }
        stack().dirty = true;
    }
}

fn redraw() {
    let mut s = stack();
    s.dirty = false;
    let img = if s.dialogs.is_empty() { None } else { Some((draw::render(&s.dialogs), OV_W as u32, OV_H as u32)) };
    drop(s);
    super::window::set_overlay(img);
}

fn is_input(msg: u32) -> bool {
    (0x100..=0x109).contains(&msg) || (0x200..=0x20e).contains(&msg)
}

/// Show a dialog and run it until it ends; returns its result.
fn run(c: &mut Cpu, mut d: Dialog, param: u32) -> u32 {
    let hwnd = d.hwnd;
    let first = d.controls.first().map_or(0, |c| c.hwnd);
    let proc_ = d.proc_;
    d.focus = d.controls.iter().position(|c| c.interactive());
    stack().dialogs.push(d);
    if proc_ != 0 {
        guest_call(c, proc_, &[hwnd, WM_INITDIALOG, first, param]);
    }
    // the game's launcher comes before its main window
    super::window::open("Frontschweine");
    redraw();
    let mut phase = draw::caret_phase();
    loop {
        let done = stack().dialogs.iter().find(|d| d.hwnd == hwnd).and_then(|d| d.result);
        if let Some(r) = done {
            stack().dialogs.retain(|d| d.hwnd != hwnd);
            redraw();
            return r;
        }
        if let Some(m) = user32::next_message(c, Duration::from_millis(10)) {
            if is_input(m.msg) {
                let mut out = Vec::new();
                if let Some(top) = stack().dialogs.last_mut() {
                    top.input(&m, &mut out);
                }
                stack().dirty = true;
                deliver(c, out);
            } else if m.msg == user32::WM_CLOSE && m.hwnd == user32::main_window() {
                // the window's close button: like Esc in the dialog
                let mut out = Vec::new();
                if let Some(top) = stack().dialogs.last_mut() {
                    top.input(&Msg { hwnd: m.hwnd, msg: 0x100, wp: 0x1b, lp: 0 }, &mut out);
                }
                deliver(c, out);
            } else {
                user32::dispatch(c, &m);
            }
        }
        // the caret blinks in a focused text
        if draw::caret_phase() != phase {
            phase = draw::caret_phase();
            let mut s = stack();
            if let Some(top) = s.dialogs.last() {
                let typed = |c: &Control| matches!(c.kind, Kind::Edit { .. }) || c.typed_combo();
                if top.focus.is_some_and(|k| typed(&top.controls[k])) {
                    s.dirty = true;
                }
            }
        }
        if stack().dirty {
            redraw();
        }
    }
}

/// Whether `hwnd` is one of ours (a dialog or one of its controls).
pub fn owns(hwnd: u32) -> bool {
    hwnd & 0xffff_0000 == 0x00da_0000
}

/// Find the dialog and control (None: the dialog itself) of a window handle.
fn locate(s: &Stack, hwnd: u32) -> Option<(usize, Option<usize>)> {
    let di = s.dialogs.iter().position(|d| d.hwnd == hwnd & !0xff)?;
    let k = hwnd & 0xff;
    if k == 0 { Some((di, None)) } else { (k as usize <= s.dialogs[di].controls.len()).then_some((di, Some(k as usize - 1))) }
}

/// SendMessage to a dialog or control.
pub fn send(c: &mut Cpu, hwnd: u32, msg: u32, wp: u32, lp: u32) -> u32 {
    let mut s = stack();
    let Some((di, k)) = locate(&s, hwnd) else { return 0 };
    match k {
        None => {
            let p = s.dialogs[di].proc_;
            drop(s);
            if p != 0 { guest_call(c, p, &[hwnd, msg, wp, lp]) } else { 0 }
        }
        Some(k) => {
            let mut out = Vec::new();
            let r = control_message(&mut s.dialogs[di], k, msg, wp, lp, &mut out);
            s.dirty = true;
            drop(s);
            deliver(c, out);
            r
        }
    }
}

/// Run `f` on a control (or the dialog's own fields when `hwnd` is the dialog).
pub fn with<R>(hwnd: u32, f: impl FnOnce(&mut Dialog, Option<usize>) -> R) -> Option<R> {
    let mut s = stack();
    let (di, k) = locate(&s, hwnd)?;
    s.dirty = true;
    Some(f(&mut s.dialogs[di], k))
}

/// The text of control `id` in the open dialog made from template `template`.
pub fn text_in(template: u32, id: u32) -> Option<String> {
    let mut s = stack();
    let d = s.dialogs.iter_mut().rev().find(|d| d.template == template)?;
    d.control(id).map(|c| c.text.clone())
}

/// The window rectangle of a dialog or control, in overlay pixels.
pub fn rect(hwnd: u32) -> Option<[i32; 4]> {
    with(hwnd, |d, k| k.map_or(d.rect, |k| d.controls[k].rect))
}

// ---- imports --------------------------------------------------------------------------------------

fn dialog_box_param(c: &mut Cpu) -> u32 {
    let (tmpl, proc_, param) = (arg(c, 1), arg(c, 3), arg(c, 4));
    let id = ResId::from_guest(tmpl);
    let Some((at, size)) = resource(ResId::Id(5), id) else {
        log!("warn: DialogBoxParamA: dialog {id:?} not found");
        return ERR;
    };
    let Some(t) = Template::parse(mem().slice(at, size as usize)) else {
        log!("warn: DialogBoxParamA: dialog {id:?} cannot be read");
        return ERR;
    };
    let tid = if let ResId::Id(n) = id { n } else { 0 };
    if trace() {
        log!("dialog {id:?} opens");
    }
    let hwnd = {
        let mut s = stack();
        s.serial = s.serial.wrapping_add(1);
        0x00da_0000 | (s.serial & 0xff) << 8
    };
    run(c, Dialog::new(hwnd, tid, proc_, &t), param)
}

fn end_dialog(c: &mut Cpu) -> u32 {
    let (h, r) = (arg(c, 0), arg(c, 1));
    with(h, |d, _| d.result = Some(r)).is_some() as u32
}

fn get_dlg_item(c: &mut Cpu) -> u32 {
    let (h, id) = (arg(c, 0), arg(c, 1));
    with(h, |d, _| d.find(id).map_or(0, |k| d.controls[k].hwnd)).unwrap_or(0)
}

fn item(h: u32, id: u32) -> u32 {
    with(h, |d, _| d.find(id).map_or(0, |k| d.controls[k].hwnd)).unwrap_or(0)
}

fn set_dlg_item_text(c: &mut Cpu) -> u32 {
    let (h, id, p) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let i = item(h, id);
    if i == 0 {
        return 0;
    }
    send(c, i, WM_SETTEXT, 0, p);
    1
}

fn get_dlg_item_text(c: &mut Cpu) -> u32 {
    let (h, id, p, n) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    let i = item(h, id);
    if i == 0 {
        return 0;
    }
    send(c, i, WM_GETTEXT, n, p)
}

fn send_dlg_item_message(c: &mut Cpu) -> u32 {
    let (h, id, msg, wp, lp) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
    let i = item(h, id);
    if i == 0 {
        return 0;
    }
    send(c, i, msg, wp, lp)
}

fn check_dlg_button(c: &mut Cpu) -> u32 {
    let (h, id, v) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let i = item(h, id);
    if i != 0 {
        send(c, i, 0xf1, v, 0);
    }
    (i != 0) as u32
}

fn is_dlg_button_checked(c: &mut Cpu) -> u32 {
    let (h, id) = (arg(c, 0), arg(c, 1));
    let i = item(h, id);
    if i == 0 { 0 } else { send(c, i, 0xf0, 0, 0) }
}

/// MessageBoxA as one of our dialogs: the text and the buttons its type asks for.
fn message_box(c: &mut Cpu) -> u32 {
    let (text, title, kind) = (cstr(arg(c, 1)), if arg(c, 2) != 0 { cstr(arg(c, 2)) } else { String::new() }, arg(c, 3));
    log!("MessageBox [{title}] {text}");
    let buttons: &[(&str, u32)] = match kind & 0xf {
        1 => &[("OK", 1), ("Cancel", 2)],
        2 => &[("Abort", 3), ("Retry", 4), ("Ignore", 5)],
        3 => &[("Yes", 6), ("No", 7), ("Cancel", 2)],
        4 => &[("Yes", 6), ("No", 7)],
        5 => &[("Retry", 4), ("Cancel", 2)],
        _ => &[("OK", 1)],
    };
    if user32::main_window() == 0 {
        return buttons[0].1;
    }
    // lay it out in dialog units like a template: text lines, then a row of buttons
    let lines: Vec<&str> = text.lines().collect();
    let width = lines.iter().map(|l| draw::text_width(l) * 2 / 3 + 1).max().unwrap_or(0).max(buttons.len() as i32 * 56) + 20;
    let th = lines.len() as i32 * 9;
    let mut items: Vec<Item> = lines
        .iter()
        .enumerate()
        .map(|(k, l)| Item { class: Class::Static, id: u32::MAX, text: l.to_string(), style: WS_VISIBLE | 1, ex_style: 0, rect: [10, 10 + 9 * k as i32, width - 20, 9] })
        .collect();
    let bx = (width - buttons.len() as i32 * 56 + 6) / 2;
    for (k, &(t, id)) in buttons.iter().enumerate() {
        items.push(Item { class: Class::Button, id, text: t.into(), style: WS_VISIBLE | (k == 0) as u32, ex_style: 0, rect: [bx + 56 * k as i32, th + 18, 50, 14] });
    }
    let t = Template { style: if title.is_empty() { 0 } else { WS_CAPTION }, title, rect: [0, 0, width, th + 40], font: None, items };
    let hwnd = {
        let mut s = stack();
        s.serial = s.serial.wrapping_add(1);
        0x00da_0000 | (s.serial & 0xff) << 8
    };
    run(c, Dialog::new(hwnd, 0, 0, &t), 0)
}

pub const IMPORTS: &[Import] = &[
    ("DialogBoxParamA", 5, dialog_box_param),
    ("EndDialog", 2, end_dialog),
    ("GetDlgItem", 2, get_dlg_item),
    ("SetDlgItemTextA", 3, set_dlg_item_text),
    ("GetDlgItemTextA", 4, get_dlg_item_text),
    ("SendDlgItemMessageA", 5, send_dlg_item_message),
    ("CheckDlgButton", 3, check_dlg_button),
    ("IsDlgButtonChecked", 2, is_dlg_button_checked),
    ("MessageBoxA", 4, message_box),
];
