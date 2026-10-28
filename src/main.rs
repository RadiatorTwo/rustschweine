//! Hogs of War recompiled: compiles the player's warhogs.exe and Data/_d3d.dll and runs them.
//!
//!     rustschweine [game folder]
//!
//! Without a path: `game_files/cd` (the CD's contents) in the current folder.

use rustschweine::discover::{Code, Program};
use rustschweine::game::Game;
use rustschweine::jit::compile_cached;
use rustschweine::smc::Smc;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    // guest calls nest on the host stack like the original's: run on a thread with a large one
    let code = std::thread::Builder::new().stack_size(256 << 20).spawn(run).unwrap().join().unwrap();
    std::process::exit(code as i32);
}

fn run() -> u32 {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "game_files/cd".into()));
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let game = Game::load(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let t = Instant::now();
    let mut code = Code::new(&game.all);
    let ranges = game.functions(&mut code);
    let prog = Program::build(&mut code, &ranges);
    let smc = Smc::find(&prog, &mut code);
    let found = t.elapsed().as_millis();
    // the machine code is kept in the game folder, for this program and these files
    let cache = dir.join("compiled.bin");
    let key = cache_key(&game);
    let compiled = compile_cached(&prog, &smc, &mut code, rustschweine::platform::HOOKS, key.map(|k| (cache.as_path(), k)));
    let st = &compiled.stats;
    if st.cached {
        eprintln!("compiled code of {} functions loaded in {} ms", st.functions, t.elapsed().as_millis());
    } else {
        eprintln!(
            "compiled {} functions in {} ms (finding them {found}, lifting {}, compiling {})",
            st.functions,
            t.elapsed().as_millis(),
            st.lift_ms,
            st.compile_ms
        );
    }
    let code = rustschweine::platform::run_game(&game, dir);
    drop(compiled);
    code
}

/// What the compiled code depends on: this program (any change to the recompiler) and the
/// game's two files. None when our own file cannot be read.
fn cache_key(game: &Game) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let own = std::fs::read(std::env::current_exe().ok()?).ok()?;
    let mut h = Sha256::new();
    h.update(&own);
    h.update(&game.all.data);
    Some(h.finalize().into())
}
