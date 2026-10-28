//! Finding the code: every function's instructions (recursive descent from its entry,
//! following branches and switch tables), plus extra entry points for every place other
//! code jumps or calls into the middle of, and for code addresses stored in data.

use crate::image::Image;
use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Decoding depends only on the address, so every instruction is decoded once.
pub struct Code<'a> {
    pub img: &'a Image,
    cache: HashMap<u32, Option<Instruction>>,
}

impl<'a> Code<'a> {
    pub fn new(img: &'a Image) -> Self {
        Code { img, cache: HashMap::new() }
    }

    pub fn at(&mut self, pc: u32) -> Option<Instruction> {
        let img = self.img;
        *self.cache.entry(pc).or_insert_with(|| {
            let bytes = img.read(pc, 16)?;
            let i = Decoder::with_ip(32, &bytes, pc as u64, DecoderOptions::NONE).decode();
            (!i.is_invalid()).then_some(i)
        })
    }
}

/// A direct branch or call target (jmp/jcc/loop/jecxz/call rel).
pub fn direct_target(i: &Instruction) -> Option<u32> {
    matches!(i.op0_kind(), OpKind::NearBranch32 | OpKind::NearBranch16).then(|| i.near_branch_target() as u32)
}

pub struct Func {
    pub entry: u32,
    pub lo: u32,
    pub hi: u32,
    pub name: String,
    pub ins: BTreeSet<u32>,
    /// jmp [index*4 + table]: the instruction's address -> its targets
    pub tables: HashMap<u32, Vec<u32>>,
    pub targets: HashSet<u32>,
    pub calls: HashSet<u32>,
}

impl Func {
    pub fn inside(&self, a: u32) -> bool {
        self.lo <= a && a < self.hi
    }

    fn new(code: &mut Code, entry: u32, lo: u32, hi: u32, name: String) -> Func {
        let mut f = Func {
            entry,
            lo,
            hi,
            name,
            ins: BTreeSet::new(),
            tables: HashMap::new(),
            targets: HashSet::new(),
            calls: HashSet::new(),
        };
        f.decode(code);
        f
    }

    fn decode(&mut self, code: &mut Code) {
        let mut todo = vec![self.entry];
        while let Some(mut pc) = todo.pop() {
            if !self.inside(pc) {
                continue;
            }
            while !self.ins.contains(&pc) {
                if !self.inside(pc) {
                    // runs on past the end: needs an entry there
                    self.targets.insert(pc);
                    break;
                }
                let Some(i) = code.at(pc) else { break };
                self.ins.insert(pc);
                let fc = i.flow_control();
                match fc {
                    FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch if direct_target(&i).is_some() => {
                        let t = direct_target(&i).unwrap();
                        self.targets.insert(t);
                        if self.inside(t) {
                            todo.push(t);
                        }
                        if fc == FlowControl::UnconditionalBranch {
                            break;
                        }
                    }
                    FlowControl::IndirectBranch | FlowControl::UnconditionalBranch => {
                        if i.op0_kind() == OpKind::Memory
                            && i.memory_index() != Register::None
                            && i.memory_index_scale() == 4
                            && i.memory_base() == Register::None
                        {
                            // "neg ecx; jmp [ecx*4 + T]" (memcpy's tail copy) only reads entries before T
                            let neg = self.ins.contains(&(pc.wrapping_sub(2)))
                                && code.at(pc - 2).is_some_and(|p| {
                                    p.mnemonic() == Mnemonic::Neg
                                        && p.op0_kind() == OpKind::Register
                                        && p.op0_register() == i.memory_index()
                                });
                            let tbl = self.table(code, i.memory_displacement32(), if neg { 8 } else { 4 });
                            for &t in &tbl {
                                self.targets.insert(t);
                                if self.inside(t) {
                                    todo.push(t);
                                }
                            }
                            self.tables.insert(pc, tbl);
                        }
                        break;
                    }
                    _ => match i.mnemonic() {
                        Mnemonic::Ret | Mnemonic::Retf | Mnemonic::Iretd | Mnemonic::Int3 | Mnemonic::Hlt => break,
                        Mnemonic::Call => {
                            if let Some(t) = direct_target(&i) {
                                self.calls.insert(t);
                            }
                        }
                        _ => {}
                    },
                }
                pc += i.len() as u32;
            }
        }
    }

