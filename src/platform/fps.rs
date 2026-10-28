//! The frame rate in missions. The game's logic steps every 40 ms (`[[0x537f24] + 0x314]`):
//! the main loop (0x47e360) calls the step 0x47f930 again and again; when a step is due it runs
//! the logic (0x49fea0) and then draws once (0x44e290). While it waits, we draw more pictures.
//!
//! An extra picture runs the game's own drawing again. Drawing changes some of the game's
//! state (counters, blinking, freed objects, scratch buffers), so an extra picture is a
//! transaction: the EXE's data and the heap are write-protected, the first write to a page
//! saves it (`on_write_fault`), and afterwards every saved page is copied back and the heap's
//! bookkeeping put back. No other guest thread runs meanwhile (`EXCLUSIVE`). Windows only.
//!
//! `HOGS_FPS`: unset or `max` unlimited, a number caps the pictures per second, 0 the original
//! (one picture per step).

use super::{EXCLUSIVE, guest_call, heap_state, heap_top, log, mem, set_heap_state};
use crate::rt::{Cpu, ECX, call_original};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The step: runs the logic and draws when 40 ms have passed.
pub const STEP: u32 = 0x47f930;
/// The drawing of a mission picture (thiscall, ecx = [RENDERER]).
pub const RENDER: u32 = 0x44e290;
const RENDERER: u32 = 0x51c4f8;

/// The EXE's data section (.data), and the heap's start: the game's state.
const EXE_DATA: (u32, u32) = (0x4c0000, 0x54f000);
const HEAP_LO: u32 = 0x2000_0000;
/// The engine DLL's data section: its state points into the heap.
const DLL_DATA: (u32, u32) = (0x1002_2000, 0x11ed_7000);
const PAGE: usize = 0x1000;

thread_local! {
    static DRAWN: Cell<bool> = const { Cell::new(false) };
    static LAST_PICTURE: Cell<Option<Instant>> = const { Cell::new(None) };
    static EXTRA: Cell<bool> = const { Cell::new(false) };
}

/// The cap in pictures per second: None unlimited, Some(0) the original.
fn cap() -> Option<u32> {
    static CAP: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| std::env::var("HOGS_FPS").ok().and_then(|v| v.parse().ok()))
}

/// Whether pictures are drawn between the steps now (a mission is on): flips do not wait for
/// the display's refresh then.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The camera ([CAMERA]): its position (floats at [[cam + 0x18] + 4...]) and angles
/// (+0x84, +0x88, +0x8c; 4096 a turn). The drawing builds the view from these
/// (AddDisplayWindow at 0x44e411).
const CAMERA: u32 = 0x537f98;
const TURN: i32 = 4096;
/// A step moves the camera less than this; more is a cut, not interpolated.
const CUT: f32 = 1500.0;
/// The length of a step.
const STEP_TIME: Duration = Duration::from_millis(40);

#[derive(Clone, Copy, PartialEq, Debug)]
struct Cam {
    pos: [f32; 3],
    ang: [i32; 3],
}

impl Cam {
    fn read() -> Option<Cam> {
        let m = mem();
        let cam = m.rd32(CAMERA);
        let p = if cam != 0 { m.rd32(cam + 0x18) } else { 0 };
        if p == 0 {
            return None;
        }
        let f = |a: u32| f32::from_bits(m.rd32(a));
        Some(Cam { pos: [f(p + 4), f(p + 8), f(p + 12)], ang: [0x84, 0x88, 0x8c].map(|o| m.rd32(cam + o) as i32) })
    }

    fn write(&self) {
        let m = mem();
        let cam = m.rd32(CAMERA);
        let p = m.rd32(cam + 0x18);
        for k in 0..3 {
            m.wr32(p + 4 + 4 * k as u32, self.pos[k].to_bits());
            m.wr32(cam + 0x84 + 4 * k as u32, self.ang[k] as u32);
        }
    }

