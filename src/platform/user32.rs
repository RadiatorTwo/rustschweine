//! USER32: window classes and windows, the message queue, timers, keyboard and mouse state.
//! The game's window procedures are guest code; input comes from `window`.

use super::dialog;
use super::window::{self, Input};
use super::{Import, arg, cstr, guest_call, log, mem, put_cstr};
use crate::rt::Cpu;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// The desktop the game sees. Windowed, DirectDraw's primary surface is this whole desktop and
/// only the main window's part of it is shown.
pub const DESKTOP_W: u32 = 1600;
pub const DESKTOP_H: u32 = 1200;

pub const WM_CREATE: u32 = 0x01;
pub const WM_DESTROY: u32 = 0x02;
pub const WM_SIZE: u32 = 0x05;
pub const WM_ACTIVATE: u32 = 0x06;
pub const WM_SETFOCUS: u32 = 0x07;
pub const WM_KILLFOCUS: u32 = 0x08;
pub const WM_PAINT: u32 = 0x0f;
pub const WM_CLOSE: u32 = 0x10;
pub const WM_QUIT: u32 = 0x12;
pub const WM_ACTIVATEAPP: u32 = 0x1c;
pub const WM_KEYDOWN: u32 = 0x100;
pub const WM_KEYUP: u32 = 0x101;
pub const WM_CHAR: u32 = 0x102;
pub const WM_SYSKEYDOWN: u32 = 0x104;
pub const WM_SYSKEYUP: u32 = 0x105;
pub const WM_TIMER: u32 = 0x113;
pub const WM_MOUSEMOVE: u32 = 0x200;
pub const WM_MOUSEWHEEL: u32 = 0x20a;

#[derive(Clone, Copy, Debug)]
pub struct Msg {
    pub hwnd: u32,
    pub msg: u32,
    pub wp: u32,
    pub lp: u32,
}

struct Win {
    proc_: u32,
    style: u32,
    exstyle: u32,
    rect: [i32; 4],
    title: String,
    user: u32,
    id: u32,
    shown: bool,
    /// needs painting (Windows' update region, whole or nothing): WM_PAINT comes when the
    /// queue is empty, until ValidateRect or BeginPaint
    dirty: bool,
}

struct Timer {
    hwnd: u32,
    id: u32,
    ms: u32,
    proc_: u32,
    due: u32,
}

struct State {
    classes: BTreeMap<String, u32>,
    wins: BTreeMap<u32, Win>,
    next: u32,
    main: u32,
    focus: u32,
    queue: VecDeque<Msg>,
    timers: Vec<Timer>,
    /// GetAsyncKeyState: bit 7 down, bit 0 pressed since the last query
    keys: [u8; 256],
    mouse: (i32, i32),
    /// hooks: id, kind, procedure
    hooks: Vec<(u32, u32, u32)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    classes: BTreeMap::new(),
    wins: BTreeMap::new(),
    next: 0x10000,
    main: 0,
    focus: 0,
    queue: VecDeque::new(),
    timers: Vec::new(),
    keys: [0; 256],
    mouse: (0, 0),
    hooks: Vec::new(),
});

fn st() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The game's main window (the first top-level one).
pub fn main_window() -> u32 {
    st().main
}

/// The main window's client rectangle on the desktop.
pub fn main_rect() -> [i32; 4] {
    let s = st();
    s.wins.get(&s.main).map_or([0, 0, 640, 480], |w| w.rect)
}

/// The mouse position in the main window's client area.
pub fn mouse_pos() -> (i32, i32) {
    st().mouse
}

pub fn key_down(vk: u8) -> bool {
    st().keys[vk as usize] & 0x80 != 0
}

fn trace_msg() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var_os("HOGS_TRACE_MSG").is_some())
}

/// Call a window's procedure.
pub fn send(c: &mut Cpu, hwnd: u32, msg: u32, wp: u32, lp: u32) -> u32 {
    if dialog::owns(hwnd) {
        return dialog::send(c, hwnd, msg, wp, lp);
    }
    let proc_ = st().wins.get(&hwnd).map(|w| w.proc_);
    if trace_msg() {
        log!("msg {hwnd:#x} {msg:#x} {wp:#x} {lp:#x}");
    }
    match proc_ {
        Some(p) if p != 0 => guest_call(c, p, &[hwnd, msg, wp, lp]),
        _ => def_window_proc_for(c, hwnd, msg, wp, lp),
    }
}

