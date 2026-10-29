//! Widescreen in missions (`display::widescreen`, the launcher's "Breitbild"): the 3D shows
//! more to the left and right, as wide as the picture's aspect, the same height; the HUD stays
//! where it is, in the middle 4:3 part. The game keeps its 640x480 screen: the picture just
//! reaches past it, game x from -m to 640 + m (`d3d.rs` draws it so).
//!
//! What the engine leaves out as off the screen is widened while a mission picture is drawn
//! (`around`, from `fps::render`) and put back after, so the game's logic sees only its own
//! values:
//! - [CULL_X] (afIfaceInit 0x1000fca0: focal length / screen width): the objects' visibility
//!   test 0x10010610 keeps a point when |x * [CULL_X]| < z.

use super::{display, mem};
use std::cell::Cell;
use std::time::{Duration, Instant};

/// The engine's focal length (float) and horizontal culling factor (float).
const FOCAL: u32 = 0x1013_1b1c;
const CULL_X: u32 = 0x1013_e6b4;

thread_local! {
    /// The last mission picture's margin, and when it was drawn.
    static MARGIN: Cell<(f32, Option<Instant>)> = const { Cell::new((0.0, None)) };
}

/// The game pixels the picture reaches past the 640x480 screen on each side for a picture of
/// `w` x `h` (0 for 4:3 and narrower).
pub fn margin_for(w: u32, h: u32) -> f32 {
    if h == 0 {
        return 0.0;
    }
    (480.0 * w as f32 / h as f32 - 640.0).max(0.0) / 2.0
}

/// The margin of mission pictures (0 with widescreen off, and when no mission picture was drawn
/// for half a second: a menu, the results). It stays between them, so what is drawn before or
/// after the engine's drawing (the same picture) is drawn in the same wide framebuffer.
pub fn margin() -> f32 {
    match MARGIN.get() {
        (m, Some(t)) if t.elapsed() < Duration::from_millis(500) => m,
        _ => 0.0,
    }
}

/// Draw a mission picture (`f`) widened by the margin, the engine's values put back after.
pub fn around(f: impl FnOnce()) {
    let m = if display::widescreen() {
        let (w, h) = display::draw_size();
        margin_for(w, h)
    } else {
        0.0
    };
    MARGIN.set((m, Some(Instant::now())));
    if m == 0.0 {
        return f();
    }
    let mm = mem();
    let old = mm.rd32(CULL_X);
    let cull = f32::from_bits(mm.rd32(FOCAL)) / (640.0 + 2.0 * m);
    if cull.to_bits() != old {
        mm.wr32(CULL_X, cull.to_bits());
    }
    f();
    if mm.rd32(CULL_X) != old {
        mm.wr32(CULL_X, old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margins() {
        assert_eq!(margin_for(1024, 768), 0.0);
        assert_eq!(margin_for(1280, 1024), 0.0);
        assert!((margin_for(1920, 1080) - 106.666_67).abs() < 1e-3);
        assert!((margin_for(2560, 1080) - 248.888_9).abs() < 1e-3);
        assert_eq!(margin_for(0, 0), 0.0);
    }
}
