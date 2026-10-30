# Plan — what is done, what is open

Updated 2026-10-30. Details on how things work: CLAUDE.md.

## Done

- [x] CD image extracted to `game_files/cd` (PC version, track 1 is ISO 9660)
- [x] Step 1: project set up, core taken from future_rust_cop: image (EXE + DLL as one,
      exports), finding functions without Ghidra, x86 -> Cranelift, x87, SMC fields, runtime,
      JIT; tests without game files (discovery, misc, SMC, x87)
- [x] Both programs found and compiled: 2874 entries, all compile; missing only
      `fnsave`/`frstor` in one C runtime function of the DLL (0x10014372)
- [x] Step 3 (2026-10-28): platform layer from future_rust_cop; the game starts, passes the
      CD check, shows its launcher (LAUNCHERBOX, our dialog overlay), loads `_d3d.dll`
      (DllMain, GetProcAddress of its exports), the title screen and the front end (main
      menu, keyboard through DirectInput 7 works). See CLAUDE.md for what each part does.
- [x] Step 4, first stage (2026-10-28): Direct3D 7 on OpenGL (`d3d.rs`): pre-transformed
      vertices (FVF 0x1c4), texture stage 0, blending, alpha test, z-buffer, culling, flat
      shading, vertex and table fog; the back buffer is kept in step with DirectDraw (load
      before drawing, write back before a blit or flip). The team screen's 3D pig and the
      first mission (Trainingslager: landscape, sky, snow, HUD) are drawn.

## Open

### Ghidra export (when needed, by the user)

Not needed for compiling: our own discovery finds the functions. Ask the user for it when
one of these comes up:
- [x] Comparing function boundaries (2026-10-28, `examples/ghidra.rs`): Ghidra 12.2 finds
      1588 + 393 functions; ours miss only 20 C runtime functions nothing calls, and find ~720
      Ghidra misses (callbacks and vtable entries, reached only through their addresses)
