//! DirectInput (5 and 7: Hogs of War creates it with DirectInputCreateEx) for the keyboard,
//! the mouse and joysticks, the joysticks on gamepads through gilrs.
//!
//! Keyboard and mouse are read by their state (GetDeviceState) from the window's input
//! (`user32`): the keyboard's 256 keys by DIK code, the mouse's movement since the last read
//! and its buttons. Buffered data (GetDeviceData) is not offered yet.
//!
//! Joysticks (as written for Future Cop, not yet tried with Hogs of War): attached gamepads,
//! at most two, c_dfDIJoystick (80-byte DIJOYSTATE) with lX, lY and buttons 0...31; range and
//! dead zone as the game sets them. Our button order: 0 RT, 1 LT, 2 B, 3 A, 4 X, 5 Y, 6 LB,
//! 7 RB, 8 Back, 9 R3, 10 L3, 11...14 D-pad up, down, left, right.
//!
//! gilrs lives on the main thread and is polled at every `window::pump`; the game's threads
//! read the last state. `HOGS_PAD_LOG=1` logs pads and the game's use of them. `pad@...` items
//! in HOGS_INPUT drive a scripted pad (pad 0, before the real ones).

use super::{Import, alloc, arg, free, guest_call, log, mem, put_cstr};
use crate::rt::Cpu;
use gilrs::{Axis, Button, Gilrs};
use std::cell::RefCell;
use std::sync::Mutex;

const DI_OK: u32 = 0;
const E_NOTIMPL: u32 = 0x8000_4001;
const E_NOINTERFACE: u32 = 0x8000_4002;
const E_INVALIDARG: u32 = 0x8007_0057;
const DIERR_DEVICENOTREG: u32 = 0x8004_0154;

const DIDEVTYPE_JOYSTICK: u32 = 4;
const DIDEVTYPEJOYSTICK_GAMEPAD: u32 = 3;
const DIDC_ATTACHED: u32 = 1;
const DIPROP_RANGE: u32 = 4;
const DIPROP_DEADZONE: u32 = 5;
const DIPH_DEVICE: u32 = 0;
const DIPH_BYOFFSET: u32 = 1;

/// A GUID as its 16 bytes in memory.
const fn guid(d1: u32, d2: u16, d3: u16, d4: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (d1.to_le_bytes(), d2.to_le_bytes(), d3.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]]
}

const DI_D4: [u8; 8] = [0xbf, 0xc7, 0x44, 0x45, 0x53, 0x54, 0x00, 0x00];
const IID_IUNKNOWN: [u8; 16] = guid(0, 0, 0, [0xc0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_IDIRECTINPUTA: [u8; 16] = guid(0x8952_1360, 0xaa8a, 0x11cf, DI_D4);
const IID_IDIRECTINPUTDEVICEA: [u8; 16] = guid(0x5944_e680, 0xc92e, 0x11cf, DI_D4);
const IID_IDIRECTINPUTDEVICE2A: [u8; 16] = guid(0x5944_e682, 0xc92e, 0x11cf, DI_D4);
const IID_IDIRECTINPUT7A: [u8; 16] = guid(0x9a4c_b684, 0x236d, 0x11d3, [0x8e, 0x9d, 0x00, 0xc0, 0x4f, 0x68, 0x44, 0xae]);
const IID_IDIRECTINPUTDEVICE7A: [u8; 16] = guid(0x57d7_c6bc, 0x2356, 0x11d3, [0x8e, 0x9d, 0x00, 0xc0, 0x4f, 0x68, 0x44, 0xae]);
const GUID_SYSMOUSE: [u8; 16] = guid(0x6f1d_2b60, 0xd5a0, 0x11cf, DI_D4);
const GUID_SYSKEYBOARD: [u8; 16] = guid(0x6f1d_2b61, 0xd5a0, 0x11cf, DI_D4);
/// the device number (+8) of the keyboard and the mouse; joysticks are 0 and 1
const KEYBOARD: u32 = 0x100;
const MOUSE: u32 = 0x101;
/// Pad n's instance GUID has 0xfc0b0000 + n in its first dword; the product GUID is the base.
const PAD_GUID: u32 = 0xfc0b_0000;

fn pad_guid(n: u32) -> [u8; 16] {
    guid(PAD_GUID + n, 0xd5a0, 0x11cf, DI_D4)
}

fn trace() -> bool {
    static T: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *T.get_or_init(|| std::env::var("HOGS_PAD_LOG").is_ok_and(|v| v != "0"))
}

macro_rules! pad_log {
    ($($t:tt)*) => { if trace() { log!("pad: {}", format!($($t)*)) } };
}

// ---- pad state --------------------------------------------------------------------------------

/// One pad as the game sees it: left stick (-1...1, y down), our DirectInput buttons (bit n =
/// button n), Start.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct Pad {
    stick: [f32; 2],
    buttons: u32,
    start: bool,
}

/// A connected pad: its state, the buttons pressed since the game last read it (a short press
/// between two reads still counts: the Controller dialog reads every 250 ms), gilrs' id (None:
/// the scripted pad), its name.
struct Entry {
    pad: Pad,
    pressed: u32,
    id: Option<usize>,
    name: String,
}

/// The connected pads (the scripted one first).
static PADS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
/// Start's new state on the first pad, for the next pump (it becomes the Escape key).
static START: Mutex<Option<bool>> = Mutex::new(None);
/// The scripted pad, once HOGS_INPUT has a `pad@` item.
static SCRIPTED: Mutex<Option<Pad>> = Mutex::new(None);

fn scripted_pad() -> bool {
    static S: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("HOGS_INPUT").is_ok_and(|v| v.contains("pad@")))
}