pub fn post(hwnd: u32, msg: u32, wp: u32, lp: u32) {
    st().queue.push_back(Msg { hwnd, msg, wp, lp });
}

/// Turn window-system input into messages and key state; due timers post WM_TIMER.
fn poll(wait: Duration) {
    super::ddraw::show_pending();
    window::pump(wait);
    let mut guard = st();
    let s = &mut *guard;
    let hwnd = s.main;
    while let Some(i) = window::next_input() {
        let m = |msg, wp, lp| Msg { hwnd, msg, wp, lp };
        match i {
            Input::Key { vk, scan, down, repeat } => {
                let k = &mut s.keys[vk as usize];
                if down {
                    *k = if *k & 0x80 == 0 { 0x81 } else { *k | 0x80 };
                } else {
                    *k &= !0x80;
                }
                let lp = 1 | (scan as u32) << 16 | if repeat { 1 << 30 } else { 0 } | if down { 0 } else { 3 << 30 };
                let sys = vk == 0x12 || key_alt(&s.keys) && vk != 0x12;
                let msg = match (down, sys) {
                    (true, false) => WM_KEYDOWN,
                    (false, false) => WM_KEYUP,
                    (true, true) => WM_SYSKEYDOWN,
                    (false, true) => WM_SYSKEYUP,
                };
                s.queue.push_back(m(msg, vk as u32, lp));
            }
            Input::Char(ch) => s.queue.push_back(m(WM_CHAR, ch as u32, 1)),
            Input::Move(x, y) => {
                s.mouse = (x, y);
                s.queue.push_back(m(WM_MOUSEMOVE, buttons(&s.keys), (x as u32 & 0xffff) | (y as u32) << 16));
            }
            Input::Button(b, down) => {
                let vk = [1u8, 2, 4][b as usize];
                let k = &mut s.keys[vk as usize];
                *k = if down { 0x81 } else { *k & !0x80 };
                let msg = [0x201, 0x204, 0x207][b as usize] + if down { 0 } else { 1 };
                let (x, y) = s.mouse;
                s.queue.push_back(m(msg, buttons(&s.keys), (x as u32 & 0xffff) | (y as u32) << 16));
            }
            Input::Wheel(n) => {
                let (x, y) = s.mouse;
                s.queue.push_back(m(WM_MOUSEWHEEL, buttons(&s.keys) | ((n * 120) as u32) << 16, (x as u32 & 0xffff) | (y as u32) << 16));
            }
            Input::Close => s.queue.push_back(m(WM_CLOSE, 0, 0)),
            // The game pauses itself when it is deactivated (WM_ACTIVATEAPP 0), as Windows 98
            // games did; here it keeps running in the background. Keys held when the window
            // loses the focus would never see their release: they are let go.
            Input::Focus(false) => {
                for vk in 0..256 {
                    if s.keys[vk] & 0x80 != 0 {
                        s.keys[vk] &= !0x80;
                        s.queue.push_back(m(WM_KEYUP, vk as u32, 0xc000_0001));
                    }
                }
            }
            Input::Focus(true) | Input::Poke(..) | Input::Pad(..) => {}
        }
    }
    let now = super::kernel32::ms_since_start();
    let mut due = Vec::new();
    for t in &mut s.timers {
        if now.wrapping_sub(t.due) as i32 >= 0 {
            t.due = now + t.ms.max(1);
            due.push(Msg { hwnd: t.hwnd, msg: WM_TIMER, wp: t.id, lp: t.proc_ });
        }
    }
    for m in due {
        // one pending WM_TIMER per timer, as Windows does
        if !s.queue.iter().any(|q| q.msg == WM_TIMER && q.hwnd == m.hwnd && q.wp == m.wp) {
            s.queue.push_back(m);
        }
    }
}

fn key_alt(keys: &[u8; 256]) -> bool {
    keys[0x12] & 0x80 != 0
}

