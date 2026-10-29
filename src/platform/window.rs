//! The game window (winit) and what is shown in it (OpenGL, `gl.rs`). Everything here lives
//! on the game's main thread: the game creates its window, pumps its messages and presents its
//! frames there.

use std::cell::RefCell;
use std::collections::VecDeque;
use super::gl::Gl;
use std::sync::{Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::Duration;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::window::{Window, WindowId};

/// Input for the game, in game terms: Windows virtual-key codes and frame pixels.
#[derive(Clone, Copy, Debug)]
pub enum Input {
    Key { vk: u8, scan: u8, down: bool, repeat: bool },
    Char(u8),
    Move(i32, i32),
    /// 0 left, 1 right, 2 middle
    Button(u8, bool),
    /// mouse wheel: lines, positive away from the user
    Wheel(i32),
    Close,
    Focus(bool),
    /// scripted only: a dword written to guest memory (address, value)
    Poke(u32, u32),
    /// scripted only: a control of the scripted pad (`dinput::CONTROLS`) and its value
    Pad(usize, f32),
}

struct App {
    want: Option<String>,
    window: Option<Window>,
    gl: Option<Gl>,
    input: VecDeque<Input>,
    /// a frame shown before the window exists
    early: Option<(Vec<u32>, u32, u32)>,
    /// the overlay changed since the last screenshot
    overlay_changed: bool,
    cursor: PhysicalPosition<f64>,
    /// an Alt key is held (Alt+Enter toggles fullscreen)
    alt: bool,
    shift: bool,
    /// times `keep_fullscreen` had to make the window fullscreen again
    refits: u32,
}

struct Host {
    event_loop: EventLoop<()>,
    app: App,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// The thread that owns the window (the game's main thread), and a frame another thread
/// presented, shown at the main thread's next pump.
static MAIN: OnceLock<ThreadId> = OnceLock::new();
static PENDING: Mutex<Option<(Vec<u32>, u32, u32)>> = Mutex::new(None);

/// A video frame to show (0RGB pixels, w x h, where: see `set_video`) or None, shown at the
/// main thread's next pump.
type VideoFrame = Option<(Vec<u32>, u32, u32, [f32; 4])>;
static VIDEO: Mutex<Option<VideoFrame>> = Mutex::new(None);

fn on_main() -> bool {
    *MAIN.get_or_init(|| std::thread::current().id()) == std::thread::current().id()
}

fn with_host<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    HOST.with(|h| {
        let mut h = h.borrow_mut();
        let host = h.get_or_insert_with(|| {
            #[allow(unused_mut)]
            let mut b = EventLoop::builder();
            #[cfg(windows)]
            winit::platform::windows::EventLoopBuilderExtWindows::with_any_thread(&mut b, true);
            #[cfg(all(unix, not(target_os = "android"), not(target_vendor = "apple")))]
            winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut b, true);
            Host {
                event_loop: b.build().expect("cannot open the window system"),
                app: App {
                    want: None,
                    window: None,
                    gl: None,
                    input: VecDeque::new(),
                    early: None,
                    overlay_changed: false,
                    cursor: PhysicalPosition::new(0.0, 0.0),
                    alt: false,
                    shift: false,
                    refits: 0,
                },
            }
        });
        f(host)
    })
}

/// Open the window (shown at the next pump).
pub fn open(title: &str) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if h.app.window.is_none() {
            h.app.want = Some(title.to_string());
        }
    });
    pump(Duration::ZERO);
}

/// Handle the window system's events, waiting up to `wait` for one.
pub fn pump(wait: Duration) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if let Some((px, w, hh)) = PENDING.lock().unwrap().take() {
            h.app.frame(&px, w, hh);
        }
        if let Some(v) = VIDEO.lock().unwrap().take() {
            if let Some(gl) = &mut h.app.gl {
                gl.video(v.as_ref().map(|(px, w, hh, r)| (&px[..], *w, *hh, *r)));
                h.app.draw();
            }
        }
        let _ = h.event_loop.pump_app_events(Some(wait), &mut h.app);
        scripted(&mut h.app.input);
        // a pad's Start is the Escape key (pause)
        if let Some(down) = super::dinput::poll() {
            let (vk, scan) = vk_of(KeyCode::Escape).unwrap();
            h.app.input.push_back(Input::Key { vk, scan, down, repeat: false });
        }
    });
}