thread_local! {
    /// gilrs on the main thread; None when it could not start (then no pads)
    /// (never dropped: its thread must not end at exit)
    static GILRS: RefCell<Option<Option<&'static mut Gilrs>>> = const { RefCell::new(None) };
}

const BUTTONS: [Button; 13] = [
    Button::East,
    Button::South,
    Button::West,
    Button::North,
    Button::LeftTrigger,
    Button::RightTrigger,
    Button::Select,
    Button::RightThumb,
    Button::LeftThumb,
    Button::DPadUp,
    Button::DPadDown,
    Button::DPadLeft,
    Button::DPadRight,
];
/// our button numbers for `BUTTONS`
const BUTTON_NUMBERS: [u32; 13] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14];

/// Our button for a gilrs event that presses one.
fn pressed_by(e: &gilrs::EventType) -> Option<u32> {
    match *e {
        gilrs::EventType::ButtonChanged(Button::RightTrigger2, v, _) if v > 0.3 => Some(0),
        gilrs::EventType::ButtonChanged(Button::LeftTrigger2, v, _) if v > 0.3 => Some(1),
        gilrs::EventType::ButtonPressed(b, _) => BUTTONS.iter().position(|&x| x == b).map(|k| BUTTON_NUMBERS[k]),
        _ => None,
    }
}

fn read(g: &gilrs::Gamepad) -> Pad {
    let trigger = |b| g.button_data(b).map_or(0.0, |d| d.value()) > 0.3;
    let mut buttons = trigger(Button::RightTrigger2) as u32 | (trigger(Button::LeftTrigger2) as u32) << 1;
    for (b, n) in BUTTONS.into_iter().zip(BUTTON_NUMBERS) {
        buttons |= (g.is_pressed(b) as u32) << n;
    }
    Pad {
        stick: [g.value(Axis::LeftStickX), -g.value(Axis::LeftStickY)],
        buttons,
        start: g.is_pressed(Button::Start),
    }
}

fn new_gilrs() -> Option<&'static mut Gilrs> {
    let g = Gilrs::new().map_err(|e| log!("warn: no gamepads: {e}")).ok()?;
    Some(Box::leak(Box::new(g)))
}

/// Start gilrs on the main thread before the game: pads are found some time after (Windows
/// reports them ~75 ms later), and the game looks for them once, at its start.
pub fn start() {
    GILRS.with(|g| {
        g.borrow_mut().get_or_insert_with(new_gilrs);
    });
}

/// Read the pads (main thread, at every pump). Returns Start's new state when it changed on the
/// first pad (it becomes the Escape key).
pub fn poll() -> Option<bool> {
    update();
    START.lock().unwrap().take()
}

