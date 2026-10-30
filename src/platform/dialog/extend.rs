//! Our own settings inside the game's dialogs:
//! - `adjust` changes the template before the dialog is built: adds our controls (ids from
//!   `FIRST_OURS`);
//! - `init` runs after the game's WM_INITDIALOG: fills our controls, hides game controls (and
//!   tells the game what we chose in its own);
//! - `ok` runs when the dialog's OK (Start) button is pressed: saves our settings.
//!
//! The launcher (LAUNCHERBOX): its Display Mode list (the game's modes, 640x480 to 1024x768)
//! makes way for ours, the display's resolutions (`display.rs`); the game keeps 640x480 (we
//! draw the 3D at the window's size anyway, and its menus are made for 640x480). A
//! "Vollbild" box goes below Detail Level, a "Breitbild" box (missions' 3D as wide as the
//! window, `wide.rs`) beside it, below them an "Antialiasing" list (antialiasing,
//! `display::msaa`) and a "Bildrate" list (pictures per second in missions, `fps.rs`).

use super::template::{Class, Item, Template};
use super::{Dialog, FIRST_OURS, Kind, Notify};
use crate::platform::{display, fps, window};

/// The launcher's template, by name.
pub const LAUNCHER: &str = "LAUNCHERBOX";
/// Its Display Mode list (item data: w << 16 | h) and Detail Level list.
const GAME_MODE: u32 = 1002;
const DETAIL: u32 = 1003;
/// The mode the game is given: what its menus are made for.
const GAME_SIZE: u32 = 640 << 16 | 480;
/// A combo box's choice was made (the game keeps the mode from it, not from the list at Start).
const CBN_SELENDOK: u32 = 9;

const RESOLUTION: u32 = FIRST_OURS;
const FULLSCREEN: u32 = FIRST_OURS + 1;
const WIDESCREEN: u32 = FIRST_OURS + 2;
const MSAA: u32 = FIRST_OURS + 3;
const MSAA_LABEL: u32 = FIRST_OURS + 4;
/// The antialiasing choices: samples per pixel (more than the graphics card has: its most).
const SAMPLES: [(&str, u32); 4] = [("Aus", 0), ("2x", 2), ("4x", 4), ("8x", 8)];
const RATE: u32 = FIRST_OURS + 5;
const RATE_LABEL: u32 = FIRST_OURS + 6;

const VISIBLE: u32 = 0x1000_0000;
const TABSTOP: u32 = 0x0001_0000;

fn launcher(d: &str) -> bool {
    d.eq_ignore_ascii_case(LAUNCHER)
}

pub fn adjust(name: &str, t: &mut Template) {
    if !launcher(name) {
        return;
    }
    let place = |id: u32| t.items.iter().find(|i| i.id == id).map(|i| i.rect);
    let (Some(mode), Some(detail)) = (place(GAME_MODE), place(DETAIL)) else { return };
    // a drop-down list (CBS_DROPDOWNLIST) where the game's is
    t.items.push(Item { class: Class::ComboBox, id: RESOLUTION, text: String::new(), style: VISIBLE | TABSTOP | 3, ex_style: 0, rect: mode });
    // auto check boxes (BS_AUTOCHECKBOX) below the Detail Level group, above Start, side by side
    let (y, half) = (detail[1] + 23, detail[2] / 2);
    t.items.push(Item { class: Class::Button, id: FULLSCREEN, text: "Vollbild".into(), style: VISIBLE | TABSTOP | 3, ex_style: 0, rect: [detail[0], y, half, 10] });
    t.items.push(Item { class: Class::Button, id: WIDESCREEN, text: "Breitbild".into(), style: VISIBLE | TABSTOP | 3, ex_style: 0, rect: [detail[0] + half, y, detail[2] - half, 10] });
    // a label and a drop-down list below them
    let y = y + 13;
    t.items.push(Item { class: Class::Static, id: MSAA_LABEL, text: "Antialiasing".into(), style: VISIBLE, ex_style: 0, rect: [detail[0], y + 2, half, 10] });
    t.items.push(Item { class: Class::ComboBox, id: MSAA, text: String::new(), style: VISIBLE | TABSTOP | 3, ex_style: 0, rect: [detail[0] + half, y, detail[2] - half, detail[3]] });
    let y = y + 14;
    // one more row: the game's buttons below (Start, Ende) and the dialog's bottom move down
    for i in t.items.iter_mut().filter(|i| i.id < FIRST_OURS && i.rect[1] >= y - 14) {
        i.rect[1] += 14;
    }
    t.rect[3] += 14;
    t.items.push(Item { class: Class::Static, id: RATE_LABEL, text: "Bildrate".into(), style: VISIBLE, ex_style: 0, rect: [detail[0], y + 2, half, 10] });
    t.items.push(Item { class: Class::ComboBox, id: RATE, text: String::new(), style: VISIBLE | TABSTOP | 3, ex_style: 0, rect: [detail[0] + half, y, detail[2] - half, detail[3]] });
}

