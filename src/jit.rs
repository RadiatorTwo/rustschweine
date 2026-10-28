//! Compiling the whole program with Cranelift's JIT: every function is declared up front
//! (so calls between them are direct), lifted, compiled, and entered through a trampoline.
//! Lifting runs on one thread, Cranelift's compiling on all cores; the machine code with its
//! relocations can be kept in a file (`compile_cached`), so later starts only link it.

use crate::discover::{Code, Program};
use crate::lift::{Lifter, Refs};
use crate::rt::{self, Cpu, DISPATCH, Dispatch, Hook};
use crate::smc::Smc;
use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, LibCall, Signature, UserFuncName, types::*};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::control::ControlPlane;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, ModuleReloc, ModuleRelocTarget, default_libcall_names};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

pub struct Compiled {
    _module: JITModule,
    enter: extern "C" fn(*mut Cpu, *const u8),
    pub fns: HashMap<u32, *const u8>,
    pub stats: Stats,
}

#[derive(Default)]
pub struct Stats {
    pub functions: usize,
    pub complete: usize,
    pub instructions: usize,
    pub missing: BTreeMap<String, usize>,
    pub failed: Vec<(u32, String)>,
    pub lift_ms: u128,
    pub compile_ms: u128,
    /// the machine code came from the cache file
    pub cached: bool,
}

impl Compiled {
    /// Call the guest function at `addr` (its return address must already be on the guest stack).
    pub fn call(&self, cpu: &mut Cpu, addr: u32) {
        let f = self.fns[&addr];
        (self.enter)(cpu, f);
    }
}

/// A compiled function: its machine code and what is patched into it when it is linked.
struct Blob {
    id: FuncId,
    align: u64,
    bytes: Vec<u8>,
    relocs: Vec<ModuleReloc>,
}

/// Compile every function; `hooks` replace guest functions by host functions (the recompiled
/// original stays callable through `rt::call_original`).
pub fn compile(prog: &Program, smc: &Smc, code: &mut Code, hooks: &[(u32, Hook)]) -> Compiled {
    compile_cached(prog, smc, code, hooks, None)
}