fn update() {
    let scripted = {
        let mut s = SCRIPTED.lock().unwrap();
        if scripted_pad() {
            s.get_or_insert_default();
        }
        *s
    };
    let mut pads = PADS.lock().unwrap();
    let pressed_before = |id| pads.iter().find(|e| e.id == id).map_or(0, |e| e.pressed);
    let mut now: Vec<Entry> = scripted
        .map(|pad| Entry { pad, pressed: pressed_before(None) | pad.buttons, id: None, name: "Scripted pad".to_string() })
        .into_iter()
        .collect();
    GILRS.with(|g| {
        let mut g = g.borrow_mut();
        let Some(g) = g.get_or_insert_with(new_gilrs) else { return };
        let mut pressed: Vec<(usize, u32)> = Vec::new();
        while let Some(e) = g.next_event() {
            match e.event {
                gilrs::EventType::Connected => pad_log!("connected: {}", g.gamepad(e.id).name()),
                gilrs::EventType::Disconnected => pad_log!("disconnected: {}", g.gamepad(e.id).name()),
                ref ev => {
                    if let Some(b) = pressed_by(ev) {
                        pressed.push((e.id.into(), 1 << b));
                    }
                }
            }
        }
        now.extend(g.gamepads().map(|(id, p)| {
            let id: usize = id.into();
            let pad = read(&p);
            let new = pressed.iter().filter(|e| e.0 == id).fold(0, |a, e| a | e.1);
            Entry { pad, pressed: pressed_before(Some(id)) | new | pad.buttons, id: Some(id), name: p.name().to_string() }
        }));
    });
    let was = pads.first().is_some_and(|e| e.pad.start);
    let start = now.first().is_some_and(|e| e.pad.start);
    *pads = now;
    if was != start {
        *START.lock().unwrap() = Some(start);
    }
}

/// Pad n as the game reads it: its buttons held or pressed since the last read.
fn pad(n: u32) -> Pad {
    // fresh on the main thread (the Controller dialog reads from there)
    if GILRS.with(|g| g.borrow().is_some()) {
        update();
    }
    let mut pads = PADS.lock().unwrap();
    let Some(e) = pads.get_mut(n as usize) else { return Pad::default() };
    let mut p = e.pad;
    p.buttons |= std::mem::take(&mut e.pressed);
    p
}

/// The scripted pad's controls (HOGS_INPUT `pad@A*500`, `pad@LX=-1*1000`): our buttons in
/// order (the D-pad's too), then Start and the stick's axes (y up, as gilrs has it).
pub const CONTROLS: [&str; 18] =
    ["RT", "LT", "B", "A", "X", "Y", "LB", "RB", "Back", "R3", "L3", "Up", "Down", "Left", "Right", "Start", "LX", "LY"];

/// Set one control of the scripted pad (`CONTROLS[k]`): held when `v` is not 0, or the axis' value.
pub fn script(k: usize, v: f32) {
    let mut s = SCRIPTED.lock().unwrap();
    let p = s.get_or_insert_default();
    let on = v != 0.0;
    match k {
        0..=14 => p.buttons = p.buttons & !(1 << k) | (on as u32) << k,
        15 => p.start = on,
        16 => p.stick[0] = v,
        _ => p.stick[1] = -v,
    }
}

/// Make sure the pads were read once (EnumDevices may come before the first pump).
fn ensure_polled() {
    super::window::pump(std::time::Duration::ZERO);
}

// ---- IDirectInputA ----------------------------------------------------------------------------
// object: vtable, references

fn guid_at(p: u32) -> [u8; 16] {
    mem().slice(p, 16).try_into().unwrap()
}

fn direct_input_create(c: &mut Cpu) -> u32 {
    let (version, out) = (arg(c, 1), arg(c, 2));
    let o = super::com_new(DIRECT_INPUT, 4);
    mem().wr32(o + 4, 1);
    mem().wr32(out, o);
    pad_log!("DirectInputCreateA(version {version:#x})");
    DI_OK
}