pub(super) fn init(d: &mut Dialog, out: &mut Vec<Notify>) {
    if !launcher(&d.name) {
        return;
    }
    // the game's mode: 640x480 whatever it had saved
    if let Some(k) = d.find(GAME_MODE) {
        let c = &mut d.controls[k];
        c.visible = false;
        if let Kind::Combo { items, sel, .. } = &mut c.kind {
            if let Some(i) = items.iter().position(|i| i.1 == GAME_SIZE) {
                *sel = i as i32;
                out.push(d.command(k, CBN_SELENDOK));
            }
        }
    }
    let (desktop, modes) = window::display_modes();
    let current = display::chosen().map_or(0, |(w, h)| w << 16 | h);
    if let Some(c) = d.control(RESOLUTION) {
        if let Kind::Combo { items, sel, .. } = &mut c.kind {
            items.push((format!("Desktop ({}x{})", desktop.0, desktop.1), 0));
            items.extend(modes.iter().map(|&(w, h)| (format!("{w}x{h}"), w << 16 | h)));
            if current != 0 && !items.iter().any(|i| i.1 == current) {
                items.push((format!("{}x{}", current >> 16, current & 0xffff), current));
            }
            *sel = items.iter().position(|i| i.1 == current).unwrap_or(0) as i32;
        }
    }
    let full = display::fullscreen();
    if let Some(c) = d.control(FULLSCREEN) {
        c.check = full as u32;
    }
    let wide = display::widescreen_saved();
    if let Some(c) = d.control(WIDESCREEN) {
        c.check = wide as u32;
    }
    let samples = display::msaa_saved();
    if let Some(c) = d.control(MSAA) {
        if let Kind::Combo { items, sel, .. } = &mut c.kind {
            items.extend(SAMPLES.iter().map(|&(t, n)| (t.to_string(), n)));
            *sel = SAMPLES.iter().rposition(|&(_, n)| n <= samples).unwrap_or(0) as i32;
        }
    }
    let rate = fps::chosen();
    if let Some(c) = d.control(RATE) {
        if let Kind::Combo { items, sel, .. } = &mut c.kind {
            items.extend(fps::CHOICES.iter().map(|&f| {
                let text = match f {
                    0 => "Original (25)".to_string(),
                    fps::UNLIMITED => "Unbegrenzt".to_string(),
                    f => f.to_string(),
                };
                (text, f)
            }));
            *sel = fps::CHOICES.iter().position(|&f| f == rate).unwrap_or(0) as i32;
        }
    }
}

pub fn ok(d: &mut Dialog) {
    if !launcher(&d.name) {
        return;
    }
    let size = d.control(RESOLUTION).and_then(|c| match &c.kind {
        Kind::Combo { items, sel, .. } => items.get(*sel as usize).map(|i| i.1),
        _ => None,
    });
    if let Some(on) = d.control(FULLSCREEN).map(|c| c.check != 0) {
        display::set_fullscreen(on);
    }
    if let Some(on) = d.control(WIDESCREEN).map(|c| c.check != 0) {
        display::set_widescreen(on);
    }
    let samples = d.control(MSAA).and_then(|c| match &c.kind {
        Kind::Combo { items, sel, .. } => items.get(*sel as usize).map(|i| i.1),
        _ => None,
    });
    if let Some(n) = samples {
        display::set_msaa(n);
    }
    let rate = d.control(RATE).and_then(|c| match &c.kind {
        Kind::Combo { items, sel, .. } => items.get(*sel as usize).map(|i| i.1),
        _ => None,
    });
    if let Some(f) = rate {
        fps::set_chosen(f);
    }
    if let Some(v) = size {
        display::set_chosen((v != 0).then_some((v >> 16, v & 0xffff)));
    }
}