/// As `compile`, with the lifted functions' machine code kept in `cache` (a file, and the key
/// it is valid for: what made it, our program and the game's EXE). A file with another key is
/// replaced.
pub fn compile_cached(prog: &Program, smc: &Smc, code: &mut Code, hooks: &[(u32, Hook)], cache: Option<(&Path, [u8; 32])>) -> Compiled {
    let mut flags = settings::builder();
    flags.set("opt_level", "speed").unwrap();
    flags.set("is_pic", "false").unwrap();
    flags.set("use_colocated_libcalls", "false").unwrap();
    flags.set("preserve_frame_pointers", "true").unwrap();
    let isa = cranelift_native::builder()
        .expect("host not supported by Cranelift")
        .finish(settings::Flags::new(flags))
        .expect("Cranelift settings");
    let host_cc = isa.default_call_conv();
    let mut jb = JITBuilder::with_isa(isa, default_libcall_names());
    jb.symbol("rt_lookup", rt::rt_lookup as *const u8);
    jb.symbol("rt_foreign", rt::rt_foreign as *const u8);
    jb.symbol("rt_unimpl", rt::rt_unimpl as *const u8);
    jb.symbol("rt_trap", rt::rt_trap as *const u8);
    jb.symbol("rt_string", rt::rt_string as *const u8);
    jb.symbol("rt_cpuid", rt::rt_cpuid as *const u8);
    jb.symbol("rt_rdtsc", rt::rt_rdtsc as *const u8);
    jb.symbol("rt_x87", rt::rt_x87 as *const u8);
    jb.symbol("rt_fpu_round", rt::rt_fpu_round as *const u8);
    jb.symbol("rt_yield", rt::rt_yield as *const u8);
    jb.symbol("rt_hook", rt::rt_hook as *const u8);
    jb.symbol("rt_poll", &rt::POLL as *const _ as *const u8);
    let mut module = JITModule::new(jb);
    let tcfg = module.target_config();
    let ptr = tcfg.pointer_type();

    let sig = |cc: CallConv, params: &[cranelift_codegen::ir::Type], ret: Option<cranelift_codegen::ir::Type>| {
        let mut s = Signature::new(cc);
        s.params.extend(params.iter().map(|&t| AbiParam::new(t)));
        s.returns.extend(ret.map(AbiParam::new));
        s
    };
    // guest functions: fn(cpu) with the tail calling convention (return_call between them)
    let guest_sig = sig(CallConv::Tail, &[ptr], None);
    let mut decl = |name: &str, s: &Signature| module.declare_function(name, Linkage::Import, s).unwrap();
    let h_lookup = decl("rt_lookup", &sig(host_cc, &[ptr, I32], Some(ptr)));
    let h_foreign = decl("rt_foreign", &sig(host_cc, &[ptr], None));
    let h_unimpl = decl("rt_unimpl", &sig(host_cc, &[ptr, I32], None));
    let h_trap = decl("rt_trap", &sig(host_cc, &[ptr, I32, I32], None));
    let h_string = decl("rt_string", &sig(host_cc, &[ptr, I32], None));
    let h_cpuid = decl("rt_cpuid", &sig(host_cc, &[ptr], None));
    let h_rdtsc = decl("rt_rdtsc", &sig(host_cc, &[ptr], None));
    let h_x87 = decl("rt_x87", &sig(host_cc, &[ptr, I32, I32], None));
    let h_yield = decl("rt_yield", &sig(host_cc, &[ptr], None));
    let h_fpu_round = decl("rt_fpu_round", &sig(host_cc, &[I32, F64, F64, I32], Some(F64)));
    let h_hook = decl("rt_hook", &sig(host_cc, &[ptr, I32], None));
    let d_poll = module.declare_data("rt_poll", Linkage::Import, true, false).unwrap();

    let ids: BTreeMap<u32, FuncId> = prog
        .fns
        .keys()
        .map(|&a| (a, module.declare_function(&format!("F_{a:x}"), Linkage::Local, &guest_sig).unwrap()))
        .collect();
    for h in hooks.iter().filter(|h| !ids.contains_key(&h.0)) {
        eprintln!("warn: hook at {:#x}: no function there", h.0);
    }
    // a hooked function: F_x calls the host (rt_hook), the lifted code becomes G_x
    let originals: BTreeMap<u32, FuncId> = hooks
        .iter()
        .filter(|h| ids.contains_key(&h.0))
        .map(|h| (h.0, module.declare_function(&format!("G_{:x}", h.0), Linkage::Local, &guest_sig).unwrap()))
        .collect();
    let foreign_id = module.declare_function("foreign_stub", Linkage::Local, &guest_sig).unwrap();
    let enter_id = module.declare_function("enter", Linkage::Local, &sig(host_cc, &[ptr, ptr], None)).unwrap();

    let mut stats = Stats { functions: prog.fns.len(), ..Default::default() };
    let mut ctx = module.make_context();
    let mut fctx = FunctionBuilderContext::new();
    let t0 = Instant::now();
    let id_of = |addr: u32| originals.get(&addr).copied().unwrap_or(ids[&addr]);

    // the cache is only for code that does not count its calls (HOGS_COUNT: their counters'
    // addresses are constants in it)
    let cache = cache.filter(|_| rt::count_file().is_none());
    let mut blobs = cache.and_then(|(path, key)| load(path, &key));
    stats.cached = blobs.is_some();
    let blobs = match blobs.take() {
        Some(b) => b,
        None => {
            // lifting: one thread (the decoder's cache is shared)
            let mut lifted: Vec<(u32, FuncId, Function)> = Vec::with_capacity(prog.fns.len());
            for (&addr, f) in &prog.fns {
                let id = id_of(addr);
                ctx.func.signature = guest_sig.clone();
                ctx.func.name = UserFuncName::user(0, id.as_u32());
                let missing = {
                    let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                    let refs = Refs {
                        lookup: module.declare_func_in_func(h_lookup, b.func),
                        unimpl: module.declare_func_in_func(h_unimpl, b.func),
                        trap: module.declare_func_in_func(h_trap, b.func),
                        string: module.declare_func_in_func(h_string, b.func),
                        cpuid: module.declare_func_in_func(h_cpuid, b.func),
                        rdtsc: module.declare_func_in_func(h_rdtsc, b.func),
                        x87: module.declare_func_in_func(h_x87, b.func),
                        fpu_round: module.declare_func_in_func(h_fpu_round, b.func),
                        yield_: module.declare_func_in_func(h_yield, b.func),
                        poll: module.declare_data_in_func(d_poll, b.func),
                        guest_sig: b.import_signature(guest_sig.clone()),
                    };
                    let mut get_fn = |func: &mut Function, a: u32| module.declare_func_in_func(ids[&a], func);
                    Lifter::new(b, f, prog, smc, &refs, &mut get_fn, tcfg).lift(code)
                };
                stats.instructions += f.ins.len();
                if missing.is_empty() {
                    stats.complete += 1;
                }
                for m in missing {
                    *stats.missing.entry(m).or_default() += 1;
                }
                lifted.push((addr, id, std::mem::replace(&mut ctx.func, Function::new())));
                module.clear_context(&mut ctx);
            }
            stats.lift_ms = t0.elapsed().as_millis();

            // compiling: every core takes the next function
            let results = compile_all(module.isa(), lifted);
            let mut blobs = Vec::with_capacity(results.len());
            for (addr, id, r) in results {
                match r {
                    Ok(b) => blobs.push(b),
                    Err(e) => {
                        stats.failed.push((addr, e));
                        // keep the program linkable: this function only reports itself
                        ctx.func.signature = guest_sig.clone();
                        ctx.func.name = UserFuncName::user(0, id.as_u32());
                        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                        let unimpl = module.declare_func_in_func(h_unimpl, b.func);
                        let blk = b.create_block();
                        b.append_block_params_for_function_params(blk);
                        b.switch_to_block(blk);
                        let cpu = b.block_params(blk)[0];
                        let a = b.ins().iconst(I32, addr as i64);
                        b.ins().call(unimpl, &[cpu, a]);
                        b.ins().return_(&[]);
                        b.seal_all_blocks();
                        b.finalize(tcfg);
                        module.define_function(id, &mut ctx).expect("stub function");
                        module.clear_context(&mut ctx);
                    }
                }
            }
            // only a complete program is kept
            if let Some((path, key)) = cache.filter(|_| stats.failed.is_empty()) {
                save(path, &key, &blobs);
            }
            blobs
        }
    };
    for b in &blobs {
        module.define_function_bytes(b.id, b.align, &b.bytes, &b.relocs).expect("defining a compiled function");
    }

    for (&addr, _) in &originals {
        let id = ids[&addr];
        ctx.func.signature = guest_sig.clone();
        ctx.func.name = UserFuncName::user(0, id.as_u32());
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let f = module.declare_func_in_func(h_hook, b.func);
            let blk = b.create_block();
            b.append_block_params_for_function_params(blk);
            b.switch_to_block(blk);
            let cpu = b.block_params(blk)[0];
            let a = b.ins().iconst(I32, addr as i64);
            b.ins().call(f, &[cpu, a]);
            b.ins().return_(&[]);
            b.seal_all_blocks();
            b.finalize(tcfg);
        }
        module.define_function(id, &mut ctx).unwrap();
        module.clear_context(&mut ctx);
    }

    // foreign_stub(cpu): rt_foreign(cpu) -- the target of every call that is not recompiled code
    ctx.func.signature = guest_sig.clone();
    ctx.func.name = UserFuncName::user(0, foreign_id.as_u32());
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let f = module.declare_func_in_func(h_foreign, b.func);
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        let cpu = b.block_params(blk)[0];
        b.ins().call(f, &[cpu]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(tcfg);
    }
    module.define_function(foreign_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);

    // enter(cpu, fn): the host calls guest code (tail convention) through this
    ctx.func.signature = sig(host_cc, &[ptr, ptr], None);
    ctx.func.name = UserFuncName::user(0, enter_id.as_u32());
    {
        let mut b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let gs = b.import_signature(guest_sig.clone());
        let blk = b.create_block();
        b.append_block_params_for_function_params(blk);
        b.switch_to_block(blk);
        let (cpu, f) = (b.block_params(blk)[0], b.block_params(blk)[1]);
        b.ins().call_indirect(gs, f, &[cpu]);
        b.ins().return_(&[]);
        b.seal_all_blocks();
        b.finalize(tcfg);
    }
    module.define_function(enter_id, &mut ctx).unwrap();
    module.clear_context(&mut ctx);

    module.finalize_definitions().expect("linking the JIT code");
    stats.compile_ms = t0.elapsed().as_millis() - stats.lift_ms;

    let fns: HashMap<u32, *const u8> = ids.iter().map(|(&a, &id)| (a, module.get_finalized_function(id))).collect();
    let mut table: Vec<(u32, usize)> = fns.iter().map(|(&a, &p)| (a, p as usize)).collect();
    table.sort_unstable();
    let foreign_stub = module.get_finalized_function(foreign_id) as usize;
    let enter = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(*mut Cpu, *const u8)>(module.get_finalized_function(enter_id))
    };
    let mut hooks: Vec<(u32, Hook, usize)> = hooks
        .iter()
        .filter_map(|&(a, h)| Some((a, h, module.get_finalized_function(*originals.get(&a)?) as usize)))
        .collect();
    hooks.sort_unstable_by_key(|h| h.0);
    let _ = DISPATCH.set(Dispatch { table, foreign_stub, enter, hooks });
    Compiled { _module: module, enter, fns, stats }
}

