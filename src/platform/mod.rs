//! The platform layer: the Windows and DirectX functions the game imports, implemented in
//! Rust on guest memory, and everything around them (process start, guest heap, handles,
//! threads that take turns, calls back into game code).
//!
//! Imports are reached through thunks: every IAT slot holds an address in the thunk area,
//! a call there ends in `rt_foreign` and our event handler, which runs the Rust function,
//! puts its result in eax and removes the return address and (stdcall) arguments.

pub mod advapi32;
pub mod bink;
pub mod cdaudio;
pub mod crt;
pub mod d3d;
pub mod ddraw;
pub mod dialog;
pub mod dinput;
pub mod dplay;
pub mod display;
pub mod dsound;
pub mod fps;
pub mod gdi32;
pub mod gl;
pub mod ground;
pub mod objects;
pub mod kernel32;
pub mod misc;
pub mod sample;
pub mod user32;
pub mod wide;
pub mod window;
pub mod winmm;

pub use crate::image::ResId;
use crate::image::Image;
use crate::rt::{self, Cpu, EAX, ESP, Event, Memory};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

// guest address space
pub const TIB: u32 = 0x0001_0000;
pub const STACK_LO: u32 = 0x0010_0000;
pub const STACK_HI: u32 = 0x0040_0000;
pub const IMAGE_BASE: u32 = 0x0040_0000;
/// warhogs.exe ends at 0x55e000, Data/_d3d.dll lies at 0x10000000-0x11ee9000
const ARENA_LO: u32 = 0x2000_0000;
const ARENA_HI: u32 = 0x7000_0000;
const THUNK_LO: u32 = 0x7f00_0000;
const THUNK_N: u32 = 0x4000;
/// return address of host calls into guest code (never jumped to: `ret` returns to the host)
const RET_MAGIC: u32 = 0x7ffe_0000;

static MEM: OnceLock<Memory> = OnceLock::new();

/// Guest memory.
pub fn mem() -> &'static Memory {
    MEM.get().expect("guest memory not set up")
}

macro_rules! log {
    ($($t:tt)*) => { eprintln!($($t)*) };
}
pub(crate) use log;