    /// Between `self` (t = 0) and `to` (t = 1); angles the short way round.
    fn lerp(&self, to: &Cam, t: f32) -> Cam {
        let mut c = *self;
        for k in 0..3 {
            c.pos[k] += (to.pos[k] - self.pos[k]) * t;
            let d = (to.ang[k] - self.ang[k]).rem_euclid(TURN);
            let d = if d > TURN / 2 { d - TURN } else { d };
            c.ang[k] = (self.ang[k] + (d as f32 * t).round() as i32).rem_euclid(TURN);
        }
        c
    }

    fn near(&self, to: &Cam) -> bool {
        (0..3).all(|k| (to.pos[k] - self.pos[k]).abs() < CUT)
    }
}

thread_local! {
    /// The camera of the last two steps, and when the last one was drawn.
    static CAMS: Cell<Option<(Cam, Cam, Instant)>> = const { Cell::new(None) };
}

/// Whether an extra picture is being drawn.
pub fn in_extra() -> bool {
    EXTRA.get()
}

/// The drawing. A step's own picture shows the camera of the step before (pictures show the
/// game one step late, so those between can show it moving from there to this step's).
pub fn render(c: &mut Cpu) {
    if EXTRA.get() || cap() == Some(0) {
        call_original(c, RENDER);
        return;
    }
    DRAWN.set(true);
    let Some(cur) = Cam::read() else {
        CAMS.set(None);
        call_original(c, RENDER);
        return;
    };
    let prev = CAMS.get().map_or(cur, |(_, last, _)| last);
    let prev = if prev.near(&cur) { prev } else { cur };
    CAMS.set(Some((prev, cur, Instant::now())));
    prev.write();
    call_original(c, RENDER);
    // the game's camera stays as the step left it (the drawing may have changed it)
    cur.write();
}

pub fn step(c: &mut Cpu) {
    DRAWN.set(false);
    call_original(c, STEP);
    if DRAWN.get() {
        LAST_PICTURE.set(Some(Instant::now()));
        ACTIVE.store(cap() != Some(0), Ordering::Relaxed);
        return;
    }
    let Some(last) = LAST_PICTURE.get() else { return };
    // no step for long (a pause, a menu over the mission): not in a mission any more
    if last.elapsed() > Duration::from_millis(500) || !ACTIVE.load(Ordering::Relaxed) {
        ACTIVE.store(false, Ordering::Relaxed);
        return;
    }
    if let Some(n) = cap()
        && last.elapsed() < Duration::from_secs_f64(1.0 / n.max(1) as f64)
    {
        return;
    }
    if mem().rd32(RENDERER) == 0 {
        return;
    }
    extra_picture(c);
    LAST_PICTURE.set(Some(Instant::now()));
}

/// Draw a picture and undo what that changed of the game's state.
fn extra_picture(c: &mut Cpu) {
    let t0 = Instant::now();
    let saved_regs = c.r;
    let heap = heap_state();
    EXCLUSIVE.store(true, Ordering::Relaxed);
    arm(&[EXE_DATA, DLL_DATA, (HEAP_LO, (heap_top() + PAGE as u32 - 1) & !(PAGE as u32 - 1))]);
    let t1 = Instant::now();
    EXTRA.set(true);
    // the camera between the last two steps (written now: put back with the rest)
    if let Some((prev, cur, at)) = CAMS.get() {
        let t = (at.elapsed().as_secs_f32() / STEP_TIME.as_secs_f32()).min(1.0);
        prev.lerp(&cur, t).write();
    }
    c.r[ECX] = mem().rd32(RENDERER);
    guest_call(c, RENDER, &[]);
    EXTRA.set(false);
    let t2 = Instant::now();
    disarm();
    set_heap_state(heap);
    EXCLUSIVE.store(false, Ordering::Relaxed);
    c.r = saved_regs;
    let t3 = Instant::now();
    TIMES.lock().unwrap().push([t1 - t0, t2 - t1, t3 - t2]);
}