/// MK_* flags for mouse messages
fn buttons(keys: &[u8; 256]) -> u32 {
    (keys[1] >> 7) as u32 | ((keys[2] >> 7) as u32) << 1 | ((keys[0x10] >> 7) as u32) << 2 | ((keys[0x11] >> 7) as u32) << 3 | ((keys[4] >> 7) as u32) << 4
}

/// The next message for this thread, waiting up to `wait` for one (modal loops).
pub fn next_message(c: &mut Cpu, wait: Duration) -> Option<Msg> {
    poll(Duration::ZERO);
    if let Some(m) = take_hooked(c, 0, 0, true) {
        return Some(m);
    }
    poll(wait);
    take_hooked(c, 0, 0, true)
}

/// Take a message; key messages pass the keyboard hooks first, which may swallow them.
fn take_hooked(c: &mut Cpu, min: u32, max: u32, remove: bool) -> Option<Msg> {
    loop {
        let m = take_msg(min, max, remove)?;
        if !remove || !(WM_KEYDOWN..=WM_SYSKEYUP).contains(&m.msg) {
            return Some(m);
        }
        let hooks: Vec<u32> = st().hooks.iter().filter(|h| h.1 == WH_KEYBOARD).map(|h| h.2).collect();
        // a keyboard hook gets the key, the repeat count and flags as in the message
        let swallowed = hooks.into_iter().any(|p| guest_call(c, p, &[0, m.wp, m.lp]) != 0);
        if !swallowed {
            return Some(m);
        }
    }
}

/// What DispatchMessageA does with a message.
pub fn dispatch(c: &mut Cpu, m: &Msg) -> u32 {
    if m.msg == WM_TIMER && m.lp != 0 {
        return guest_call(c, m.lp, &[m.hwnd, m.msg, m.wp, super::kernel32::ms_since_start()]);
    }
    send(c, m.hwnd, m.msg, m.wp, m.lp)
}

fn take_msg(min: u32, max: u32, remove: bool) -> Option<Msg> {
    let mut s = st();
    let wanted = |msg| min == 0 && max == 0 || (min..=max).contains(&msg);
    let Some(k) = s.queue.iter().position(|m| wanted(m.msg)) else {
        // WM_PAINT is not queued: it comes when nothing else is waiting
        let h = s.wins.iter().find(|(_, w)| w.dirty).map(|(&h, _)| h).filter(|_| wanted(WM_PAINT))?;
        return Some(Msg { hwnd: h, msg: WM_PAINT, wp: 0, lp: 0 });
    };
    if remove { s.queue.remove(k) } else { s.queue.get(k).copied() }
}

/// InvalidateRect: the window (0: every window) needs painting.
pub fn invalidate(hwnd: u32) {
    for (&h, w) in st().wins.iter_mut() {
        if hwnd == 0 || h == hwnd {
            w.dirty = true;
        }
    }
}

fn validate(hwnd: u32) {
    if let Some(w) = st().wins.get_mut(&hwnd) {
        w.dirty = false;
    }
}

fn write_msg(p: u32, m: &Msg) {
    let mm = mem();
    let (x, y) = st().mouse;
    for (k, v) in [m.hwnd, m.msg, m.wp, m.lp, super::kernel32::ms_since_start(), x as u32, y as u32].into_iter().enumerate() {
        mm.wr32(p + 4 * k as u32, v);
    }
}

fn peek_message(c: &mut Cpu) -> u32 {
    let (p, min, max, flags) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 4));
    poll(Duration::ZERO);
    match take_hooked(c, min, max, flags & 1 != 0) {
        Some(m) => {
            write_msg(p, &m);
            1
        }
        None => {
            // menus poll in a loop: let others run; a mission draws between its steps instead
            // (giving the time slice away there costs whole milliseconds when the core is busy)
            if !super::fps::active() {
                std::thread::yield_now();
            }
            0
        }
    }
}

fn get_message(c: &mut Cpu) -> u32 {
    let (p, min, max) = (arg(c, 0), arg(c, 2), arg(c, 3));
    loop {
        poll(Duration::ZERO);
        if let Some(m) = take_hooked(c, min, max, true) {
            write_msg(p, &m);
            return (m.msg != WM_QUIT) as u32;
        }
        poll(Duration::from_millis(5));
    }
}