/// Compile the lifted functions on every core; the results in their order.
fn compile_all(isa: &dyn cranelift_codegen::isa::TargetIsa, lifted: Vec<(u32, FuncId, Function)>) -> Vec<(u32, FuncId, Result<Blob, String>)> {
    let n = lifted.len();
    let work: Vec<Mutex<Option<(u32, FuncId, Function)>>> = lifted.into_iter().map(|l| Mutex::new(Some(l))).collect();
    let done: Vec<Mutex<Option<(u32, FuncId, Result<Blob, String>)>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let k = next.fetch_add(1, Ordering::Relaxed);
                    if k >= n {
                        break;
                    }
                    let (addr, id, func) = work[k].lock().unwrap().take().unwrap();
                    let mut ctx = cranelift_codegen::Context::for_function(func);
                    let r = match ctx.compile(isa, &mut ControlPlane::default()) {
                        Ok(_) => {
                            let c = ctx.compiled_code().unwrap();
                            Ok(Blob {
                                id,
                                align: c.buffer.min_alignment as u64,
                                bytes: c.code_buffer().to_vec(),
                                relocs: c.buffer.relocs().iter().map(|r| ModuleReloc::from_mach_reloc(r, &ctx.func, id)).collect(),
                            })
                        }
                        Err(e) => Err(format!("{:?}", e.inner)),
                    };
                    *done[k].lock().unwrap() = Some((addr, id, r));
                }
            });
        }
    });
    done.into_iter().map(|d| d.into_inner().unwrap().unwrap()).collect()
}