    /// Jump table entries; MSVC's memcpy indexes some tables with negative values, so valid
    /// entries just before the table count too.
    fn table(&self, code: &Code, at: u32, back: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for k in 1..=back {
            let t = code.img.u32(at - 4 * k);
            if !self.inside(t) || self.ins.contains(&(at - 4 * k)) {
                break;
            }
            out.push(t);
        }
        // tables sometimes point into a neighbouring "function" (gap-filling splits):
        // accept any code address, foreign ones become extra entry points
        let mut fwd: Vec<u32> = Vec::new();
        loop {
            let slot = at + 4 * fwd.len() as u32;
            let mut t = code.img.u32(slot);
            if fwd.len() > 512 || (!fwd.is_empty() && self.ins.contains(&slot)) {
                break;
            }
            if !code.img.is_code(t) {
                if !fwd.is_empty() {
                    break; // an unused leading slot may hold anything
                }
                t = 0;
            }
            fwd.push(t);
        }
        out.extend(fwd.into_iter().filter(|&x| x != 0));
        out
    }
}

pub struct Program {
    /// entry -> function
    pub fns: BTreeMap<u32, Func>,
}

impl Program {
    pub fn build(code: &mut Code, ranges: &[(u32, u32, String)]) -> Program {
        let mut fns: BTreeMap<u32, Func> = BTreeMap::new();
        for (lo, hi, name) in ranges {
            fns.insert(*lo, Func::new(code, *lo, *lo, *hi, name.clone()));
        }
        // code addresses stored in data that point into a function's decoded code (tables of
        // jump targets) must be enterable too; those pointing elsewhere were weighed by
        // `find_functions`
        let insn_at: HashSet<u32> = fns.values().flat_map(|f| f.ins.iter().copied()).collect();
        let data_ptrs: HashSet<u32> = code
            .img
            .sections
            .iter()
            .filter(|s| !s.exec)
            .flat_map(|s| words(code.img, s))
            .filter(|t| insn_at.contains(t) && !fns.contains_key(t))
            .collect();
        // add entries until nothing new turns up
        let los: Vec<u32> = ranges.iter().map(|r| r.0).collect();
        let mut todo: Vec<Vec<u32>> =
            fns.values().map(|f| f.targets.iter().chain(&f.calls).copied().filter(|&t| !(f.inside(t) && f.ins.contains(&t))).collect()).collect();
        todo.push(data_ptrs.into_iter().collect());
        while !todo.is_empty() {
            let mut next = Vec::new();
            let mut wanted: Vec<u32> = todo.into_iter().flatten().collect();
            wanted.sort_unstable();
            wanted.dedup();
            for t in wanted {
                if fns.contains_key(&t) || !code.img.is_code(t) {
                    continue;
                }
                let k = los.partition_point(|&lo| lo <= t);
                if k == 0 || t >= ranges[k - 1].1 {
                    continue;
                }
                let (lo, hi, name) = &ranges[k - 1];
                let e = Func::new(code, t, *lo, *hi, format!("{name}+0x{:x}", t - lo));
                next.push(e.targets.iter().chain(&e.calls).copied().filter(|&x| !(e.inside(x) && e.ins.contains(&x))).collect());
                fns.insert(t, e);
            }
            todo = next;
        }
        Program { fns }
    }
}

/// The dwords of a section's raw data, aligned.
fn words(img: &Image, s: &crate::image::Section) -> Vec<u32> {
    let blob = img.read(s.va, s.rsize as usize).unwrap_or_default();
    blob.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
}

