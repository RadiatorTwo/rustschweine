//! The game's programs: warhogs.exe, its 3D engine, Data/_d3d.dll, which the EXE loads with
//! LoadLibrary and calls through its exports, and binkw32.dll (the videos), which the EXE
//! imports. All are recompiled together.

use crate::discover::Code;
use crate::image::Image;
use std::path::Path;

pub const EXE: &str = "warhogs.exe";
pub const ENGINE: &str = "Data/_d3d.dll";
pub const BINK: &str = "binkw32.dll";
/// binkw32.dll wants 0x10000000 like the engine: moved past the engine (which ends at
/// 0x11ee9000)
pub const BINK_BASE: u32 = 0x1200_0000;

pub struct Game {
    pub exe: Image,
    pub engine: Image,
    pub bink: Image,
    /// all as one image, for finding and compiling the code
    pub all: Image,
}

impl Game {
    /// From the game's folder (the CD's contents or the installed game).
    pub fn load(dir: &Path) -> std::io::Result<Game> {
        let exe = Image::load(&dir.join(EXE))?;
        let engine = Image::load(&dir.join(ENGINE))?;
        let mut bink = Image::load(&dir.join(BINK))?;
        bink.rebase(BINK_BASE);
        let all = Image::combine(&[&exe, &engine, &bink]);
        Ok(Game { exe, engine, bink, all })
    }

    /// Where finding the functions starts: the entry points and the DLLs' exports.
    pub fn seeds(&self) -> Vec<(u32, String)> {
        let mut s = vec![
            (self.exe.entry, "entry".to_string()),
            (self.engine.entry, "engine_DllMain".to_string()),
            (self.bink.entry, "bink_DllMain".to_string()),
        ];
        s.extend(self.engine.exports());
        s.extend(self.bink.exports());
        s
    }

    /// Every function of the programs: sorted (start, end, name).
    pub fn functions(&self, code: &mut Code) -> Vec<(u32, u32, String)> {
        crate::discover::find_functions(code, &self.seeds())
    }
}