/// One imported function: name, number of 32-bit argument slots it removes (stdcall; -1 for
/// cdecl), and the implementation, which returns eax.
pub type ImpFn = fn(&mut Cpu) -> u32;
pub type Import = (&'static str, i32, ImpFn);

/// The n-th 32-bit argument of the import being called.
pub fn arg(c: &Cpu, n: u32) -> u32 {
    mem().rd32(c.r[ESP] + 4 + 4 * n)
}

// ---- guest memory helpers ---------------------------------------------------------------------

/// A NUL-terminated guest string (bytes as Latin-1).
pub fn cstr(a: u32) -> String {
    let m = mem();
    (0..).map(|k| m.rd8(a + k)).take_while(|&b| b != 0).map(|b| b as char).collect()
}

/// Copy a string into a guest buffer of `n` bytes, NUL-terminated and cut to fit; returns the
/// number of bytes copied without the NUL.
pub fn put_cstr(a: u32, s: &str, n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    let b: Vec<u8> = s.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
    let k = b.len().min(n as usize - 1);
    let m = mem();
    m.slice(a, k).copy_from_slice(&b[..k]);
    m.wr8(a + k as u32, 0);
    k as u32
}

/// A new guest string (from the heap, never freed).
pub fn new_cstr(s: &str) -> u32 {
    let p = alloc(s.len() as u32 + 1);
    put_cstr(p, s, s.len() as u32 + 1);
    p
}

// ---- guest heap -------------------------------------------------------------------------------

/// Blocks from one arena, 16-byte aligned; free blocks are merged with their neighbours.
#[derive(Clone)]
pub struct Heap {
    top: u32,
    mapped: u32,
    used: BTreeMap<u32, u32>,
    free_at: BTreeMap<u32, u32>,
    free_size: BTreeMap<(u32, u32), ()>,
}

static HEAP: Mutex<Heap> =
    Mutex::new(Heap { top: ARENA_LO, mapped: ARENA_LO, used: BTreeMap::new(), free_at: BTreeMap::new(), free_size: BTreeMap::new() });

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Heap {
    fn take_free(&mut self, at: u32, size: u32) {
        self.free_at.remove(&at);
        self.free_size.remove(&(size, at));
    }
    fn put_free(&mut self, mut at: u32, mut size: u32) {
        if let Some((&a, &s)) = self.free_at.range(..at).next_back() {
            if a + s == at {
                self.take_free(a, s);
                at = a;
                size += s;
            }
        }
        if let Some(&s) = self.free_at.get(&(at + size)) {
            self.take_free(at + size, s);
            size += s;
        }
        if at + size == self.top {
            self.top = at;
        } else {
            self.free_at.insert(at, size);
            self.free_size.insert((size, at), ());
        }
    }
    fn alloc(&mut self, size: u32, align: u32) -> u32 {
        let size = (size.max(1) + 15) & !15;
        let align = align.max(16);
        if align == 16 {
            if let Some((&(s, a), _)) = self.free_size.range((size, 0)..).next() {
                self.take_free(a, s);
                if s - size >= 64 {
                    self.put_free(a + size, s - size);
                    self.used.insert(a, size);
                } else {
                    self.used.insert(a, s);
                }
                return a;
            }
        }
        let a = (self.top + align - 1) & !(align - 1);
        if a as u64 + size as u64 > ARENA_HI as u64 {
            return 0;
        }
        if a > self.top {
            let gap = a - self.top;
            let t = self.top;
            self.top = a;
            self.put_free_gap(t, gap);
        }
        self.top = a + size;
        if self.top > self.mapped {
            let hi = (self.top + 0xf_ffff) & !0xf_ffff;
            mem().map(self.mapped, hi);
            self.mapped = hi;
        }
        self.used.insert(a, size);
        a
    }
    fn put_free_gap(&mut self, at: u32, size: u32) {
        self.free_at.insert(at, size);
        self.free_size.insert((size, at), ());
    }
}

/// Zeroed guest memory, 16-byte aligned; 0 when the arena is full.
pub fn alloc(size: u32) -> u32 {
    alloc_aligned(size, 16)
}

pub fn alloc_aligned(size: u32, align: u32) -> u32 {
    let p = lock(&HEAP).alloc(size, align);
    if p != 0 {
        mem().slice(p, size as usize).fill(0);
    }
    p
}

/// The heap's bookkeeping, to put back what an extra picture allocated or freed (`fps.rs`).
pub fn heap_state() -> Heap {
    lock(&HEAP).clone()
}

pub fn set_heap_state(mut h: Heap) {
    let mut cur = lock(&HEAP);
    // what was mapped since stays mapped
    h.mapped = h.mapped.max(cur.mapped);
    *cur = h;
}

/// The end of the heap's used part (for debugging tools that compare memory).
pub fn heap_top() -> u32 {
    lock(&HEAP).top
}

/// Free a block from `alloc`; anything else is ignored (returns false).
pub fn free(p: u32) -> bool {
    let mut h = lock(&HEAP);
    match h.used.remove(&p) {
        Some(s) => {
            h.put_free(p, s);
            true
        }
        None => false,
    }
}

/// The usable size of a block from `alloc` (0 if it is none).
pub fn block_size(p: u32) -> u32 {
    lock(&HEAP).used.get(&p).copied().unwrap_or(0)
}

/// Grow or shrink a block, keeping its contents; new bytes are zero.
pub fn realloc(p: u32, size: u32) -> u32 {
    if p == 0 {
        return alloc(size);
    }
    let old = block_size(p);
    if old == 0 {
        return 0;
    }
    if old >= size {
        return p;
    }
    let q = alloc(size);
    if q != 0 {
        mem().slice(q, old as usize).copy_from_slice(&mem().slice(p, old as usize).to_vec());
        free(p);
    }
    q
}

// ---- thunks -------------------------------------------------------------------------------------

struct Thunk {
    name: String,
    nargs: i32,
    f: Option<ImpFn>,
    /// not implemented: returns 0, and its first call is logged
    stub: bool,
}

static THUNKS: Mutex<Vec<Thunk>> = Mutex::new(Vec::new());

/// The name of thunk `k` (for the profiler).
pub fn thunk_name(k: usize) -> String {
    lock(&THUNKS).get(k).map_or_else(|| "?".into(), |t| t.name.clone())
}

fn thunk_add(name: String, nargs: i32, f: Option<ImpFn>) -> u32 {
    let mut t = lock(&THUNKS);
    assert!((t.len() as u32) < THUNK_N, "out of thunks");
    t.push(Thunk { name, nargs, stub: false, f });
    THUNK_LO + 4 * (t.len() as u32 - 1)
}

fn ret0(_: &mut Cpu) -> u32 {
    0
}

/// A guest address for a function without an implementation yet: it returns 0, and its first
/// call is logged by name.
pub fn thunk_not_implemented(name: &str, nargs: i32) -> u32 {
    let a = thunk_add(name.to_string(), nargs, Some(ret0));
    lock(&THUNKS).last_mut().unwrap().stub = true;
    a
}

/// A guest address that runs `f` when called.
pub fn thunk(name: &str, nargs: i32, f: ImpFn) -> u32 {
    thunk_add(name.to_string(), nargs, Some(f))
}

// ---- COM objects --------------------------------------------------------------------------------

static VTABLES: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

/// A new COM object: a guest block whose first dword points at a guest vtable of thunks for
/// `methods` (one vtable per method table, shared by all objects). `extra` bytes follow the
/// vtable pointer for the caller's use.
pub fn com_new(methods: &'static [Import], extra: u32) -> u32 {
    let vt = {
        let key = methods.as_ptr() as usize;
        let found = lock(&VTABLES).iter().find(|e| e.0 == key).map(|e| e.1);
        match found {
            Some(v) => v,
            None => {
                let v = alloc(4 * methods.len() as u32);
                for (k, &(name, n, f)) in methods.iter().enumerate() {
                    mem().wr32(v + 4 * k as u32, thunk(name, n, f));
                }
                lock(&VTABLES).push((key, v));
                v
            }
        }
    };
    let obj = alloc(4 + extra);
    mem().wr32(obj, vt);
    obj
}

// ---- guest threads take turns ------------------------------------------------------------------

/// Only one thread runs guest code at a time (a fair ticket lock): the game's threads race when
/// they truly run in parallel. Host code (imports) runs without the turn.
struct Turn {
    tickets: Mutex<(u64, u64)>,
    cv: Condvar,
}

static TURN: Turn = Turn { tickets: Mutex::new((0, 0)), cv: Condvar::new() };

/// While set, the thread holding the turn keeps it, also in imports: no other guest thread
/// runs (an extra picture between the game's steps, `fps.rs`, is undone afterwards and must
/// not undo another thread's work).
pub static EXCLUSIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
    pub static LAST_ERROR: Cell<u32> = const { Cell::new(0) };
}

