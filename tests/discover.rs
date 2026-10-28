//! Finding functions without a disassembler's export: through calls, through addresses in
//! instruction operands and in data, and not in jump tables or data decoded as code. Needs no
//! game files.

use iced_x86::BlockEncoderOptions;
use iced_x86::code_asm::*;
use rustschweine::discover::{Code, find_functions};
use rustschweine::image::{Image, Section};

const TEXT: u32 = 0x401000;
const DATA: u32 = 0x402000;

#[test]
fn finds_functions() {
    let mut a = CodeAssembler::new(32).unwrap();
    let (mut called, mut pushed, mut stored) = (a.create_label(), a.create_label(), a.create_label());
    // entry: calls one function, passes another's address (a callback)
    a.call(called).unwrap();
    a.push(0x7777_7777).unwrap(); // patched below: the callback's address
    a.ret().unwrap();
    a.int3().unwrap();
    a.set_label(&mut called).unwrap();
    a.mov(eax, 1).unwrap();
    a.ret().unwrap();
    a.int3().unwrap();
    a.set_label(&mut pushed).unwrap();
    a.mov(eax, 2).unwrap();
    a.ret().unwrap();
    a.int3().unwrap();
    a.set_label(&mut stored).unwrap();
    a.mov(eax, 3).unwrap();
    a.ret().unwrap();
    a.int3().unwrap();
    // never referenced
    a.mov(eax, 4).unwrap();
    a.ret().unwrap();
    let r = a.assemble_options(TEXT as u64, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
    let at = |l: &CodeLabel| r.label_ip(l).unwrap() as u32;
    let mut text = r.inner.code_buffer.clone();
    // `push imm32` is 68 xx xx xx xx, right after the 5-byte call
    text[6..10].copy_from_slice(&at(&pushed).to_le_bytes());
    text.resize(0x1000, 0xcc);
    // data: a pointer to a function, and one into the middle of an instruction (`mov eax, 3`
    // is 5 bytes; data that merely looks like an address)
    let mut data = Vec::new();
    data.extend(at(&stored).to_le_bytes());
    data.extend((at(&stored) + 2).to_le_bytes());
    data.resize(0x100, 0);
    let n = data.len() as u32;
    let img = Image {
        data: [text, data].concat(),
        base: 0x400000,
        entry: TEXT,
        sections: vec![
            Section { name: ".text".into(), va: TEXT, vsize: 0x1000, raw: 0, rsize: 0x1000, exec: true },
            Section { name: ".data".into(), va: DATA, vsize: n, raw: 0x1000, rsize: n, exec: false },
        ],
    };
    let mut code = Code::new(&img);
    let found: Vec<u32> = find_functions(&mut code, &[(TEXT, "entry".into())]).iter().map(|f| f.0).collect();
    assert_eq!(found, vec![TEXT, at(&called), at(&pushed), at(&stored)]);
}
