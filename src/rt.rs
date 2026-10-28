//! Run-time support for the recompiled code: the guest CPU state, guest memory, and the
//! helpers generated code calls.

use std::sync::OnceLock;

/// Guest CPU state shared by all recompiled functions. Generated code keeps registers and
/// flags in its own variables and stores them here around calls, returns and jumps out.
#[repr(C)]
pub struct Cpu {
    /// eax ecx edx ebx esp ebp esi edi
    pub r: [u32; 8],
    pub cf: u8,
    pub pf: u8,
    pub zf: u8,
    pub sf: u8,
    pub of: u8,
    pub df: u8,
    pub _pad: [u8; 2],
    /// guest address of the thread information block
    pub fs_base: u32,
    /// host address of guest address 0
    pub mem: *mut u8,
    /// address of the foreign call in progress (see `rt_lookup`)
    pub foreign: u32,
    /// x87 registers: ST(i) is `st[(top + i) & 7]`. Doubles: the CRT sets 53-bit precision.
    pub st: [f64; 8],
    /// The integer a `fild qword` loaded into a slot and the bits of the double it became:
    /// while the slot still holds that double, `fistp qword` stores the integer, so the
    /// FPU memcpy idiom (fild/fistp qword) keeps all 64 bits.
    pub st_int: [i64; 8],
    pub st_int_of: [u64; 8],
    pub top: u32,
    /// x87 status word without TOP (condition codes C0-C3, exception bits) and control word
    pub fsw: u16,
    pub fcw: u16,
    /// MMX registers (their aliasing with the x87 registers is not modelled)
    pub mm: [u64; 8],
    /// Nonzero: guest code returns all the way out (generated code checks it after every
    /// call). Set by an `Event` handler.
    pub stop: u32,
}

pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const EBX: usize = 3;
pub const ESP: usize = 4;
pub const ESI: usize = 6;
pub const EDI: usize = 7;

impl Cpu {
    pub fn new(mem: &Memory) -> Cpu {
        Cpu {
            r: [0; 8],
            cf: 0,
            pf: 0,
            zf: 0,
            sf: 0,
            of: 0,
            df: 0,
            _pad: [0; 2],
            fs_base: 0,
            mem: mem.base,
            foreign: 0,
            st: [0.0; 8],
            st_int: [0; 8],
            st_int_of: [0; 8],
            top: 0,
            fsw: 0,
            fcw: 0x37f,
            mm: [0; 8],
            stop: 0,
        }
    }
}

/// The guest's 4 GB address space: guest address `a` is `base + a`, so game pointers stay
/// 32-bit on any host. One more page is reserved behind it: an access at 0xffffffff runs
/// past 4 GB (a CPU would wrap around to 0) but stays inside the reservation.
pub const GUEST_SPAN: usize = (1 << 32) + 0x1000;

pub struct Memory {
    pub base: *mut u8,
}

// guest memory is shared by the guest threads, which take turns (see `platform`)
unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

impl Memory {
    pub fn reserve() -> Memory {
        #[cfg(windows)]
        let base = unsafe {
            use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
            VirtualAlloc(std::ptr::null(), GUEST_SPAN, MEM_RESERVE, PAGE_NOACCESS) as *mut u8
        };
        #[cfg(unix)]
        let base = unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                GUEST_SPAN,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            if p == libc::MAP_FAILED { std::ptr::null_mut() } else { p as *mut u8 }
        };
        assert!(!base.is_null(), "cannot reserve 4 GB of address space");
        Memory { base }
    }

    /// Make guest addresses [lo, hi) readable and writable.
    pub fn map(&self, lo: u32, hi: u32) {
        let (lo, hi) = (lo & !0xfff, (hi as u64 + 0xfff) & !0xfff);
        let p = unsafe { self.base.add(lo as usize) };
        let n = (hi - lo as u64) as usize;
        #[cfg(windows)]
        let ok = unsafe {
            use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
            !VirtualAlloc(p as _, n, MEM_COMMIT, PAGE_READWRITE).is_null()
        };
        #[cfg(unix)]
        let ok = unsafe { libc::mprotect(p as _, n, libc::PROT_READ | libc::PROT_WRITE) == 0 };
        assert!(ok, "cannot map guest memory {lo:#x}..{hi:#x}");
    }

    pub fn slice(&self, a: u32, n: usize) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base.add(a as usize), n) }
    }
    pub fn rd8(&self, a: u32) -> u8 {
        unsafe { *self.base.add(a as usize) }
    }
    pub fn wr8(&self, a: u32, v: u8) {
        unsafe { *self.base.add(a as usize) = v }
    }
    pub fn rd16(&self, a: u32) -> u16 {
        unsafe { (self.base.add(a as usize) as *const u16).read_unaligned() }
    }
    pub fn wr16(&self, a: u32, v: u16) {
        unsafe { (self.base.add(a as usize) as *mut u16).write_unaligned(v) }
    }
    pub fn rd32(&self, a: u32) -> u32 {
        unsafe { (self.base.add(a as usize) as *const u32).read_unaligned() }
    }
    pub fn wr32(&self, a: u32, v: u32) {
        unsafe { (self.base.add(a as usize) as *mut u32).write_unaligned(v) }
    }
}

