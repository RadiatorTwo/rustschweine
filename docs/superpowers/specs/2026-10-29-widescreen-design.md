# Widescreen in missions

## Intent

Hogs of War draws a 4:3 picture (640x480 game coordinates). On today's wide displays the
port shows it with black bars left and right. With widescreen on, a mission's 3D world fills
the whole width: it shows more to the left and right, the same height as now, nothing
stretched. This is a step of PLAN.md's "Widescreen, a frame rate setting".

## Goals

- In missions, the 3D (sky, ground, water, objects, trees, hogs, 2D polys and sprites in the
  world, particles such as snow) covers the full width of the window or display.
- The vertical view stays as it is now; the horizontal view widens with the aspect ratio
  (16:9, 16:10, 21:9, whatever the window/display is).
- Nothing at the sides is missing: what the engine throws away as "off screen" for 4:3 must be
  kept for the wider picture.
- A setting in the launcher: a "Breitbild" checkbox next to "Vollbild", saved in the game's
  registry key like `Fullscreen`. Off: everything as today.

## Non-goals (for now)

- The HUD (compass, timer, map, texts, the weapon menu) stays where it is now: in the central
  4:3 area, at its 640x480 positions. Moving it to the screen edges is a later step.
- Menus and front end (title, team screen, launcher, other 2D screens) stay 4:3 with black bars:
  they are fixed 640x480 pictures.
- Widening the vertical view, or changing the field of view in any other way.

## Anti-goals (a failure even if it "works")

- Anything stretched or squashed (circles not round).
- Holes at the sides: ground, objects or effects that pop in only once they reach the old 4:3
  area, or that are cut off at the old 4:3 edges (scissor/viewport).
- The HUD or menus moved, scaled or cut off.
- With the checkbox off, any difference from today.
- A noticeable loss of speed (missions run at several hundred pictures a second now).

## Constraints

- Only code is shared; nothing derived from the EXE/DLL is committed (see CLAUDE.md).
- The game itself keeps believing in a 640x480 screen; its menus break at other sizes
  (CLAUDE.md, launcher). The widening is done by us, around the game.
- Must keep working with high resolution (`Render::phys`), `HOGS_LOWRES=1`, fullscreen,
  window resizing, and the frame-rate transaction (`fps.rs`: host code writing guest memory
  in an extra picture should write only changed values).
- Clean Rust, matching the surrounding code and its comment style.

## What is known (recon, 2026-10-29)

- `afIfaceInit` (0x1000fca0) gets the screen size and sets the screen centre (integers
  [0x11ab8ca0], [0x11ab8d30] = w/2, h/2; also [0x1002c51c] elsewhere), the focal length
  [0x10131b1c], and two culling factors: [0x1013e6b4] = focal / w, [0x1013e6b8] = focal / h.
  `Begin2D` (0x100065d0) and 0x10002f2f also set or use the focal length (zoom).
- The objects' visibility test 0x10010610 (used by `objects.rs` as `VISIBLE`) and another test
  at 0x1000db7f keep a point when |x * [0x1013e6b4]| < z and |y * [0x1013e6b8]| < z: the
  horizontal factor decides what is "off screen" left and right.
- Projection (PLAN.md): x = cx + X * inv, y = cy - Y * inv. With cx unchanged (320), the
  wider picture is simply game x below 0 and above 640.
- The ground is drawn by us (`ground.rs`) from the whole 64x64 grid, the water whole; the
  engine's ground visibility masks (by yaw) do not limit ours.
- Our Direct3D layer (`d3d.rs`) draws in a framebuffer sized for the 4:3 picture (`phys`),
  maps game coordinates by the target's size, scales viewport and scissor; `show` puts the
  picture in the window with bars.

## How (decided)

- A "wide" picture: game x from -m to 640 + m, m = (480 * aspect - 640) / 2, aspect = the
  window's (fullscreen: the display's or the chosen resolution's) width / height. The
  framebuffer is that wide; every Direct3D draw and our own shaders map game x + m. So the
  3D extends to the sides and everything placed at 0..640 (the HUD) stays in the middle,
  unscaled.
- The engine's horizontal culling factor [0x1013e6b4] becomes focal / (480 * aspect), set
  again whenever the engine writes it (afIfaceInit, zoom), so objects at the sides are kept.
  Any other place that throws away or clips at x 0..640 (sprites, 2D polys in the world,
  text, particles, the background quad in fog colour, the device viewport/scissor of
  0..640) is found and widened the same way.
- Wide only while a mission's 3D is shown; menus and front end stay 4:3 with bars.
- Registry value `Widescreen` (REG_DWORD, default off) in the game's key, checkbox
  "Breitbild" in `dialog/extend.rs` next to "Vollbild"; `display.rs` holds it, like
  `Fullscreen`.
- If the aspect is 4:3 or narrower, widescreen does nothing.

## Calls made by the designer (not discussed with the user)

- Aspect: that of the size the 3D is drawn for (`display::draw_size`): the window's, in
  fullscreen the chosen resolution's if there is one, else the display's. A 4:3 resolution
  chosen on a wide display stays 4:3 with bars.
- 2D things tied to world positions (hog names, health bars, aim markers, damage numbers,
  2D polys and sprites in the world) count as world: they must appear at the sides too. Only
  screen-fixed 2D (compass, timer, map, mission texts, menus) is HUD.
- The mouse keeps mapping to the central 4:3 area (0..640) as now; the sides are not
  pointable. Missions are played by keyboard.
- Widening must change only the picture, not the game: the engine's culling factor and any
  other widened value is set only while the engine draws (and put back afterwards), so game
  logic that might read "on screen" results sees the 4:3 values.

## Left to the builder

- How exactly "a mission's 3D is shown" is detected (Begin3D, the mission step loop in
  `fps.rs`, the render target), and what happens in a mission's 2D screens (pause, results)
  as long as they are not stretched.
- Where m lives and how it reaches the shaders (uniform, `size`).
- How DirectDraw pixels (the `low` 640x480 texture) are placed in the wide framebuffer (in the
  middle).
- Finding the remaining culling/clipping places by testing (screenshots while turning, objects
  at the sides) and the engine's code; documenting them in the module comments and PLAN.md.
- Whether an env variable (e.g. `HOGS_WIDE`) overrides the setting for scripted tests.