fn turn_take() {
    if HELD.get() {
        return;
    }
    let mut t = lock(&TURN.tickets);
    let me = t.0;
    t.0 += 1;
    while t.1 != me {
        // the running thread hands over at its next loop head
        rt::POLL.store(1, Ordering::Relaxed);
        t = TURN.cv.wait(t).unwrap_or_else(|e| e.into_inner());
    }
    rt::POLL.store((t.0 != t.1 + 1) as u32, Ordering::Relaxed);
    HELD.set(true);
}

fn turn_give() {
    if EXCLUSIVE.load(Ordering::Relaxed) {
        return;
    }
    HELD.set(false);
    let mut t = lock(&TURN.tickets);
    t.1 += 1;
    rt::POLL.store((t.0 != t.1) as u32, Ordering::Relaxed);
    drop(t);
    TURN.cv.notify_all();
}

/// At a loop head with another thread waiting: let it run first.
fn turn_yield() {
    if HELD.get() && !EXCLUSIVE.load(Ordering::Relaxed) {
        turn_give();
        turn_take();
    }
}

pub fn set_last_error(e: u32) {
    LAST_ERROR.set(e);
}

// ---- calls between guest and host ---------------------------------------------------------------

/// Call guest code at `addr` with 32-bit arguments (cdecl or stdcall); returns eax.
pub fn guest_call(c: &mut Cpu, addr: u32, args: &[u32]) -> u32 {
    // the turn first: guest memory is written only with it (an extra picture, `fps.rs`, undoes
    // what was written while it ran)
    let took = !HELD.get();
    if took {
        turn_take();
    }
    let m = mem();
    let esp = c.r[ESP];
    for &a in args.iter().rev() {
        c.r[ESP] -= 4;
        m.wr32(c.r[ESP], a);
    }
    c.r[ESP] -= 4;
    m.wr32(c.r[ESP], RET_MAGIC);
    rt::run(c, addr);
    if took {
        turn_give();
    }
    c.r[ESP] = esp;
    c.r[EAX]
}

