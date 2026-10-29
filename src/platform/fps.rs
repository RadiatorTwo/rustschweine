//! The frame rate in missions. The game's logic steps every 40 ms (`[[0x537f24] + 0x314]`):
//! the main loop (0x47e360) calls the step 0x47f930 again and again; when a step is due it runs
//! the logic (0x49fea0) and then draws once (0x44e290). While it waits, we draw more pictures.
//!
//! An extra picture runs the game's own drawing again. Drawing changes some of the game's
//! state (counters, blinking, freed objects, scratch buffers), so an extra picture is a
//! transaction: the pages it writes of the EXE's and the engine's data and of the heap are put
//! back afterwards, and the heap's bookkeeping. Which pages were written the system tells
//! (guest memory is reserved with write watch: `GetWriteWatch`); their contents before come
//! from a copy of those ranges, brought up to date before each extra picture with the pages
//! written since. No other guest thread runs meanwhile (`EXCLUSIVE`). Windows only.
//!
//! `HOGS_FPS`: unset or `max` unlimited, a number caps the pictures per second, 0 the original
//! (one picture per step).

use super::{EXCLUSIVE, guest_call, heap_state, heap_top, log, mem, set_heap_state};
use crate::rt::{Cpu, ECX, call_original};
use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
/// the display's refresh then. A mission left for the front end (Escape) calls the step no
/// more, so it is on only while the step was called lately (else the menus ran unpaced).
pub fn active() -> bool {
    let since = (super::kernel32::start_time().elapsed().as_millis() as u64).saturating_sub(LAST_STEP.load(Ordering::Relaxed));
    ACTIVE.load(Ordering::Relaxed) && since < 200
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// When the step was last called, in ms since the start.
static LAST_STEP: AtomicU64 = AtomicU64::new(0);

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

/// The camera as text (position, angles), for the marks of a recording (`window.rs`).
pub fn camera_text() -> Option<String> {
    let c = Cam::read()?;
    Some(format!("pos={:.2},{:.2},{:.2} ang={},{},{}", c.pos[0], c.pos[1], c.pos[2], c.ang[0], c.ang[1], c.ang[2]))
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
        super::wide::around(|| call_original(c, RENDER));
        return;
    }
    DRAWN.set(true);
    let Some(cur) = Cam::read() else {
        CAMS.set(None);
        super::wide::around(|| call_original(c, RENDER));
        return;
    };
    let prev = CAMS.get().map_or(cur, |(_, last, _)| last);
    let prev = if prev.near(&cur) { prev } else { cur };
    CAMS.set(Some((prev, cur, Instant::now())));
    prev.write();
    STEPS.set(STEPS.get().wrapping_add(1));
    BLEND.set(Some(0.0));
    super::wide::around(|| call_original(c, RENDER));
    BLEND.set(None);
    // the game's camera stays as the step left it (the drawing may have changed it)
    cur.write();
}

thread_local! {
    /// While a mission picture is drawn: how far it is from the step before to the last one
    /// (0 in a step's own picture); and the steps' pictures counted.
    static BLEND: Cell<Option<f32>> = const { Cell::new(None) };
    static STEPS: Cell<u32> = const { Cell::new(0) };
}

/// While a mission picture is drawn: where between the last two steps it shows the game (0:
/// the step before, a step's own picture; up to 1 in the pictures after it), and the number
/// of the last step's picture. None when pictures are not drawn between steps.
pub fn blend() -> Option<(f32, u32)> {
    BLEND.get().map(|t| (t, STEPS.get()))
}

pub fn step(c: &mut Cpu) {
    LAST_STEP.store(super::kernel32::start_time().elapsed().as_millis() as u64, Ordering::Relaxed);
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
    let ranges = [EXE_DATA, DLL_DATA, (HEAP_LO, (heap_top() + PAGE as u32 - 1) & !(PAGE as u32 - 1))];
    if !SHADOW.with_borrow_mut(|s| s.sync(&ranges)) {
        EXCLUSIVE.store(false, Ordering::Relaxed);
        return;
    }
    let t1 = Instant::now();
    EXTRA.set(true);
    // the camera between the last two steps (written now: put back with the rest)
    if let Some((prev, cur, at)) = CAMS.get() {
        let t = (at.elapsed().as_secs_f32() / STEP_TIME.as_secs_f32()).min(1.0);
        prev.lerp(&cur, t).write();
        BLEND.set(Some(t));
    }
    c.r[ECX] = mem().rd32(RENDERER);
    guest_call(c, RENDER, &[]);
    BLEND.set(None);
    EXTRA.set(false);
    let t2 = Instant::now();
    SHADOW.with_borrow_mut(|s| s.restore());
    set_heap_state(heap);
    EXCLUSIVE.store(false, Ordering::Relaxed);
    c.r = saved_regs;
    let t3 = Instant::now();
    TIMES.lock().unwrap().push([t1 - t0, t2 - t1, t3 - t2]);
}

