//! binkw32.dll, the videos (Language\Streams\*.bik): not yet. BinkOpen fails, so the game
//! skips them.

use super::{Import, log, new_cstr};
use crate::rt::Cpu;
use std::sync::OnceLock;

fn bink_open(c: &mut Cpu) -> u32 {
    if super::trace_api() {
        log!("BinkOpen {}: videos are not played yet", super::cstr(super::arg(c, 0)));
    }
    0
}

fn bink_get_error(_: &mut Cpu) -> u32 {
    static E: OnceLock<u32> = OnceLock::new();
    *E.get_or_init(|| new_cstr("Bink videos are not supported yet"))
}

fn ret0(_: &mut Cpu) -> u32 {
    0
}
fn ret1(_: &mut Cpu) -> u32 {
    1
}

pub const IMPORTS: &[Import] = &[
    ("_BinkOpen@8", 2, bink_open),
    ("_BinkGetError@0", 0, bink_get_error),
    ("_BinkSetSoundSystem@8", 2, ret1),
    ("_BinkOpenDirectSound@4", 1, ret0),
    ("_BinkDDSurfaceType@4", 1, ret0),
    ("_BinkDoFrame@4", 1, ret0),
    ("_BinkNextFrame@4", 1, ret0),
    ("_BinkWait@4", 1, ret0),
    ("_BinkCopyToBuffer@28", 7, ret0),
    ("_BinkClose@4", 1, ret0),
];