/// Recompiled functions by guest address (sorted), the stub for everything else, and the
/// trampoline the host enters guest code through.
pub struct Dispatch {
    pub table: Vec<(u32, usize)>,
    pub foreign_stub: usize,
    pub enter: extern "C" fn(*mut Cpu, *const u8),
    /// hooked functions (sorted): the host function and the recompiled original
    pub hooks: Vec<(u32, Hook, usize)>,
}

/// A host function that replaces the guest function at its address; it runs the original
/// with `call_original`.
pub type Hook = fn(&mut Cpu);

pub static DISPATCH: OnceLock<Dispatch> = OnceLock::new();

fn die(msg: String) -> ! {
    eprintln!("{msg}");
    std::process::exit(101)
}

/// The function to run for a guest address: a recompiled function, or the foreign stub,
/// which passes the address on to `rt_foreign`.
pub extern "C" fn rt_lookup(cpu: *mut Cpu, addr: u32) -> *const u8 {
    let d = DISPATCH.get().expect("dispatch table not set");
    match d.table.binary_search_by_key(&addr, |e| e.0) {
        Ok(k) => d.table[k].1 as *const u8,
        Err(_) => {
            unsafe { (*cpu).foreign = addr };
            d.foreign_stub as *const u8
        }
    }
}

/// Nonzero while a guest thread waits for its turn (see `platform`): generated code checks it
/// at loop heads and calls `rt_yield`, so a thread spinning on a flag lets the others run.
pub static POLL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `HOGS_COUNT=<file>`: every function entry counts its calls (function address, counter);
/// the platform layer writes them to the file every second.
pub static COUNTS: std::sync::Mutex<Vec<(u32, &'static std::sync::atomic::AtomicU32)>> = std::sync::Mutex::new(Vec::new());

/// The call counter of the function at `addr` (made on first use).
pub fn counter(addr: u32) -> &'static std::sync::atomic::AtomicU32 {
    let mut c = COUNTS.lock().unwrap();
    if let Some(&(_, n)) = c.iter().find(|e| e.0 == addr) {
        return n;
    }
    let n = Box::leak(Box::new(std::sync::atomic::AtomicU32::new(0)));
    c.push((addr, n));
    n
}

/// Where `HOGS_COUNT` writes the counts (None: functions do not count).
pub fn count_file() -> Option<&'static str> {
    static F: OnceLock<Option<String>> = OnceLock::new();
    F.get_or_init(|| std::env::var("HOGS_COUNT").ok().filter(|v| !v.is_empty())).as_deref()
}

static YIELD: OnceLock<fn()> = OnceLock::new();

/// What `rt_yield` does (hand the turn to a waiting thread).
pub fn set_yield(f: fn()) {
    let _ = YIELD.set(f);
}

pub extern "C" fn rt_yield(_cpu: *mut Cpu) {
    if let Some(f) = YIELD.get() {
        f()
    }
}

/// What a hooked function's recompiled entry calls.
pub extern "C" fn rt_hook(cpu: *mut Cpu, addr: u32) {
    let d = DISPATCH.get().expect("dispatch table not set");
    let k = d.hooks.binary_search_by_key(&addr, |e| e.0).expect("hook");
    (d.hooks[k].1)(unsafe { &mut *cpu });
}

/// Run the original of the hooked function at `addr`: like a call to it, its return address
/// must be on the guest stack (and its `ret` pops it).
pub fn call_original(cpu: &mut Cpu, addr: u32) {
    let d = DISPATCH.get().expect("dispatch table not set");
    let k = d.hooks.binary_search_by_key(&addr, |e| e.0).expect("hook");
    (d.enter)(cpu, d.hooks[k].2 as *const u8);
}