/// HOGS_INPUT="9000:click@484,303 12000:Enter 15000:Up*3000": input at fixed times (ms
/// after start) for tests. `click@x,y` / `rclick@x,y` (frame pixels, after a move there),
/// `move@x,y`, a key held 100 ms or `*ms` (Enter, Escape, Space, Tab, Up, Down, Left, Right,
/// Ctrl, Alt, Shift, A-Z, 0-9, Numpad0-9, F1-F12), `close` (the window's close button),
/// `blur` / `focus` (the window loses or gets the focus), `poke@511fc0=1` (a dword written to
/// guest memory, hex address and value),
/// `text@Name` (typed characters, `_` a space), `pad@A*500` (a control of the scripted pad held,
/// `dinput::CONTROLS`), `pad@LX=-1*1000` (its stick at a value, -1...1, y up).
fn scripted(input: &mut VecDeque<Input>) {
    static SCRIPT: Mutex<Option<VecDeque<(u64, Input)>>> = Mutex::new(None);
    let mut s = SCRIPT.lock().unwrap();
    let script = s.get_or_insert_with(|| std::env::var("HOGS_INPUT").map(|v| parse_script(&v)).unwrap_or_default());
    let now = super::kernel32::start_time().elapsed().as_millis() as u64;
    while script.front().is_some_and(|e| e.0 <= now) {
        match script.pop_front().unwrap().1 {
            Input::Poke(a, v) => super::mem().wr32(a, v),
            Input::Pad(k, v) => super::dinput::script(k, v),
            i => input.push_back(i),
        }
    }
}

fn parse_script(text: &str) -> VecDeque<(u64, Input)> {
    let mut ev = Vec::new();
    for item in text.split_whitespace() {
        let Some((t, what)) = item.split_once(':') else { continue };
        let Ok(t) = t.parse::<u64>() else { continue };
        let at = |s: &str| -> Option<(i32, i32)> {
            let (x, y) = s.split_once(',')?;
            Some((x.parse().ok()?, y.parse().ok()?))
        };
        if let Some(text) = what.strip_prefix("text@") {
            // typed characters (WM_CHAR), 50 ms apart; `_` is a space
            for (n, ch) in text.chars().filter(|&c| (c as u32) < 256).enumerate() {
                ev.push((t + 50 * n as u64, Input::Char(if ch == '_' { b' ' } else { ch as u8 })));
            }
        } else if let Some(p) = what.strip_prefix("pad@") {
            let (name, hold) = p.split_once('*').map_or((p, 100), |(n, h)| (n, h.parse().unwrap_or(100)));
            let (name, v) = name.split_once('=').map_or((name, 1.0), |(n, v)| (n, v.parse().unwrap_or(0.0)));
            let Some(k) = super::dinput::CONTROLS.iter().position(|&c| c == name) else {
                eprintln!("HOGS_INPUT: unknown pad control {name} ({})", super::dinput::CONTROLS.join(", "));
                continue;
            };
            ev.push((t, Input::Pad(k, v)));
            ev.push((t + hold, Input::Pad(k, 0.0)));
        } else if let Some(p) = what.strip_prefix("poke@") {
            let hex = |s: &str| u32::from_str_radix(s, 16).ok();
            if let Some((a, v)) = p.split_once('=').and_then(|(a, v)| Some((hex(a)?, hex(v)?))) {
                ev.push((t, Input::Poke(a, v)));
            }
        } else if let Some((b, xy)) = what.split_once('@') {
            let Some((x, y)) = at(xy) else { continue };
            ev.push((t, Input::Move(x, y)));
            let b = match b {
                "click" => 0,
                "rclick" => 1,
                _ => continue,
            };
            ev.push((t + 30, Input::Button(b, true)));
            ev.push((t + 130, Input::Button(b, false)));
        } else if what == "close" {
            ev.push((t, Input::Close));
        } else if what == "blur" || what == "focus" {
            ev.push((t, Input::Focus(what == "focus")));
        } else {
            let (name, hold) = what.split_once('*').map_or((what, 100), |(n, h)| (n, h.parse().unwrap_or(100)));
            let Some((vk, scan)) = key_by_name(name).and_then(vk_of) else {
                eprintln!("HOGS_INPUT: unknown key {name}");
                continue;
            };
            ev.push((t, Input::Key { vk, scan, down: true, repeat: false }));
            ev.push((t + hold, Input::Key { vk, scan, down: false, repeat: false }));
        }
    }
    ev.sort_by_key(|e| e.0);
    ev.into()
}