/// DirectInputCreateEx(instance, version, IID_IDirectInput7A, out, outer)
fn direct_input_create_ex(c: &mut Cpu) -> u32 {
    let (version, out) = (arg(c, 1), arg(c, 3));
    let o = super::com_new(DIRECT_INPUT7, 4);
    mem().wr32(o + 4, 1);
    mem().wr32(out, o);
    pad_log!("DirectInputCreateEx(version {version:#x})");
    DI_OK
}

fn di_query(c: &mut Cpu) -> u32 {
    let (this, iid, out) = (arg(c, 0), guid_at(arg(c, 1)), arg(c, 2));
    if iid != IID_IUNKNOWN && iid != IID_IDIRECTINPUTA && iid != IID_IDIRECTINPUT7A {
        mem().wr32(out, 0);
        return E_NOINTERFACE;
    }
    add_ref(c);
    mem().wr32(out, this);
    DI_OK
}

/// AddRef and Release of both interfaces: the count is at +4.
fn add_ref(c: &mut Cpu) -> u32 {
    let o = arg(c, 0);
    let n = mem().rd32(o + 4) + 1;
    mem().wr32(o + 4, n);
    n
}

fn release(c: &mut Cpu) -> u32 {
    let o = arg(c, 0);
    let n = mem().rd32(o + 4).saturating_sub(1);
    mem().wr32(o + 4, n);
    if n == 0 {
        free(o);
    }
    n
}

fn create_device(c: &mut Cpu) -> u32 {
    let (g, out) = (guid_at(arg(c, 1)), arg(c, 2));
    new_device(g, out)
}

/// CreateDeviceEx(guid, iid, out, outer) of IDirectInput7A
fn create_device_ex(c: &mut Cpu) -> u32 {
    let (g, out) = (guid_at(arg(c, 1)), arg(c, 3));
    new_device(g, out)
}

fn new_device(g: [u8; 16], out: u32) -> u32 {
    let km = if g == GUID_SYSKEYBOARD {
        Some(KEYBOARD)
    } else if g == GUID_SYSMOUSE {
        Some(MOUSE)
    } else {
        None
    };
    if let Some(n) = km {
        let d = super::com_new(DEVICE, DEVICE_SIZE);
        mem().wr32(d + 4, 1);
        mem().wr32(d + 8, n);
        mem().wr32(out, d);
        return DI_OK;
    }
    let n = u32::from_le_bytes(g[..4].try_into().unwrap()).wrapping_sub(PAD_GUID);
    if n >= 2 || g != pad_guid(n) {
        mem().wr32(out, 0);
        return DIERR_DEVICENOTREG;
    }
    let d = super::com_new(DEVICE, DEVICE_SIZE);
    let m = mem();
    m.wr32(d + 4, 1);
    m.wr32(d + 8, n);
    for axis in 0..2 {
        m.wr32(d + RANGE + 8 * axis, 0);
        m.wr32(d + RANGE + 8 * axis + 4, 0xffff);
    }
    m.wr32(out, d);
    pad_log!("CreateDevice: pad {n}");
    DI_OK
}

/// EnumDevices(type, callback, ref, flags): the pads (at most two), DIDEVICEINSTANCEA (DX5).
fn enum_devices(c: &mut Cpu) -> u32 {
    let (kind, cb, user) = (arg(c, 1) & 0xff, arg(c, 2), arg(c, 3));
    if kind != 0 && kind != DIDEVTYPE_JOYSTICK {
        return DI_OK;
    }
    ensure_polled();
    let names: Vec<String> = PADS.lock().unwrap().iter().take(2).map(|e| e.name.clone()).collect();
    let ddi = alloc(0x244);
    let m = mem();
    for (n, name) in names.iter().enumerate() {
        pad_log!("EnumDevices: pad {n}: {name}");
        m.slice(ddi, 0x244).fill(0);
        m.wr32(ddi, 0x244);
        m.slice(ddi + 4, 16).copy_from_slice(&pad_guid(n as u32));
        m.slice(ddi + 0x14, 16).copy_from_slice(&pad_guid(0));
        m.wr32(ddi + 0x24, DIDEVTYPE_JOYSTICK | DIDEVTYPEJOYSTICK_GAMEPAD << 8);
        put_cstr(ddi + 0x28, name, 260);
        put_cstr(ddi + 0x12c, name, 260);
        if guest_call(c, cb, &[ddi, user]) == 0 {
            break;
        }
    }
    free(ddi);
    DI_OK
}