/// Run the guest code at `addr` (recompiled or not) until it returns; its return address
/// must already be on the guest stack.
pub fn run(cpu: &mut Cpu, addr: u32) {
    let d = DISPATCH.get().expect("dispatch table not set");
    let f = rt_lookup(cpu, addr);
    (d.enter)(cpu, f);
}

/// Something guest code cannot go on with by itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// a call or jump to an address that is not recompiled code (imports, later)
    Foreign(u32),
    /// division by zero (kind 0) or int3/hlt/ud2 (kind 1) at an address
    Trap(u32, u32),
    /// an instruction the lifter does not support
    Unimpl(u32),
}

/// What happens on an `Event`. The handler may set `Cpu::stop` to unwind out of guest
/// code; otherwise execution goes on after it (a foreign call returns to its caller).
pub type Handler = fn(&mut Cpu, Event);

static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Replace the default handler, which reports the event and exits.
pub fn set_handler(h: Handler) {
    let _ = HANDLER.set(h);
}

fn event(cpu: &mut Cpu, e: Event) {
    match HANDLER.get() {
        Some(h) => h(cpu, e),
        None => match e {
            Event::Foreign(a) => die(format!("call to unknown address {a:#x} (esp {:#x})", cpu.r[ESP])),
            Event::Trap(a, 0) => die(format!("division by zero at {a:#x}")),
            Event::Trap(a, _) => die(format!("int3/hlt/ud2 at {a:#x}")),
            Event::Unimpl(a) => die(format!("instruction at {a:#x} is not supported yet")),
        },
    }
}

pub extern "C" fn rt_foreign(cpu: *mut Cpu) {
    let cpu = unsafe { &mut *cpu };
    event(cpu, Event::Foreign(cpu.foreign))
}

/// After this the generated code returns (with `stop` set by a handler, all the way out).
pub extern "C" fn rt_unimpl(cpu: *mut Cpu, addr: u32) {
    event(unsafe { &mut *cpu }, Event::Unimpl(addr))
}

/// After this the generated code returns, like `rt_unimpl`.
pub extern "C" fn rt_trap(cpu: *mut Cpu, addr: u32, kind: u32) {
    event(unsafe { &mut *cpu }, Event::Trap(addr, kind))
}

pub extern "C" fn rt_cpuid(cpu: *mut Cpu) {
    let c = unsafe { &mut *cpu };
    // a plain Pentium with FPU, TSC and MMX
    match c.r[EAX] {
        0 => c.r[..4].copy_from_slice(&[1, 0x756e6547, 0x6c65746e, 0x49656e69]),
        _ => c.r[..4].copy_from_slice(&[0x543, 0, 0, 0x0080_8111]),
    }
}

pub extern "C" fn rt_rdtsc(cpu: *mut Cpu) {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let t = START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64;
    let c = unsafe { &mut *cpu };
    c.r[EAX] = t as u32;
    c.r[EDX] = (t >> 32) as u32;
}

