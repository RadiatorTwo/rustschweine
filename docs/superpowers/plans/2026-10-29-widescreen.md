# Widescreen Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In missions the 3D fills the full width of a wide window/display (more to the left and right, same height), the HUD stays in the central 4:3 area, switched by a "Breitbild" box in the launcher.

**Architecture:** While a mission picture is drawn (`fps::render`, hook on 0x44e290) a new module `wide.rs` widens the engine's horizontal culling factor and tells `d3d.rs` the margin m (game pixels each side). `d3d.rs` then draws into a framebuffer that covers game x from -m to 640 + m; every shader maps game coordinates by an `origin` uniform, so everything at 0..640 (the HUD) stays in the middle, unscaled. Values written into guest memory are put back after the drawing.

**Tech Stack:** Rust, glow (OpenGL), the recompiled game (guest memory via `mem()`), hooks (`platform::HOOKS`).

**Spec:** `docs/superpowers/specs/2026-10-29-widescreen-design.md`

## Global Constraints

- Nothing derived from the EXE/DLL is committed; `game_files/` stays out of git.
- The game keeps a 640x480 screen; only our layer widens.
- Setting off (or aspect <= 4:3): pictures identical to today (`examples/pngdiff.rs` shows no difference).
- Registry value `Widescreen`, REG_DWORD, default 0, in the game's key; checkbox text "Breitbild".
- m = max(0, (480 * aspect - 640) / 2), aspect = width / height of `display::draw_size()`.
- Culling factor [0x1013e6b4] = focal / (640 + 2m) while drawing, focal = f32 at [0x10131b1c]; old value put back after.
- Only the picture changes: no widened value stays in guest memory outside the drawing.
- No visible loss of speed (`HOGS_FPS_LOG=1` in the first mission stays within ~5% of before).
- Code and comments in the style of the surrounding modules (plain English, why not what).

## Review Focus

- Window resized or Alt+Enter during a mission: the next picture uses the new margin, no stretched or stale picture (one picture of old size is fine).
- Leaving a mission (results, menus): back to 4:3 with bars, the wide framebuffer not shown.
- `HOGS_LOWRES=1` with widescreen: framebuffer (640 + 2m) x 480, still correct.
- Extra pictures between steps (`fps.rs` transaction) and the step's own picture both widen; after each the factor is the engine's again.
- Front end's Direct3D (team screen's pig) not widened: drawn outside 0x44e290.

---

### Task 1: The setting

**Files:**
- Modify: `src/platform/display.rs` (after `set_fullscreen`)
- Modify: `src/platform/dialog/extend.rs:11,40-100` (template, `init`, `ok`)

**Interfaces:**
- Produces: `pub fn display::widescreen() -> bool` (registry `Widescreen` != 0; env `HOGS_WIDE=0|1` overrides, for scripted tests), `pub fn display::set_widescreen(on: bool)` (saves).

- [ ] **Step 1:** Add `widescreen`/`set_widescreen` in `display.rs` like `fullscreen`/`remember_fullscreen`; module comment gets a line on `Widescreen`.
- [ ] **Step 2:** In `extend.rs` add an auto check box id `WIDESCREEN`, text "Breitbild", one row (12 dialog units) below "Vollbild" (move Start down if it overlaps); `init` checks it from `display::widescreen()`, `ok` saves it. Module comment: the box is mentioned.
- [ ] **Step 3: Verify.** `cargo build --release`; run with `HOGS_SHOT=<scratch>/l{t}.png HOGS_INPUT="1500:click@<box>"` (box position from a first shot), Start; `game_files/cd/registry.txt` then holds `Widescreen = dword:00000001`; the shot shows both boxes, not overlapping.
- [ ] **Step 4: Commit** `display.rs`, `extend.rs`: "Breitbild box in the launcher".

### Task 2: `wide.rs`: margin and the drawing's widening

**Files:**
- Create: `src/platform/wide.rs` (module comment: what is widened and why only while drawing)
- Modify: `src/platform/mod.rs` (`mod wide;`), `src/platform/fps.rs:122-150` (`render`: every `call_original(c, RENDER)` inside `wide::around`)

**Interfaces:**
- Consumes: `display::widescreen()`, `display::draw_size()`.
- Produces:
  - `pub fn margin_for(w: u32, h: u32) -> f32` (pure; game pixels each side)
  - `pub fn margin() -> f32` (0.0 unless a mission picture is being drawn with widescreen on)
  - `pub fn around(f: impl FnOnce())` (sets the margin and the culling factor, runs `f`, puts both back)

- [ ] **Step 1: Write the failing test** in `wide.rs`:

```rust
#[test]
fn margins() {
    assert_eq!(margin_for(1024, 768), 0.0);
    assert_eq!(margin_for(1280, 1024), 0.0);
    assert!((margin_for(1920, 1080) - 106.666_67).abs() < 1e-3);
    assert!((margin_for(2560, 1080) - 248.888_9).abs() < 1e-3);
    assert_eq!(margin_for(0, 0), 0.0);
}
```