fn read_msg(p: u32) -> Msg {
    let m = mem();
    Msg { hwnd: m.rd32(p), msg: m.rd32(p + 4), wp: m.rd32(p + 8), lp: m.rd32(p + 12) }
}

fn dispatch_message(c: &mut Cpu) -> u32 {
    let m = read_msg(arg(c, 0));
    dispatch(c, &m)
}

fn translate_message(_: &mut Cpu) -> u32 {
    // WM_CHAR comes from the window system's text input already
    0
}

fn post_message(c: &mut Cpu) -> u32 {
    post(arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    1
}

fn send_message(c: &mut Cpu) -> u32 {
    let (h, m, wp, lp) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    send(c, h, m, wp, lp)
}

fn post_quit_message(c: &mut Cpu) -> u32 {
    post(0, WM_QUIT, arg(c, 0), 0);
    0
}

fn def_window_proc_for(c: &mut Cpu, hwnd: u32, msg: u32, wp: u32, lp: u32) -> u32 {
    match msg {
        WM_CLOSE => {
            destroy(c, hwnd);
            0
        }
        0x0d => {
            // WM_GETTEXT
            let t = st().wins.get(&hwnd).map(|w| w.title.clone()).unwrap_or_default();
            put_cstr(lp, &t, wp)
        }
        0x0e => st().wins.get(&hwnd).map_or(0, |w| w.title.len() as u32),
        WM_PAINT => {
            // BeginPaint and EndPaint
            validate(hwnd);
            0
        }
        0x14 | 0x20 => 1, // WM_ERASEBKGND, WM_SETCURSOR
        0x84 => 1,        // WM_NCHITTEST: client
        _ => {
            let _ = (wp, lp);
            0
        }
    }
}

fn def_window_proc(c: &mut Cpu) -> u32 {
    let (h, m, wp, lp) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    def_window_proc_for(c, h, m, wp, lp)
}

fn register_class(c: &mut Cpu) -> u32 {
    let p = arg(c, 0);
    let (proc_, name) = (mem().rd32(p + 4), mem().rd32(p + 36));
    let name = if name > 0xffff { cstr(name) } else { format!("#{name}") };
    st().classes.insert(name.to_ascii_lowercase(), proc_);
    0xc000 + st().classes.len() as u32
}

fn create_window_ex(c: &mut Cpu) -> u32 {
    let a: Vec<u32> = (0..12).map(|k| arg(c, k)).collect();
    let class = if a[1] > 0xffff { cstr(a[1]) } else { format!("#{}", a[1]) };
    let title = if a[2] != 0 { cstr(a[2]) } else { String::new() };
    let def = |v: u32, d: i32| if v == 0x8000_0000 { d } else { v as i32 };
    let (x, y) = (def(a[4], 0), def(a[5], 0));
    let (w, h) = (def(a[6], 640), def(a[7], 480));
    let (hwnd, first) = {
        let mut s = st();
        let proc_ = s.classes.get(&class.to_ascii_lowercase()).copied().unwrap_or(0);
        s.next += 4;
        let hwnd = s.next;
        s.wins.insert(
            hwnd,
            Win { proc_, style: a[3], exstyle: a[0], rect: [x, y, x + w, y + h], title: title.clone(), user: 0, id: a[9], shown: false, dirty: false },
        );
        let first = s.main == 0 && a[8] == 0;
        if first {
            s.main = hwnd;
            s.focus = hwnd;
        }
        (hwnd, first)
    };
    if first {
        window::open(if title.is_empty() { "Frontschweine" } else { &title });
    }
    // CREATESTRUCT
    let cs = super::alloc(48);
    for (k, v) in [a[11], a[10], a[9], a[8], h as u32, w as u32, y as u32, x as u32, a[3], a[2], a[1], a[0]].into_iter().enumerate() {
        mem().wr32(cs + 4 * k as u32, v);
    }
    let r = send(c, hwnd, WM_CREATE, 0, cs);
    super::free(cs);
    if r == 0xffff_ffff {
        st().wins.remove(&hwnd);
        return 0;
    }
    if a[3] & 0x1000_0000 != 0 {
        show(c, hwnd);
    }
    hwnd
}

fn show(c: &mut Cpu, hwnd: u32) {
    let first = match st().wins.get_mut(&hwnd) {
        Some(w) if !w.shown => {
            w.shown = true;
            w.dirty = true;
            true
        }
        _ => false,
    };
    if first && hwnd == main_window() {
        // what Windows sends when the window shows and becomes active
        send(c, hwnd, WM_ACTIVATEAPP, 1, 0);
        send(c, hwnd, WM_ACTIVATE, 1, 0);
        send(c, hwnd, WM_SETFOCUS, 0, 0);
    }
}

fn destroy(c: &mut Cpu, hwnd: u32) {
    if !st().wins.contains_key(&hwnd) {
        return;
    }
    send(c, hwnd, WM_DESTROY, 0, 0);
    let mut s = st();
    s.wins.remove(&hwnd);
    s.timers.retain(|t| t.hwnd != hwnd);
}

fn destroy_window(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    if dialog::owns(h) {
        return dialog::with(h, |d, k| match k {
            None => d.result = Some(0),
            Some(k) => d.controls[k].visible = false,
        })
        .is_some() as u32;
    }
    destroy(c, h);
    1
}

fn show_window(c: &mut Cpu) -> u32 {
    let (h, cmd) = (arg(c, 0), arg(c, 1));
    if dialog::owns(h) {
        return dialog::with(h, |d, k| {
            // the dialog itself is always shown
            k.map_or(1, |k| std::mem::replace(&mut d.controls[k].visible, cmd != 0) as u32)
        })
        .unwrap_or(0);
    }
    let was = st().wins.get(&h).is_some_and(|w| w.shown);
    if cmd != 0 {
        show(c, h);
    }
    was as u32
}

fn set_window_pos(c: &mut Cpu) -> u32 {
    let (h, x, y, w, hh, flags) = (arg(c, 0), arg(c, 2) as i32, arg(c, 3) as i32, arg(c, 4) as i32, arg(c, 5) as i32, arg(c, 6));
    if dialog::owns(h) {
        // controls: client coordinates of the dialog; the dialog stays centred
        dialog::with(h, |d, k| {
            if let Some(k) = k {
                let (cap, o) = (if d.caption { 14 } else { 0 }, d.rect);
                let r = &mut d.controls[k].rect;
                if flags & 2 == 0 {
                    let (dx, dy) = (o[0] + x - r[0], o[1] + cap + y - r[1]);
                    *r = [r[0] + dx, r[1] + dy, r[2] + dx, r[3] + dy];
                }
                if flags & 1 == 0 {
                    r[2] = r[0] + w;
                    r[3] = r[1] + hh;
                }
                if flags & 0x80 != 0 {
                    d.controls[k].visible = false;
                } else if flags & 0x40 != 0 {
                    d.controls[k].visible = true;
                }
            }
        });
        return 1;
    }
    let mut s = st();
    if let Some(win) = s.wins.get_mut(&h) {
        let r = win.rect;
        let (nx, ny) = if flags & 2 != 0 { (r[0], r[1]) } else { (x, y) };
        let (nw, nh) = if flags & 1 != 0 { (r[2] - r[0], r[3] - r[1]) } else { (w, hh) };
        win.rect = [nx, ny, nx + nw, ny + nh];
    }
    1
}

fn write_rect(p: u32, r: [i32; 4]) {
    for (k, v) in r.into_iter().enumerate() {
        mem().wr32(p + 4 * k as u32, v as u32);
    }
}

fn get_window_rect(c: &mut Cpu) -> u32 {
    if let Some(r) = dialog::rect(arg(c, 0)) {
        write_rect(arg(c, 1), r);
        return 1;
    }
    let r = st().wins.get(&arg(c, 0)).map_or([0, 0, DESKTOP_W as i32, DESKTOP_H as i32], |w| w.rect);
    write_rect(arg(c, 1), r);
    1
}

fn get_client_rect(c: &mut Cpu) -> u32 {
    if let Some(r) = dialog::rect(arg(c, 0)) {
        write_rect(arg(c, 1), [0, 0, r[2] - r[0], r[3] - r[1]]);
        return 1;
    }
    let r = st().wins.get(&arg(c, 0)).map_or([0, 0, DESKTOP_W as i32, DESKTOP_H as i32], |w| w.rect);
    write_rect(arg(c, 1), [0, 0, r[2] - r[0], r[3] - r[1]]);
    1
}

fn get_window_long(c: &mut Cpu) -> u32 {
    let (h, i) = (arg(c, 0), arg(c, 1) as i32);
    let s = st();
    let Some(w) = s.wins.get(&h) else { return 0 };
    match i {
        -4 => w.proc_,
        -6 => super::IMAGE_BASE,
        -12 => w.id,
        -16 => w.style,
        -20 => w.exstyle,
        -21 => w.user,
        _ => 0,
    }
}

fn set_window_long(c: &mut Cpu) -> u32 {
    let (h, i, v) = (arg(c, 0), arg(c, 1) as i32, arg(c, 2));
    let mut s = st();
    let Some(w) = s.wins.get_mut(&h) else { return 0 };
    let f = match i {
        -4 => &mut w.proc_,
        -12 => &mut w.id,
        -16 => &mut w.style,
        -20 => &mut w.exstyle,
        -21 => &mut w.user,
        _ => return 0,
    };
    std::mem::replace(f, v)
}

fn set_window_text(c: &mut Cpu) -> u32 {
    let (h, p) = (arg(c, 0), arg(c, 1));
    if dialog::owns(h) {
        dialog::send(c, h, 0x0c, 0, p);
        return 1;
    }
    let t = cstr(p);
    if let Some(w) = st().wins.get_mut(&h) {
        w.title = t;
    }
    1
}

fn set_timer(c: &mut Cpu) -> u32 {
    let (hwnd, id, ms, proc_) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    let mut s = st();
    let id = if hwnd == 0 { 0x100 + s.timers.len() as u32 } else { id };
    s.timers.retain(|t| !(t.hwnd == hwnd && t.id == id));
    let due = super::kernel32::ms_since_start() + ms.max(1);
    s.timers.push(Timer { hwnd, id, ms, proc_, due });
    if id == 0 { 1 } else { id }
}

fn kill_timer(c: &mut Cpu) -> u32 {
    let (hwnd, id) = (arg(c, 0), arg(c, 1));
    let mut s = st();
    s.timers.retain(|t| !(t.hwnd == hwnd && t.id == id));
    s.queue.retain(|m| !(m.msg == WM_TIMER && m.hwnd == hwnd && m.wp == id));
    1
}

fn get_async_key_state(c: &mut Cpu) -> u32 {
    let vk = (arg(c, 0) & 0xff) as usize;
    let mut s = st();
    let k = s.keys[vk];
    s.keys[vk] &= !1;
    ((k as u32 & 0x80) << 8) | (k as u32 & 1)
}

fn get_key_state(c: &mut Cpu) -> u32 {
    let vk = (arg(c, 0) & 0xff) as usize;
    // Num Lock reads as on: the default controls are the keypad's number keys
    let toggled = (vk == 0x90) as u32;
    let k = st().keys[vk] as u32;
    (if k & 0x80 != 0 { 0xff80 } else { 0 }) | toggled
}

fn keybd_event(c: &mut Cpu) -> u32 {
    let (vk, flags) = (arg(c, 0) & 0xff, arg(c, 2));
    let mut s = st();
    if flags & 2 != 0 {
        s.keys[vk as usize] &= !0x80;
    } else {
        s.keys[vk as usize] |= 0x81;
    }
    0
}

fn system_parameters_info(c: &mut Cpu) -> u32 {
    let (action, pv) = (arg(c, 0), arg(c, 2));
    match action {
        0x30 if pv != 0 => write_rect(pv, [0, 0, DESKTOP_W as i32, DESKTOP_H as i32]), // SPI_GETWORKAREA
        0x61 | 0x11 => {}                                                               // screen saver
        _ => log!("warn: SystemParametersInfoA({action:#x}) ignored"),
    }
    1
}

fn get_system_metrics(c: &mut Cpu) -> u32 {
    match arg(c, 0) {
        0 | 16 | 61 | 59 => DESKTOP_W,
        1 | 17 | 62 | 60 => DESKTOP_H,
        2 | 3 => 16,
        4 => 19,
        5 | 6 => 1,
        32 | 33 => 4,
        45 | 46 => 2,
        _ => 0,
    }
}

fn adjust_window_rect(_: &mut Cpu) -> u32 {
    // the window is all client area: the rectangle stays
    1
}

fn client_to_screen(_: &mut Cpu) -> u32 {
    // the window's client area starts at its own position on the desktop
    1
}

fn set_rect(c: &mut Cpu) -> u32 {
    write_rect(arg(c, 0), [arg(c, 1) as i32, arg(c, 2) as i32, arg(c, 3) as i32, arg(c, 4) as i32]);
    1
}

fn begin_paint(c: &mut Cpu) -> u32 {
    let (h, ps) = (arg(c, 0), arg(c, 1));
    validate(h);
    mem().slice(ps, 64).fill(0);
    mem().wr32(ps, super::gdi32::SCREEN_DC);
    let r = st().wins.get(&h).map_or([0; 4], |w| w.rect);
    write_rect(ps + 8, [0, 0, r[2] - r[0], r[3] - r[1]]);
    super::gdi32::SCREEN_DC
}

/// InvertRect on a dialog control's DC: the game marks the key box waiting for a key.
fn invert_rect(c: &mut Cpu) -> u32 {
    let dc = arg(c, 0);
    if dialog::owns(dc) {
        dialog::with(dc, |d, k| {
            if let Some(k) = k {
                d.controls[k].inverted ^= true;
            }
        });
    }
    1
}

fn get_dc(c: &mut Cpu) -> u32 {
    // a dialog control's DC is its window handle (for InvertRect)
    let h = arg(c, 0);
    if dialog::owns(h) {
        return h;
    }
    super::gdi32::SCREEN_DC
}

fn set_focus(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    if dialog::owns(h) {
        dialog::with(h, |d, k| d.focus = k);
        return 0;
    }
    let mut s = st();
    std::mem::replace(&mut s.focus, arg(c, 0))
}

fn find_window(_: &mut Cpu) -> u32 {
    0 // no other copy of the game is running
}

fn enable_window(c: &mut Cpu) -> u32 {
    let (h, on) = (arg(c, 0), arg(c, 1) != 0);
    // returns whether it was disabled before
    dialog::with(h, |d, k| k.map_or(0, |k| !std::mem::replace(&mut d.controls[k].enabled, on) as u32)).unwrap_or(0)
}

fn wsprintf(c: &mut Cpu) -> u32 {
    let (buf, fmt) = (arg(c, 0), cstr(arg(c, 1)));
    let s = super::printf(&fmt, |k| arg(c, 2 + k));
    put_cstr(buf, &s, s.len() as u32 + 1)
}

fn ret0(_: &mut Cpu) -> u32 {
    0
}
fn ret1(_: &mut Cpu) -> u32 {
    1
}
fn invalidate_rect(c: &mut Cpu) -> u32 {
    invalidate(arg(c, 0));
    1
}
fn validate_rect(c: &mut Cpu) -> u32 {
    validate(arg(c, 0));
    1
}
/// UpdateWindow: WM_PAINT at once if the window needs painting.
fn update_window(c: &mut Cpu) -> u32 {
    let h = arg(c, 0);
    if st().wins.get(&h).is_some_and(|w| w.dirty) {
        send(c, h, WM_PAINT, 0, 0);
    }
    1
}
fn first_arg(c: &mut Cpu) -> u32 {
    arg(c, 0)
}
const WH_KEYBOARD: u32 = 2;

fn set_hook(c: &mut Cpu) -> u32 {
    let (kind, proc_) = (arg(c, 0), arg(c, 1));
    if kind != WH_KEYBOARD {
        log!("warn: SetWindowsHookExA({kind}) not supported");
    }
    let mut s = st();
    let id = 0x4400 + s.hooks.len() as u32;
    s.hooks.push((id, kind, proc_));
    id
}

fn unhook(c: &mut Cpu) -> u32 {
    let id = arg(c, 0);
    st().hooks.retain(|h| h.0 != id);
    1
}
fn load_resource_handle(_: &mut Cpu) -> u32 {
    0x5500
}
fn show_cursor(c: &mut Cpu) -> u32 {
    if arg(c, 0) != 0 { 1 } else { 0xffff_ffff }
}

/// EnumWindows(callback, lparam): our windows, until the callback returns FALSE.
fn enum_windows(c: &mut Cpu) -> u32 {
    let (cb, lp) = (arg(c, 0), arg(c, 1));
    let wins: Vec<u32> = st().wins.keys().copied().collect();
    for h in wins {
        if super::guest_call(c, cb, &[h, lp]) == 0 {
            break;
        }
    }
    1
}

fn copy_rect(c: &mut Cpu) -> u32 {
    let (d, s) = (arg(c, 0), arg(c, 1));
    let m = mem();
    for k in 0..4 {
        m.wr32(d + 4 * k, m.rd32(s + 4 * k));
    }
    1
}

pub const IMPORTS: &[Import] = &[
    ("EnumWindows", 2, enum_windows),
    ("CopyRect", 2, copy_rect),
    ("TranslateAcceleratorA", 3, ret0),
    ("DestroyMenu", 1, ret1),
    ("GetParent", 1, ret0),
    ("LockWindowUpdate", 1, ret1),
    ("RegisterClassA", 1, register_class),
    ("UnregisterClassA", 2, ret1),
    ("CreateWindowExA", 12, create_window_ex),
    ("DestroyWindow", 1, destroy_window),
    ("ShowWindow", 2, show_window),
    ("UpdateWindow", 1, update_window),
    ("SetWindowPos", 7, set_window_pos),
    ("GetWindowRect", 2, get_window_rect),
    ("GetClientRect", 2, get_client_rect),
    ("GetWindowLongA", 2, get_window_long),
    ("SetWindowLongA", 3, set_window_long),
    ("SetWindowTextA", 2, set_window_text),
    ("AdjustWindowRect", 3, adjust_window_rect),
    ("AdjustWindowRectEx", 4, adjust_window_rect),
    ("ClientToScreen", 2, client_to_screen),
    ("SetMenu", 2, ret1),
    ("GetMenu", 1, ret0),
    ("SetActiveWindow", 1, first_arg),
    ("SetForegroundWindow", 1, ret1),
    ("SetFocus", 1, set_focus),
    ("EnableWindow", 2, enable_window),
    ("IsIconic", 1, ret0),
    ("FindWindowA", 2, find_window),
    ("DefWindowProcA", 4, def_window_proc),
    ("PeekMessageA", 5, peek_message),
    ("GetMessageA", 4, get_message),
    ("TranslateMessage", 1, translate_message),
    ("DispatchMessageA", 1, dispatch_message),
    ("PostMessageA", 4, post_message),
    ("SendMessageA", 4, send_message),
    ("SendNotifyMessageA", 4, send_message),
    ("PostQuitMessage", 1, post_quit_message),
    ("SetTimer", 4, set_timer),
    ("KillTimer", 2, kill_timer),
    ("ValidateRect", 2, validate_rect),
    ("InvalidateRect", 3, invalidate_rect),
    ("GetDC", 1, get_dc),
    ("ReleaseDC", 2, ret1),
    ("BeginPaint", 2, begin_paint),
    ("EndPaint", 2, ret1),
    ("InvertRect", 2, invert_rect),
    ("SetRect", 5, set_rect),
    ("LoadIconA", 2, load_resource_handle),
    ("LoadCursorA", 2, load_resource_handle),
    ("SetCursor", 1, ret0),
    ("ShowCursor", 1, show_cursor),
    ("SetCursorPos", 2, ret1),
    ("GetAsyncKeyState", 1, get_async_key_state),
    ("GetKeyState", 1, get_key_state),
    ("keybd_event", 4, keybd_event),
    ("SystemParametersInfoA", 4, system_parameters_info),
    ("GetSystemMetrics", 1, get_system_metrics),
    ("SetWindowsHookExA", 4, set_hook),
    ("UnhookWindowsHookEx", 1, unhook),
    ("CallNextHookEx", 4, ret0),
    ("wsprintfA", -1, wsprintf),
];