/// String instructions: `op` = kind | size << 4 | rep << 8 (kind: 0 movs, 1 stos, 2 lods,
/// 3 scas, 4 cmps; rep: 0 none, 1 rep/repe, 2 repne).
pub extern "C" fn rt_string(cpu: *mut Cpu, op: u32) {
    let c = unsafe { &mut *cpu };
    let (kind, size, rep) = (op & 15, (op >> 4) & 15, op >> 8);
    let m = c.mem;
    let rd = |a: u32| -> u32 {
        unsafe {
            let p = m.add(a as usize);
            match size {
                1 => *p as u32,
                2 => (p as *const u16).read_unaligned() as u32,
                _ => (p as *const u32).read_unaligned(),
            }
        }
    };
    let wr = |a: u32, v: u32| unsafe {
        let p = m.add(a as usize);
        match size {
            1 => *p = v as u8,
            2 => (p as *mut u16).write_unaligned(v as u16),
            _ => (p as *mut u32).write_unaligned(v),
        }
    };
    let step = if c.df != 0 { (size as u32).wrapping_neg() } else { size };
    let mask = if size == 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
    // rep movs / rep stos forwards in one go, unless a copy overlaps so that it repeats what
    // it wrote (destination inside the source)
    let n = c.r[ECX];
    if rep != 0 && c.df == 0 && (kind == 0 || kind == 1) && n != 0 {
        let bytes = n.wrapping_mul(size);
        let (s, d) = (c.r[ESI], c.r[EDI]);
        if bytes / size == n && d.checked_add(bytes).is_some() && (kind == 1 || (s.checked_add(bytes).is_some() && (d <= s || d >= s + bytes))) {
            unsafe {
                let dp = m.add(d as usize);
                if kind == 0 {
                    std::ptr::copy(m.add(s as usize), dp, bytes as usize);
                    c.r[ESI] = s + bytes;
                } else if size == 1 {
                    std::ptr::write_bytes(dp, c.r[EAX] as u8, bytes as usize);
                } else {
                    for i in 0..n as usize {
                        wr(d + (i as u32) * size, c.r[EAX] & mask);
                    }
                }
            }
            c.r[EDI] = d + bytes;
            c.r[ECX] = 0;
            return;
        }
    }
    loop {
        if rep != 0 && c.r[ECX] == 0 {
            break;
        }
        match kind {
            0 => {
                wr(c.r[EDI], rd(c.r[ESI]));
                c.r[ESI] = c.r[ESI].wrapping_add(step);
                c.r[EDI] = c.r[EDI].wrapping_add(step);
            }
            1 => {
                wr(c.r[EDI], c.r[EAX] & mask);
                c.r[EDI] = c.r[EDI].wrapping_add(step);
            }
            2 => {
                c.r[EAX] = (c.r[EAX] & !mask) | rd(c.r[ESI]);
                c.r[ESI] = c.r[ESI].wrapping_add(step);
            }
            _ => {
                let (x, y) = if kind == 3 {
                    let v = (c.r[EAX] & mask, rd(c.r[EDI]));
                    c.r[EDI] = c.r[EDI].wrapping_add(step);
                    v
                } else {
                    let v = (rd(c.r[ESI]), rd(c.r[EDI]));
                    c.r[ESI] = c.r[ESI].wrapping_add(step);
                    c.r[EDI] = c.r[EDI].wrapping_add(step);
                    v
                };
                let r = x.wrapping_sub(y) & mask;
                let bits = 8 * size;
                c.cf = (x < y) as u8;
                c.of = ((((x ^ y) & (x ^ r)) >> (bits - 1)) & 1) as u8;
                c.zf = (r == 0) as u8;
                c.sf = ((r >> (bits - 1)) & 1) as u8;
                c.pf = ((r as u8).count_ones() % 2 == 0) as u8;
            }
        }
        if rep == 0 {
            break;
        }
        c.r[ECX] -= 1;
        if (kind == 3 || kind == 4) && ((rep == 1 && c.zf == 0) || (rep == 2 && c.zf != 0)) {
            break;
        }
    }
}

/// The x87 operations generated code leaves to `rt_x87`.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum X87 {
    /// push the 80-bit float at `addr`
    LoadF80,
    /// store ST(0) as an 80-bit float at `addr`, pop
    StoreF80,
    Fxam,
    Fsin,
    Fcos,
    Fsincos,
    Fptan,
    Fpatan,
    F2xm1,
    Fyl2x,
    Fyl2xp1,
    Fscale,
    Fprem,
    Fprem1,
    Fxtract,
}

const X87_OPS: [X87; 15] = [
    X87::LoadF80,
    X87::StoreF80,
    X87::Fxam,
    X87::Fsin,
    X87::Fcos,
    X87::Fsincos,
    X87::Fptan,
    X87::Fpatan,
    X87::F2xm1,
    X87::Fyl2x,
    X87::Fyl2xp1,
    X87::Fscale,
    X87::Fprem,
    X87::Fprem1,
    X87::Fxtract,
];

const C0: u16 = 0x100;
const C1: u16 = 0x200;
const C2: u16 = 0x400;
const C3: u16 = 0x4000;

impl Cpu {
    fn st_slot(&self, i: u32) -> usize {
        ((self.top + i) & 7) as usize
    }
    pub fn st(&self, i: u32) -> f64 {
        self.st[self.st_slot(i)]
    }
    pub fn set_st(&mut self, i: u32, v: f64) {
        self.st[self.st_slot(i)] = v;
    }
    pub fn fpush(&mut self, v: f64) {
        self.top = (self.top + 7) & 7;
        self.set_st(0, v);
    }
    pub fn fpop(&mut self) {
        self.top = (self.top + 1) & 7;
    }
    fn set_cc(&mut self, cc: u16) {
        self.fsw = (self.fsw & !(C0 | C1 | C2 | C3)) | cc;
    }
}

