//! Compares our function discovery with a Ghidra export:  ghidra <game folder>
//! Reads data/warhogs_functions.json and data/d3d_functions.json (written by
//! tools/ghidra/ExportFunctions.java: one object per line) and lists the functions Ghidra
//! found that we do not have as an entry, those we have that Ghidra does not, and the names
//! Ghidra gave (other than FUN_...) that data/symbols.txt does not have yet.
use rustschweine::discover::{Code, Program};
use rustschweine::game::Game;
use std::collections::BTreeMap;
use std::path::Path;

/// (address, size, name) of each function in an export.
fn read(path: &str) -> Vec<(u32, u32, String)> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let field = |line: &str, key: &str| -> Option<String> {
        let rest = &line[line.find(&format!("\"{key}\": "))? + key.len() + 4..];
        Some(match rest.strip_prefix('"') {
            Some(s) => s[..s.find('"')?].to_string(),
            None => rest[..rest.find([',', '}'])?].to_string(),
        })
    };
    text.lines()
        .filter_map(|l| {
            let a = u32::from_str_radix(field(l, "addr")?.trim_start_matches("0x"), 16).ok()?;
            Some((a, field(l, "size")?.parse().ok()?, field(l, "name")?))
        })
        .collect()
}

fn main() {
    let dir = std::env::args().nth(1).expect("path to the game folder");
    let game = Game::load(Path::new(&dir)).unwrap();
    let mut code = Code::new(&game.all);
    let ranges = game.functions(&mut code);
    let prog = Program::build(&mut code, &ranges);
    // every instruction we decode, for "inside one of ours"
    let decoded: std::collections::HashSet<u32> = prog.fns.values().flat_map(|f| f.ins.iter().copied()).collect();
    let known = rustschweine::image::symbols();
    for file in ["data/warhogs_functions.json", "data/d3d_functions.json"] {
        let g = read(file);
        let ours: BTreeMap<u32, &str> = ranges.iter().map(|(lo, _, n)| (*lo, n.as_str())).filter(|(a, _)| g.first().is_some_and(|f| (a >> 28) == (f.0 >> 28))).collect();
        let gset: std::collections::HashSet<u32> = g.iter().map(|f| f.0).collect();
        let missing: Vec<_> = g.iter().filter(|f| !ours.contains_key(&f.0) && !prog.fns.contains_key(&f.0)).collect();
        let extra: Vec<_> = ours.keys().filter(|a| !gset.contains(a)).collect();
        println!("{file}: Ghidra {} functions, ours {}", g.len(), ours.len());
        println!("  Ghidra's, not an entry of ours: {} ({} of them are instructions we decode inside a function)", missing.len(), missing.iter().filter(|f| decoded.contains(&f.0)).count());
        for f in missing.iter().filter(|f| !decoded.contains(&f.0)).take(40) {
            println!("    {:#x} {} bytes {}", f.0, f.1, f.2);
        }
        // inside a Ghidra function (we split it, or it is a second entry) or outside all
        let gstart: BTreeMap<u32, u32> = g.iter().map(|f| (f.0, f.1)).collect();
        let inside = |a: u32| gstart.range(..a).next_back().is_some_and(|(s, n)| a < s + n);
        let (ins, out): (Vec<u32>, Vec<u32>) = extra.iter().map(|a| **a).partition(|a| inside(*a));
        println!("  ours, not Ghidra's: {} ({} inside a Ghidra function, {} outside all)", extra.len(), ins.len(), out.len());
        if std::env::args().any(|a| a == "-v") {
            for a in ins.iter().take(15) {
                let (s, n) = gstart.range(..*a).next_back().unwrap();
                println!("    {a:#x} inside {s:#x} ({n} bytes)");
            }
            for a in out.iter().take(15) {
                println!("    {a:#x} outside");
            }
        }
        let names: Vec<_> = g.iter().filter(|f| !f.2.starts_with("FUN_") && !f.2.starts_with("thunk_FUN_") && !known.contains_key(&f.0)).collect();
        println!("  names not in symbols.txt: {}", names.len());
        for f in names {
            println!("    {:#x} {}", f.0, f.2);
        }
    }
}