/// Function boundaries without a disassembler's export: every function entry reachable from
/// `seeds` (the entry point, a DLL's exports) through calls, plus code addresses stored in data
/// that no decoded code covers yet (callbacks, tables of functions). Each function runs to the
/// next entry; sorted (start, end, name).
pub fn find_functions(code: &mut Code, seeds: &[(u32, String)]) -> Vec<(u32, u32, String)> {
    let img = code.img;
    let section_of = |a: u32| img.sections.iter().find(|s| s.exec && a >= s.va && a < s.va + s.vsize.max(s.rsize));
    let mut names: HashMap<u32, String> = seeds.iter().filter(|s| img.is_code(s.0)).cloned().collect();
    let mut entries: BTreeSet<u32> = names.keys().copied().collect();
    // decoded instruction starts, and bytes strictly inside a decoded instruction
    let mut seen: (HashSet<u32>, HashSet<u32>) = Default::default();
    let decode = |code: &mut Code, entries: &mut BTreeSet<u32>, seen: &mut (HashSet<u32>, HashSet<u32>), mut todo: Vec<u32>| {
        while let Some(e) = todo.pop() {
            let s = section_of(e).unwrap();
            let f = Func::new(code, e, s.va, s.va + s.vsize.max(s.rsize), String::new());
            for &a in &f.ins {
                if seen.0.insert(a) {
                    seen.1.extend(a + 1..a + code.at(a).unwrap().len() as u32);
                }
            }
            for &t in &f.calls {
                if img.is_code(t) && entries.insert(t) {
                    todo.push(t);
                }
            }
        }
    };
    decode(code, &mut entries, &mut seen, names.keys().copied().collect());
    // code addresses that nothing decoded reaches: functions only called through pointers,
    // stored in data (also in the code sections, where the linker put some tables of
    // functions: words no decoded instruction covers) or as an instruction's operand (window
    // procedures, callbacks). One at a time, so one found inside another's code does not
    // split it.
    let data: Vec<u32> = img.sections.iter().filter(|s| !s.exec).flat_map(|s| words(img, s)).collect();
    let mut tried = HashSet::new();
    loop {
        let covered = |a: u32| (a..a + 4).any(|b| seen.0.contains(&b) || seen.1.contains(&b));
        let in_code: Vec<u32> = img
            .sections
            .iter()
            .filter(|s| s.exec)
            .flat_map(|s| (s.va..s.va + s.vsize.min(s.rsize)).step_by(4))
            .filter(|&a| !covered(a))
            .map(|a| img.u32(a))
            .collect();
        let operands = seen.0.iter().filter_map(|&a| {
            let i = code.at(a).unwrap();
            (i.op_kinds().any(|k| k == OpKind::Immediate32)).then(|| i.immediate32())
        });
        let mut wanted: Vec<u32> = data.iter().copied().chain(in_code).chain(operands.collect::<Vec<_>>()).filter(|&t| img.is_code(t) && tried.insert(t)).collect();
        if wanted.is_empty() {
            break;
        }
        wanted.sort_unstable();
        for t in wanted {
            if !seen.0.contains(&t)
                && !seen.1.contains(&t)
                && !matches!(img.byte(t), 0x00 | 0x90 | 0xCC)
                && plausible(code, t, &seen)
                && entries.insert(t)
            {
                decode(code, &mut entries, &mut seen, vec![t]);
            }
        }
    }
    let list: Vec<u32> = entries.into_iter().collect();
    list.iter()
        .enumerate()
        .map(|(k, &lo)| {
            let s = section_of(lo).unwrap();
            let end = s.va + s.vsize.max(s.rsize);
            let hi = list.get(k + 1).copied().filter(|&n| n < end).unwrap_or(end);
            (lo, hi, names.remove(&lo).unwrap_or_else(|| format!("sub_{lo:x}")))
        })
        .collect()
}

/// Whether `t`, found only as a number, looks like the start of a function: its code decodes
/// without running into the middle of known instructions (`seen`: their starts, and the
/// bytes inside them; known code it jumps to is fine) and without instructions compiled
/// 32-bit code does not use (what data decoded as code is full of).
fn plausible(code: &mut Code, t: u32, seen: &(HashSet<u32>, HashSet<u32>)) -> bool {
    use Mnemonic::*;
    let img = code.img;
    let Some(s) = img.sections.iter().find(|s| s.exec && t >= s.va && t < s.va + s.vsize) else { return false };
    let f = Func::new(code, t, s.va, s.va + s.vsize, String::new());
    f.ins.iter().filter(|a| !seen.0.contains(a)).all(|&a| {
        let i = code.at(a).unwrap();
        let odd = matches!(
            i.mnemonic(),
            Aaa | Aad | Aam | Aas | Daa | Das | Arpl | Bound | Lds | Les | Lfs | Lgs | Lss | Into | Int1 | Iretd | Retf | Salc
                | Insb | Insd | Insw | Outsb | Outsd | Outsw | In | Out | Hlt | Cli | Sti | Enter | Wait
        ) || i.op_kinds().any(|k| matches!(k, OpKind::FarBranch16 | OpKind::FarBranch32))
            || (i.memory_base() != Register::None && i.memory_base().size() == 2)
            || !matches!(i.segment_prefix(), Register::None | Register::FS);
        !odd && !(a..a + i.len() as u32).any(|b| seen.0.contains(&b) && b != a || seen.1.contains(&b))
    }) && !f.ins.is_empty()
}
