//! The test harness: x86 snippets assembled with iced's code_asm become a tiny program at
//! 0x401000 that is recompiled like the game's code and called with guest memory set up.

#![allow(dead_code)]

use rustschweine::discover::{Code, Program};
use rustschweine::image::{Image, Section};
use rustschweine::jit::{Compiled, compile};
use rustschweine::rt::{Cpu, ESP, Memory};
use rustschweine::smc::Smc;
use iced_x86::IcedError;
use iced_x86::code_asm::CodeAssembler;

pub const TEXT: u32 = 0x401000;
const STACK_LO: u32 = 0x0010_0000;
const STACK_HI: u32 = 0x0020_0000;
pub const SCRATCH: u32 = 0x0100_0000;
const RET_MAGIC: u32 = 0x7ffe_0000;

pub type Snippet = fn(&mut CodeAssembler) -> Result<(), IcedError>;

pub struct Guest {
    c: Compiled,
    pub mem: Memory,
    pub smc: Smc,
    addr: Vec<u32>,
}

impl Guest {
    /// Recompile the snippets; snippet k is called with `call(k, ...)`.
    pub fn new(snippets: &[(&str, Snippet)]) -> Guest {
        let mut data = Vec::new();
        let mut ranges = Vec::new();
        for &(name, f) in snippets {
            let at = TEXT + data.len() as u32;
            let mut a = CodeAssembler::new(32).unwrap();
            f(&mut a).unwrap();
            data.extend(a.assemble(at as u64).unwrap());
            ranges.push((at, TEXT + data.len() as u32, name.to_string()));
            data.resize(data.len().next_multiple_of(16), 0xcc);
        }
        let n = data.len() as u32;
        let img = Image { data, base: 0x400000, entry: TEXT, sections: vec![Section { name: ".text".into(), va: TEXT, vsize: n, raw: 0, rsize: n, exec: true }] };
        let mut code = Code::new(&img);
        let prog = Program::build(&mut code, &ranges);
        // code that writes its own fields is recompiled like the game's renderer
        let smc = Smc::find(&prog, &mut code);
        let c = compile(&prog, &smc, &mut code, &[]);
        assert_eq!(c.stats.complete, snippets.len(), "not supported: {:?}", c.stats.missing);
        let mem = Memory::reserve();
        mem.map(TEXT, TEXT + n);
        mem.slice(TEXT, n as usize).copy_from_slice(&img.data);
        mem.map(STACK_LO, STACK_HI);
        mem.map(SCRATCH, SCRATCH + 0x1000);
        Guest { c, mem, addr: ranges.iter().map(|r| r.0).collect(), smc }
    }

    /// cdecl call of snippet `k` with dword arguments; returns the CPU afterwards.
    pub fn call(&self, k: usize, args: &[u32]) -> Cpu {
        let mut cpu = Cpu::new(&self.mem);
        let mut top = STACK_HI - 0x100;
        for &a in args.iter().rev() {
            top -= 4;
            self.mem.wr32(top, a);
        }
        top -= 4;
        self.mem.wr32(top, RET_MAGIC);
        cpu.r[ESP] = top;
        self.c.call(&mut cpu, self.addr[k]);
        cpu
    }
}


pub fn dbl(x: f64) -> [u32; 2] {
    let b = x.to_bits();
    [b as u32, (b >> 32) as u32]
}

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// doubles of every kind: specials, small integers and halves, any bit pattern
    pub fn f64(&mut self) -> f64 {
        const SPECIAL: [f64; 10] = [0.0, -0.0, 1.0, -1.0, 0.5, 2.5, -2.5, f64::INFINITY, f64::NEG_INFINITY, f64::NAN];
        match self.next() % 4 {
            0 => SPECIAL[(self.next() % 10) as usize],
            1 => ((self.next() % 2001) as f64 - 1000.0) / 2.0,
            2 => ((self.next() >> 11) as f64 - (1u64 << 52) as f64) * 2f64.powi((self.next() % 80) as i32 - 40),
            _ => f64::from_bits(self.next()),
        }
    }
}