- [x] Names of the C runtime from Function ID, in `data/symbols.txt` (few: Function ID knows
      only ~40 of the game's runtime functions; memcpy, malloc, sprintf are not among them)
- [ ] Decompiler for understanding the game: when working on the renderer in `_d3d.dll`
      (step 4: resolution, widescreen, how it uses Direct3D) or on crashes in game logic

How (Ghidra 11, needs a JDK 21; details in plain steps: ask Claude):
1. File > New Project (Non-Shared), then File > Import File: `game_files/cd/warhogs.exe`
   and `game_files/cd/Data/_d3d.dll` (format "Portable Executable (PE)", the default).
2. Open each (double-click), answer "Analyze now?" with Yes, keep the default analyzers
   (Function ID is on), Analyze; wait until the progress bar at the bottom right is gone.
3. Window > Script Manager > "Manage Script Directories" (the list icon): add
   `tools/ghidra` of this repo; run `ExportFunctions` (category Export) and save as
   `data/warhogs_functions.json` / `data/d3d_functions.json` (boundaries and names only:
   fine to commit). Then: a comparison tool, names into `data/symbols.txt`.

### 2. Code
- [x] `plausible` in `src/discover.rs` no longer counts `Wait` (fwait) as an instruction
      compiled code never has: MSVC code does contain a lone `fwait` (found in rust_metal,
      2026-10-29: 3 of 21 functions missed there came back). Here no change: 2714 functions,
      2874 entries before and after (2026-10-29)
- [ ] `fnsave`/`frstor` in the lifter (DLL's C runtime, error path of a math function)
- [ ] Differential test against Unicorn (future_rust_cop's `src/bin/difftest.rs`)
- [ ] Unreached code that turns out to be needed at run time (rt_lookup reports it)

### 3. Platform, what is left
- [ ] Setup: finding the game in `game_files` (CD image or folder), checking the SHA-256s,
      a release build without console (future_rust_cop's `setup.rs`, `splash.rs`)
- [ ] The launcher's picture (static control with bitmap 109: drawn as "#109")
- [ ] DirectInput buffered data (GetDeviceData) if the game wants it; mouse in menus
- [ ] DirectPlay with Hogs of War (taken over from Future Cop, untried; lobby is a stub)
- [ ] Settings dialogs the game may open (PIGINFO, WEATHERBOX look like debug dialogs)

### 4. 3D
- [ ] Play the first mission through and compare with the original (pictures of the PC
      version): colours, fog, transparency, the light rays
- [ ] Not used so far, not done: colour-key textures, texture stage 1, palette textures,
      untransformed vertices (SetTransform), mipmaps, vertex buffers (warnings say so)
- [ ] Speed: the 640x480 back buffer is read back each frame (fine now; with high
      resolution the 2D blits should be drawn in GL instead, see step 5)

### Speed
- [x] Front end too fast (2026-10-28): it has no clock and relied on Flip waiting for the
      monitor; Flip now waits for a 60 Hz refresh (`ddraw::vertical_blank`)
- [x] Unlimited frame rate in missions, first stage (2026-10-28, `fps.rs`): the logic keeps
      its 40 ms steps; while the loop waits, the drawing (0x44e290) runs again as a
      transaction (game memory write-protected, pages saved at their first write, put back
      after) with the camera between the last two steps; pictures show the game one step
      late. 70-110 pictures a second in the first mission. Made fast for it: Direct3D draws
      batched by state, flips show the GL picture without reading it back, 24-bit x87
      arithmetic inline (it went to `rt_fpu_round`: 41 million calls a second), `__ftol` by
      the host (`crt.rs`)
- [x] Trees made missions slow (2026-10-28): the HUD and names are BltFast'ed glyph by
      glyph onto the Direct3D picture, which was read back from the GPU (waiting for it) and
      loaded again each frame; such blits are drawn by GL now (`d3d::blit`). `rep movs/stos`
      in one go (`rt_string`). With trees in view ~90 pictures a second (59 at worst before)
- [ ] Our own renderer for missions instead of the engine's CPU drawing (the goal: ~10x).
      Replacing single engine functions does not pay: 0x10013f20 (project a point, 1.1
      million calls a second) in Rust was only ~2x faster (the switch from recompiled code
      to the host costs as much as the maths) and saved 0.2 ms a picture. Measured with
      trees in view (~11 ms a picture): the scene walk 0x10003380 (from sub_10001540, from
      DisplayCurrentHole) takes 8.6 ms, of it afDrawObj (objects such as trees, ~135 a
      picture) 4.7 ms; afDrawAnimModel (hogs) 0.5 ms. Not the GPU (drawing nothing: ~10%
      faster), not the transaction's page faults (none: no change).
      Plan: the landscape first (map and heights on the GPU once, the camera per picture),
      then objects, then hogs.
      Done (2026-10-29): the landscape by the GPU (`ground.rs`: the corners in a vertex
      buffer, the engine's projection in a shader, drawn at 0x1000c040; the engine's ground
      off by its visibility masks; `HOGS_GROUND=engine` for comparing). The bigger win was
      elsewhere: the main thread waited ~40% of the time for the GL driver's thread, which
      was busy with tens of thousands of calls a picture (GL state reset after every Direct3D
      call, all uniforms and states set for every batch). GL state is now kept and set only
      when it changes (`d3d.rs` `applied`, `gl::TOUCHED`). Blt copies onto the Direct3D picture
      go to GL as BltFast's do. Mission now 120-200 pictures a second (was 85-120), main
      thread ~90% busy; `HOGS_SAMPLE` names the module and our caller for host time.
      Done (2026-10-29, second part): objects, trees, sky (way 2) and the hogs (bones) by
      the GPU (`objects.rs`: afDrawObj replaced; models in a vertex buffer, transform, light,
      projection in a shader; `HOGS_OBJECTS=check` compares every vertex with the engine's:
      within 0.006 pixels, colours and uv exact), the water too (`ground.rs`: 1024
      DrawPrimitive calls a picture before). The transaction by write watch instead of write
      protection and page faults (`fps.rs` `Shadow`). First mission: 155 -> ~570 pictures a
      second; the main thread now waits ~30% in Flip (the GPU, the swap).
      Next: what is left on the CPU: the visibility test 0x10010610 transforms all of a
      model's vertices when its quick test fails (0x10011810, 0x10010910 ~4%); 2D polys and
      sprites in the world (afDraw2dPolyIn3dWorld, sort types 3/14); the scene walk itself.
      Hogs' moving parts still at 25 a second (see below). Known so far:
      - afSetMap(map): the EXE's map is 64x64 tiles of 0x14 bytes (+0 height word, +0xe
        type in the low 7 bits, +0x12 texture word); the engine's grid is 65x65 corners of
        0x68 bytes at 0x11afaa38 (rows 0x1a68 apart): x and z floats at -8 and -4 (-0x4000 +
        0x200 a tile), height*2 at +0, type at +0x30. Heights change in play
        (afAdjustMapHeights, afAdjustMapTile: craters).
      - Files (OpenHoW describes them): .PMG landscape (16x16 blocks of 368 bytes: 25 corners,
        16 tiles), .POG objects, .PTG textures, .MAD models.
      - 0x10003380(first bucket 0x270f, last 0) draws the whole scene: background quad
        (0x1000b6e0, fog colour), sky objects (sort bucket 0x270f), water mesh (0x1000b440,
        alpha 0x80, scrolls), then the ground, then 0x1000c040/0x1000c310, then the sort
        list far to near (12-byte nodes {data, type, next}, bucket = depth >> 3; type 10
        afDrawObj, 11 afDrawAnimModel, 13 text, 14 2D poly, 15 line, 3 sprites).
        Objects are not tied to tiles; z-buffer on (LESSEQUAL, z written).
      - Ground: blocks of 4x4 tiles in a 33x33 window around the camera, skipped by masks
        (`language	ims
view00N.bmp`, by yaw); per tile its 4 corners projected
        (0x10013f20) and 2 triangles (D,C,A)(A,B,D) appended to one of 32 page buffers
        (0x1013ed08 + p*0xc7380, counts 0x1013e440); flushed at 0x10003b83 by page:
        SetTexture(page record 0x1004cf90 + p*0xd4, +0) + DrawPrimitive. Then 0x1000c040
        is called: the place for our own ground (before every object).
      - Corner (0x68 bytes, 0x11afaa30 + (row*65 + col)*0x68): x +0, z +4, height +8,
        diffuse +0xc (lit once by SetGroundLightDirection), screen x/y +0x10/+0x14, stamp
        +0x20, depth d +0x28, rhw +0x2c, z +0x30, u[4] +0x48, v[4] +0x58 (a tile's uv
        from its corner A, rotation/flip baked by afSetMap), page +0x40, sub-tile +0x42.
        Page flag (record +0x10 + sub*4) 2: the tile is not drawn (water shows).
      - Projection (checked): d = third camera row . (p - cam); inv = 1/d if d > 937, else
        0.0021345 - d*1.139e-6 (no near clipping); x = cx + X*inv, y = cy - Y*inv;
        rhw = 8*inv; z = max(d*2e-6, 1.527e-5). Fog: table, linear by w.
      - Not known: SetGroundLightDirection's formula (diffuse changes at run time?),
        0x1000c040/0x1000c310/0x10009810, sort-list types 0 and 4
- [x] Hogs move smoothly (2026-10-29, `objects.rs` `smooth`): their 15 bone matrices (world
      space, worked out by the engine at each drawing: walking, turning and the animation in
      one) are kept from the last two steps' pictures and blended in the pictures between,
      element by element with each row's length kept (as quaternions they changed: the
      matrices are not pure turns). A step's picture shows the step before, as the camera.
- [x] Ground behind the camera drawn over everything while walking (2026-10-29, ours: the
      engine's masks left those blocks out): cut off at the near plane (`ground.rs` shader;
      the water too, which the engine draws whole)
- [x] Dark stripes sweeping over the picture when walking along the green path (2026-10-29,
      second try): the 2D polys in the world (afDraw2dPolyIn3dWorld 0x1000ea50, sort node
      type drawn at 0x100042d6 as a fan of 4: the green path's patches, alpha 0x80) projected
      by the engine with corners behind the camera: negative rhw, x and y far off. Our Direct3D
      shader took w = 1 for rhw <= 0 and drew them at those places; now w = 1/rhw also when
      negative (x, y were projected by the same depth), GL cuts them at w = 0 (`d3d.rs` VS).
      Found by every picture (HOGS_SHOT_MS=0, a few seconds), triangles wider than 250 pixels
      and the return address of DrawPrimitive. The first try (objects' corners behind the
      camera kept there, `objects.rs`) was not it but stays (right for trees and the sky's dome)
- [x] Still at 25 a second: the hogs' names, shadows and other objects that move (shells,
      crates falling). Done 2026-10-30, checked by eye 2026-10-31 (nothing stutters): the game's objects
      (list at [0x537fbc], next +4, body [obj+0x18] with the position at +4, as the camera's)
      blended between the last two steps like the camera (`fps.rs` `bodies_step`); the hogs'
      bones follow that position (anim record +0x70), so `objects.rs` `smooth` now blends only
      the pose, relative to it
- [ ] More speed: profile with `HOGS_SAMPLE=1` (now Flip ~30%: the swap; see "Next" above)
- [x] Busy CPU (2026-10-29): with all 16 cores loaded a mission fell from ~550 to ~90 pictures
      a second: PeekMessage gave the time slice away when it found nothing (`yield_now`). Not
      in missions now: ~270 under full load. What is left under load is the GL driver
      (nvoglv64, its own thread)
- [x] GL calls (2026-10-29): `apply` sends only the states that change (`d3d.rs` `Cache`, per
      program the fragment uniforms), own programs' uniforms only when changed
      (`d3d::Uniform`): ~550 -> ~610. Measured: all objects' GL calls together cost ~6%
      (without them ~670), so drawing objects together (instancing) would bring little and
      change the drawing order; not done
- [x] A setting for the rate (2026-10-30): the launcher's "Bildrate" list (Original (25), 60 ...
      240, Unbegrenzt), REG_DWORD `FrameRate` as future_rust_cop's; `HOGS_FPS` overrides it.
      The launcher's Start and Ende moved down a row: Start is now at click@424,350.
      Pacing (`fps.rs` `shown`): the next picture is due 1/cap s after the last flip, started
      the drawing's time before; the step too waits for its slot (returning what it returned
      last), so its picture does not come right after an extra one (the logic catches up by
      its own clock, [0x520884]). Before, 240 gave ~175 a second and frame times 1.4-7 ms;
      now 240 a second, 99% of frame times 3.5-5.1 ms (`HOGS_FPS_LOG` logs them)
- [ ] Linux: the transaction needs mprotect + SIGSEGV (`fps.rs` is Windows only)
- [ ] Front end: 60 Hz is still a little fast; find its intended rate (30? 25?)

### 5. Rest
- [ ] DirectSound works for the front end's sounds (from future_rust_cop); check in game
- [x] CD audio tracks (music, 2026-10-28): MCI in `cdaudio.rs`, read from the `.cue`/`.bin`
      image next to the game folder, mixed by `dsound.rs`; the menu plays track 2
- [x] Music checked by ear in the menu and in a mission (user, 2026-10-28)
- [x] Music volume (2026-10-28): a mixer with one line, CD audio, and its volume control
      (`cdaudio.rs`); the game's master slider and fades set it, mixed by `dsound.rs`
- [ ] Setup must keep the CD image's `.cue` next to the game
- [x] Bink videos (2026-10-30): binkw32.dll (from the CD) recompiled with the game, moved to
      0x12000000 (`Image::rebase`), the EXE's imports bound to its exports, its DllMain run
      before the EXE's entry. The game skipped the videos because of Footprint 2 (read only in
      0x43ee10: 2 = no videos); now 1 (from InstallSource). Intro logos seen in shots
- [x] Videos checked by the user (2026-10-30): sound plays, no flicker (the shots catch both
      pages), the intro's videos play one after another. MMX in binkw32.dll (Paddw, Pmulhw,
      ... in ~9 functions) is not in the lifter: not reached so far
