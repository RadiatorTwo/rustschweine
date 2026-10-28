//! The size missions are drawn at with the Glide renderer: the resolution chosen in
//! Preferences (our list replaces the game's Display Mode list there, `dialog/extend.rs`),
//! saved as REG_DWORD `RenderSize` = width << 16 | height in the game's key; 0 or none means
//! the window's size (fullscreen: the display's). A window gets the chosen size; fullscreen,
//! the picture is scaled to the display.
//!
//! Fullscreen (borderless, on the window's display) is REG_DWORD `Fullscreen` in the game's
//! key (default off): our box in Preferences (the game hides its own with Glide) and Alt+Enter
//! set it; the window starts that way.

use super::{advapi32, window};
use std::sync::atomic::{AtomicU32, Ordering};

/// The chosen size as w << 16 | h, 0 for the window's; u32::MAX until read from the registry.
static CHOSEN: AtomicU32 = AtomicU32::new(u32::MAX);

fn packed() -> u32 {
    let v = CHOSEN.load(Ordering::Relaxed);
    if v != u32::MAX {
        return v;
    }
    let v = advapi32::value("", "RenderSize").and_then(|(_, d)| Some(u32::from_le_bytes(d.get(..4)?.try_into().ok()?))).unwrap_or(0);
    CHOSEN.store(v, Ordering::Relaxed);
    v
}

/// The chosen resolution, None for the window's size.
pub fn chosen() -> Option<(u32, u32)> {
    let v = packed();
    let (w, h) = (v >> 16, v & 0xffff);
    (w >= 320 && h >= 200).then_some((w, h))
}

/// Preferences' OK: saved, and the window takes the size.
pub fn set_chosen(size: Option<(u32, u32)>) {
    let v = size.map_or(0, |(w, h)| w << 16 | h);
    CHOSEN.store(v, Ordering::Relaxed);
    advapi32::set_value("", "RenderSize", 4, &v.to_le_bytes());
    if let Some((w, h)) = size {
        window::set_window_size(w, h);
    }
}

/// The size to draw missions at.
pub fn render_size() -> (u32, u32) {
    chosen().unwrap_or_else(window::window_size)
}

/// The saved fullscreen choice.
pub fn fullscreen() -> bool {
    advapi32::value("", "Fullscreen").is_some_and(|(_, d)| d.first().is_some_and(|&b| b != 0))
}

/// Save the fullscreen choice (Alt+Enter: the window already switched).
pub fn remember_fullscreen(on: bool) {
    advapi32::set_value("", "Fullscreen", 4, &(on as u32).to_le_bytes());
}

/// Preferences' OK: saved, and the window switches.
pub fn set_fullscreen(on: bool) {
    remember_fullscreen(on);
    window::set_fullscreen(on);
}