fn trace_api() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var_os("HOGS_TRACE_API").is_some())
}

/// `HOGS_PROF=1`: time spent in imports (and waiting for the turn after them), per import,
/// logged every 2 s with the share of the wall time.
fn prof() -> bool {
    static P: OnceLock<bool> = OnceLock::new();
    *P.get_or_init(|| std::env::var_os("HOGS_PROF").is_some_and(|v| v != "0"))
}

fn prof_add(k: usize, call: std::time::Duration, wait: std::time::Duration) {
    use std::time::{Duration, Instant};
    static P: Mutex<Option<(Instant, BTreeMap<usize, (u32, Duration, Duration)>)>> = Mutex::new(None);
    let mut p = lock(&P);
    let (t0, per) = p.get_or_insert_with(|| (Instant::now(), BTreeMap::new()));
    let e = per.entry(k).or_default();
    e.0 += 1;
    e.1 += call;
    e.2 += wait;
    let wall = t0.elapsed();
    if wall.as_secs_f32() < 2.0 {
        return;
    }
    let mut v: Vec<_> = per.iter().collect();
    v.sort_by_key(|e| std::cmp::Reverse(e.1.1 + e.1.2));
    let names = lock(&THUNKS);
    let pct = |d: Duration| 100.0 * d.as_secs_f32() / wall.as_secs_f32();
    let (call, wait) = v.iter().fold((Duration::ZERO, Duration::ZERO), |a, e| (a.0 + e.1.1, a.1 + e.1.2));
    log!("prof: imports {:.1}% (+{:.1}% waiting for the turn) of {:.1} s", pct(call), pct(wait), wall.as_secs_f32());
    for (k, (n, call, wait)) in v.iter().take(8) {
        log!("prof:   {:<28} {:>8} calls {:>5.1}% +{:.1}%", names[**k].name, n, pct(*call), pct(*wait));
    }
    *p = None;
}

fn on_event(c: &mut Cpu, e: Event) {
    match e {
        Event::Foreign(a) if (THUNK_LO..THUNK_LO + 4 * THUNK_N).contains(&a) => {
            let k = ((a - THUNK_LO) / 4) as usize;
            let (f, nargs) = {
                let t = lock(&THUNKS);
                let Some(th) = t.get(k) else { fatal(c, &format!("call to unused thunk {a:#x}")) };
                if trace_api() {
                    let args: Vec<String> = (0..th.nargs.max(0) as u32).map(|n| format!("{:#x}", arg(c, n))).collect();
                    log!("api {}({}) from {:#x}", th.name, args.join(", "), mem().rd32(c.r[ESP]));
                }
                match th.f {
                    Some(f) => {
                        if th.stub {
                            static SEEN: Mutex<BTreeMap<String, ()>> = Mutex::new(BTreeMap::new());
                            if lock(&SEEN).insert(th.name.clone(), ()).is_none() {
                                log!("warn: {} is not implemented (returns 0)", th.name);
                            }
                        }
                        (f, th.nargs)
                    }
                    None => fatal(c, &format!("import {} is not implemented yet", th.name)),
                }
            };
            LAST_CALL.store(mem().rd32(c.r[ESP]), std::sync::atomic::Ordering::Relaxed);
            let t0 = prof().then(std::time::Instant::now);
            turn_give();
            sample::IN_IMPORT.store(k + 1, Ordering::Relaxed);
            let r = f(c);
            sample::IN_IMPORT.store(0, Ordering::Relaxed);
            let t1 = prof().then(std::time::Instant::now);
            turn_take();
            if let (Some(t0), Some(t1)) = (t0, t1) {
                prof_add(k, t1 - t0, t1.elapsed());
            }
            c.r[EAX] = r;
            c.r[ESP] += 4 + 4 * nargs.max(0) as u32;
        }
        Event::Foreign(a) => fatal(c, &format!("call to unknown address {a:#x}")),
        Event::Trap(a, 0) => fatal(c, &format!("division by zero at {a:#x}")),
        Event::Trap(a, _) => fatal(c, &format!("int3/hlt/ud2 at {a:#x}")),
        Event::Unimpl(a) => fatal(c, &format!("unsupported instruction at {a:#x}")),
    }
}