// ---- the cache file ---------------------------------------------------------------------------
// "HOGSJIT1", the key (32 bytes), the number of functions; for each: its id, alignment and code
// (length, bytes), its relocations (number; each: offset, kind, target, addend). Numbers are
// little-endian u32 (addends i64). Targets: 0 a function or data object (namespace, index),
// 1 a libcall (its number in `LibCall::all_libcalls`), 2 a place in a function (id, offset).

const MAGIC: &[u8; 8] = b"HOGSJIT1";

/// The relocation kinds the x86-64 code uses (their numbers in the file).
const RELOCS: [Reloc; 6] = [Reloc::Abs4, Reloc::Abs8, Reloc::X86PCRel4, Reloc::X86CallPCRel4, Reloc::X86CallPLTRel4, Reloc::X86GOTPCRel4];

fn save(path: &Path, key: &[u8; 32], blobs: &[Blob]) {
    let mut out = Vec::with_capacity(blobs.iter().map(|b| b.bytes.len() + 24 * b.relocs.len() + 16).sum::<usize>() + 48);
    let u32_ = |out: &mut Vec<u8>, v: u32| out.extend_from_slice(&v.to_le_bytes());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(key);
    u32_(&mut out, blobs.len() as u32);
    for b in blobs {
        u32_(&mut out, b.id.as_u32());
        u32_(&mut out, b.align as u32);
        u32_(&mut out, b.bytes.len() as u32);
        out.extend_from_slice(&b.bytes);
        u32_(&mut out, b.relocs.len() as u32);
        for r in &b.relocs {
            let Some(kind) = RELOCS.iter().position(|&k| k == r.kind) else {
                eprintln!("compiled code not kept: relocation {:?}", r.kind);
                return;
            };
            u32_(&mut out, r.offset);
            u32_(&mut out, kind as u32);
            match &r.name {
                ModuleRelocTarget::User { namespace, index } => {
                    u32_(&mut out, 0);
                    u32_(&mut out, *namespace);
                    u32_(&mut out, *index);
                }
                ModuleRelocTarget::LibCall(l) => {
                    u32_(&mut out, 1);
                    u32_(&mut out, LibCall::all_libcalls().iter().position(|x| x == l).unwrap() as u32);
                    u32_(&mut out, 0);
                }
                ModuleRelocTarget::FunctionOffset(f, o) => {
                    u32_(&mut out, 2);
                    u32_(&mut out, f.as_u32());
                    u32_(&mut out, *o);
                }
                other => {
                    eprintln!("compiled code not kept: relocation target {other:?}");
                    return;
                }
            }
            out.extend_from_slice(&r.addend.to_le_bytes());
        }
    }
    // written beside it first: a game started meanwhile never reads half a file
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, &out).and_then(|_| std::fs::rename(&tmp, path)) {
        eprintln!("{}: compiled code not kept: {e}", path.display());
    }
}

