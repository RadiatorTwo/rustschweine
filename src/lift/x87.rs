//! The x87 FPU. Registers are doubles in `Cpu::st` (the CRT sets 53-bit precision, so every
//! result is rounded to a double anyway); TOP is a Cranelift variable like the integer
//! registers, so ST(i) is the slot `(top + i) & 7`, found at run time. Things that are rare
//! or need libm (transcendental functions, 80-bit memory operands) go to `rt_x87`.

use super::{Lifter, mf, ty};
use crate::rt::{Cpu, FpuOp, X87};
use cranelift_codegen::ir::{InstBuilder, Value, condcodes::FloatCC, condcodes::IntCC, types::*};
use iced_x86::{Instruction, MemorySize, Mnemonic, OpKind, Register};
use std::mem::offset_of;

const C0: i64 = 0x100;
const C2: i64 = 0x400;
const C3: i64 = 0x4000;

pub fn is_x87(m: Mnemonic) -> bool {
    use Mnemonic::*;
    matches!(
        m,
        Fld | Fst | Fstp | Fild | Fist | Fistp | Fisttp | Fxch | Fadd | Faddp | Fiadd | Fsub | Fsubp | Fisub | Fsubr
            | Fsubrp | Fisubr | Fmul | Fmulp | Fimul | Fdiv | Fdivp | Fidiv | Fdivr | Fdivrp | Fidivr | Fchs | Fabs
            | Fsqrt | Frndint | Fld1 | Fldz | Fldpi | Fldl2e | Fldl2t | Fldlg2 | Fldln2 | Fcom | Fcomp | Fcompp
            | Fucom | Fucomp | Fucompp | Ficom | Ficomp | Ftst | Fcomi | Fcomip | Fucomi | Fucomip | Fnstsw | Fnstcw
            | Fldcw | Fnclex | Fninit | Ffree | Fincstp | Fdecstp | Fnop | Fxam | Fsin | Fcos | Fsincos | Fptan
            | Fpatan | F2xm1 | Fyl2x | Fyl2xp1 | Fscale | Fprem | Fprem1 | Fxtract
    )
}

/// Why an x87 instruction cannot be lifted, if it cannot (operand forms the game never uses).
pub fn unsupported(i: &Instruction) -> Option<String> {
    use MemorySize::*;
    if i.op_kind(0) == OpKind::Memory || i.op_kind(1) == OpKind::Memory {
        let ok = matches!(i.memory_size(), Float32 | Float64 | Float80 | Int16 | Int32 | Int64 | UInt16);
        if !ok {
            return Some(format!("{:?} {:?}", i.mnemonic(), i.memory_size()));
        }
    }
    None
}

impl Lifter<'_, '_> {
    // ---- the register stack --------------------------------------------------------------
    /// Host pointer to the physical slot of ST(k), as a byte offset into the 8-entry arrays.
    fn slot(&mut self, k: u32) -> Value {
        let t = self.b.use_var(self.top);
        let t = if k == 0 { t } else { self.b.ins().iadd_imm_s(t, k as i64) };
        let t = self.b.ins().band_imm_u(t, 7);
        let t = self.b.ins().uextend(I64, t);
        let t = self.b.ins().ishl_imm_u(t, 3);
        self.b.ins().iadd(self.cpu, t)
    }
    fn st(&mut self, k: u32) -> Value {
        let p = self.slot(k);
        self.b.ins().load(F64, mf(), p, offset_of!(Cpu, st) as i32)
    }
    fn set_st(&mut self, k: u32, v: Value) {
        let p = self.slot(k);
        self.b.ins().store(mf(), v, p, offset_of!(Cpu, st) as i32);
    }
    fn add_top(&mut self, d: i64) {
        let t = self.b.use_var(self.top);
        let t = self.b.ins().iadd_imm_s(t, d);
        let t = self.b.ins().band_imm_u(t, 7);
        self.b.def_var(self.top, t);
    }
    fn fpush(&mut self, v: Value) {
        self.add_top(-1);
        self.set_st(0, v);
    }
    fn fpop(&mut self) {
        self.add_top(1);
    }