/// Return address of the last import call, for crash reports.
static LAST_CALL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Report an access violation in guest memory (a bad game pointer) before the process dies.
fn report_fault(kind: &str, a: usize, pc: usize) {
    let base = mem().base as usize;
    if a >= base && a - base < rt::GUEST_SPAN {
        // the recompiled function the host code belongs to: the nearest start below it
        let func = rt::DISPATCH.get().and_then(|d| d.table.iter().filter(|e| e.1 <= pc).max_by_key(|e| e.1).map(|e| e.0));
        log!(
            "fatal: guest memory {kind} at {:#x} in the function at {} (last import call returned to {:#x})",
            a - base,
            func.map_or("?".into(), crate::image::named),
            LAST_CALL.load(std::sync::atomic::Ordering::Relaxed)
        );
    } else {
        log!("fatal: host memory {kind} at {a:#x} (code {pc:#x})");
    }
}

#[cfg(windows)]
fn catch_faults() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, EXCEPTION_POINTERS};
    unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        let rec = unsafe { &*(*info).ExceptionRecord };
        if rec.ExceptionCode == EXCEPTION_ACCESS_VIOLATION as NTSTATUS {
            let kind = ["read", "write", "execute"].get(rec.ExceptionInformation[0]).unwrap_or(&"access");
            report_fault(kind, rec.ExceptionInformation[1], rec.ExceptionAddress as usize);
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }
    crate::image::symbols(); // read now, not while handling a fault
    unsafe { AddVectoredExceptionHandler(0, Some(on_fault)) };
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn catch_faults() {
    extern "C" fn on_fault(sig: i32, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
        unsafe {
            let g = &(*(ctx as *const libc::ucontext_t)).uc_mcontext.gregs;
            // page fault error code: bit 1 a write, bit 4 an instruction fetch
            let err = g[libc::REG_ERR as usize];
            let kind = if err & 0x10 != 0 { "execute" } else if err & 2 != 0 { "write" } else { "read" };
            report_fault(kind, (*info).si_addr() as usize, g[libc::REG_RIP as usize] as usize);
            // the default action at the next try: the process ends as it would have
            libc::signal(sig, libc::SIG_DFL);
        }
    }
    crate::image::symbols(); // read now, not while handling a fault
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_fault as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    }
}

#[cfg(not(any(windows, all(target_os = "linux", target_arch = "x86_64"))))]
fn catch_faults() {}

fn fatal(c: &Cpu, msg: &str) -> ! {
    let m = mem();
    let sp = c.r[ESP];
    let stack: Vec<String> = (0..8).map(|k| format!("{:08x}", m.rd32(sp + 4 * k))).collect();
    log!("fatal: {msg}");
    log!("  eax {:08x} ecx {:08x} edx {:08x} ebx {:08x}", c.r[0], c.r[1], c.r[2], c.r[3]);
    log!("  esp {:08x} ebp {:08x} esi {:08x} edi {:08x}", c.r[4], c.r[5], c.r[6], c.r[7]);
    log!("  stack: {}", stack.join(" "));
    std::process::exit(101)
}

// ---- threads ------------------------------------------------------------------------------------

/// A thread information block at `tib` for a stack [lo, hi).
fn init_tib(tib: u32, lo: u32, hi: u32) {
    let m = mem();
    m.wr32(tib, 0xffff_ffff); // end of the SEH chain
    m.wr32(tib + 4, hi);
    m.wr32(tib + 8, lo);
    m.wr32(tib + 0x18, tib);
}

/// Guest state for a new thread: its own TIB and a 1 MB stack from the heap.
pub fn new_thread_cpu() -> Cpu {
    let tib = alloc(0x1000);
    let lo = alloc_aligned(1 << 20, 0x1000);
    let hi = lo + (1 << 20);
    init_tib(tib, lo, hi);
    let mut c = Cpu::new(mem());
    c.fs_base = tib;
    c.fcw = 0x27f;
    c.r[ESP] = hi - 64;
    c
}

// ---- files --------------------------------------------------------------------------------------

static GAME_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The game's folder (the CD's contents: warhogs.exe, Data, Maps, ...).
pub fn game_dir() -> &'static Path {
    GAME_DIR.get().expect("game folder not set")
}