- [ ] **Step 2:** `cargo test --release margins` fails (no `margin_for`).
- [ ] **Step 3:** Implement `margin_for`, `margin` (thread-local `Cell<f32>`), `around` (m from `draw_size()` when `widescreen()`; if m > 0 write [0x1013e6b4] = focal / (640 + 2m) only when it differs, restore the read value after `f`). Wrap the calls in `fps::render`.
- [ ] **Step 4:** `cargo test --release` passes.
- [ ] **Step 5: Verify** objects are kept at the sides: with `HOGS_WIDE=1` and `RenderSize = dword:050002d0` (1280x720) in `registry.txt` (back it up, restore after), `HOGS_OBJECTS=check` in the first mission logs no differences. (The picture itself is still 4:3 until Task 3.)
- [ ] **Step 6: Commit** `wide.rs`, `mod.rs`, `fps.rs`: "Widescreen: engine's culling widened while a mission picture is drawn".

### Task 3: The wide framebuffer

**Files:**
- Modify: `src/platform/d3d.rs` (`Render`: `phys`, `bind`, `copy`, `scissor`, viewport/`Clear`, `show`, `apply`'s uniforms, VS, FS)
- Modify: `src/platform/ground.rs:150-175`, `src/platform/objects.rs:455-485` (VS: `origin`)

**Interfaces:**
- Consumes: `wide::margin()`.
- Produces: uniform `vec2 origin` (game coordinates of the framebuffer's top left: (-m, 0)) and `size` = (640 + 2m, 480) in every program (`Uniforms`), set by `apply`; `fn widen(r: [u32; 4], size: (u32, u32), m: f32) -> [f32; 4]` in `d3d.rs` (pure).

- [ ] **Step 1: Write the failing test** in `d3d.rs`:

```rust
#[test]
fn widened() {
    // a rectangle over the whole width reaches the wide picture's edges, others stay
    assert_eq!(widen([0, 0, 640, 480], (640, 480), 100.0), [-100.0, 0.0, 840.0, 480.0]);
    assert_eq!(widen([0, 60, 640, 300], (640, 480), 100.0), [-100.0, 60.0, 840.0, 300.0]);
    assert_eq!(widen([10, 0, 100, 50], (640, 480), 100.0), [10.0, 0.0, 100.0, 50.0]);
    assert_eq!(widen([0, 0, 640, 480], (640, 480), 0.0), [0.0, 0.0, 640.0, 480.0]);
}
```

- [ ] **Step 2:** `cargo test --release widened` fails.
- [ ] **Step 3:** Implement. `Render` keeps `margin: f32` (taken in `bind`; a change is a resize, between pictures only, as `phys` now); `phys` from the wide size ((640 + 2m) x 480 scaled by `window::fit`); VS `p = (pos.xy - origin + 0.5 - 1/64) / size * 2 - 1` (all three shaders), FS `g` adds `origin`; `scissor` and `Clear` take `widen(...)`'d rectangles (x scaled from -m); `copy` puts DirectDraw's 640x480 `low` pixels at x m..m+640 (and takes them back from there); `show` passes the wide size so the window fits the wide aspect; the mouse keeps mapping the middle 4:3 part to 0..640 (`window.rs`, where it maps window pixels to the picture).
- [ ] **Step 4:** `cargo test --release` passes.
- [ ] **Step 5: Verify** (registry as in Task 2): first mission with `HOGS_WIDE=1`: shot fills 1280x720, compass, timer, map at the same place relative to the middle 4:3 part as with `HOGS_WIDE=0`, circles round; with `HOGS_WIDE=0` `examples/pngdiff.rs` against a shot from before this task shows no difference; the team screen with `HOGS_WIDE=1` still 4:3 with bars; `HOGS_LOWRES=1 HOGS_WIDE=1` correct; `HOGS_FPS_LOG=1` within ~5%.
- [ ] **Step 6: Commit** `d3d.rs`, `ground.rs`, `objects.rs`: "Widescreen: missions drawn into a wide framebuffer".

### Task 4: The other places that cut off at 0..640

**Files:**
- Modify: `src/platform/wide.rs` (each place widened in `around`, documented in its module comment), others as found
- Modify: `PLAN.md` (step 4: widescreen done, what was found), `CLAUDE.md` (`HOGS_WIDE`, `wide.rs` in the platform list)

**Interfaces:**
- Consumes: `wide::around`, `wide::margin()`.

- [ ] **Step 1: Find.** Scripted first mission with `HOGS_WIDE=1`, shots every 250 ms while the camera turns (`HOGS_INPUT` with arrow keys held), plus `HOGS_D3D_LOG`. List what appears late or is cut at the old 4:3 edges: the background quad in fog colour (0x1000b6e0), 2D polys in the world (0x1000ea50), sprites (sort type 3), hog names/text (type 13), particles (snow), the second visibility test at 0x1000db7f, the device viewport.
- [ ] **Step 2: Fix each** in `around` (guest values set while drawing, put back after) or in `d3d.rs` (our side), one at a time, each checked by the same shots.
- [ ] **Step 3:** `cargo test --release` passes; final shots: nothing missing at the sides while turning, the HUD unchanged, `HOGS_WIDE=0` identical to before.
- [ ] **Step 4: Commit** with spec and plan (`docs/superpowers/...`): "Widescreen: everything at the sides drawn".
