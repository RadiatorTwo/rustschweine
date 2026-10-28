//! Self-modifying code: fields of instructions that the game writes with
//! `mov [code address], x`. Fields that are exactly an operand (immediate, displacement,
//! branch offset) are read from guest memory by the generated code.

use crate::discover::{Code, Program};
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};
use std::collections::{BTreeMap, HashMap};

/// A code field written at run time: its guest address and size in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub at: u32,
    pub size: u32,
}

#[derive(Default)]
pub struct Smc {
    /// instruction address -> its immediate (for branches: the offset), read at run time
    pub imm: HashMap<u32, Field>,
    /// instruction address -> its memory displacement, read at run time
    pub disp: HashMap<u32, Field>,
}

impl Smc {
    pub fn find(prog: &Program, code: &mut Code) -> Smc {
        let mut starts: Vec<u32> = prog.fns.values().flat_map(|f| f.ins.iter().copied()).collect();
        starts.sort_unstable();
        starts.dedup();

        let mut fields: BTreeMap<u32, u32> = BTreeMap::new();
        for &a in &starts {
            let i = code.at(a).unwrap();
            if i.mnemonic() == Mnemonic::Mov
                && i.op0_kind() == OpKind::Memory
                && i.memory_base() == Register::None
                && i.memory_index() == Register::None
                && code.img.is_code(i.memory_displacement32())
            {
                fields.insert(i.memory_displacement32(), i.memory_size().size() as u32);
            }
        }

        let mut smc = Smc::default();
        for (&f, &n) in &fields {
            let k = starts.partition_point(|&a| a <= f);
            let Some(&a) = k.checked_sub(1).map(|k| &starts[k]) else { continue };
            let i = code.at(a).unwrap();
            if f >= a + i.len() as u32 {
                eprintln!("warning: code field {f:#x} is not inside a decoded instruction");
                continue;
            }
            let co = offsets(code, a);
            let off = (f - a) as usize;
            let field = Field { at: f, size: n };
            if co.has_immediate() && off == co.immediate_offset() && n as usize == co.immediate_size() {
                smc.imm.insert(a, field);
            } else if co.has_displacement() && off == co.displacement_offset() && n as usize == co.displacement_size() {
                smc.disp.insert(a, field);
            } else {
                eprintln!("warning: code field {f:#x} is not an operand of `{i}` at {a:#x}");
            }
        }
        smc
    }
}

fn offsets(code: &Code, a: u32) -> iced_x86::ConstantOffsets {
    let bytes = code.img.read(a, 16).unwrap();
    let mut d = Decoder::with_ip(32, &bytes, a as u64, DecoderOptions::NONE);
    let i = d.decode();
    d.get_constant_offsets(&i)
}
