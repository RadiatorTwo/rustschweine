//! The resolution chosen in the launcher (our list replaces the game's Display Mode list there,
//! `dialog/extend.rs`; the game itself keeps 640x480), saved as REG_DWORD `RenderSize` =
//! width << 16 | height in the game's key; 0 or none means the window's size (fullscreen: the
//! display's). A window gets the chosen size; fullscreen, the 3D is drawn at it and the
//! picture scaled to the display (`draw_size`).
//!
//! Fullscreen (borderless, on the window's display) is REG_DWORD `Fullscreen` in the game's
//! key (default off): our "Vollbild" box in the launcher and Alt+Enter set it; the window
//! starts that way.
//!
//! Widescreen (missions' 3D as wide as the picture's aspect, `wide.rs`) is REG_DWORD
//! `Widescreen` in the game's key (default off): our "Breitbild" box in the launcher;
//! `HOGS_WIDE=0|1` overrides it (scripted tests).

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

/// The size Direct3D's pictures are made for (their 4:3 part is drawn): the window's;
/// fullscreen, the chosen resolution if there is one.
pub fn draw_size() -> (u32, u32) {
    match chosen() {
        Some(size) if window::is_fullscreen() => size,
        _ => window::window_size(),
    }
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

/// The widescreen choice (`HOGS_WIDE` first): 0 off, 1 on, 2 not read yet (asked for every
/// mission picture).
static WIDE: AtomicU32 = AtomicU32::new(2);

pub fn widescreen() -> bool {
    let v = WIDE.load(Ordering::Relaxed);
    if v != 2 {
        return v == 1;
    }
    let on = match std::env::var("HOGS_WIDE").ok().as_deref() {
        Some(v) => v != "0",
        None => widescreen_saved(),
    };
    WIDE.store(on as u32, Ordering::Relaxed);
    on
}

/// The saved widescreen choice (the launcher's box shows it, not `HOGS_WIDE`).
pub fn widescreen_saved() -> bool {
    advapi32::value("", "Widescreen").is_some_and(|(_, d)| d.first().is_some_and(|&b| b != 0))
}

/// Preferences' OK: saved (`HOGS_WIDE` still overrides it).
pub fn set_widescreen(on: bool) {
    advapi32::set_value("", "Widescreen", 4, &(on as u32).to_le_bytes());
    WIDE.store(2, Ordering::Relaxed);
}