- [x] High resolution (2026-10-29, `d3d.rs` `Render::phys`): Direct3D draws into a framebuffer
      of the size the window shows the 4:3 picture in (a new size after a flip); the game's
      coordinates stay 640x480 (vertices to clip space by the target's size, viewport and
      scissor scaled). DirectDraw's pixels go through a 640x480 texture (`low`): stretched in,
      shrunk out (glBlitFramebuffer). Same speed (~750 a second). `HOGS_LOWRES=1`: as before.
      The 2D (HUD, text) is still 640x480 pixels, stretched
- [x] Resolution and fullscreen in the launcher (2026-10-29, `dialog/extend.rs`, `display.rs`):
      our list of the display's modes (and "Desktop") replaces the game's Display Mode list,
      a "Vollbild" box below Detail Level (borderless; Alt+Enter still switches). The game is
      always given 640x480 (with 1024x768 its own menus came out enlarged and cut off, the HUD
      smaller): its hidden list is set to it and CBN_SELENDOK sent, which is where the game
      takes the mode from (0x481910), not from the list at Start
- [x] Widescreen (2026-10-29, `wide.rs`, spec and plan in `docs/superpowers/`): the launcher's
      "Breitbild" box (`Widescreen` in the game's key, `HOGS_WIDE=0|1`); in missions the 3D
      as wide as the window's aspect, the same height, the HUD in the middle 4:3 part (moving
      it to the edges: later), menus 4:3. `d3d.rs` draws into a framebuffer reaching `margin`
      game pixels past the 640x480 screen on each side (`origin` uniform; viewport and clears
      over the whole width widened). While a mission picture is drawn the engine's horizontal
      culling factor [0x1013e6b4] is widened (objects at the sides kept) and the weather's
      flakes (screen pixels, wrapped at the screen's width) wrap at the wide width and move
      left by the margin. Not checked yet: explosions and other effects at the sides
- [x] A frame rate setting (see Speed)
