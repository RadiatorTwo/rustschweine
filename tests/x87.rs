//! x87 lifting checked against Rust: small assembled snippets are recompiled like the game's
//! code and run on random inputs. Needs no game files.

mod common;

use common::{Guest, Rng, SCRATCH, Snippet, dbl};
use rustschweine::rt::{EAX, f64_to_f80};
use iced_x86::IcedError;
use iced_x86::code_asm::*;


/// ST0 = -sqrt(|(a - a*b) + b/a|), through fld st(i), fxch and the popping forms.
fn arith(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.fld(qword_ptr(esp + 4))?; // a
    a.fld(qword_ptr(esp + 12))?; // b a
    a.fld(st1)?; // a b a
    a.fmul_2(st0, st1)?; // ab b a
    a.fsubr(qword_ptr(esp + 4))?; // a-ab b a
    a.fxch(st0, st2)?; // a b t
    a.fdivp(st1, st0)?; // b/a t
    a.faddp(st1, st0)?; // t+b/a
    a.fabs()?;
    a.fsqrt()?;
    a.fchs()?;
    a.ret()
}

/// (x, y, cw, out): out = x+y, x-y, x*y, x/y, sqrt|x| as doubles, computed under control
/// word cw (Direct3D's 24 bits to nearest takes the generated code's float path)
fn single(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(eax, dword_ptr(esp + 24))?;
    a.fnstcw(word_ptr(eax + 40))?;
    a.fldcw(word_ptr(esp + 20))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fadd(qword_ptr(esp + 12))?;
    a.fstp(qword_ptr(eax))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fsub(qword_ptr(esp + 12))?;
    a.fstp(qword_ptr(eax + 8))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fmul(qword_ptr(esp + 12))?;
    a.fstp(qword_ptr(eax + 16))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fdiv(qword_ptr(esp + 12))?;
    a.fstp(qword_ptr(eax + 24))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fabs()?;
    a.fsqrt()?;
    a.fstp(qword_ptr(eax + 32))?;
    a.fldcw(word_ptr(eax + 40))?;
    a.ret()
}

/// memcpy of a qword through the FPU: fild/fistp qword [src] -> [dst]
fn copy64(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(eax, dword_ptr(esp + 4))?;
    a.mov(ecx, dword_ptr(esp + 8))?;
    a.fild(qword_ptr(eax))?;
    a.fistp(qword_ptr(ecx))?;
    a.ret()
}

/// (x: f64, cw: u16, out): out = {i32, i16, _, i64} rounded with control word cw
fn round(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(eax, dword_ptr(esp + 16))?;
    a.fnstcw(word_ptr(eax + 16))?;
    a.fldcw(word_ptr(esp + 12))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fist(dword_ptr(eax))?;
    a.fist(word_ptr(eax + 4))?;
    a.fistp(qword_ptr(eax + 8))?;
    a.fldcw(word_ptr(eax + 16))?;
    a.ret()
}

/// (a, b) -> eax = 0 equal, 1 greater, 2 less, 3 unordered, via fcompp/fnstsw/sahf/jcc;
/// ecx = the status word
fn compare(a: &mut CodeAssembler) -> Result<(), IcedError> {
    let (mut gt, mut lt, mut un) = (a.create_label(), a.create_label(), a.create_label());
    a.fld(qword_ptr(esp + 12))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fcompp()?;
    a.fnstsw(ax)?;
    a.movzx(ecx, ax)?;
    a.sahf()?;
    a.jp(un)?;
    a.ja(gt)?;
    a.jb(lt)?;
    a.mov(eax, 0)?;
    a.ret()?;
    a.set_label(&mut gt)?;
    a.mov(eax, 1)?;
    a.ret()?;
    a.set_label(&mut lt)?;
    a.mov(eax, 2)?;
    a.ret()?;
    a.set_label(&mut un)?;
    a.mov(eax, 3)?;
    a.ret()
}

/// p -> {i16 at 0, i32 at 4, f32 at 8}: f32 at 12 and i16 at 16 = (i16 + i32) * f32
fn mixed(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(eax, dword_ptr(esp + 4))?;
    a.fild(word_ptr(eax))?;
    a.fiadd(dword_ptr(eax + 4))?;
    a.fmul(dword_ptr(eax + 8))?;
    a.fst(dword_ptr(eax + 12))?;
    a.fistp(word_ptr(eax + 16))?;
    a.ret()
}

/// (x: f64, p): tbyte at p, and back to a double at p + 16
fn f80(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.mov(eax, dword_ptr(esp + 12))?;
    a.fld(qword_ptr(esp + 4))?;
    a.fstp(tword_ptr(eax))?;
    a.fld(tword_ptr(eax))?;
    a.fstp(qword_ptr(eax + 16))?;
    a.ret()
}

/// eight pushes (TOP wraps around) summed up: ST0 = 1+1+...+pi
fn stack8(a: &mut CodeAssembler) -> Result<(), IcedError> {
    a.fldpi()?;
    for _ in 0..7 {
        a.fld1()?;
    }
    for _ in 0..7 {
        a.faddp(st1, st0)?;
    }
    a.ret()
}

const SNIPPETS: [(&str, Snippet); 8] =
    [("arith", arith), ("copy64", copy64), ("round", round), ("compare", compare), ("mixed", mixed), ("f80", f80), ("stack8", stack8), ("single", single)];

fn same(x: f64, y: f64) -> bool {
    x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan())
}