static TIMES: Mutex<Vec<[Duration; 3]>> = Mutex::new(Vec::new());

// ---- pages saved at their first write ---------------------------------------------------------

/// Write-protected host ranges (start, end) while armed.
static RANGES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
static ARMED: AtomicBool = AtomicBool::new(false);
/// The saved pages: host address and contents.
static SAVED: Mutex<Vec<(usize, Box<[u8]>)>> = Mutex::new(Vec::new());
/// Pages saved in all, for the statistics.
static FAULTS: AtomicUsize = AtomicUsize::new(0);

fn host(a: u32) -> usize {
    mem().slice(a, 0).as_ptr() as usize
}

#[cfg(windows)]
fn protect(at: usize, n: usize, write: bool) {
    use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};
    let mut old = 0;
    let ok = unsafe { VirtualProtect(at as _, n, if write { PAGE_READWRITE } else { PAGE_READONLY }, &mut old) };
    assert!(ok != 0, "VirtualProtect {at:#x} +{n:#x}");
}

#[cfg(not(windows))]
fn protect(_: usize, _: usize, _: bool) {}

fn arm(ranges: &[(u32, u32)]) {
    let mut r = RANGES.lock().unwrap();
    r.clear();
    for &(lo, hi) in ranges {
        let (a, b) = (host(lo), host(lo) + (hi - lo) as usize);
        protect(a, b - a, false);
        r.push((a, b));
    }
    ARMED.store(true, Ordering::SeqCst);
}

fn disarm() {
    ARMED.store(false, Ordering::SeqCst);
    for &(a, b) in RANGES.lock().unwrap().iter() {
        protect(a, b - a, true);
    }
    let mut saved = SAVED.lock().unwrap();
    for (at, page) in saved.drain(..) {
        unsafe { std::ptr::copy_nonoverlapping(page.as_ptr(), at as *mut u8, PAGE) };
    }
}

/// A write fault at host address `a`: when it is in an armed range, save the page and let the
/// write go on. Called from the exception handler.
pub fn on_write_fault(a: usize) -> bool {
    if !ARMED.load(Ordering::SeqCst) {
        return false;
    }
    let page = a & !(PAGE - 1);
    if !RANGES.lock().unwrap().iter().any(|&(lo, hi)| page >= lo && page < hi) {
        return false;
    }
    let copy: Box<[u8]> = unsafe { std::slice::from_raw_parts(page as *const u8, PAGE) }.into();
    SAVED.lock().unwrap().push((page, copy));
    protect(page, PAGE, true);
    FAULTS.fetch_add(1, Ordering::Relaxed);
    true
}

/// `HOGS_FPS_LOG=1`: pictures per second and pages saved per extra picture, every 2 s.
pub fn stats() {
    static T: Mutex<Option<(Instant, u32)>> = Mutex::new(None);
    if std::env::var_os("HOGS_FPS_LOG").is_none() {
        return;
    }
    let mut t = T.lock().unwrap();
    let (since, n) = t.get_or_insert((Instant::now(), 0));
    *n += 1;
    let secs = since.elapsed().as_secs_f64();
    if secs >= 2.0 {
        let mut times = TIMES.lock().unwrap();
        let k = times.len().max(1) as u32;
        let sum = times.drain(..).fold([Duration::ZERO; 3], |a, t| [a[0] + t[0], a[1] + t[1], a[2] + t[2]]);
        log!(
            "fps: {:.0} pictures a second, {} extra; per extra: {} pages saved, protect {:?}, draw {:?}, put back {:?}",
            *n as f64 / secs,
            k,
            FAULTS.swap(0, Ordering::Relaxed) / k as usize,
            sum[0] / k,
            sum[1] / k,
            sum[2] / k
        );
        *t = None;
    }
}