fn ok(_: &mut Cpu) -> u32 {
    DI_OK
}

fn not_impl(_: &mut Cpu) -> u32 {
    E_NOTIMPL
}

static DIRECT_INPUT7: &[Import] = &[
    ("IDirectInput7A::QueryInterface", 3, di_query),
    ("IDirectInput7A::AddRef", 1, add_ref),
    ("IDirectInput7A::Release", 1, release),
    ("IDirectInput7A::CreateDevice", 4, create_device),
    ("IDirectInput7A::EnumDevices", 5, enum_devices),
    ("IDirectInput7A::GetDeviceStatus", 2, ok),
    ("IDirectInput7A::RunControlPanel", 3, ok),
    ("IDirectInput7A::Initialize", 3, ok),
    ("IDirectInput7A::FindDevice", 4, not_impl),
    ("IDirectInput7A::CreateDeviceEx", 5, create_device_ex),
];

static DIRECT_INPUT: &[Import] = &[
    ("IDirectInputA::QueryInterface", 3, di_query),
    ("IDirectInputA::AddRef", 1, add_ref),
    ("IDirectInputA::Release", 1, release),
    ("IDirectInputA::CreateDevice", 4, create_device),
    ("IDirectInputA::EnumDevices", 5, enum_devices),
    ("IDirectInputA::GetDeviceStatus", 2, ok),
    ("IDirectInputA::RunControlPanel", 3, ok),
    ("IDirectInputA::Initialize", 3, ok),
];

// ---- IDirectInputDevice2A ---------------------------------------------------------------------
// object: vtable, references, pad number, X and Y: range (min, max), dead zone (0...10000)

const RANGE: u32 = 0xc;
const DEAD_ZONE: u32 = 0x1c;
const DEVICE_SIZE: u32 = 0x20;

fn dev_query(c: &mut Cpu) -> u32 {
    let (this, iid, out) = (arg(c, 0), guid_at(arg(c, 1)), arg(c, 2));
    if ![IID_IUNKNOWN, IID_IDIRECTINPUTDEVICEA, IID_IDIRECTINPUTDEVICE2A, IID_IDIRECTINPUTDEVICE7A].contains(&iid) {
        mem().wr32(out, 0);
        return E_NOINTERFACE;
    }
    add_ref(c);
    mem().wr32(out, this);
    DI_OK
}

/// GetDeviceData(object size, data, count, flags): no buffered data yet.
fn get_device_data(c: &mut Cpu) -> u32 {
    if arg(c, 3) != 0 {
        mem().wr32(arg(c, 3), 0);
    }
    DI_OK
}

/// GetCapabilities(DIDEVCAPS): attached, no force feedback, 2 axes, 15 buttons, no POV.
fn get_capabilities(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let m = mem();
    let size = m.rd32(p);
    if size < 24 {
        return E_INVALIDARG;
    }
    m.slice(p + 4, size as usize - 4).fill(0);
    for (k, v) in [DIDC_ATTACHED, DIDEVTYPE_JOYSTICK | DIDEVTYPEJOYSTICK_GAMEPAD << 8, 2, 15, 0].into_iter().enumerate() {
        m.wr32(p + 4 + 4 * k as u32, v);
    }
    DI_OK
}

/// The axes (0 X, 1 Y) a property header (dwObj +8, dwHow +0xc) names.
fn axes_of(h: u32) -> std::ops::Range<u32> {
    match (mem().rd32(h + 0xc), mem().rd32(h + 8)) {
        (DIPH_DEVICE, _) => 0..2,
        (DIPH_BYOFFSET, o @ (0 | 4)) => o / 4..o / 4 + 1,
        _ => 0..0,
    }
}

/// SetProperty(property, header): the range and the dead zone of X and Y; others are accepted.
fn set_property(c: &mut Cpu) -> u32 {
    let (d, prop, h) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let m = mem();
    for axis in axes_of(h) {
        match prop {
            DIPROP_RANGE => {
                m.wr32(d + RANGE + 8 * axis, m.rd32(h + 0x10));
                m.wr32(d + RANGE + 8 * axis + 4, m.rd32(h + 0x14));
            }
            DIPROP_DEADZONE => m.wr32(d + DEAD_ZONE + 4 * axis, m.rd32(h + 0x10)),
            _ => {}
        }
    }
    pad_log!("SetProperty({prop}) of pad {}", m.rd32(d + 8));
    DI_OK
}