const FN: [KeyCode; 12] = [
    KeyCode::F1, KeyCode::F2, KeyCode::F3, KeyCode::F4, KeyCode::F5, KeyCode::F6, KeyCode::F7, KeyCode::F8, KeyCode::F9, KeyCode::F10,
    KeyCode::F11, KeyCode::F12,
];

fn key_by_name(n: &str) -> Option<KeyCode> {
    use KeyCode::*;
    const LETTERS: [KeyCode; 26] = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU,
        KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const DIGITS: [KeyCode; 10] = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    const PAD: [KeyCode; 10] = [Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9];
    let b = n.as_bytes();
    if b.len() == 1 && b[0].is_ascii_alphabetic() {
        return Some(LETTERS[(b[0].to_ascii_uppercase() - b'A') as usize]);
    }
    if b.len() == 1 && b[0].is_ascii_digit() {
        return Some(DIGITS[(b[0] - b'0') as usize]);
    }
    if let Some(d) = n.strip_prefix("Numpad").and_then(|d| d.parse::<usize>().ok()) {
        return PAD.get(d).copied();
    }
    if let Some(d) = n.strip_prefix('F').and_then(|d| d.parse::<usize>().ok()) {
        return FN.get(d.wrapping_sub(1)).copied();
    }
    Some(match n {
        "Enter" | "Return" => Enter,
        "Escape" | "Esc" => Escape,
        "Space" => Space,
        "Tab" => Tab,
        "Up" => ArrowUp,
        "Down" => ArrowDown,
        "Left" => ArrowLeft,
        "Right" => ArrowRight,
        "Ctrl" => ControlLeft,
        "Alt" => AltLeft,
        "Shift" => ShiftLeft,
        _ => return None,
    })
}

/// The next input event, if any (call `pump` first).
pub fn next_input() -> Option<Input> {
    if !on_main() {
        return None;
    }
    with_host(|h| h.app.input.pop_front())
}

/// Show a frame of 0RGB pixels, scaled to the window with its aspect kept.
pub fn present(pixels: &[u32], w: u32, h: u32) {
    if !on_main() {
        *PENDING.lock().unwrap() = Some((pixels.to_vec(), w, h));
        return;
    }
    with_host(|host| host.app.frame(pixels, w, h));
}

/// Run `f` with the OpenGL context (None before the window exists or off the main thread).
pub fn with_gl<R>(f: impl FnOnce(&mut Gl) -> R) -> Option<R> {
    if !on_main() {
        return None;
    }
    with_host(|h| h.app.gl.as_mut().map(f))
}

/// Show a texture something rendered (first row at the bottom), w x h, with a gamma; `margin`
/// pixels left and right of it are past the game's screen (widescreen).
pub fn show_texture(tex: glow::Texture, w: u32, h: u32, margin: f32, gamma: f32) {
    if !on_main() {
        return;
    }
    with_host(|host| {
        if let Some(gl) = &mut host.app.gl {
            gl.shown = (tex, w, h, false, gamma);
            gl.margin = margin;
            host.app.draw();
        }
    });
}

/// HOGS_SHOT=file.png: the picture shown is written there (as the window shows it), at most
/// every half second (`HOGS_SHOT_MS`), and at every change of the overlay (for checks without looking). A
/// `{t}` in the name is replaced by the time since start in ms (as in HOGS_INPUT), so every
/// picture is kept.
fn shot_path() -> Option<&'static String> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var("HOGS_SHOT").ok()).as_ref()
}