static TIMES: Mutex<Vec<[Duration; 3]>> = Mutex::new(Vec::new());

// ---- pages put back -------------------------------------------------------------------------

/// Pages put back in all, for the statistics.
static RESTORED: AtomicUsize = AtomicUsize::new(0);

/// A copy of guest memory ranges as they were before an extra picture.
#[derive(Default)]
struct Shadow {
    /// guest start of each range and its copy
    ranges: Vec<(u32, Vec<u8>)>,
    /// written pages, as the system reports them
    found: Vec<usize>,
}

thread_local! {
    static SHADOW: std::cell::RefCell<Shadow> = std::cell::RefCell::new(Shadow::default());
}

fn host(a: u32) -> usize {
    mem().slice(a, 0).as_ptr() as usize
}

/// The pages written in `n` bytes at host address `at` since it was last asked (and from now
/// on not written), into `out`: None when the system cannot tell.
#[cfg(windows)]
fn written(at: usize, n: usize, out: &mut Vec<usize>) -> Option<()> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    out.clear();
    out.resize(n / PAGE, 0);
    let mut count = out.len();
    let mut gran = 0u32;
    let r = unsafe { GetWriteWatch(WRITE_WATCH_FLAG_RESET, at as _, n, out.as_mut_ptr() as _, &mut count, &mut gran) };
    out.truncate(count);
    (r == 0).then_some(())
}

#[cfg(not(windows))]
fn written(_: usize, _: usize, _: &mut Vec<usize>) -> Option<()> {
    None
}

impl Shadow {
    /// Bring the copy up to date with `ranges` (guest start, end): the pages written since the
    /// last time; a range new or grown is copied (its new part). False when written pages
    /// cannot be found (no extra pictures then).
    fn sync(&mut self, ranges: &[(u32, u32)]) -> bool {
        if self.ranges.len() != ranges.len() || self.ranges.iter().zip(ranges).any(|(s, r)| s.0 != r.0) {
            self.ranges = ranges.iter().map(|r| (r.0, Vec::new())).collect();
        }
        let mut found = std::mem::take(&mut self.found);
        for (k, &(lo, hi)) in ranges.iter().enumerate() {
            let at = host(lo);
            let copy = &mut self.ranges[k].1;
            if written(at, (hi - lo) as usize, &mut found).is_none() {
                self.ranges.clear();
                return false;
            }
            for &p in &found {
                let off = p - at;
                if off + PAGE <= copy.len() {
                    copy[off..off + PAGE].copy_from_slice(mem().slice(lo + off as u32, PAGE));
                }
            }
            let old = copy.len().min((hi - lo) as usize);
            copy.truncate(old);
            copy.extend_from_slice(mem().slice(lo + old as u32, (hi - lo) as usize - old));
        }
        self.found = found;
        true
    }

    /// Put back the pages written since `sync`.
    fn restore(&mut self) {
        let mut found = std::mem::take(&mut self.found);
        for (lo, copy) in &self.ranges {
            let at = host(*lo);
            if written(at, copy.len(), &mut found).is_none() {
                continue;
            }
            for &p in &found {
                let off = p - at;
                mem().slice(*lo + off as u32, PAGE).copy_from_slice(&copy[off..off + PAGE]);
            }
            RESTORED.fetch_add(found.len(), Ordering::Relaxed);
        }
        self.found = found;
    }
}

/// `HOGS_FPS_LOG=1`: pictures per second and pages put back per extra picture, every 2 s.
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
            "fps: {:.0} pictures a second, {} extra; per extra: {} pages put back, copy {:?}, draw {:?}, put back {:?}",
            *n as f64 / secs,
            k,
            RESTORED.swap(0, Ordering::Relaxed) / k as usize,
            sum[0] / k,
            sum[1] / k,
            sum[2] / k
        );
        *t = None;
    }
}