fn get_property(c: &mut Cpu) -> u32 {
    let (d, prop, h) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let m = mem();
    let axis = axes_of(h).start.min(1);
    match prop {
        DIPROP_RANGE => {
            m.wr32(h + 0x10, m.rd32(d + RANGE + 8 * axis));
            m.wr32(h + 0x14, m.rd32(d + RANGE + 8 * axis + 4));
        }
        DIPROP_DEADZONE => m.wr32(h + 0x10, m.rd32(d + DEAD_ZONE + 4 * axis)),
        _ => return E_NOTIMPL,
    }
    DI_OK
}

fn acquire(c: &mut Cpu) -> u32 {
    pad_log!("Acquire: pad {}", mem().rd32(arg(c, 0) + 8));
    DI_OK
}

/// An axis value in the device's range: the stick outside its dead zone, else exactly the
/// centre (the game tests the sign).
fn axis_value(d: u32, axis: u32, p: &Pad) -> u32 {
    let m = mem();
    let (lo, hi) = (m.rd32(d + RANGE + 8 * axis) as i32 as f32, m.rd32(d + RANGE + 8 * axis + 4) as i32 as f32);
    let dz = m.rd32(d + DEAD_ZONE + 4 * axis).min(9999) as f32 / 10000.0;
    let s = p.stick[axis as usize].clamp(-1.0, 1.0);
    let v = if s.abs() > dz { s.signum() * (s.abs() - dz) / (1.0 - dz) } else { 0.0 };
    let centre = (lo + hi) / 2.0;
    if v == 0.0 {
        return centre.floor() as i32 as u32;
    }
    let x = centre + v * (hi - lo) / 2.0;
    // away from the centre, so a small push never reads as the centre
    (if v > 0.0 { x.ceil() } else { x.floor() }).clamp(lo, hi) as i32 as u32
}

/// The DIK code of a virtual key (0: none).
fn dik(vk: u8) -> u8 {
    const LETTERS: [u8; 26] =
        [0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18, 0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c];
    const PAD: [u8; 10] = [0x52, 0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49];
    match vk {
        b'A'..=b'Z' => LETTERS[(vk - b'A') as usize],
        b'0' => 0x0b,
        b'1'..=b'9' => 2 + vk - b'1',
        0x60..=0x69 => PAD[(vk - 0x60) as usize],
        0x70..=0x79 => 0x3b + vk - 0x70,
        0x7a | 0x7b => 0x57 + vk - 0x7a,
        0x1b => 0x01,
        0x0d => 0x1c,
        0x20 => 0x39,
        0x09 => 0x0f,
        0x08 => 0x0e,
        // Shift, Ctrl and Alt by their halves: left, right
        0xa0 => 0x2a,
        0xa1 => 0x36,
        0xa2 => 0x1d,
        0xa3 => 0x9d,
        0xa4 => 0x38,
        0xa5 => 0xb8,
        0x25 => 0xcb,
        0x26 => 0xc8,
        0x27 => 0xcd,
        0x28 => 0xd0,
        0x2d => 0xd2,
        0x2e => 0xd3,
        0x24 => 0xc7,
        0x23 => 0xcf,
        0x21 => 0xc9,
        0x22 => 0xd1,
        _ => 0,
    }
}

/// The keyboard's state: 256 bytes by DIK code, 0x80 when down.
fn keyboard_state(out: u32, size: u32) -> u32 {
    let mut s = [0u8; 256];
    for vk in 1..=255u8 {
        let k = dik(vk);
        if k != 0 && super::user32::key_down(vk) {
            s[k as usize] = 0x80;
        }
    }
    let n = (size as usize).min(256);
    mem().slice(out, n).copy_from_slice(&s[..n]);
    DI_OK
}

