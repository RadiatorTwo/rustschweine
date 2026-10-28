//! The game's two programs: warhogs.exe and its 3D engine, Data/_d3d.dll, which the EXE loads
//! with LoadLibrary and calls through its exports. Both are recompiled together.

use crate::discover::Code;
use crate::image::Image;
use std::path::Path;

pub const EXE: &str = "warhogs.exe";
pub const ENGINE: &str = "Data/_d3d.dll";

pub struct Game {
    pub exe: Image,
    pub engine: Image,
    /// both as one image, for finding and compiling the code
    pub all: Image,
}

impl Game {
    /// From the game's folder (the CD's contents or the installed game).
    pub fn load(dir: &Path) -> std::io::Result<Game> {
        let exe = Image::load(&dir.join(EXE))?;
        let engine = Image::load(&dir.join(ENGINE))?;
        let all = Image::combine(&[&exe, &engine]);
        Ok(Game { exe, engine, all })
    }

    /// Where finding the functions starts: both entry points and the engine's exports.
    pub fn seeds(&self) -> Vec<(u32, String)> {
        let mut s = vec![(self.exe.entry, "entry".to_string()), (self.engine.entry, "engine_DllMain".to_string())];
        s.extend(self.engine.exports());
        s
    }

    /// Every function of both programs: sorted (start, end, name).
    pub fn functions(&self, code: &mut Code) -> Vec<(u32, u32, String)> {
        crate::discover::find_functions(code, &self.seeds())
    }
}