/// x * 2^trunc(n), without overflowing in the scale factor itself.
fn scale(x: f64, n: f64) -> f64 {
    let mut n = n.trunc().clamp(-4000.0, 4000.0) as i32;
    let mut x = x;
    while n != 0 {
        let k = n.clamp(-1000, 1000);
        x *= 2f64.powi(k);
        n -= k;
    }
    x
}

/// An 80-bit extended float (little-endian bytes) as the nearest double.
pub fn f80_to_f64(b: &[u8; 10]) -> f64 {
    let mant = u64::from_le_bytes(b[..8].try_into().unwrap());
    let se = u16::from_le_bytes([b[8], b[9]]);
    let sign = if se & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (se & 0x7fff) as i32;
    if exp == 0x7fff {
        return if mant << 1 == 0 { sign * f64::INFINITY } else { f64::NAN };
    }
    // value = mant * 2^(exp - 16383 - 63); u64 -> f64 rounds to nearest, which is all the
    // rounding needed outside the double's denormal range
    sign * scale(mant as f64, (exp.max(1) - 16383 - 63) as f64)
}

/// A double as an 80-bit extended float (exact: every double fits).
pub fn f64_to_f80(v: f64) -> [u8; 10] {
    let bits = v.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1 << 52) - 1);
    let (e, m) = if exp == 0x7ff {
        (0x7fff, 1 << 63 | frac << 11)
    } else if exp == 0 && frac == 0 {
        (0, 0)
    } else if exp == 0 {
        // a denormal double is a normal extended float
        let shift = frac.leading_zeros() as i32;
        (16383 - 1022 - (shift - 11), frac << shift)
    } else {
        (exp - 1023 + 16383, 1 << 63 | frac << 11)
    };
    let mut out = [0u8; 10];
    out[..8].copy_from_slice(&m.to_le_bytes());
    out[8..].copy_from_slice(&(sign | e as u16).to_le_bytes());
    out
}

/// x87 operations that are rare or need libm; `op` is an `X87`.
pub extern "C" fn rt_x87(cpu: *mut Cpu, op: u32, addr: u32) {
    let c = unsafe { &mut *cpu };
    let Some(&op) = X87_OPS.get(op as usize) else { die(format!("rt_x87: unknown operation {op}")) };
    let x = c.st(0);
    match op {
        X87::LoadF80 => {
            let b: [u8; 10] = unsafe { std::slice::from_raw_parts(c.mem.add(addr as usize), 10) }.try_into().unwrap();
            c.fpush(f80_to_f64(&b));
        }
        X87::StoreF80 => {
            let b = f64_to_f80(x);
            unsafe { std::slice::from_raw_parts_mut(c.mem.add(addr as usize), 10) }.copy_from_slice(&b);
            c.fpop();
        }
        X87::Fxam => {
            // empty registers are not tracked: an empty one reads as what it last held
            let class = if x.is_nan() {
                C0
            } else if x.is_infinite() {
                C2 | C0
            } else if x == 0.0 {
                C3
            } else if x.is_subnormal() {
                C3 | C2
            } else {
                C2
            };
            c.set_cc(class | if x.is_sign_negative() { C1 } else { 0 });
        }
        X87::Fsin | X87::Fcos | X87::Fsincos | X87::Fptan => {
            match op {
                X87::Fsin => c.set_st(0, x.sin()),
                X87::Fcos => c.set_st(0, x.cos()),
                X87::Fsincos => {
                    c.set_st(0, x.sin());
                    c.fpush(x.cos());
                }
                _ => {
                    c.set_st(0, x.tan());
                    c.fpush(1.0);
                }
            }
            c.set_cc(0);
        }
        X87::Fpatan => {
            let y = c.st(1);
            c.set_st(1, y.atan2(x));
            c.fpop();
        }
        X87::F2xm1 => c.set_st(0, (x * std::f64::consts::LN_2).exp_m1()),
        X87::Fyl2x => {
            let y = c.st(1);
            c.set_st(1, y * x.log2());
            c.fpop();
        }
        X87::Fyl2xp1 => {
            let y = c.st(1);
            c.set_st(1, y * x.ln_1p() / std::f64::consts::LN_2);
            c.fpop();
        }
        X87::Fscale => {
            let n = c.st(1);
            c.set_st(0, scale(x, n));
        }
        X87::Fprem | X87::Fprem1 => {
            let y = c.st(1);
            let (q, r) = if matches!(op, X87::Fprem) {
                ((x / y).trunc(), x % y)
            } else {
                let q = (x / y).round_ties_even();
                (q, x - q * y)
            };
            c.set_st(0, r);
            // the low three quotient bits go to C0 C3 C1; C2 = 0: the reduction is complete
            let qb = if q.is_finite() { (q.abs() % 8.0) as u16 } else { 0 };
            let bit = |k: u16, cc: u16| if qb & k != 0 { cc } else { 0 };
            c.set_cc(bit(4, C0) | bit(2, C3) | bit(1, C1));
        }
        X87::Fxtract => {
            if x == 0.0 || !x.is_finite() {
                c.set_st(0, if x == 0.0 { f64::NEG_INFINITY } else { x.abs() });
                c.fpush(x);
            } else {
                // exponent from the bits, so that 1 <= |significand| < 2 exactly
                let e = (f64_to_f80(x)[8] as i32 | ((f64_to_f80(x)[9] as i32 & 0x7f) << 8)) - 16383;
                c.set_st(0, e as f64);
                c.fpush(scale(x, -e as f64));
            }
        }
    }
}