/// The mouse's state (DIMOUSESTATE or DIMOUSESTATE2): movement since the last read, buttons.
fn mouse_state(out: u32, size: u32) -> u32 {
    static LAST: Mutex<Option<(i32, i32)>> = Mutex::new(None);
    let (x, y) = super::user32::mouse_pos();
    let (lx, ly) = LAST.lock().unwrap().replace((x, y)).unwrap_or((x, y));
    let m = mem();
    m.slice(out, size as usize).fill(0);
    m.wr32(out, (x - lx) as u32);
    m.wr32(out + 4, (y - ly) as u32);
    for (k, vk) in [1u8, 2, 4].into_iter().enumerate() {
        if 12 + k < size as usize && super::user32::key_down(vk) {
            m.wr8(out + 12 + k as u32, 0x80);
        }
    }
    DI_OK
}

/// GetDeviceState(size, out): the keyboard, the mouse, or a joystick's DIJOYSTATE (lX, lY, the
/// other axes centred, POVs centred, buttons).
fn get_device_state(c: &mut Cpu) -> u32 {
    let (d, size, out) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let m = mem();
    match m.rd32(d + 8) {
        KEYBOARD => return keyboard_state(out, size),
        MOUSE => return mouse_state(out, size),
        _ => {}
    }
    let p = pad(m.rd32(d + 8));
    let mut s = [0u8; 0x50];
    s[0..4].copy_from_slice(&axis_value(d, 0, &p).to_le_bytes());
    s[4..8].copy_from_slice(&axis_value(d, 1, &p).to_le_bytes());
    s[0x20..0x30].fill(0xff);
    for b in 0..32 {
        s[0x30 + b] = if p.buttons >> b & 1 != 0 { 0x80 } else { 0 };
    }
    let n = (size as usize).min(s.len());
    m.slice(out, n).copy_from_slice(&s[..n]);
    DI_OK
}

static DEVICE: &[Import] = &[
    ("IDirectInputDevice2A::QueryInterface", 3, dev_query),
    ("IDirectInputDevice2A::AddRef", 1, add_ref),
    ("IDirectInputDevice2A::Release", 1, release),
    ("IDirectInputDevice2A::GetCapabilities", 2, get_capabilities),
    ("IDirectInputDevice2A::EnumObjects", 4, ok),
    ("IDirectInputDevice2A::GetProperty", 3, get_property),
    ("IDirectInputDevice2A::SetProperty", 3, set_property),
    ("IDirectInputDevice2A::Acquire", 1, acquire),
    ("IDirectInputDevice2A::Unacquire", 1, ok),
    ("IDirectInputDevice2A::GetDeviceState", 3, get_device_state),
    ("IDirectInputDevice2A::GetDeviceData", 5, get_device_data),
    ("IDirectInputDevice2A::SetDataFormat", 2, ok),
    ("IDirectInputDevice2A::SetEventNotification", 2, ok),
    ("IDirectInputDevice2A::SetCooperativeLevel", 3, ok),
    ("IDirectInputDevice2A::GetObjectInfo", 4, not_impl),
    ("IDirectInputDevice2A::GetDeviceInfo", 2, not_impl),
    ("IDirectInputDevice2A::RunControlPanel", 3, ok),
    ("IDirectInputDevice2A::Initialize", 4, ok),
    ("IDirectInputDevice2A::CreateEffect", 5, not_impl),
    ("IDirectInputDevice2A::EnumEffects", 4, ok),
    ("IDirectInputDevice2A::GetEffectInfo", 3, not_impl),
    ("IDirectInputDevice2A::GetForceFeedbackState", 2, not_impl),
    ("IDirectInputDevice2A::SendForceFeedbackCommand", 2, not_impl),
    ("IDirectInputDevice2A::EnumCreatedEffectObjects", 4, ok),
    ("IDirectInputDevice2A::Escape", 2, not_impl),
    ("IDirectInputDevice2A::Poll", 1, ok),
    ("IDirectInputDevice2A::SendDeviceData", 5, not_impl),
    ("IDirectInputDevice7A::EnumEffectsInFile", 5, not_impl),
    ("IDirectInputDevice7A::WriteEffectToFile", 5, not_impl),
];

pub const IMPORTS: &[Import] = &[("DirectInputCreateA", 4, direct_input_create), ("DirectInputCreateEx", 5, direct_input_create_ex)];
