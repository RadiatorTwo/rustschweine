//! Self-modifying code: a snippet rewrites a shift count, a displacement and a jump offset in
//! its own code (like the game's renderer), and the recompiled code must follow. Needs no
//! game files.

mod common;

use common::{Guest, SCRATCH, Snippet, TEXT};
use rustschweine::rt::{EAX, EDX, ESI};
use iced_x86::code_asm::*;
use iced_x86::{BlockEncoderOptions, IcedError};
use std::sync::Mutex;

/// Addresses of the fields and jump targets, from a first assembly (code_asm cannot write
/// "label + 2" as an absolute address in 32-bit code).
#[derive(Clone, Copy, Default)]
struct Marks {
    shl: u32,
    load: u32,
    jmp: u32,
    one: u32,
    two: u32,
}
static MARKS: Mutex<Marks> = Mutex::new(Marks { shl: 0, load: 0, jmp: 0, one: 0, two: 0 });

/// (count, address, offset): edx = 1 << count, eax = [address], esi = 1 or 2 depending on
/// where the rewritten jump goes
fn smc(a: &mut CodeAssembler) -> Result<(), IcedError> {
    smc_with(a, *MARKS.lock().unwrap()).map(|_| ())
}

fn smc_with(a: &mut CodeAssembler, m: Marks) -> Result<[CodeLabel; 5], IcedError> {
    let mut l = [a.create_label(), a.create_label(), a.create_label(), a.create_label(), a.create_label()];
    // a static path to the second target, so discovery decodes it (in the game, rewritten
    // jumps go to code that is reachable or whose address is stored in data)
    a.cmp(dword_ptr(esp + 4), -1)?;
    a.je(l[4])?;
    a.mov(eax, dword_ptr(esp + 4))?;
    a.mov(byte_ptr(m.shl + 2), al)?; // shl edx, imm8
    a.mov(eax, dword_ptr(esp + 8))?;
    a.mov(dword_ptr(m.load + 2), eax)?; // mov eax, [ebx + disp32]
    a.mov(eax, dword_ptr(esp + 12))?;
    a.mov(dword_ptr(m.jmp + 1), eax)?; // jmp rel32
    a.mov(edx, 1)?;
    a.set_label(&mut l[0])?;
    a.shl(edx, 2)?; // not 1: that has a form without an immediate
    a.xor(ebx, ebx)?;
    a.set_label(&mut l[1])?;
    a.mov(eax, dword_ptr(ebx + 0x1000))?;
    a.set_label(&mut l[2])?;
    a.db(&[0xe9, 0, 0, 0, 0])?; // jmp rel32 to the next instruction (the assembler would shorten it)
    a.set_label(&mut l[3])?;
    a.mov(esi, 1)?;
    a.ret()?;
    a.set_label(&mut l[4])?;
    a.mov(esi, 2)?;
    a.ret()?;
    Ok(l)
}

const SNIPPETS: [(&str, Snippet); 1] = [("smc", smc)];

#[test]
fn self_modifying() {
    // first pass: where the labels end up (the snippet is the first one, at TEXT)
    let mut a = CodeAssembler::new(32).unwrap();
    let l = smc_with(&mut a, Marks { shl: TEXT, load: TEXT, jmp: TEXT, one: 0, two: 0 }).unwrap();
    let r = a.assemble_options(TEXT as u64, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
    let ip = |k: usize| r.label_ip(&l[k]).unwrap() as u32;
    let m = Marks { shl: ip(0), load: ip(1), jmp: ip(2), one: ip(3), two: ip(4) };
    *MARKS.lock().unwrap() = m;

    let g = Guest::new(&SNIPPETS);
    assert!(g.smc.imm.contains_key(&m.shl) && g.smc.imm.contains_key(&m.jmp), "shift count and jump offset found");
    assert!(g.smc.disp.contains_key(&m.load), "displacement found");

    for (k, count) in [3u32, 0, 31, 7].into_iter().enumerate() {
        let at = SCRATCH + 4 * k as u32;
        g.mem.wr32(at, 0xabc0 + k as u32);
        let target = if k % 2 == 0 { m.one } else { m.two };
        let rel = target.wrapping_sub(m.jmp + 5);
        let cpu = g.call(0, &[count, at, rel]);
        assert_eq!(cpu.r[EDX], 1 << count, "shl by a rewritten count {count}");
        assert_eq!(cpu.r[EAX], 0xabc0 + k as u32, "load through a rewritten displacement");
        assert_eq!(cpu.r[ESI], if k % 2 == 0 { 1 } else { 2 }, "jump through a rewritten offset");
    }
}