/// x87 rounding of a double to an n-byte integer (indefinite = smallest integer)
fn round_ref(x: f64, rc: u16, bits: u32) -> i64 {
    let r = match rc {
        0 => x.round_ties_even(),
        1 => x.floor(),
        2 => x.ceil(),
        _ => x.trunc(),
    };
    let lim = 2f64.powi(bits as i32 - 1);
    if r >= -lim && r < lim { r as i64 } else { i64::MIN >> (64 - bits) }
}

#[test]
fn x87() {
    let g = Guest::new(&SNIPPETS);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let m = &g.mem;
    for _ in 0..3000 {
        let (x, y) = (rng.f64(), rng.f64());

        let cpu = g.call(0, &[dbl(x)[0], dbl(x)[1], dbl(y)[0], dbl(y)[1]]);
        let want = -((x - x * y) + y / x).abs().sqrt();
        assert!(same(cpu.st(0), want), "arith({x}, {y}) = {}, want {want}", cpu.st(0));
        assert_eq!(cpu.top, 7, "arith leaves one value");

        let v = rng.next();
        m.slice(SCRATCH, 8).copy_from_slice(&v.to_le_bytes());
        let cpu = g.call(1, &[SCRATCH, SCRATCH + 8]);
        assert_eq!(m.slice(SCRATCH + 8, 8), &v.to_le_bytes(), "copy64({v:#x})");
        assert_eq!(cpu.top, 0);

        let rc = (rng.next() % 4) as u16;
        let cw = 0x27f | rc << 10;
        let cpu = g.call(2, &[dbl(x)[0], dbl(x)[1], cw as u32, SCRATCH]);
        let i32_ = m.rd32(SCRATCH) as i32 as i64;
        let i16_ = i16::from_le_bytes([m.rd8(SCRATCH + 4), m.rd8(SCRATCH + 5)]) as i64;
        let i64_ = i64::from_le_bytes(m.slice(SCRATCH + 8, 8).try_into().unwrap());
        let want = (round_ref(x, rc, 32), round_ref(x, rc, 16), round_ref(x, rc, 64));
        assert_eq!((i32_, i16_, i64_), want, "round({x}, rc {rc})");
        assert_eq!(cpu.fcw, 0x37f, "control word restored");

        let cpu = g.call(3, &[dbl(x)[0], dbl(x)[1], dbl(y)[0], dbl(y)[1]]);
        let want = if x.is_nan() || y.is_nan() {
            3
        } else if x > y {
            1
        } else if x < y {
            2
        } else {
            0
        };
        assert_eq!(cpu.r[EAX], want, "compare({x}, {y})");
        let sw = cpu.r[1];
        assert_eq!((sw >> 11) & 7, 0, "fcompp pops both: TOP 0 in the status word");

        let (i, j, f) = (rng.next() as i16, rng.next() as i32 >> (rng.next() % 32), rng.f64() as f32);
        m.slice(SCRATCH, 2).copy_from_slice(&i.to_le_bytes());
        m.wr32(SCRATCH + 4, j as u32);
        m.wr32(SCRATCH + 8, f.to_bits());
        g.call(4, &[SCRATCH]);
        let p = (i as f64 + j as f64) * f as f64;
        let got_f = f32::from_bits(m.rd32(SCRATCH + 12));
        let got_i = i16::from_le_bytes([m.rd8(SCRATCH + 16), m.rd8(SCRATCH + 17)]) as i64;
        assert!(got_f.to_bits() == (p as f32).to_bits() || (p.is_nan() && got_f.is_nan()), "mixed f32 ({i}, {j}, {f})");
        assert_eq!(got_i, round_ref(p, 0, 16), "mixed i16 ({i}, {j}, {f})");

        g.call(5, &[dbl(x)[0], dbl(x)[1], SCRATCH]);
        assert_eq!(m.slice(SCRATCH, 10), &f64_to_f80(x), "f80 bytes of {x}");
        let back = f64::from_bits(u64::from_le_bytes(m.slice(SCRATCH + 16, 8).try_into().unwrap()));
        assert!(same(back, x), "f80 round trip of {x} gave {back}");
    }
    // 24 bits to nearest (inline) and truncating (rt_fpu_round), against rt_fpu_round itself;
    // floats' sums and products often lie on a float's midpoint
    for k in 0..20000 {
        let (mut x, mut y) = (rng.f64(), rng.f64());
        if k % 2 == 0 {
            x = x as f32 as f64;
            y = y as f32 as f64;
        }
        if k % 7 == 0 {
            // small integers: exact sums on midpoints
            x = (rng.next() % 0x200_0000) as f64;
            y = (rng.next() % 0x100) as f64;
        }
        for cw in [0x3fu32, 0x7f, 0xc3f] {
            g.call(7, &[dbl(x)[0], dbl(x)[1], dbl(y)[0], dbl(y)[1], cw, SCRATCH]);
            for (op, at) in [(0u32, 0u32), (1, 8), (2, 16), (3, 24), (4, 32)] {
                let got = f64::from_bits(u64::from_le_bytes(m.slice(SCRATCH + at, 8).try_into().unwrap()));
                let a = if op == 4 { x.abs() } else { x };
                let want = rustschweine::rt::rt_fpu_round(op, a, y, cw);
                assert!(same(got, want), "op {op} ({x:e}, {y:e}) under cw {cw:#x}: {got:e}, want {want:e}");
            }
        }
    }
    assert_eq!(f64_to_f80(1.0), [0, 0, 0, 0, 0, 0, 0, 0x80, 0xff, 0x3f]);
    assert_eq!(f64_to_f80(-2.0), [0, 0, 0, 0, 0, 0, 0, 0x80, 0x00, 0xc0]);
    let cpu = g.call(6, &[]);
    assert_eq!(cpu.st(0), 7.0 + std::f64::consts::PI);
    assert_eq!(cpu.top, 7);
}
