//! Lifting: one x86 function -> one Cranelift function.
//!
//! Registers and flags live in Cranelift variables (SSA), loaded from the `Cpu` struct on
//! entry and stored back around calls, returns and jumps out of the function. Every
//! instruction starts a block; a jump inside the function is a jump, a jump into another
//! function is a tail call (`return_call`, tail calling convention), so loops that span
//! functions do not grow the host stack. Flags nobody reads are removed by Cranelift.

use crate::discover::{Func, Program, direct_target};
use crate::rt::Cpu;
use crate::smc::{Field, Smc};
use cranelift_codegen::ir::{self, Block, FuncRef, GlobalValue, InstBuilder, MemFlagsData, SigRef, Type, Value, condcodes::IntCC, types::*};
use cranelift_codegen::isa::TargetFrontendConfig;
use cranelift_frontend::{FunctionBuilder, Switch, Variable};
use iced_x86::{ConditionCode, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::HashMap;
use std::mem::offset_of;

mod x87;

/// Host helpers and the guest function signature, imported into each function.
pub struct Refs {
    pub lookup: FuncRef,
    pub unimpl: FuncRef,
    pub trap: FuncRef,
    pub string: FuncRef,
    pub cpuid: FuncRef,
    pub rdtsc: FuncRef,
    pub x87: FuncRef,
    pub fpu_round: FuncRef,
    pub yield_: FuncRef,
    /// the address of `rt::POLL`, a symbol (not a constant: the code is cached, and the
    /// program's statics move from run to run)
    pub poll: GlobalValue,
    pub guest_sig: SigRef,
}

const TRAP_DIV0: i64 = 0;
const TRAP_INT: i64 = 1;

fn gpr_ok(r: Register) -> bool {
    r == Register::None || r.is_gpr32()
}

/// Why an instruction cannot be lifted yet, if it cannot. Checked before any code is
/// emitted, so an unsupported instruction becomes a clean call to `rt_unimpl`.
pub fn unsupported(i: &Instruction) -> Option<String> {
    use Mnemonic::*;
    for k in 0..i.op_count() {
        match i.op_kind(k) {
            OpKind::Register => {
                let r = i.op_register(k);
                // segment registers only as push/pop es (bytes in code that is never run)
                let seg = r.is_segment_register() && matches!(i.mnemonic(), Mnemonic::Push | Mnemonic::Pop);
                if !(r.is_gpr8() || r.is_gpr16() || r.is_gpr32() || r.is_st() || r.is_mm() || seg) {
                    return Some(format!("register {r:?}"));
                }
            }
            OpKind::Memory => {
                if !gpr_ok(i.memory_base()) || !gpr_ok(i.memory_index()) {
                    return Some("16-bit addressing".into());
                }
                if !matches!(i.memory_segment(), Register::DS | Register::SS | Register::ES | Register::CS | Register::FS) {
                    return Some(format!("segment {:?}", i.memory_segment()));
                }
            }
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8_2nd
            | OpKind::NearBranch32
            | OpKind::NearBranch16
            | OpKind::MemorySegSI
            | OpKind::MemorySegESI
            | OpKind::MemoryESDI
            | OpKind::MemoryESEDI => {}
            k => return Some(format!("operand {k:?}")),
        }
    }
    let m = i.mnemonic();
    if x87::is_x87(m) {
        return x87::unsupported(i);
    }
    let ok = match m {
        Mov | Movzx | Movsx | Lea | Add | Adc | Sub | Sbb | Cmp | And | Or | Xor | Test | Inc | Dec | Neg | Not | Shl
        | Sal | Shr | Sar | Rol | Ror | Rcl | Rcr | Push | Pop | Pushad | Popad | Pushfd | Popfd | Lahf | Sahf | Clc | Stc | Cmc
        | Cld | Std | Xchg | Bt | Bts | Btr | Btc | Cdq | Cwde | Cbw | Cwd | Xlatb | Div | Idiv | Leave | Nop | Wait
        | Pause | Cli | Sti | Cpuid | Rdtsc | Movsb | Movsw | Movsd | Stosb | Stosw | Stosd | Lodsb | Lodsw | Lodsd
        | Scasb | Scasw | Scasd | Cmpsb | Cmpsw | Cmpsd | Jmp | Call | Ret | Loop | Loope | Loopne | Jecxz | Jcxz
        | Int3 | Hlt | Ud2 | Int | Into | Emms | In | Out => true,
        // port input without rep (the gameport)
        Insb | Insw | Insd => !(i.has_rep_prefix() || i.has_repe_prefix() || i.has_repne_prefix()),
        Movd | Movq => (0..i.op_count()).all(|k| i.op_kind(k) != OpKind::Register || !i.op_register(k).is_xmm()),
        Bswap => i.op0_register().is_gpr32(),
        Shld | Shrd => i.op0_register().is_gpr32() || (i.op0_kind() == OpKind::Memory && i.memory_size().size() == 4),
        Mul => i.op_count() == 1 && op_size(i, 0) != 2,
        Imul => !(i.op_count() == 1 && op_size(i, 0) == 2),
        Enter => i.immediate8_2nd() == 0,
        _ => i.condition_code() != ConditionCode::None && (i.is_jcc_short_or_near() || is_setcc(i) || is_cmovcc(i)),
    };
    // string instructions also exist as SSE (movsd/cmpsd xmm): only the string forms here
    let string = matches!(m, Movsd | Cmpsd) && !matches!(i.op0_kind(), OpKind::MemoryESEDI | OpKind::MemorySegESI);
    if ok && !string { None } else { Some(format!("{m:?}")) }
}

pub fn is_setcc(i: &Instruction) -> bool {
    use Mnemonic::*;
    matches!(i.mnemonic(), Seto | Setno | Setb | Setae | Sete | Setne | Setbe | Seta | Sets | Setns | Setp | Setnp | Setl | Setge | Setle | Setg)
}

pub fn is_cmovcc(i: &Instruction) -> bool {
    use Mnemonic::*;
    matches!(
        i.mnemonic(),
        Cmovo | Cmovno | Cmovb | Cmovae | Cmove | Cmovne | Cmovbe | Cmova | Cmovs | Cmovns | Cmovp | Cmovnp | Cmovl | Cmovge | Cmovle | Cmovg
    )
}

fn op_size(i: &Instruction, k: u32) -> u32 {
    match i.op_kind(k) {
        OpKind::Register => i.op_register(k).size() as u32,
        OpKind::Memory => i.memory_size().size() as u32,
        OpKind::Immediate8 | OpKind::Immediate8_2nd => 1,
        OpKind::Immediate16 | OpKind::Immediate8to16 => 2,
        _ => 4,
    }
}

fn ty(size: u32) -> Type {
    match size {
        1 => I8,
        2 => I16,
        4 => I32,
        8 => I64,
        _ => panic!("operand size {size}"),
    }
}

pub struct Lifter<'a, 'f> {
    pub b: FunctionBuilder<'f>,
    f: &'a Func,
    prog: &'a Program,
    smc: &'a Smc,
    refs: &'a Refs,
    get_fn: &'a mut dyn FnMut(&mut ir::Function, u32) -> FuncRef,
    fns: HashMap<u32, FuncRef>,
    cpu: Value,
    mem: Value,
    r: [Variable; 8],
    cf: Variable,
    pf: Variable,
    zf: Variable,
    sf: Variable,
    of: Variable,
    /// x87 TOP (0..7)
    top: Variable,
    cfg: TargetFrontendConfig,
    blocks: HashMap<u32, Block>,
    /// blocks that leave the function towards an address
    exits: HashMap<u32, Block>,
    dyn_block: Option<Block>,
    /// mnemonics that could not be lifted (for statistics)
    pub missing: Vec<String>,
}

fn mf() -> MemFlagsData {
    MemFlagsData::new()
}

impl<'a, 'f> Lifter<'a, 'f> {
    pub fn new(
        mut b: FunctionBuilder<'f>,
        f: &'a Func,
        prog: &'a Program,
        smc: &'a Smc,
        refs: &'a Refs,
        get_fn: &'a mut dyn FnMut(&mut ir::Function, u32) -> FuncRef,
        cfg: TargetFrontendConfig,
    ) -> Self {
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let cpu = b.block_params(entry)[0];
        let mem = b.ins().load(I64, mf(), cpu, offset_of!(Cpu, mem) as i32);
        let r = std::array::from_fn(|_| b.declare_var(I32));
        let (cf, pf, zf, sf, of) = (b.declare_var(I8), b.declare_var(I8), b.declare_var(I8), b.declare_var(I8), b.declare_var(I8));
        let top = b.declare_var(I32);
        let mut l = Lifter {
            b,
            f,
            prog,
            smc,
            refs,
            get_fn,
            fns: HashMap::new(),
            cpu,
            mem,
            r,
            cf,
            pf,
            zf,
            sf,
            of,
            top,
            cfg,
            blocks: HashMap::new(),
            exits: HashMap::new(),
            dyn_block: None,
            missing: Vec::new(),
        };
        l.load_state();
        l
    }

    // ---- state -----------------------------------------------------------------------------
    fn flag_vars(&self) -> [(Variable, usize); 5] {
        [
            (self.cf, offset_of!(Cpu, cf)),
            (self.pf, offset_of!(Cpu, pf)),
            (self.zf, offset_of!(Cpu, zf)),
            (self.sf, offset_of!(Cpu, sf)),
            (self.of, offset_of!(Cpu, of)),
        ]
    }

    fn load_state(&mut self) {
        for k in 0..8 {
            let v = self.b.ins().load(I32, mf(), self.cpu, (offset_of!(Cpu, r) + 4 * k) as i32);
            self.b.def_var(self.r[k], v);
        }
        for (var, off) in self.flag_vars() {
            let v = self.b.ins().load(I8, mf(), self.cpu, off as i32);
            self.b.def_var(var, v);
        }
        let t = self.b.ins().load(I32, mf(), self.cpu, offset_of!(Cpu, top) as i32);
        self.b.def_var(self.top, t);
    }

    fn store_state(&mut self) {
        for k in 0..8 {
            let v = self.b.use_var(self.r[k]);
            self.b.ins().store(mf(), v, self.cpu, (offset_of!(Cpu, r) + 4 * k) as i32);
        }
        for (var, off) in self.flag_vars() {
            let v = self.b.use_var(var);
            self.b.ins().store(mf(), v, self.cpu, off as i32);
        }
        let t = self.b.use_var(self.top);
        self.b.ins().store(mf(), t, self.cpu, offset_of!(Cpu, top) as i32);
    }

    fn reg(&mut self, k: usize) -> Value {
        self.b.use_var(self.r[k])
    }
    fn set_reg(&mut self, k: usize, v: Value) {
        self.b.def_var(self.r[k], v);
    }
    fn iconst(&mut self, t: Type, v: i64) -> Value {
        self.b.ins().iconst(t, v)
    }
    fn flag(&mut self, v: Variable) -> Value {
        self.b.use_var(v)
    }
    fn set_flag(&mut self, var: Variable, v: Value) {
        self.b.def_var(var, v);
    }
    fn zero_flags(&mut self, vars: &[Variable]) {
        let z = self.iconst(I8, 0);
        for &v in vars {
            self.b.def_var(v, z);
        }
    }

    // ---- memory ----------------------------------------------------------------------------
    fn host(&mut self, a: Value) -> Value {
        let a = self.b.ins().uextend(I64, a);
        self.b.ins().iadd(self.mem, a)
    }
    fn load(&mut self, a: Value, size: u32) -> Value {
        let p = self.host(a);
        self.b.ins().load(ty(size), mf(), p, 0)
    }
    fn store(&mut self, a: Value, v: Value) {
        let p = self.host(a);
        self.b.ins().store(mf(), v, p, 0);
    }

    /// The effective address of the instruction's memory operand (guest address, i32).
    fn addr(&mut self, i: &Instruction) -> Value {
        let mut a = match self.smc.disp.get(&i.ip32()) {
            Some(&f) => self.field(f, 4),
            None => self.iconst(I32, i.memory_displacement32() as i64),
        };
        if i.memory_base() != Register::None {
            let b = self.reg(gpr(i.memory_base()));
            a = self.b.ins().iadd(a, b);
        }
        if i.memory_index() != Register::None {
            let mut x = self.reg(gpr(i.memory_index()));
            let s = i.memory_index_scale();
            if s != 1 {
                x = self.b.ins().imul_imm_u(x, s as i64);
            }
            a = self.b.ins().iadd(a, x);
        }
        if i.memory_segment() == Register::FS {
            let fs = self.b.ins().load(I32, mf(), self.cpu, offset_of!(Cpu, fs_base) as i32);
            a = self.b.ins().iadd(a, fs);
        }
        a
    }

    /// A code field the game rewrites, read from guest memory now; narrower fields are
    /// sign-extended like the CPU does.
    fn field(&mut self, f: Field, size: u32) -> Value {
        let a = self.iconst(I32, f.at as i64);
        let v = self.load(a, f.size);
        if f.size < size {
            self.b.ins().sextend(ty(size), v)
        } else if f.size > size {
            self.b.ins().ireduce(ty(size), v)
        } else {
            v
        }
    }

    /// The instruction's immediate when the game rewrites it (else None: use the decoded one).
    fn dyn_imm(&mut self, i: &Instruction, size: u32) -> Option<Value> {
        let f = *self.smc.imm.get(&i.ip32())?;
        Some(self.field(f, size))
    }

    // ---- operands --------------------------------------------------------------------------
    fn read_reg(&mut self, r: Register) -> Value {
        let k = gpr(r);
        let full = self.reg(k);
        match r.size() {
            4 => full,
            2 => self.b.ins().ireduce(I16, full),
            _ if matches!(r, Register::AH | Register::CH | Register::DH | Register::BH) => {
                let s = self.b.ins().ushr_imm_u(full, 8);
                self.b.ins().ireduce(I8, s)
            }
            _ => self.b.ins().ireduce(I8, full),
        }
    }

    fn write_reg(&mut self, r: Register, v: Value) {
        let k = gpr(r);
        let new = match r.size() {
            4 => v,
            2 => {
                let old = self.reg(k);
                let old = self.b.ins().band_imm_u(old, 0xffff_0000u32 as i64);
                let v = self.b.ins().uextend(I32, v);
                self.b.ins().bor(old, v)
            }
            _ if matches!(r, Register::AH | Register::CH | Register::DH | Register::BH) => {
                let old = self.reg(k);
                let old = self.b.ins().band_imm_u(old, !0xff00u32 as i64);
                let v = self.b.ins().uextend(I32, v);
                let v = self.b.ins().ishl_imm_u(v, 8);
                self.b.ins().bor(old, v)
            }
            _ => {
                let old = self.reg(k);
                let old = self.b.ins().band_imm_u(old, !0xffu32 as i64);
                let v = self.b.ins().uextend(I32, v);
                self.b.ins().bor(old, v)
            }
        };
        self.set_reg(k, new);
    }

    /// Operand `k` as a value of `size` bytes (immediates are cut to that size).
    fn rd(&mut self, i: &Instruction, k: u32, size: u32) -> Value {
        match i.op_kind(k) {
            OpKind::Register => self.read_reg(i.op_register(k)),
            OpKind::Memory => {
                let a = self.addr(i);
                self.load(a, size)
            }
            _ => {
                if let Some(v) = self.dyn_imm(i, size) {
                    return v;
                }
                let v = i.immediate(k);
                let v = if size >= 8 { v } else { v & ((1u64 << (8 * size)) - 1) };
                self.iconst(ty(size), v as i64)
            }
        }
    }

    fn wr(&mut self, i: &Instruction, k: u32, v: Value) {
        match i.op_kind(k) {
            OpKind::Register => self.write_reg(i.op_register(k), v),
            _ => {
                let a = self.addr(i);
                self.store(a, v)
            }
        }
    }

    fn push32(&mut self, v: Value) {
        let sp = self.reg(4);
        let sp = self.b.ins().iadd_imm_s(sp, -4);
        self.set_reg(4, sp);
        self.store(sp, v);
    }
    fn pop32(&mut self) -> Value {
        let sp = self.reg(4);
        let v = self.load(sp, 4);
        let sp = self.b.ins().iadd_imm_s(sp, 4);
        self.set_reg(4, sp);
        v
    }

    // ---- flags -----------------------------------------------------------------------------
    fn msb(&mut self, v: Value) -> Value {
        self.b.ins().icmp_imm_u(IntCC::SignedLessThan, v, 0)
    }

    fn set_zsp(&mut self, r: Value) {
        let z = self.b.ins().icmp_imm_u(IntCC::Equal, r, 0);
        let s = self.msb(r);
        let lo = if self.b.func.dfg.value_type(r) == I8 { r } else { self.b.ins().ireduce(I8, r) };
        let n = self.b.ins().popcnt(lo);
        let n = self.b.ins().band_imm_u(n, 1);
        let p = self.b.ins().icmp_imm_u(IntCC::Equal, n, 0);
        self.set_flag(self.zf, z);
        self.set_flag(self.sf, s);
        self.set_flag(self.pf, p);
    }

    fn cond(&mut self, cc: ConditionCode) -> Value {
        use ConditionCode as C;
        let (cf, pf, zf, sf, of) = (self.cf, self.pf, self.zf, self.sf, self.of);
        let (v, negate) = match cc {
            C::o | C::no => (self.flag(of), cc == C::no),
            C::b | C::ae => (self.flag(cf), cc == C::ae),
            C::e | C::ne => (self.flag(zf), cc == C::ne),
            C::s | C::ns => (self.flag(sf), cc == C::ns),
            C::p | C::np => (self.flag(pf), cc == C::np),
            C::be | C::a => {
                let (c, z) = (self.flag(cf), self.flag(zf));
                (self.b.ins().bor(c, z), cc == C::a)
            }
            C::l | C::ge => {
                let (s, o) = (self.flag(sf), self.flag(of));
                (self.b.ins().bxor(s, o), cc == C::ge)
            }
            C::le | C::g => {
                let (s, o, z) = (self.flag(sf), self.flag(of), self.flag(zf));
                let x = self.b.ins().bxor(s, o);
                (self.b.ins().bor(z, x), cc == C::g)
            }
            C::None => panic!("no condition"),
        };
        if negate { self.b.ins().bxor_imm_u(v, 1) } else { v }
    }

    // ---- control flow --------------------------------------------------------------------------
    fn guest_fn(&mut self, addr: u32) -> FuncRef {
        if let Some(&r) = self.fns.get(&addr) {
            return r;
        }
        let r = (self.get_fn)(self.b.func, addr);
        self.fns.insert(addr, r);
        r
    }

    /// Leave the function towards `tgt`: a tail call to a function, or through dispatch.
    fn exit_to(&mut self, tgt: Value, known: Option<u32>) {
        self.store_state();
        if let Some(t) = known.filter(|t| self.prog.fns.contains_key(t)) {
            let f = self.guest_fn(t);
            self.b.ins().return_call(f, &[self.cpu]);
        } else {
            let call = self.b.ins().call(self.refs.lookup, &[self.cpu, tgt]);
            let fp = self.b.inst_results(call)[0];
            self.b.ins().return_call_indirect(self.refs.guest_sig, fp, &[self.cpu]);
        }
    }

    /// The block that continues at `tgt`: the instruction's own block inside the function,
    /// or a new block that leaves the function.
    fn target_block(&mut self, tgt: u32) -> Block {
        if self.f.inside(tgt) {
            if let Some(&b) = self.blocks.get(&tgt) {
                return b;
            }
        }
        // filled at the end: Cranelift wants a block finished before switching away
        if let Some(&b) = self.exits.get(&tgt) {
            return b;
        }
        let blk = self.b.create_block();
        self.exits.insert(tgt, blk);
        blk
    }

    fn goto(&mut self, tgt: u32) {
        let blk = self.target_block(tgt);
        self.b.ins().jump(blk, &[]);
    }

    fn branch(&mut self, c: Value, tgt: u32, next: u32) {
        let (t, n) = (self.target_block(tgt), self.target_block(next));
        self.b.ins().brif(c, t, &[], n, &[]);
    }

    /// Jump to a target only known at run time, through the function's one shared switch.
    fn jump_dyn(&mut self, t: Value) {
        let blk = *self.dyn_block.get_or_insert_with(|| {
            let b = self.b.create_block();
            self.b.append_block_param(b, I32);
            b
        });
        self.b.ins().jump(blk, &[t.into()]);
    }

    fn call_to(&mut self, i: &Instruction, tgt: Value, known: Option<u32>) {
        let ret = self.iconst(I32, i.next_ip32() as i64);
        self.push32(ret);
        self.store_state();
        if let Some(t) = known.filter(|t| self.prog.fns.contains_key(t)) {
            let f = self.guest_fn(t);
            self.b.ins().call(f, &[self.cpu]);
        } else {
            let call = self.b.ins().call(self.refs.lookup, &[self.cpu, tgt]);
            let fp = self.b.inst_results(call)[0];
            self.b.ins().call_indirect(self.refs.guest_sig, fp, &[self.cpu]);
        }
        self.load_state();
        // an event handler asked to stop: return all the way out
        let stop = self.b.ins().load(I32, mf(), self.cpu, offset_of!(Cpu, stop) as i32);
        let (out, on) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(stop, out, &[], on, &[]);
        self.b.switch_to_block(out);
        self.b.ins().return_(&[]);
        self.b.switch_to_block(on);
    }

    fn trap(&mut self, addr: u32, kind: i64) {
        self.store_state();
        let (a, k) = (self.iconst(I32, addr as i64), self.iconst(I32, kind));
        self.b.ins().call(self.refs.trap, &[self.cpu, a, k]);
        self.b.ins().return_(&[]);
    }

    /// Run a host helper on the synced state.
    fn helper(&mut self, f: FuncRef, extra: Option<i64>) {
        self.store_state();
        match extra {
            Some(x) => {
                let x = self.iconst(I32, x);
                self.b.ins().call(f, &[self.cpu, x])
            }
            None => self.b.ins().call(f, &[self.cpu]),
        };
        self.load_state();
    }

    // ---- the function ---------------------------------------------------------------------------
    pub fn lift(mut self, code: &mut crate::discover::Code) -> Vec<String> {
        let addrs: Vec<u32> = self.f.ins.iter().copied().collect();
        for &a in &addrs {
            let b = self.b.create_block();
            self.blocks.insert(a, b);
        }
        if crate::rt::count_file().is_some() {
            let ptr = self.cfg.pointer_type();
            let at = self.b.ins().iconst(ptr, crate::rt::counter(self.f.entry) as *const _ as i64);
            let n = self.b.ins().load(I32, mf(), at, 0);
            let n = self.b.ins().iadd_imm_u(n, 1);
            self.b.ins().store(mf(), n, at, 0);
        }
        match self.blocks.get(&self.f.entry) {
            Some(&b) => {
                self.b.ins().jump(b, &[]);
            }
            None => self.trap(self.f.entry, TRAP_INT),
        }
        // loop heads: targets of backward branches inside the function
        let heads: std::collections::HashSet<u32> = addrs
            .iter()
            .filter_map(|&a| direct_target(&code.at(a).unwrap()).filter(|&t| t <= a && self.f.inside(t)))
            .collect();
        for &a in &addrs {
            let i = code.at(a).unwrap();
            let blk = self.blocks[&a];
            self.b.switch_to_block(blk);
            if heads.contains(&a) {
                self.poll();
            }
            self.emit(&i);
        }
        let mut exits: Vec<(u32, Block)> = self.exits.iter().map(|(&t, &b)| (t, b)).collect();
        exits.sort_unstable();
        for (tgt, blk) in exits {
            self.b.switch_to_block(blk);
            let t = self.iconst(I32, tgt as i64);
            self.exit_to(t, Some(tgt));
        }
        if let Some(d) = self.dyn_block {
            self.b.switch_to_block(d);
            let t = self.b.block_params(d)[0];
            let mut sw = Switch::new();
            for &a in &addrs {
                sw.set_entry(a as u128, self.blocks[&a]);
            }
            let out = self.b.create_block();
            sw.emit(&mut self.b, t, out);
            self.b.switch_to_block(out);
            self.exit_to(t, None);
        }
        self.b.seal_all_blocks();
        let missing = std::mem::take(&mut self.missing);
        self.b.finalize(self.cfg);
        missing
    }

    /// At a loop head: let another guest thread run if one is waiting for its turn.
    fn poll(&mut self) {
        let ptr = self.cfg.pointer_type();
        let at = self.b.ins().symbol_value(ptr, self.refs.poll);
        let v = self.b.ins().load(I32, mf(), at, 0);
        let (slow, on) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(v, slow, &[], on, &[]);
        self.b.switch_to_block(slow);
        self.helper(self.refs.yield_, None);
        self.b.ins().jump(on, &[]);
        self.b.switch_to_block(on);
    }

    /// One instruction, continuing at the next unless it ends its block.
    fn emit(&mut self, i: &Instruction) {
        let ends = match unsupported(i) {
            Some(why) => {
                self.missing.push(why);
                self.store_state();
                let x = self.iconst(I32, i.ip32() as i64);
                self.b.ins().call(self.refs.unimpl, &[self.cpu, x]);
                self.b.ins().return_(&[]);
                true
            }
            None => self.insn(i),
        };
        if !ends {
            self.goto(i.next_ip32());
        }
    }

    /// One instruction; true if it ended its block (jump, return, trap).
    fn insn(&mut self, i: &Instruction) -> bool {
        use Mnemonic::*;
        let m = i.mnemonic();
        match i.flow_control() {
            FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch | FlowControl::IndirectBranch => {
                return self.jump_insn(i);
            }
            FlowControl::Call | FlowControl::IndirectCall => {
                match direct_target(i) {
                    Some(_) if self.smc.imm.contains_key(&i.ip32()) => {
                        let t = self.rel_target(i);
                        self.call_to(i, t, None);
                    }
                    Some(t) => {
                        let v = self.iconst(I32, t as i64);
                        self.call_to(i, v, Some(t));
                    }
                    None => {
                        let v = self.rd(i, 0, 4);
                        self.call_to(i, v, None);
                    }
                }
                return false;
            }
            FlowControl::Return => {
                let n = if i.op_count() > 0 { i.immediate16() as i64 } else { 0 };
                let sp = self.reg(4);
                let sp = self.b.ins().iadd_imm_s(sp, 4 + n);
                self.set_reg(4, sp);
                self.store_state();
                self.b.ins().return_(&[]);
                return true;
            }
            _ => {}
        }
        if matches!(m, Int3 | Hlt | Ud2 | Int | Into) {
            self.trap(i.ip32(), TRAP_INT);
            return true;
        }
        if x87::is_x87(m) {
            self.x87(i);
            return false;
        }
        if is_setcc(i) {
            let c = self.cond(i.condition_code());
            self.wr(i, 0, c);
            return false;
        }
        if is_cmovcc(i) {
            let c = self.cond(i.condition_code());
            let n = op_size(i, 0);
            let (old, new) = (self.rd(i, 0, n), self.rd(i, 1, n));
            let v = self.b.ins().select(c, new, old);
            self.wr(i, 0, v);
            return false;
        }
        match m {
            Nop | Wait | Pause | Cli | Sti => {}
            Push | Pop if i.op0_kind() == OpKind::Register && i.op0_register().is_segment_register() => {
                // a flat selector; what is popped is dropped
                if m == Push {
                    let v = self.iconst(I32, 0x23);
                    self.push32(v);
                } else {
                    self.pop32();
                }
            }
            Mov => {
                let v = self.rd(i, 1, op_size(i, 0));
                self.wr(i, 0, v);
            }
            Movzx | Movsx => {
                let v = self.rd(i, 1, op_size(i, 1));
                let t = ty(op_size(i, 0));
                let v = if m == Movzx { self.b.ins().uextend(t, v) } else { self.b.ins().sextend(t, v) };
                self.wr(i, 0, v);
            }
            Lea => {
                let a = self.addr(i);
                let v = if op_size(i, 0) == 2 { self.b.ins().ireduce(I16, a) } else { a };
                self.wr(i, 0, v);
            }
            Add | Adc | Sub | Sbb | Cmp | And | Or | Xor | Test => self.alu(i, m),
            Inc | Dec => {
                let n = op_size(i, 0);
                let x = self.rd(i, 0, n);
                let r = if m == Inc { self.b.ins().iadd_imm_s(x, 1) } else { self.b.ins().iadd_imm_s(x, -1) };
                let min = (1u64 << (8 * n - 1)) as i64;
                let o = self.b.ins().icmp_imm_u(IntCC::Equal, if m == Inc { r } else { x }, min);
                self.set_flag(self.of, o);
                self.set_zsp(r);
                self.wr(i, 0, r);
            }
            Neg => {
                let n = op_size(i, 0);
                let x = self.rd(i, 0, n);
                let r = self.b.ins().ineg(x);
                let c = self.b.ins().icmp_imm_u(IntCC::NotEqual, x, 0);
                let o = self.b.ins().icmp_imm_u(IntCC::Equal, x, (1u64 << (8 * n - 1)) as i64);
                self.set_flag(self.cf, c);
                self.set_flag(self.of, o);
                self.set_zsp(r);
                self.wr(i, 0, r);
            }
            Not => {
                let x = self.rd(i, 0, op_size(i, 0));
                let r = self.b.ins().bnot(x);
                self.wr(i, 0, r);
            }
            Shl | Sal | Shr | Sar | Rol | Ror | Rcl | Rcr => self.shift(i, m),
            Shld | Shrd => self.shxd(i, m),
            Push => {
                let n = op_size(i, 0);
                if n == 2 && !matches!(i.op0_kind(), OpKind::Immediate8to16 | OpKind::Immediate16) {
                    let v = self.rd(i, 0, 2);
                    let sp = self.reg(4);
                    let sp = self.b.ins().iadd_imm_s(sp, -2);
                    self.set_reg(4, sp);
                    self.store(sp, v);
                } else {
                    let v = self.rd(i, 0, 4);
                    self.push32(v);
                }
            }
            Pop => {
                if op_size(i, 0) == 2 {
                    let sp = self.reg(4);
                    let v = self.load(sp, 2);
                    let sp2 = self.b.ins().iadd_imm_s(sp, 2);
                    self.set_reg(4, sp2);
                    self.wr(i, 0, v);
                } else {
                    let v = self.pop32();
                    self.wr(i, 0, v);
                }
            }
            Pushad => {
                let s = self.reg(4);
                for k in [0, 1, 2, 3] {
                    let v = self.reg(k);
                    self.push32(v);
                }
                self.push32(s);
                for k in [5, 6, 7] {
                    let v = self.reg(k);
                    self.push32(v);
                }
            }
            Popad => {
                for k in [7, 6, 5] {
                    let v = self.pop32();
                    self.set_reg(k, v);
                }
                let sp = self.reg(4);
                let sp = self.b.ins().iadd_imm_s(sp, 4); // the saved esp is skipped
                self.set_reg(4, sp);
                for k in [3, 2, 1, 0] {
                    let v = self.pop32();
                    self.set_reg(k, v);
                }
            }
            Pushfd => {
                let df = self.b.ins().load(I8, mf(), self.cpu, offset_of!(Cpu, df) as i32);
                let mut v = self.iconst(I32, 0x202);
                for (var, bit) in [(self.cf, 0), (self.pf, 2), (self.zf, 6), (self.sf, 7), (self.of, 11)] {
                    let f = self.flag(var);
                    let f = self.b.ins().uextend(I32, f);
                    let f = self.b.ins().ishl_imm_u(f, bit);
                    v = self.b.ins().bor(v, f);
                }
                let d = self.b.ins().uextend(I32, df);
                let d = self.b.ins().ishl_imm_u(d, 10);
                v = self.b.ins().bor(v, d);
                self.push32(v);
            }
            Popfd => {
                let v = self.pop32();
                for (var, bit) in [(self.cf, 0), (self.pf, 2), (self.zf, 6), (self.sf, 7), (self.of, 11)] {
                    let f = self.bit8(v, bit);
                    self.set_flag(var, f);
                }
                let d = self.bit8(v, 10);
                self.b.ins().store(mf(), d, self.cpu, offset_of!(Cpu, df) as i32);
            }
            Lahf => {
                let mut v = self.iconst(I32, 0x200);
                for (var, bit) in [(self.cf, 8), (self.pf, 10), (self.zf, 14), (self.sf, 15)] {
                    let f = self.flag(var);
                    let f = self.b.ins().uextend(I32, f);
                    let f = self.b.ins().ishl_imm_u(f, bit);
                    v = self.b.ins().bor(v, f);
                }
                let a = self.reg(0);
                let a = self.b.ins().band_imm_u(a, !0xff00u32 as i64);
                let a = self.b.ins().bor(a, v);
                self.set_reg(0, a);
            }
            Sahf => {
                let a = self.reg(0);
                for (var, bit) in [(self.cf, 8), (self.pf, 10), (self.zf, 14), (self.sf, 15)] {
                    let f = self.bit8(a, bit);
                    self.set_flag(var, f);
                }
            }
            Clc | Stc => {
                let v = self.iconst(I8, (m == Stc) as i64);
                self.set_flag(self.cf, v);
            }
            Cmc => {
                let c = self.flag(self.cf);
                let c = self.b.ins().bxor_imm_u(c, 1);
                self.set_flag(self.cf, c);
            }
            Cld | Std => {
                let v = self.iconst(I8, (m == Std) as i64);
                self.b.ins().store(mf(), v, self.cpu, offset_of!(Cpu, df) as i32);
            }
            Xchg => {
                let n = op_size(i, 0);
                let (x, y) = (self.rd(i, 0, n), self.rd(i, 1, n));
                self.wr(i, 0, y);
                self.wr(i, 1, x);
            }
            Bswap => {
                let x = self.rd(i, 0, 4);
                let v = self.b.ins().bswap(x);
                self.wr(i, 0, v);
            }
            Bt | Bts | Btr | Btc => self.bit_test(i, m),
            Cdq => {
                let a = self.reg(0);
                let v = self.b.ins().sshr_imm_u(a, 31);
                self.set_reg(2, v);
            }
            Cwde => {
                let a = self.read_reg(Register::AX);
                let v = self.b.ins().sextend(I32, a);
                self.set_reg(0, v);
            }
            Cbw => {
                let a = self.read_reg(Register::AL);
                let v = self.b.ins().sextend(I16, a);
                self.write_reg(Register::AX, v);
            }
            Cwd => {
                let a = self.read_reg(Register::AX);
                let v = self.b.ins().sshr_imm_u(a, 15);
                self.write_reg(Register::DX, v);
            }
            Xlatb => {
                let (b, al) = (self.reg(3), self.read_reg(Register::AL));
                let al = self.b.ins().uextend(I32, al);
                let a = self.b.ins().iadd(b, al);
                let v = self.load(a, 1);
                self.write_reg(Register::AL, v);
            }
            Mul | Imul if i.op_count() == 1 => self.mul1(i, m == Imul),
            Imul => {
                let n = op_size(i, 0);
                let (a, b) = if i.op_count() == 2 { (self.rd(i, 0, n), self.rd(i, 1, n)) } else { (self.rd(i, 1, n), self.rd(i, 2, n)) };
                let (a64, b64) = (self.b.ins().sextend(I64, a), self.b.ins().sextend(I64, b));
                let p = self.b.ins().imul(a64, b64);
                let r = self.b.ins().ireduce(ty(n), p);
                let back = self.b.ins().sextend(I64, r);
                let o = self.b.ins().icmp(IntCC::NotEqual, p, back);
                self.set_flag(self.cf, o);
                self.set_flag(self.of, o);
                self.wr(i, 0, r);
            }
            Div | Idiv => self.div(i, m == Idiv),
            Leave => {
                let bp = self.reg(5);
                self.set_reg(4, bp);
                let v = self.pop32();
                self.set_reg(5, v);
            }
            Enter => {
                let bp = self.reg(5);
                self.push32(bp);
                let sp = self.reg(4);
                self.set_reg(5, sp);
                let sp = self.b.ins().iadd_imm_s(sp, -(i.immediate16() as i64));
                self.set_reg(4, sp);
            }
            Movd | Movq => self.mmx_move(i),
            Emms => {} // MMX and x87 registers are kept apart
            // hardware ports: the game polls the gameport (0x201); nothing is connected
            In => {
                let v = self.iconst(ty(op_size(i, 0)), -1i64 & ((1i64 << (8 * op_size(i, 0))) - 1));
                self.wr(i, 0, v);
            }
            Out => {}
            Insb | Insw | Insd => {
                let n = i.memory_size().size() as u32;
                let v = self.iconst(ty(n), (1i64 << (8 * n)) - 1);
                let di = self.reg(7);
                self.store(di, v);
                let df = self.b.ins().load(I8, mf(), self.cpu, offset_of!(Cpu, df) as i32);
                let df = self.b.ins().uextend(I32, df);
                let step = self.b.ins().imul_imm_u(df, -2 * n as i64);
                let step = self.b.ins().iadd_imm_s(step, n as i64);
                let di = self.b.ins().iadd(di, step);
                self.set_reg(7, di);
            }
            Cpuid => self.helper(self.refs.cpuid, None),
            Rdtsc => self.helper(self.refs.rdtsc, None),
            _ => {
                let (kind, size) = match m {
                    Movsb => (0, 1),
                    Movsw => (0, 2),
                    Movsd => (0, 4),
                    Stosb => (1, 1),
                    Stosw => (1, 2),
                    Stosd => (1, 4),
                    Lodsb => (2, 1),
                    Lodsw => (2, 2),
                    Lodsd => (2, 4),
                    Scasb => (3, 1),
                    Scasw => (3, 2),
                    Scasd => (3, 4),
                    Cmpsb => (4, 1),
                    Cmpsw => (4, 2),
                    Cmpsd => (4, 4),
                    _ => panic!("{m:?} passed the support check but has no lifting"),
                };
                let rep = if i.has_repne_prefix() { 2 } else if i.has_rep_prefix() || i.has_repe_prefix() { 1 } else { 0 };
                self.helper(self.refs.string, Some(kind | size << 4 | rep << 8));
            }
        }
        false
    }

    /// movd/movq between MMX registers (`Cpu::mm`), general registers and memory.
    fn mmx_move(&mut self, i: &Instruction) {
        let n = if i.mnemonic() == Mnemonic::Movd { 4 } else { 8 };
        let mm = |r: Register| (offset_of!(Cpu, mm) + 8 * (r.number() - Register::MM0.number())) as i32;
        let v = match i.op_kind(1) {
            OpKind::Register if i.op1_register().is_mm() => {
                let v = self.b.ins().load(I64, mf(), self.cpu, mm(i.op1_register()));
                if n == 4 { self.b.ins().ireduce(I32, v) } else { v }
            }
            _ => self.rd(i, 1, n),
        };
        match i.op_kind(0) {
            OpKind::Register if i.op0_register().is_mm() => {
                let v = if n == 4 { self.b.ins().uextend(I64, v) } else { v };
                self.b.ins().store(mf(), v, self.cpu, mm(i.op0_register()));
            }
            _ => self.wr(i, 0, v),
        }
    }

    fn bit8(&mut self, v: Value, bit: i64) -> Value {
        let s = self.b.ins().ushr_imm_u(v, bit);
        let s = self.b.ins().band_imm_u(s, 1);
        self.b.ins().ireduce(I8, s)
    }

    fn jump_insn(&mut self, i: &Instruction) -> bool {
        let next = i.next_ip32();
        if direct_target(i).is_some() && self.smc.imm.contains_key(&i.ip32()) {
            // a branch whose offset the game rewrites: the target is only known at run time
            let t = self.rel_target(i);
            let go = self.branch_cond(i);
            let far = self.b.create_block();
            match go {
                Some(c) => {
                    let n = self.target_block(next);
                    self.b.ins().brif(c, far, &[], n, &[]);
                }
                None => {
                    self.b.ins().jump(far, &[]);
                }
            }
            self.b.switch_to_block(far);
            self.jump_dyn(t);
            return true;
        }
        if let Some(t) = direct_target(i) {
            match self.branch_cond(i) {
                Some(go) => self.branch(go, t, next),
                None => self.goto(t),
            }
            return true;
        }
        // through a register or memory
        let t = self.rd(i, 0, 4);
        if let Some(tbl) = self.f.tables.get(&i.ip32()).cloned() {
            let mut targets = tbl;
            targets.sort_unstable();
            targets.dedup();
            let mut sw = Switch::new();
            for t in targets {
                let b = self.target_block(t);
                sw.set_entry(t as u128, b);
            }
            let other = self.b.create_block();
            sw.emit(&mut self.b, t, other);
            self.b.switch_to_block(other);
            self.jump_dyn(t);
        } else {
            // tail jump through a pointer (import thunks, function pointers)
            self.exit_to(t, None);
        }
        true
    }

    /// next + the rewritten offset of a jump or call
    fn rel_target(&mut self, i: &Instruction) -> Value {
        let off = self.dyn_imm(i, 4).unwrap();
        self.b.ins().iadd_imm_s(off, i.next_ip32() as i64)
    }

    /// When a direct branch is taken (None: always), updating ecx for loop.
    fn branch_cond(&mut self, i: &Instruction) -> Option<Value> {
        use Mnemonic::*;
        Some(match i.mnemonic() {
            Jmp => return None,
            Loop | Loope | Loopne => {
                let c = self.reg(1);
                let c = self.b.ins().iadd_imm_s(c, -1);
                self.set_reg(1, c);
                let mut go = self.b.ins().icmp_imm_u(IntCC::NotEqual, c, 0);
                if i.mnemonic() != Loop {
                    let z = self.flag(self.zf);
                    let z = if i.mnemonic() == Loopne { self.b.ins().bxor_imm_u(z, 1) } else { z };
                    go = self.b.ins().band(go, z);
                }
                go
            }
            Jecxz | Jcxz => {
                let c = if i.mnemonic() == Jecxz { self.reg(1) } else { self.read_reg(Register::CX) };
                self.b.ins().icmp_imm_u(IntCC::Equal, c, 0)
            }
            _ => self.cond(i.condition_code()),
        })
    }

    fn alu(&mut self, i: &Instruction, m: Mnemonic) {
        use Mnemonic::*;
        let n = op_size(i, 0);
        if m == Xor && i.op0_kind() == OpKind::Register && i.op1_kind() == OpKind::Register && i.op0_register() == i.op1_register() {
            let z = self.iconst(ty(n), 0);
            self.wr(i, 0, z);
            self.zero_flags(&[self.cf, self.of, self.sf]);
            let one = self.iconst(I8, 1);
            self.set_flag(self.zf, one);
            self.set_flag(self.pf, one);
            return;
        }
        let (x, y) = (self.rd(i, 0, n), self.rd(i, 1, n));
        let r = match m {
            Add | Adc => {
                let mut r = self.b.ins().iadd(x, y);
                let cin = self.flag(self.cf);
                if m == Adc {
                    let c = if n == 1 { cin } else { self.b.ins().uextend(ty(n), cin) };
                    r = self.b.ins().iadd(r, c);
                    let le = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, r, x);
                    let lt = self.b.ins().icmp(IntCC::UnsignedLessThan, r, x);
                    let c = self.b.ins().select(cin, le, lt);
                    self.set_flag(self.cf, c);
                } else {
                    let c = self.b.ins().icmp(IntCC::UnsignedLessThan, r, x);
                    self.set_flag(self.cf, c);
                }
                let (a, b) = (self.b.ins().bxor(x, r), self.b.ins().bxor(y, r));
                let t = self.b.ins().band(a, b);
                let o = self.msb(t);
                self.set_flag(self.of, o);
                r
            }
            Sub | Sbb | Cmp => {
                let mut r = self.b.ins().isub(x, y);
                let cin = self.flag(self.cf);
                if m == Sbb {
                    let c = if n == 1 { cin } else { self.b.ins().uextend(ty(n), cin) };
                    r = self.b.ins().isub(r, c);
                    let le = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, x, y);
                    let lt = self.b.ins().icmp(IntCC::UnsignedLessThan, x, y);
                    let c = self.b.ins().select(cin, le, lt);
                    self.set_flag(self.cf, c);
                } else {
                    let c = self.b.ins().icmp(IntCC::UnsignedLessThan, x, y);
                    self.set_flag(self.cf, c);
                }
                let (a, b) = (self.b.ins().bxor(x, y), self.b.ins().bxor(x, r));
                let t = self.b.ins().band(a, b);
                let o = self.msb(t);
                self.set_flag(self.of, o);
                r
            }
            _ => {
                let r = match m {
                    And | Test => self.b.ins().band(x, y),
                    Or => self.b.ins().bor(x, y),
                    _ => self.b.ins().bxor(x, y),
                };
                self.zero_flags(&[self.cf, self.of]);
                r
            }
        };
        self.set_zsp(r);
        if !matches!(m, Cmp | Test) {
            self.wr(i, 0, r);
        }
    }

    /// Shifts and rotates. A count of 0 (after masking to 5 bits) changes nothing, flags
    /// included, so computed results are selected by `k != 0`.
    fn shift(&mut self, i: &Instruction, m: Mnemonic) {
        use Mnemonic::*;
        let n = op_size(i, 0);
        let w = 8 * n as i64;
        let t = ty(n);
        let k = match i.op1_kind() {
            OpKind::Register => {
                let c = self.read_reg(i.op1_register());
                let c = self.b.ins().uextend(I32, c);
                self.b.ins().band_imm_u(c, 31)
            }
            _ if self.smc.imm.contains_key(&i.ip32()) => {
                let c = self.dyn_imm(i, 4).unwrap();
                self.b.ins().band_imm_u(c, 31)
            }
            _ => {
                let c = (i.immediate(1) & 31) as i64;
                if c == 0 {
                    return;
                }
                self.iconst(I32, c)
            }
        };
        let x = self.rd(i, 0, n);
        let xz = if n == 4 { x } else { self.b.ins().uextend(I32, x) };
        let xs = if n == 4 { x } else { self.b.ins().sextend(I32, x) };
        let k1 = self.b.ins().iadd_imm_s(k, -1);
        let one32 = |l: &mut Self, v: Value| {
            let v = l.b.ins().band_imm_u(v, 1);
            l.b.ins().ireduce(I8, v)
        };
        let reduce = |l: &mut Self, v: Value| if n == 4 { v } else { l.b.ins().ireduce(t, v) };
        let (r, cf, of, zsp) = match m {
            Shl | Sal => {
                let r32 = self.b.ins().ishl(xz, k);
                let r = reduce(self, r32);
                let wk = self.iconst(I32, w);
                let wk = self.b.ins().isub(wk, k);
                let c = self.b.ins().ushr(xz, wk);
                let mut c = one32(self, c);
                if n != 4 {
                    let fits = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, k, w);
                    let z = self.iconst(I8, 0);
                    c = self.b.ins().select(fits, c, z);
                }
                let s = self.msb(r);
                let o = self.b.ins().bxor(s, c);
                (r, c, o, true)
            }
            Shr => {
                let r32 = self.b.ins().ushr(xz, k);
                let r = reduce(self, r32);
                let c = self.b.ins().ushr(xz, k1);
                let c = one32(self, c);
                let o = self.msb(x);
                (r, c, o, true)
            }
            Sar => {
                let r32 = self.b.ins().sshr(xs, k);
                let r = reduce(self, r32);
                let c = self.b.ins().sshr(xs, k1);
                let c = one32(self, c);
                let o = self.iconst(I8, 0);
                (r, c, o, true)
            }
            Rcl | Rcr => {
                // rotate the (w+1)-bit value CF:x in 64 bits
                let cin = self.flag(self.cf);
                let cin = self.b.ins().uextend(I64, cin);
                let cin = self.b.ins().ishl_imm_u(cin, w);
                let x64 = self.b.ins().uextend(I64, xz);
                let v = self.b.ins().bor(cin, x64);
                let k64 = self.b.ins().uextend(I64, k);
                let kk = self.b.ins().urem_imm_u(k64, w + 1);
                let inv = self.iconst(I64, w + 1);
                let inv = self.b.ins().isub(inv, kk);
                let (a, b) = if m == Rcl {
                    (self.b.ins().ishl(v, kk), self.b.ins().ushr(v, inv))
                } else {
                    (self.b.ins().ushr(v, kk), self.b.ins().ishl(v, inv))
                };
                let v = self.b.ins().bor(a, b);
                let r = self.b.ins().ireduce(t, v);
                let c = self.b.ins().ushr_imm_u(v, w);
                let c = self.b.ins().band_imm_u(c, 1);
                let c = self.b.ins().ireduce(I8, c);
                let s = self.msb(r);
                let o = if m == Rcl {
                    self.b.ins().bxor(s, c)
                } else {
                    let r2 = self.b.ins().ushr_imm_u(r, w - 2);
                    let r2 = self.b.ins().band_imm_u(r2, 1);
                    let b2 = if n == 1 { r2 } else { self.b.ins().ireduce(I8, r2) };
                    self.b.ins().bxor(s, b2)
                };
                (r, c, o, false)
            }
            _ => {
                let kk = self.b.ins().band_imm_u(k, w - 1);
                let kk = if n == 4 { kk } else { self.b.ins().ireduce(t, kk) };
                let r = if m == Rol { self.b.ins().rotl(x, kk) } else { self.b.ins().rotr(x, kk) };
                let s = self.msb(r);
                if m == Rol {
                    let lo = self.b.ins().band_imm_u(r, 1);
                    let c = if n == 1 { lo } else { self.b.ins().ireduce(I8, lo) };
                    let o = self.b.ins().bxor(s, c);
                    (r, c, o, false)
                } else {
                    let r2 = self.b.ins().ushr_imm_u(r, w - 2);
                    let r2 = self.b.ins().band_imm_u(r2, 1);
                    let b2 = if n == 1 { r2 } else { self.b.ins().ireduce(I8, r2) };
                    let o = self.b.ins().bxor(s, b2);
                    (r, s, o, false)
                }
            }
        };
        let nz = self.b.ins().icmp_imm_u(IntCC::NotEqual, k, 0);
        let pick = |l: &mut Self, new: Value, old: Value| l.b.ins().select(nz, new, old);
        let r = pick(self, r, x);
        self.wr(i, 0, r);
        let old_cf = self.flag(self.cf);
        let v = pick(self, cf, old_cf);
        self.set_flag(self.cf, v);
        let old_of = self.flag(self.of);
        let v = pick(self, of, old_of);
        self.set_flag(self.of, v);
        if zsp {
            let (oz, os, op) = (self.flag(self.zf), self.flag(self.sf), self.flag(self.pf));
            self.set_zsp(r);
            let (nzf, nsf, npf) = (self.flag(self.zf), self.flag(self.sf), self.flag(self.pf));
            let v = pick(self, nzf, oz);
            self.set_flag(self.zf, v);
            let v = pick(self, nsf, os);
            self.set_flag(self.sf, v);
            let v = pick(self, npf, op);
            self.set_flag(self.pf, v);
        }
    }

    fn shxd(&mut self, i: &Instruction, m: Mnemonic) {
        let k = match i.op_kind(2) {
            OpKind::Register => {
                let c = self.read_reg(i.op_register(2));
                let c = self.b.ins().uextend(I32, c);
                self.b.ins().band_imm_u(c, 31)
            }
            _ if self.smc.imm.contains_key(&i.ip32()) => {
                let c = self.dyn_imm(i, 4).unwrap();
                self.b.ins().band_imm_u(c, 31)
            }
            _ => {
                let c = (i.immediate(2) & 31) as i64;
                if c == 0 {
                    return;
                }
                self.iconst(I32, c)
            }
        };
        let (x, y) = (self.rd(i, 0, 4), self.rd(i, 1, 4));
        let c32 = self.iconst(I32, 32);
        let inv = self.b.ins().isub(c32, k);
        let (r, c) = if m == Mnemonic::Shld {
            let (a, b) = (self.b.ins().ishl(x, k), self.b.ins().ushr(y, inv));
            let c = self.b.ins().ushr(x, inv);
            (self.b.ins().bor(a, b), c)
        } else {
            let (a, b) = (self.b.ins().ushr(x, k), self.b.ins().ishl(y, inv));
            let k1 = self.b.ins().iadd_imm_s(k, -1);
            let c = self.b.ins().ushr(x, k1);
            (self.b.ins().bor(a, b), c)
        };
        let c = self.b.ins().band_imm_u(c, 1);
        let c = self.b.ins().ireduce(I8, c);
        let nz = self.b.ins().icmp_imm_u(IntCC::NotEqual, k, 0);
        let r = self.b.ins().select(nz, r, x);
        self.wr(i, 0, r);
        let old = [self.flag(self.cf), self.flag(self.of), self.flag(self.zf), self.flag(self.sf), self.flag(self.pf)];
        self.set_flag(self.cf, c);
        let z = self.iconst(I8, 0);
        self.set_flag(self.of, z);
        self.set_zsp(r);
        for (var, o) in [self.cf, self.of, self.zf, self.sf, self.pf].into_iter().zip(old) {
            let new = self.flag(var);
            let v = self.b.ins().select(nz, new, o);
            self.set_flag(var, v);
        }
    }

    fn bit_test(&mut self, i: &Instruction, m: Mnemonic) {
        use Mnemonic::*;
        let update = |l: &mut Self, x: Value, bit: Value| -> Option<Value> {
            match m {
                Bts => Some(l.b.ins().bor(x, bit)),
                Btr => {
                    let nb = l.b.ins().bnot(bit);
                    Some(l.b.ins().band(x, nb))
                }
                Btc => Some(l.b.ins().bxor(x, bit)),
                _ => None,
            }
        };
        if i.op0_kind() == OpKind::Memory && i.op1_kind() == OpKind::Register {
            // a register bit offset addresses a bit string: it can reach past the dword
            let nreg = self.read_reg(i.op1_register());
            let nv = if self.b.func.dfg.value_type(nreg) == I32 { nreg } else { self.b.ins().sextend(I32, nreg) };
            let base = self.addr(i);
            let word = self.b.ins().sshr_imm_u(nv, 5);
            let word = self.b.ins().ishl_imm_u(word, 2);
            let p = self.b.ins().iadd(base, word);
            let k = self.b.ins().band_imm_u(nv, 31);
            let x = self.load(p, 4);
            let one = self.iconst(I32, 1);
            let bit = self.b.ins().ishl(one, k);
            let c = self.b.ins().ushr(x, k);
            let c = self.b.ins().band_imm_u(c, 1);
            let c = self.b.ins().ireduce(I8, c);
            self.set_flag(self.cf, c);
            if let Some(v) = update(self, x, bit) {
                self.store(p, v);
            }
            return;
        }
        let n = op_size(i, 0);
        let x = self.rd(i, 0, n);
        let k = self.rd(i, 1, n);
        let k = self.b.ins().band_imm_u(k, 8 * n as i64 - 1);
        let one = self.iconst(ty(n), 1);
        let bit = self.b.ins().ishl(one, k);
        let c = self.b.ins().ushr(x, k);
        let c = self.b.ins().band_imm_u(c, 1);
        let c = if n == 1 { c } else { self.b.ins().ireduce(I8, c) };
        self.set_flag(self.cf, c);
        if let Some(v) = update(self, x, bit) {
            self.wr(i, 0, v);
        }
    }

    fn mul1(&mut self, i: &Instruction, signed: bool) {
        let n = op_size(i, 0);
        let v = self.rd(i, 0, n);
        if n == 1 {
            let a = self.read_reg(Register::AL);
            let ext = |l: &mut Self, x: Value| if signed { l.b.ins().sextend(I16, x) } else { l.b.ins().uextend(I16, x) };
            let (a, v) = (ext(self, a), ext(self, v));
            let p = self.b.ins().imul(a, v);
            self.write_reg(Register::AX, p);
            let lo = self.b.ins().ireduce(I8, p);
            let back = ext(self, lo);
            let o = self.b.ins().icmp(IntCC::NotEqual, p, back);
            self.set_flag(self.cf, o);
            self.set_flag(self.of, o);
        } else {
            let a = self.reg(0);
            let lo = self.b.ins().imul(a, v);
            let hi = if signed { self.b.ins().smulhi(a, v) } else { self.b.ins().umulhi(a, v) };
            self.set_reg(0, lo);
            self.set_reg(2, hi);
            let o = if signed {
                let s = self.b.ins().sshr_imm_u(lo, 31);
                self.b.ins().icmp(IntCC::NotEqual, hi, s)
            } else {
                self.b.ins().icmp_imm_u(IntCC::NotEqual, hi, 0)
            };
            self.set_flag(self.cf, o);
            self.set_flag(self.of, o);
        }
    }

    fn div(&mut self, i: &Instruction, signed: bool) {
        let n = op_size(i, 0);
        let d = self.rd(i, 0, n);
        // division by zero: the game never relies on it, report it
        let zero = self.b.ins().icmp_imm_u(IntCC::Equal, d, 0);
        let (bad, ok) = (self.b.create_block(), self.b.create_block());
        self.b.ins().brif(zero, bad, &[], ok, &[]);
        self.b.switch_to_block(bad);
        self.trap(i.ip32(), TRAP_DIV0);
        self.b.switch_to_block(ok);
        let (wide, half) = match n {
            1 => (I16, I8),
            2 => (I32, I16),
            _ => (I64, I32),
        };
        let x = match n {
            1 => self.read_reg(Register::AX),
            _ => {
                let (hi, lo) = if n == 2 {
                    (self.read_reg(Register::DX), self.read_reg(Register::AX))
                } else {
                    (self.reg(2), self.reg(0))
                };
                let (hi, lo) = (self.b.ins().uextend(wide, hi), self.b.ins().uextend(wide, lo));
                let hi = self.b.ins().ishl_imm_u(hi, 8 * n as i64);
                self.b.ins().bor(hi, lo)
            }
        };
        let dw = if signed { self.b.ins().sextend(wide, d) } else { self.b.ins().uextend(wide, d) };
        let (q, r) = if signed {
            (self.b.ins().sdiv(x, dw), self.b.ins().srem(x, dw))
        } else {
            (self.b.ins().udiv(x, dw), self.b.ins().urem(x, dw))
        };
        let (q, r) = (self.b.ins().ireduce(half, q), self.b.ins().ireduce(half, r));
        match n {
            1 => {
                self.write_reg(Register::AL, q);
                self.write_reg(Register::AH, r);
            }
            2 => {
                self.write_reg(Register::AX, q);
                self.write_reg(Register::DX, r);
            }
            _ => {
                self.set_reg(0, q);
                self.set_reg(2, r);
            }
        }
    }
}

/// eax..edi -> 0..7 for any size of the register
fn gpr(r: Register) -> usize {
    r.full_register32() as usize - Register::EAX as usize
}