/// Reads the cache file's numbers in turn.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, n: usize) -> Option<&[u8]> {
        let b = self.data.get(self.at..self.at + n)?;
        self.at += n;
        Some(b)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
}

fn load(path: &Path, key: &[u8; 32]) -> Option<Vec<Blob>> {
    let data = std::fs::read(path).ok()?;
    let mut r = Reader { data: &data, at: 0 };
    if r.bytes(8)? != MAGIC || r.bytes(32)? != key {
        return None;
    }
    let n = r.u32()?;
    let mut blobs = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let id = FuncId::from_u32(r.u32()?);
        let align = r.u32()? as u64;
        let len = r.u32()? as usize;
        let bytes = r.bytes(len)?.to_vec();
        let count = r.u32()?;
        let mut relocs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let offset = r.u32()?;
            let kind = *RELOCS.get(r.u32()? as usize)?;
            let (tag, a, b) = (r.u32()?, r.u32()?, r.u32()?);
            let name = match tag {
                0 => ModuleRelocTarget::User { namespace: a, index: b },
                1 => ModuleRelocTarget::LibCall(*LibCall::all_libcalls().get(a as usize)?),
                2 => ModuleRelocTarget::FunctionOffset(FuncId::from_u32(a), b),
                _ => return None,
            };
            relocs.push(ModuleReloc { offset, kind, name, addend: r.i64()? });
        }
        blobs.push(Blob { id, align, bytes, relocs });
    }
    // all of it, and nothing behind it
    (r.at == data.len()).then_some(blobs)
}
