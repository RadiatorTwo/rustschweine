//! MMX moves, port I/O and segment pushes, checked against Rust. Needs no game files.

mod common;

use common::{Guest, Rng, SCRATCH, Snippet};
use rustschweine::rt::{EAX, ECX, EDI, ESP};
use iced_x86::IcedError;
use iced_x86::code_asm::*;

/// (src, dst): 32 bytes through mm0-mm3 with movq, plus a dword through movd mm4/ecx
fn mmx(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(edx, dword_ptr(esp + 4))?;
    a.mov(eax, dword_ptr(esp + 8))?;
    a.movq(mm0, qword_ptr(edx))?;
    a.movq(mm1, qword_ptr(edx + 8))?;
    a.movq(mm2, qword_ptr(edx + 16))?;
    a.movq(mm3, mm2)?;
    a.movq(mm2, qword_ptr(edx + 24))?;
    a.movq(qword_ptr(eax), mm0)?;
    a.movq(qword_ptr(eax + 8), mm1)?;
    a.movq(qword_ptr(eax + 16), mm3)?;
    a.movq(qword_ptr(eax + 24), mm2)?;
    a.movd(mm4, dword_ptr(edx + 32))?;
    a.movd(ecx, mm4)?;
    a.emms()?;
    a.ret()
}

/// (p): nothing on the ports: eax = in al (rest of eax kept), ecx = in eax; two insb up, one down
fn ports(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(edi, dword_ptr(esp + 4))?;
    a.mov(dx, 0x201)?;
    a.mov(eax, 0x12345600)?;
    a.out(dx, al)?;
    a.in_(al, dx)?;
    a.mov(ecx, eax)?;
    a.in_(eax, 0x40)?;
    a.xchg(eax, ecx)?;
    a.insb()?;
    a.insb()?;
    a.std()?;
    a.insb()?;
    a.cld()?;
    a.ret()
}

/// push es / pop es keep the stack balanced
fn segs(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(ecx, esp)?;
    a.push(es)?;
    a.push(es)?;
    a.pop(es)?;
    a.pop(es)?;
    a.sub(ecx, esp)?;
    a.ret()
}

const SNIPPETS: [(&str, Snippet); 3] = [("mmx", mmx), ("ports", ports), ("segs", segs)];

#[test]
fn misc() {
    let g = Guest::new(&SNIPPETS);
    let m = &g.mem;
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..100 {
        let src: Vec<u8> = (0..36).map(|_| rng.next() as u8).collect();
        m.slice(SCRATCH, 36).copy_from_slice(&src);
        m.slice(SCRATCH + 64, 32).fill(0);
        let cpu = g.call(0, &[SCRATCH, SCRATCH + 64]);
        assert_eq!(m.slice(SCRATCH + 64, 32), &src[..32]);
        assert_eq!(cpu.r[ECX], u32::from_le_bytes(src[32..36].try_into().unwrap()));
    }

    m.slice(SCRATCH, 4).fill(0);
    let cpu = g.call(1, &[SCRATCH + 1]);
    assert_eq!(cpu.r[EAX], 0x123456ff);
    assert_eq!(cpu.r[ECX], 0xffffffff);
    assert_eq!(m.slice(SCRATCH, 4), &[0, 0xff, 0xff, 0xff]);
    assert_eq!(cpu.r[EDI], SCRATCH + 2, "insb: up, up, then down");

    let cpu = g.call(2, &[]);
    assert_eq!(cpu.r[ECX], 0);
    assert_eq!(cpu.r[ESP], 0x0020_0000 - 0x100, "the return popped the return address");
}