static LAST_SHOT: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// HOGS_SHOT_MS: the time between pictures written (500 ms if not set).
fn shot_every() -> Duration {
    static MS: OnceLock<u64> = OnceLock::new();
    Duration::from_millis(*MS.get_or_init(|| std::env::var("HOGS_SHOT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500)))
}

fn shot_due(force: bool) -> bool {
    shot_path().is_some() && (force || LAST_SHOT.lock().unwrap().is_none_or(|t| t.elapsed() >= shot_every()))
}

fn write_shot(px: &[u32], w: u32, h: u32) {
    *LAST_SHOT.lock().unwrap() = Some(std::time::Instant::now());
    if let Some(path) = shot_path() {
        let t = super::kernel32::start_time().elapsed().as_millis();
        let _ = std::fs::write(path.replace("{t}", &format!("{t:06}")), png(px, w, h));
    }
}

/// An uncompressed PNG of 0RGB pixels.
pub fn png(pixels: &[u32], w: u32, h: u32) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        let c = crc(&out[start..]);
        out.extend(c.to_be_bytes());
    }
    let mut raw = Vec::with_capacity(((w * 3 + 1) * h) as usize);
    for row in pixels.chunks(w as usize).take(h as usize) {
        raw.push(0);
        for p in row {
            raw.extend([(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
        }
    }
    // zlib stream of stored deflate blocks
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(0xffff).collect();
    for (k, b) in blocks.iter().enumerate() {
        z.push((k + 1 == blocks.len()) as u8);
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(*b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend((b << 16 | a).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 13, 10, 0x1a, 10];
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Draw `pixels` (ARGB, w x h) over the game's frames, stretched to the frame's area; `None`
/// removes it.
pub fn set_overlay(overlay: Option<(Vec<u32>, u32, u32)>) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if let Some(gl) = &mut h.app.gl {
            gl.overlay(overlay.as_ref().map(|(px, w, hh)| (&px[..], *w, *hh)));
            h.app.overlay_changed = true;
            h.app.draw();
        }
    });
}

/// Show a video frame (0RGB, w x h) over the game's frames, in `rect`: x, y, width and height
/// as parts of the 4:3 picture; `None` removes it. Any thread; shown at the next pump.
pub fn set_video(frame: VideoFrame) {
    *VIDEO.lock().unwrap() = Some(frame);
}

/// Size of the last frame shown (input positions are in its pixels).
pub fn frame_size() -> (u32, u32) {
    if !on_main() {
        return (640, 480);
    }
    with_host(|h| h.app.gl.as_ref().map_or((640, 480), |g| ((g.shown.1 as f32 - 2.0 * g.margin).round() as u32, g.shown.2)))
}

/// The window's drawable size in pixels (1280x960 before it exists).
pub fn window_size() -> (u32, u32) {
    if !on_main() {
        return (1280, 960);
    }
    with_host(|h| h.app.window.as_ref().map(|w| w.inner_size()).filter(|s| s.width > 0 && s.height > 0).map_or((1280, 960), |s| (s.width, s.height)))
}

/// The display the window is on: its size and the sizes of its video modes (largest first,
/// at least 640x480).
pub fn display_modes() -> ((u32, u32), Vec<(u32, u32)>) {
    let none = ((1280, 960), Vec::new());
    if !on_main() {
        return none;
    }
    with_host(|h| {
        let Some(m) = h.app.window.as_ref().and_then(|w| w.current_monitor()) else { return none };
        let desktop = (m.size().width, m.size().height);
        let mut modes: Vec<(u32, u32)> = m.video_modes().map(|v| (v.size().width, v.size().height)).filter(|&(w, h)| w >= 640 && h >= 480).collect();
        modes.sort_unstable_by(|a, b| b.cmp(a));
        modes.dedup();
        (desktop, modes)
    })
}

/// Borderless fullscreen on the display the window is on.
fn borderless(w: &Window) -> winit::window::Fullscreen {
    winit::window::Fullscreen::Borderless(w.current_monitor())
}

/// Is the window fullscreen?
pub fn is_fullscreen() -> bool {
    on_main() && with_host(|h| h.app.window.as_ref().is_some_and(|w| w.fullscreen().is_some()))
}

/// Borderless fullscreen on the window's display, or a window again.
pub fn set_fullscreen(on: bool) {
    if !on_main() {
        return;
    }
    with_host(|h| {
        if let Some(w) = &h.app.window {
            if w.fullscreen().is_some() != on {
                w.set_fullscreen(on.then(|| borderless(w)));
            }
        }
    });
}

/// Give the window this inner size (unless it is fullscreen).
pub fn set_window_size(w: u32, h: u32) {
    if !on_main() {
        return;
    }
    with_host(|host| {
        if let Some(win) = host.app.window.as_ref().filter(|w| w.fullscreen().is_none()) {
            let _ = win.request_inner_size(winit::dpi::PhysicalSize::new(w, h));
        }
    });
}

/// The 4:3 part in the middle of a frame of w x h, where dialogs are shown: x offset and width.
pub fn overlay_span(w: u32, h: u32) -> (u32, u32) {
    let ow = w.min(h * 4 / 3);
    ((w - ow) / 2, ow)
}

/// Where a frame of w x h lands in a window of ww x wh: offset and scale.
pub fn fit(w: u32, h: u32, ww: u32, wh: u32) -> (f64, f64, f64) {
    let s = (ww as f64 / w as f64).min(wh as f64 / h as f64);
    ((ww as f64 - w as f64 * s) / 2.0, (wh as f64 - h as f64 * s) / 2.0, s)
}

impl App {
    fn frame(&mut self, px: &[u32], w: u32, h: u32) {
        match &mut self.gl {
            Some(gl) => {
                gl.frame(px, w, h);
                self.draw();
            }
            None => self.early = Some((px.to_vec(), w, h)),
        }
    }

    fn draw(&mut self) {
        let (Some(win), Some(gl)) = (&self.window, &mut self.gl) else { return };
        let size = win.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let want_shot = shot_due(self.overlay_changed);
        if let Some((px, w, h)) = gl.draw(size.width, size.height, want_shot) {
            write_shot(&px, w, h);
            self.overlay_changed = false;
        }
    }

    fn create(&mut self, el: &ActiveEventLoop) {
        let Some(title) = self.want.take() else { return };
        // HOGS_WINDOW=1920x1080: the window's starting size in pixels (tests); else the
        // resolution chosen in Preferences, else 1280x960
        let size = std::env::var("HOGS_WINDOW")
            .ok()
            .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))))
            .or_else(super::display::chosen);
        let attrs = Window::default_attributes().with_title(title);
        let attrs = match size {
            Some((w, h)) => attrs.with_inner_size(winit::dpi::PhysicalSize::new(w, h)),
            None => attrs.with_inner_size(LogicalSize::new(1280.0, 960.0)),
        };
        // HOGS_MONITOR=1: opened on that display (0 the first; tests)
        let attrs = match std::env::var("HOGS_MONITOR").ok().and_then(|v| v.parse::<usize>().ok()).and_then(|i| el.available_monitors().nth(i)) {
            Some(m) => attrs.with_position(winit::dpi::PhysicalPosition::new(m.position().x + 50, m.position().y + 50)),
            None => attrs,
        };
        let (win, gl) = Gl::new(el, attrs);
        // fullscreen once the window is where Windows put it (made fullscreen right away, it
        // got the main display's size and could still be moved to another one)
        if super::display::fullscreen() {
            win.set_fullscreen(Some(borderless(&win)));
        }
        // to the front, while our start window still has the foreground (Windows lets only the
        // foreground's process bring a window forward)
        win.focus_window();
        self.window = Some(win);
        self.gl = Some(gl);
        if let Some((px, w, h)) = self.early.take() {
            self.frame(&px, w, h);
        }
    }

    /// Fullscreen but not filling the display: Windows gave the window its first size and place
    /// after it went fullscreen (the start position on another display, from the terminal that
    /// started us). Fullscreen again, from a window (winit leaves the same fullscreen as it is).
    fn keep_fullscreen(&mut self) {
        let Some(w) = &self.window else { return };
        let Some(m) = w.current_monitor() else { return };
        let (pos, size) = (w.outer_position().ok(), w.inner_size());
        if w.fullscreen().is_some() && (size != m.size() || pos != Some(m.position())) && self.refits < 8 {
            self.refits += 1;
            w.set_fullscreen(None);
            w.set_fullscreen(Some(borderless(w)));
        }
    }

    /// The cursor in frame pixels.
    fn frame_pos(&self) -> (i32, i32) {
        let Some(win) = &self.window else { return (0, 0) };
        let size = win.inner_size();
        let (w, h, m) = self.gl.as_ref().map_or((640, 480, 0.0), |g| (g.shown.1, g.shown.2, g.margin as f64));
        let (ox, oy, s) = fit(w, h, size.width.max(1), size.height.max(1));
        let x = ((self.cursor.x - ox) / s - m).clamp(0.0, (w as f64 - 2.0 * m).round() - 1.0);
        let y = ((self.cursor.y - oy) / s).clamp(0.0, h as f64 - 1.0);
        (x as i32, y as i32)
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        self.create(el);
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.create(el);
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, e: WindowEvent) {
        match e {
            WindowEvent::CloseRequested => self.input.push_back(Input::Close),
            WindowEvent::Focused(f) => self.input.push_back(Input::Focus(f)),
            WindowEvent::Resized(size) => {
                self.keep_fullscreen();
                if let Some(gl) = &self.gl {
                    gl.resize(size.width, size.height);
                }
                self.draw();
            }
            WindowEvent::RedrawRequested => self.draw(),
            // moved to another display while fullscreen: fill that one
            WindowEvent::Moved(_) => self.keep_fullscreen(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                let (x, y) = self.frame_pos();
                self.input.push_back(Input::Move(x, y));
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y.round() as i32,
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.y / 40.0).round() as i32,
                };
                if lines != 0 {
                    self.input.push_back(Input::Wheel(lines));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let b = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    _ => return,
                };
                self.input.push_back(Input::Button(b, state == ElementState::Pressed));
            }
            WindowEvent::ModifiersChanged(m) => {
                self.alt = m.state().alt_key();
                self.shift = m.state().shift_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let down = event.state == ElementState::Pressed;
                if self.alt && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Enter | KeyCode::NumpadEnter)) {
                    // Alt+Enter: borderless fullscreen on the window's display, or back
                    if down && !event.repeat {
                        if let Some(w) = &self.window {
                            let full = w.fullscreen().is_some();
                            w.set_fullscreen((!full).then(|| borderless(w)));
                            super::display::remember_fullscreen(!full);
                        }
                    }
                    return;
                }
                if let PhysicalKey::Code(code) = event.physical_key {
                    if let Some((vk, scan)) = vk_of(code) {
                        self.input.push_back(Input::Key { vk, scan, down, repeat: event.repeat });
                    }
                }
                if down {
                    if let Some(t) = &event.text {
                        for ch in t.chars().filter(|&c| (c as u32) < 256) {
                            self.input.push_back(Input::Char(ch as u8));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Windows virtual-key code and set-1 scan code for a key.
fn vk_of(k: KeyCode) -> Option<(u8, u8)> {
    use KeyCode::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU,
        KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    const LETTER_SCAN: [u8; 26] =
        [0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c];
    if let Some(i) = letters.iter().position(|&l| l == k) {
        return Some((b'A' + i as u8, LETTER_SCAN[i]));
    }
    let digits = [Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9];
    if let Some(i) = digits.iter().position(|&d| d == k) {
        return Some((b'0' + i as u8, if i == 0 { 0x0b } else { 1 + i as u8 }));
    }
    let pad = [Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7, Numpad8, Numpad9];
    const PAD_SCAN: [u8; 10] = [0x52, 0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49];
    if let Some(i) = pad.iter().position(|&d| d == k) {
        return Some((0x60 + i as u8, PAD_SCAN[i]));
    }
    let f = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
    if let Some(i) = f.iter().position(|&d| d == k) {
        return Some((0x70 + i as u8, if i < 10 { 0x3b + i as u8 } else { 0x57 + (i as u8 - 10) }));
    }
    Some(match k {
        Escape => (0x1b, 0x01),
        Enter => (0x0d, 0x1c),
        NumpadEnter => (0x0d, 0x1c),
        Space => (0x20, 0x39),
        Tab => (0x09, 0x0f),
        Backspace => (0x08, 0x0e),
        ShiftLeft => (0x10, 0x2a),
        ShiftRight => (0x10, 0x36),
        ControlLeft | ControlRight => (0x11, 0x1d),
        AltLeft | AltRight => (0x12, 0x38),
        ArrowLeft => (0x25, 0x4b),
        ArrowUp => (0x26, 0x48),
        ArrowRight => (0x27, 0x4d),
        ArrowDown => (0x28, 0x50),
        Insert => (0x2d, 0x52),
        Delete => (0x2e, 0x53),
        Home => (0x24, 0x47),
        End => (0x23, 0x4f),
        PageUp => (0x21, 0x49),
        PageDown => (0x22, 0x51),
        NumpadAdd => (0x6b, 0x4e),
        NumpadSubtract => (0x6d, 0x4a),
        NumpadMultiply => (0x6a, 0x37),
        NumpadDivide => (0x6f, 0x35),
        NumpadDecimal => (0x6e, 0x53),
        Minus => (0xbd, 0x0c),
        Equal => (0xbb, 0x0d),
        Comma => (0xbc, 0x33),
        Period => (0xbe, 0x34),
        Slash => (0xbf, 0x35),
        Semicolon => (0xba, 0x27),
        Quote => (0xde, 0x28),
        BracketLeft => (0xdb, 0x1a),
        BracketRight => (0xdd, 0x1b),
        Backslash => (0xdc, 0x2b),
        Backquote => (0xc0, 0x29),
        Pause => (0x13, 0x45),
        _ => return None,
    })
}