/// The installed game's folder as the game sees it (`kernel32::EXE_PATH`, the registry's
/// InstallLocation); the CD is drive D:. Both are the game folder.
pub const INSTALL_DIR: &str = "C:\\Hogs";

/// Whether a guest path is on the CD (D:), which cannot be written (the game's CD check tries).
pub fn on_cd(guest: &str) -> bool {
    guest.len() >= 2 && guest.as_bytes()[1] == b':' && guest.as_bytes()[0].eq_ignore_ascii_case(&b'D')
}

/// A guest path ("D:\Maps\x.mad", "C:\Hogs\save.dat", "maps\x.mad") as a host path inside the
/// game folder, each part matched case-insensitively against what exists.
pub fn host_path(guest: &str) -> PathBuf {
    let install = &INSTALL_DIR[2..];
    let mut g = if guest.len() >= 2 && guest.as_bytes()[1] == b':' { &guest[2..] } else { guest };
    if !on_cd(guest) && g.len() >= install.len() && g[..install.len()].eq_ignore_ascii_case(install) {
        g = &g[install.len()..];
    }
    let mut p = game_dir().to_path_buf();
    for part in g.split(['\\', '/']).filter(|s| !s.is_empty() && *s != ".") {
        let found = std::fs::read_dir(&p)
            .ok()
            .and_then(|d| d.flatten().map(|e| e.file_name()).find(|n| n.to_string_lossy().eq_ignore_ascii_case(part)));
        match found {
            Some(n) => p.push(n),
            None => p.push(part),
        }
    }
    p
}

// ---- process start -------------------------------------------------------------------------------

/// The system DLLs we implement, as LoadLibraryA gives them (a module handle each).
const SYSTEM_DLLS: [&str; 10] =
    ["KERNEL32.DLL", "USER32.DLL", "GDI32.DLL", "ADVAPI32.DLL", "DDRAW.DLL", "DSOUND.DLL", "WINMM.DLL", "OLE32.DLL", "DINPUT.DLL", "COMCTL32.DLL"];
const SYSTEM_MODULE: u32 = 0x7e00_0000;

/// LoadLibraryA of a system DLL: its module handle.
pub fn system_module(name: &str) -> Option<u32> {
    let file = name.rsplit(['\\', '/']).next().unwrap_or(name).to_ascii_uppercase();
    let file = if file.ends_with(".DLL") { file } else { file + ".DLL" };
    SYSTEM_DLLS.iter().position(|&d| d == file).map(|k| SYSTEM_MODULE + 0x1_0000 * k as u32)
}

/// GetProcAddress in a system DLL: a thunk for one of our functions (0 if we have none).
pub fn system_proc(module: u32, name: &str) -> u32 {
    static MADE: Mutex<BTreeMap<(u32, String), u32>> = Mutex::new(BTreeMap::new());
    let Some(dll) = module.checked_sub(SYSTEM_MODULE).map(|k| k / 0x1_0000).and_then(|k| SYSTEM_DLLS.get(k as usize)) else { return 0 };
    let mut made = lock(&MADE);
    if let Some(&a) = made.get(&(module, name.to_string())) {
        return a;
    }
    let Some(&(n, nargs, f)) = find_import(dll, name) else { return 0 };
    let a = thunk(n, nargs, f);
    made.insert((module, name.to_string()), a);
    a
}

fn find_import(dll: &str, name: &str) -> Option<&'static Import> {
    let tables: &[&[Import]] = match dll.to_ascii_uppercase().as_str() {
        "KERNEL32.DLL" => &[kernel32::IMPORTS],
        "USER32.DLL" => &[dialog::IMPORTS, user32::IMPORTS, gdi32::USER_IMPORTS],
        "GDI32.DLL" => &[gdi32::IMPORTS],
        "ADVAPI32.DLL" => &[advapi32::IMPORTS],
        "DDRAW.DLL" => &[ddraw::IMPORTS],
        "DSOUND.DLL" => &[dsound::IMPORTS],
        "WINMM.DLL" => &[dsound::IMPORTS, misc::IMPORTS, winmm::IMPORTS],
        "OLE32.DLL" => &[misc::IMPORTS],
        "DINPUT.DLL" => &[dinput::IMPORTS],
        "BINKW32.DLL" => &[bink::IMPORTS],
        _ => &[misc::IMPORTS],
    };
    tables.iter().flat_map(|t| t.iter()).find(|i| i.0 == name)
}

