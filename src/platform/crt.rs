//! C runtime functions of the game's two programs done by the host: they are called so often
//! that the recompiled code costs much of a frame.
//!
//! `__ftol` (0x4ae390 in the EXE, 0x10014210 in the DLL; 1.5 million calls a second in a
//! mission): st(0) to a 64-bit integer, truncated, popped, in edx:eax. The original switches
//! the FPU's rounding mode twice per call (fnstcw, fldcw), which is slow in recompiled x87.

use crate::rt::{Cpu, EAX, EDX, ESP};

pub const FTOL_EXE: u32 = 0x4ae390;
pub const FTOL_DLL: u32 = 0x1001_4210;

pub fn ftol(c: &mut Cpu) {
    let v = c.st(0);
    c.fpop();
    // out of range and NaN: the "integer indefinite" value fistp stores
    let i = if v.is_nan() || v >= 9.223_372_036_854_775_807e18 || v < -9.223_372_036_854_775_808e18 { i64::MIN } else { v.trunc() as i64 };
    c.r[EAX] = i as u32;
    c.r[EDX] = (i >> 32) as u32;
    // ret
    c.r[ESP] += 4;
}