    /// Copy ST(src) with its exact integer (see `Cpu::st_int`) to ST(dst).
    fn copy_st(&mut self, dst: u32, src: u32) {
        let (ps, pd) = (self.slot(src), self.slot(dst));
        for (off, t) in [(offset_of!(Cpu, st), F64), (offset_of!(Cpu, st_int), I64), (offset_of!(Cpu, st_int_of), I64)] {
            let v = self.b.ins().load(t, mf(), ps, off as i32);
            self.b.ins().store(mf(), v, pd, off as i32);
        }
    }

    fn fsw(&mut self) -> Value {
        self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fsw) as i32)
    }
    fn set_fsw(&mut self, v: Value) {
        self.b.ins().store(mf(), v, self.cpu, offset_of!(Cpu, fsw) as i32);
    }

    // ---- memory operands -------------------------------------------------------------------
    /// The memory operand as a double (float or integer, by its size).
    fn fmem(&mut self, i: &Instruction) -> Value {
        let a = self.addr(i);
        match i.memory_size() {
            MemorySize::Float32 => {
                let v = self.load_t(a, F32);
                self.b.ins().fpromote(F64, v)
            }
            MemorySize::Float64 => self.load_t(a, F64),
            MemorySize::Int16 => {
                let v = self.load(a, 2);
                let v = self.b.ins().sextend(I32, v);
                self.b.ins().fcvt_from_sint(F64, v)
            }
            MemorySize::Int32 => {
                let v = self.load(a, 4);
                self.b.ins().fcvt_from_sint(F64, v)
            }
            MemorySize::Int64 => {
                let v = self.load(a, 8);
                self.b.ins().fcvt_from_sint(F64, v)
            }
            s => panic!("x87 memory operand {s:?}"),
        }
    }

    fn load_t(&mut self, a: Value, t: cranelift_codegen::ir::Type) -> Value {
        let p = self.host(a);
        self.b.ins().load(t, mf(), p, 0)
    }

    /// Run `rt_x87` on the synced state.
    fn x87_helper(&mut self, op: X87, addr: Option<Value>) {
        self.store_state();
        let op = self.iconst(I32, op as i64);
        let a = addr.unwrap_or_else(|| self.iconst(I32, 0));
        self.b.ins().call(self.refs.x87, &[self.cpu, op, a]);
        self.load_state();
    }

    // ---- conversions -----------------------------------------------------------------------
    /// Round to an integer as the control word's rounding mode says.
    fn round_rc(&mut self, v: Value) -> Value {
        let cw = self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fcw) as i32);
        let rc = self.b.ins().ushr_imm_u(cw, 10);
        let rc = self.b.ins().band_imm_u(rc, 3);
        let near = self.b.ins().nearest(v);
        let down = self.b.ins().floor(v);
        let up = self.b.ins().ceil(v);
        let trunc = self.b.ins().trunc(v);
        let (is1, is2, is3) = (
            self.b.ins().icmp_imm_u(IntCC::Equal, rc, 1),
            self.b.ins().icmp_imm_u(IntCC::Equal, rc, 2),
            self.b.ins().icmp_imm_u(IntCC::Equal, rc, 3),
        );
        let r = self.b.ins().select(is1, down, near);
        let r = self.b.ins().select(is2, up, r);
        self.b.ins().select(is3, trunc, r)
    }

    /// A double as a `size`-byte integer, rounded by the control word (`truncate` for
    /// fisttp); out of range or NaN gives the "integer indefinite" (the smallest integer).
    fn to_int(&mut self, v: Value, size: u32, truncate: bool) -> Value {
        let r = if truncate { self.b.ins().trunc(v) } else { self.round_rc(v) };
        let lim = (1u64 << (8 * size - 1)) as f64;
        let (lo, hi) = (self.b.ins().f64const(-lim), self.b.ins().f64const(lim));
        let ge = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, r, lo);
        let lt = self.b.ins().fcmp(FloatCC::LessThan, r, hi);
        let fits = self.b.ins().band(ge, lt);
        let x = self.b.ins().fcvt_to_sint_sat(if size == 8 { I64 } else { I32 }, r);
        let x = if size == 2 { self.b.ins().ireduce(I16, x) } else { x };
        let min = self.iconst(ty(size), (1u64 << (8 * size - 1)) as i64);
        self.b.ins().select(fits, x, min)
    }

    /// x op y as the FPU rounds it. Doubles to nearest when the control word asks for 53
    /// or 64 bits to nearest; anything else (the renderer's 24 bits, truncating) goes to
    /// `rt_fpu_round`.
    fn arith(&mut self, op: FpuOp, x: Value, y: Value) -> Value {
        let fast = match op {
            FpuOp::Add => self.b.ins().fadd(x, y),
            FpuOp::Sub => self.b.ins().fsub(x, y),
            FpuOp::Mul => self.b.ins().fmul(x, y),
            FpuOp::Div => self.b.ins().fdiv(x, y),
            FpuOp::Sqrt => self.b.ins().sqrt(x),
            FpuOp::Round => self.b.ins().fdemote(F32, x),
        };
        let fast = if matches!(op, FpuOp::Round) { self.b.ins().fpromote(F64, fast) } else { fast };
        let cw = self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fcw) as i32);
        let cw = self.b.ins().uextend(I32, cw);
        // rounding control 00 (to nearest) and precision control 1x (53 or 64 bits)
        let mode = self.b.ins().band_imm_u(cw, 0xe00);
        let usual = self.b.ins().icmp_imm_u(IntCC::Equal, mode, 0x200);
        let (slow, done) = (self.b.create_block(), self.b.create_block());
        self.b.append_block_param(done, F64);
        self.b.ins().brif(usual, done, &[fast.into()], slow, &[]);
        self.b.switch_to_block(slow);
        let o = self.iconst(I32, op as i64);
        let call = self.b.ins().call(self.refs.fpu_round, &[o, x, y, cw]);
        let r = self.b.inst_results(call)[0];
        self.b.ins().jump(done, &[r.into()]);
        self.b.switch_to_block(done);
        self.b.block_params(done)[0]
    }

    // ---- comparisons -----------------------------------------------------------------------
    /// fcom and friends: C3 C2 C0 = 000 (a > b), 001 (a < b), 100 (equal), 111 (unordered).
    fn compare_fsw(&mut self, a: Value, b: Value) {
        let bit = |l: &mut Self, cc: FloatCC, mask: i64| {
            let c = l.b.ins().fcmp(cc, a, b);
            let c = l.b.ins().uextend(I16, c);
            l.b.ins().imul_imm_u(c, mask)
        };
        let c0 = bit(self, FloatCC::UnorderedOrLessThan, C0);
        let c2 = bit(self, FloatCC::Unordered, C2);
        let c3 = bit(self, FloatCC::UnorderedOrEqual, C3);
        let sw = self.fsw();
        let sw = self.b.ins().band_imm_u(sw, 0xffff & !(C0 | C2 | C3 | 0x200));
        let sw = self.b.ins().bor(sw, c0);
        let sw = self.b.ins().bor(sw, c2);
        let sw = self.b.ins().bor(sw, c3);
        self.set_fsw(sw);
    }

    /// fcomi and friends set ZF, PF, CF like an unsigned integer compare (all three if unordered).
    fn compare_eflags(&mut self, a: Value, b: Value) {
        let z = self.b.ins().fcmp(FloatCC::UnorderedOrEqual, a, b);
        let p = self.b.ins().fcmp(FloatCC::Unordered, a, b);
        let c = self.b.ins().fcmp(FloatCC::UnorderedOrLessThan, a, b);
        self.set_flag(self.zf, z);
        self.set_flag(self.pf, p);
        self.set_flag(self.cf, c);
        self.zero_flags(&[self.of, self.sf]);
    }

    // ---- instructions ----------------------------------------------------------------------
    pub(super) fn x87(&mut self, i: &Instruction) {
        use Mnemonic::*;
        let m = i.mnemonic();
        let sti = |k: u32| i.op_register(k).number() as u32 - Register::ST0.number() as u32;
        let mem0 = i.op_count() > 0 && i.op0_kind() == OpKind::Memory;
        match m {
            Fld | Fild => {
                if mem0 && i.memory_size() == MemorySize::Float80 {
                    let a = self.addr(i);
                    self.x87_helper(X87::LoadF80, Some(a));
                } else if mem0 {
                    let v = self.fmem(i);
                    self.fpush(v);
                    if i.memory_size() == MemorySize::Int64 {
                        // remember the integer: fistp qword of this double gives it back
                        let a = self.addr(i);
                        let x = self.load(a, 8);
                        let p = self.slot(0);
                        self.b.ins().store(mf(), x, p, offset_of!(Cpu, st_int) as i32);
                        let bits = self.b.ins().load(I64, mf(), p, offset_of!(Cpu, st) as i32);
                        self.b.ins().store(mf(), bits, p, offset_of!(Cpu, st_int_of) as i32);
                    }
                } else {
                    // fld st(i): push a copy (ST(i) is ST(i+1) after the push)
                    let k = sti(0);
                    self.add_top(-1);
                    self.copy_st(0, k + 1);
                }
            }
            Fst | Fstp => {
                if mem0 && i.memory_size() == MemorySize::Float80 {
                    let a = self.addr(i);
                    self.x87_helper(X87::StoreF80, Some(a));
                } else if mem0 {
                    let v = self.st(0);
                    let a = self.addr(i);
                    let v = if i.memory_size() == MemorySize::Float32 {
                        // rounded to a float as the control word says
                        let v = self.arith(FpuOp::Round, v, v);
                        self.b.ins().fdemote(F32, v)
                    } else {
                        v
                    };
                    self.store(a, v);
                } else {
                    let k = sti(0);
                    if k != 0 {
                        self.copy_st(k, 0);
                    }
                }
                if m == Fstp && !(mem0 && i.memory_size() == MemorySize::Float80) {
                    self.fpop();
                }
            }
            Fist | Fistp | Fisttp => {
                let n = i.memory_size().size() as u32;
                let v = self.st(0);
                let mut x = self.to_int(v, n, m == Fisttp);
                if n == 8 {
                    // the exact integer, if this double came from a fild qword
                    let p = self.slot(0);
                    let bits = self.b.ins().load(I64, mf(), p, offset_of!(Cpu, st) as i32);
                    let of = self.b.ins().load(I64, mf(), p, offset_of!(Cpu, st_int_of) as i32);
                    let exact = self.b.ins().load(I64, mf(), p, offset_of!(Cpu, st_int) as i32);
                    let same = self.b.ins().icmp(IntCC::Equal, bits, of);
                    x = self.b.ins().select(same, exact, x);
                }
                let a = self.addr(i);
                self.store(a, x);
                if m != Fist {
                    self.fpop();
                }
            }
            Fxch => {
                let k = sti(1); // fxch st(0), st(i)
                let (p0, pk) = (self.slot(0), self.slot(k));
                for (off, t) in [(offset_of!(Cpu, st), F64), (offset_of!(Cpu, st_int), I64), (offset_of!(Cpu, st_int_of), I64)] {
                    let a = self.b.ins().load(t, mf(), p0, off as i32);
                    let b = self.b.ins().load(t, mf(), pk, off as i32);
                    self.b.ins().store(mf(), b, p0, off as i32);
                    self.b.ins().store(mf(), a, pk, off as i32);
                }
            }
            Fadd | Faddp | Fiadd | Fsub | Fsubp | Fisub | Fsubr | Fsubrp | Fisubr | Fmul | Fmulp | Fimul | Fdiv | Fdivp
            | Fidiv | Fdivr | Fdivrp | Fidivr => {
                // dst = dst op src (or src op dst for the reversed forms)
                let (dst, src) = if mem0 {
                    (0, self.fmem(i))
                } else if i.op_count() == 0 {
                    (1, self.st(0)) // faddp with no operands: st(1), st(0)
                } else {
                    let (d, s) = (sti(0), sti(1));
                    (d, self.st(s))
                };
                let d = self.st(dst);
                let rev = matches!(m, Fsubr | Fsubrp | Fisubr | Fdivr | Fdivrp | Fidivr);
                let (x, y) = if rev { (src, d) } else { (d, src) };
                let op = match m {
                    Fadd | Faddp | Fiadd => FpuOp::Add,
                    Fsub | Fsubp | Fisub | Fsubr | Fsubrp | Fisubr => FpuOp::Sub,
                    Fmul | Fmulp | Fimul => FpuOp::Mul,
                    _ => FpuOp::Div,
                };
                let r = self.arith(op, x, y);
                self.set_st(dst, r);
                if matches!(m, Faddp | Fsubp | Fsubrp | Fmulp | Fdivp | Fdivrp) {
                    self.fpop();
                }
            }
            Fchs | Fabs | Fsqrt | Frndint => {
                let v = self.st(0);
                let r = match m {
                    Fchs => self.b.ins().fneg(v),
                    Fabs => self.b.ins().fabs(v),
                    Fsqrt => self.arith(FpuOp::Sqrt, v, v),
                    _ => self.round_rc(v),
                };
                self.set_st(0, r);
            }
            Fld1 | Fldz | Fldpi | Fldl2e | Fldl2t | Fldlg2 | Fldln2 => {
                use std::f64::consts::*;
                let c = match m {
                    Fld1 => 1.0,
                    Fldz => 0.0,
                    Fldpi => PI,
                    Fldl2e => LOG2_E,
                    Fldl2t => LOG2_10,
                    Fldlg2 => LOG10_2,
                    _ => LN_2,
                };
                let v = self.b.ins().f64const(c);
                self.fpush(v);
            }
            Fcom | Fcomp | Fucom | Fucomp | Ficom | Ficomp | Fcompp | Fucompp | Ftst => {
                let a = self.st(0);
                let b = if m == Ftst {
                    self.b.ins().f64const(0.0)
                } else if mem0 {
                    self.fmem(i)
                } else if i.op_count() == 0 {
                    self.st(1)
                } else {
                    let k = sti(i.op_count() - 1);
                    self.st(k)
                };
                self.compare_fsw(a, b);
                let pops = match m {
                    Fcompp | Fucompp => 2,
                    Fcomp | Fucomp | Ficomp => 1,
                    _ => 0,
                };
                for _ in 0..pops {
                    self.fpop();
                }
            }
            Fcomi | Fcomip | Fucomi | Fucomip => {
                let a = self.st(0);
                let b = self.st(sti(1));
                self.compare_eflags(a, b);
                if matches!(m, Fcomip | Fucomip) {
                    self.fpop();
                }
            }
            Fnstsw => {
                let sw = self.fsw();
                let sw = self.b.ins().band_imm_u(sw, 0xffff & !0x3800);
                let t = self.b.use_var(self.top);
                let t = self.b.ins().ireduce(I16, t);
                let t = self.b.ins().ishl_imm_u(t, 11);
                let v = self.b.ins().bor(sw, t);
                self.wr(i, 0, v);
            }
            Fnstcw => {
                let v = self.b.ins().load(I16, mf(), self.cpu, offset_of!(Cpu, fcw) as i32);
                self.wr(i, 0, v);
            }
            Fldcw => {
                let v = self.rd(i, 0, 2);
                self.b.ins().store(mf(), v, self.cpu, offset_of!(Cpu, fcw) as i32);
            }
            Fnclex => {
                let sw = self.fsw();
                let sw = self.b.ins().band_imm_u(sw, 0x7f00);
                self.set_fsw(sw);
            }
            Fninit => {
                let cw = self.iconst(I16, 0x37f);
                self.b.ins().store(mf(), cw, self.cpu, offset_of!(Cpu, fcw) as i32);
                let z = self.iconst(I16, 0);
                self.set_fsw(z);
                let z = self.iconst(I32, 0);
                self.b.def_var(self.top, z);
            }
            Fincstp => self.fpop(),
            Fdecstp => self.add_top(-1),
            Ffree | Fnop => {}
            _ => {
                let op = match m {
                    Fxam => X87::Fxam,
                    Fsin => X87::Fsin,
                    Fcos => X87::Fcos,
                    Fsincos => X87::Fsincos,
                    Fptan => X87::Fptan,
                    Fpatan => X87::Fpatan,
                    F2xm1 => X87::F2xm1,
                    Fyl2x => X87::Fyl2x,
                    Fyl2xp1 => X87::Fyl2xp1,
                    Fscale => X87::Fscale,
                    Fprem => X87::Fprem,
                    Fprem1 => X87::Fprem1,
                    Fxtract => X87::Fxtract,
                    _ => panic!("{m:?} passed the support check but has no lifting"),
                };
                self.x87_helper(op, None);
            }
        }
    }
}