/// Game functions replaced by our own (`jit::compile`).
pub const HOOKS: &[(u32, crate::rt::Hook)] = &[
    (crt::FTOL_EXE, crt::ftol),
    (crt::FTOL_DLL, crt::ftol),
    (fps::RENDER, fps::render),
    (fps::STEP, fps::step),
    (ground::AFTER_GROUND, ground::after_ground),
    (ground::WATER, ground::water),
    (objects::DRAW_OBJ, objects::draw_obj),
    (objects::DRAW_ANIM_MODEL, objects::draw_anim_model),
    (wide::SNOW, wide::snow),
    (wide::RAIN, wide::rain),
];

/// The loaded engine DLL (Data/_d3d.dll): its exports, for GetProcAddress.
static ENGINE_EXPORTS: OnceLock<Vec<(u32, String)>> = OnceLock::new();
pub const ENGINE_BASE: u32 = 0x1000_0000;

/// Copy a PE image into guest memory at its base (headers and sections) and bind its imports.
fn load_image(img: &Image) {
    let m = mem();
    m.map(img.base, img.base + 0x1000);
    let hdr = &img.data[..0x1000.min(img.data.len())];
    m.slice(img.base, hdr.len()).copy_from_slice(hdr);
    for sec in &img.sections {
        let size = sec.vsize.max(sec.rsize);
        m.map(sec.va, sec.va + size);
        let bytes = img.read(sec.va, size as usize).unwrap();
        m.slice(sec.va, bytes.len()).copy_from_slice(&bytes);
    }
    let mut missing = Vec::new();
    for i in img.imports() {
        let a = match find_import(&i.dll, &i.name) {
            Some(&(name, n, f)) => thunk(name, n, f),
            None => {
                missing.push(i.name.clone());
                thunk_add(format!("{}!{}", i.dll, i.name), 0, None)
            }
        };
        m.wr32(i.iat, a);
    }
    if !missing.is_empty() {
        log!("{:#x}: {} imports not implemented yet: {}", img.base, missing.len(), missing.join(" "));
    }
}

/// The engine DLL's export `name`: its address (0 if there is none).
pub fn engine_export(name: &str) -> u32 {
    ENGINE_EXPORTS.get().and_then(|e| e.iter().find(|e| e.1 == name)).map_or(0, |e| e.0)
}

/// Load both programs into a new guest address space, bind their imports, and run the EXE on
/// this thread (which needs a large stack: guest calls nest on it). The engine DLL's DllMain
/// runs when the game loads it (`kernel32` LoadLibraryA). Returns the exit code.
pub fn run_game(game: &crate::game::Game, game_dir: PathBuf) -> u32 {
    let _ = GAME_DIR.set(game_dir);
    let _ = MEM.set(Memory::reserve());
    let m = mem();
    m.map(TIB, TIB + 0x1000);
    m.map(STACK_LO, STACK_HI);
    m.map(THUNK_LO, THUNK_LO + 4 * THUNK_N);
    m.map(RET_MAGIC, RET_MAGIC + 0x1000);
    init_tib(TIB, STACK_LO, STACK_HI);
    assert_eq!(game.engine.base, ENGINE_BASE, "the engine DLL wants another base");
    load_image(&game.exe);
    load_image(&game.engine);
    let _ = ENGINE_EXPORTS.set(game.engine.exports());

    rt::set_handler(on_event);
    rt::set_yield(turn_yield);
    catch_faults();
    sample::start();
    if std::env::var_os("HOGS_WATCH").is_some() {
        // where the game is, every 2 s (for hangs)
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let t = *lock(&TURN.tickets);
            log!("watch: last import call returned to {:#x}, turn {}/{}, poll {}", LAST_CALL.load(Ordering::Relaxed), t.1, t.0, rt::POLL.load(Ordering::Relaxed));
        });
    }
    if let Some(path) = rt::count_file() {
        // calls per function, every second: "second address calls [name]" lines
        std::thread::spawn(move || {
            use std::io::Write;
            let mut out = std::io::BufWriter::new(std::fs::File::create(path).expect("HOGS_COUNT file"));
            for sec in 1.. {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let mut counts: Vec<(u32, u32)> = lock(&rt::COUNTS).iter().map(|&(a, n)| (a, n.swap(0, Ordering::Relaxed))).collect();
                counts.sort_unstable();
                for (a, n) in counts {
                    if n != 0 {
                        let name = crate::image::symbols().get(&a).map_or("", |n| n.as_str());
                        let _ = writeln!(out, "{sec} {a:x} {n} {name}");
                    }
                }
                let _ = out.flush();
            }
        });
    }
    let mut c = Cpu::new(m);
    c.fs_base = TIB;
    c.r[ESP] = STACK_HI - 64;
    turn_take();
    c.r[ESP] -= 4;
    m.wr32(c.r[ESP], RET_MAGIC);
    rt::run(&mut c, game.exe.entry);
    turn_give();
    c.r[EAX]
}