/// Operations for `rt_fpu_round`.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum FpuOp {
    Add,
    Sub,
    Mul,
    Div,
    Sqrt,
    /// round x itself (fst to a float)
    Round,
}

/// x87 arithmetic under a control word other than "53 bits, to nearest" (generated code
/// does that case itself): the result rounded to the control word's precision (24 bits;
/// 64 is taken as 53) in its rounding mode. The renderer runs with 0xc7f: 24 bits,
/// truncating. The result is computed together with the sign of its rounding error, so
/// rounding it again gives what the FPU gives in one step.
pub extern "C" fn rt_fpu_round(op: u32, x: f64, y: f64, cw: u32) -> f64 {
    let sign = |e: f64| if e > 0.0 { 1 } else if e < 0.0 { -1 } else { 0 };
    let (s, err) = match op {
        o if o == FpuOp::Add as u32 || o == FpuOp::Sub as u32 => {
            let y = if o == FpuOp::Sub as u32 { -y } else { y };
            // TwoSum: s + e = x + y exactly
            let s = x + y;
            let b = s - x;
            (s, sign((x - (s - b)) + (y - b)))
        }
        o if o == FpuOp::Mul as u32 => {
            let p = x * y;
            (p, sign(x.mul_add(y, -p)))
        }
        o if o == FpuOp::Div as u32 => {
            let q = x / y;
            // x - q*y exactly; the quotient's error has its sign times y's
            (q, sign((-q).mul_add(y, x)) * sign(y))
        }
        o if o == FpuOp::Sqrt as u32 => {
            let r = x.sqrt();
            (r, sign((-r).mul_add(r, x)))
        }
        _ => (x, 0),
    };
    if !s.is_finite() || s == 0.0 {
        return s;
    }
    let rc = (cw >> 10) & 3;
    // storing to a float always rounds to 24 bits
    if (cw >> 8) & 3 != 0 && op != FpuOp::Round as u32 {
        // 53 bits: s is already right for "to nearest"
        return match (rc, err) {
            (1, -1) => s.next_down(),
            (2, 1) => s.next_up(),
            (3, -1) if s > 0.0 => s.next_down(),
            (3, 1) if s < 0.0 => s.next_up(),
            _ => s,
        };
    }
    // 24 bits: the representable neighbours below and above the exact result
    let lo = floor24(s, err);
    let hi = -floor24(-s, -err);
    if lo == hi {
        return lo;
    }
    match rc {
        1 => lo,
        2 => hi,
        3 => {
            if s > 0.0 {
                lo
            } else {
                hi
            }
        }
        _ => {
            let mid = (lo + hi) / 2.0;
            if s < mid || (s == mid && err < 0) {
                lo
            } else if s > mid || err > 0 {
                hi
            } else if (lo as f32).to_bits() & 1 == 0 {
                lo // a tie: to even
            } else {
                hi
            }
        }
    }
}

/// The largest 24-bit float <= s + e, where e is a rounding error smaller than half an ulp
/// of s, given only by its sign.
fn floor24(s: f64, err: i32) -> f64 {
    let floor = |v: f64| {
        let exp = ((v.to_bits() >> 52) & 0x7ff) as i64 - 1023;
        let unit = f64::from_bits(((exp - 23 + 1023).max(1) as u64) << 52);
        (v / unit).floor() * unit
    };
    // s itself representable but the exact value just below it: the 24-bit float below s
    if floor(s) == s && err < 0 { floor(s.next_down()) } else { floor(s) }
}