/// C `printf` formatting with 32-bit arguments (`arg(k)` is the k-th one): flags, width,
/// precision, h/l, and d i u x X o c s p (what wsprintf knows).
pub fn printf(fmt: &str, mut arg: impl FnMut(u32) -> u32) -> String {
    let f: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut next = 0;
    let mut take = move || {
        let v = arg(next);
        next += 1;
        v
    };
    let mut i = 0;
    while i < f.len() {
        if f[i] != '%' {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        let (mut left, mut zero, mut plus, mut space, mut alt) = (false, false, false, false, false);
        while i < f.len() && "-0+ #".contains(f[i]) {
            match f[i] {
                '-' => left = true,
                '0' => zero = true,
                '+' => plus = true,
                ' ' => space = true,
                _ => alt = true,
            }
            i += 1;
        }
        let num = |i: &mut usize, take: &mut dyn FnMut() -> u32| -> Option<usize> {
            if *i < f.len() && f[*i] == '*' {
                *i += 1;
                return Some(take() as usize);
            }
            let s = *i;
            while *i < f.len() && f[*i].is_ascii_digit() {
                *i += 1;
            }
            (s != *i).then(|| f[s..*i].iter().collect::<String>().parse().unwrap_or(0))
        };
        let width = num(&mut i, &mut take).unwrap_or(0);
        let prec = if i < f.len() && f[i] == '.' {
            i += 1;
            Some(num(&mut i, &mut take).unwrap_or(0))
        } else {
            None
        };
        while i < f.len() && "hlL".contains(f[i]) {
            i += 1;
        }
        let Some(&conv) = f.get(i) else { break };
        i += 1;
        let mut sign = String::new();
        let mut body = match conv {
            'd' | 'i' => {
                let v = take() as i32;
                if v < 0 {
                    sign.push('-');
                } else if plus {
                    sign.push('+');
                } else if space {
                    sign.push(' ');
                }
                v.unsigned_abs().to_string()
            }
            'u' => take().to_string(),
            'x' => format!("{}{:x}", if alt { "0x" } else { "" }, take()),
            'X' => format!("{}{:X}", if alt { "0X" } else { "" }, take()),
            'o' => format!("{:o}", take()),
            'p' => format!("{:08X}", take()),
            'c' => (take() as u8 as char).to_string(),
            's' => {
                let p = take();
                let s = if p == 0 { "(null)".to_string() } else { cstr(p) };
                match prec {
                    Some(n) => s.chars().take(n).collect(),
                    None => s,
                }
            }
            other => other.to_string(),
        };
        if let (Some(p), true) = (prec, "diuxXo".contains(conv)) {
            while body.len() < p {
                body.insert(0, '0');
            }
        }
        let len = sign.len() + body.len();
        let pad = width.saturating_sub(len);
        if left {
            out += &sign;
            out += &body;
            out.extend(std::iter::repeat_n(' ', pad));
        } else if zero && prec.is_none() && !"sc%".contains(conv) {
            out += &sign;
            out.extend(std::iter::repeat_n('0', pad));
            out += &body;
        } else {
            out.extend(std::iter::repeat_n(' ', pad));
            out += &sign;
            out += &body;
        }
    }
    out
}

/// A resource of the loaded EXE (first language): guest address and size of its data.
pub fn resource(ty: ResId, id: ResId) -> Option<(u32, u32)> {
    resource_in(IMAGE_BASE, ty, id)
}

/// A resource of the loaded EXE or DLL at `base`.
pub fn resource_in(base: u32, ty: ResId, id: ResId) -> Option<(u32, u32)> {
    let m = mem();
    crate::image::find_resource(&|a| m.rd8(a), base, ty, id)
}
