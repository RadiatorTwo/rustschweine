# Plan — what is done, what is open

Updated 2026-10-28. Details on how things work: CLAUDE.md.

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
      then objects, then hogs. Known so far:
      - afSetMap(map): the EXE's map is 64x64 tiles of 0x14 bytes (+0 height word, +0xe
        type in the low 7 bits, +0x12 texture word); the engine's grid is 65x65 corners of
        0x68 bytes at 0x11afaa38 (rows 0x1a68 apart): x and z floats at -8 and -4 (-0x4000 +
        0x200 a tile), height*2 at +0, type at +0x30. Heights change in play
        (afAdjustMapHeights, afAdjustMapTile: craters).
      - Files (OpenHoW describes them): .PMG landscape (16x16 blocks of 368 bytes: 25 corners,
        16 tiles), .POG objects, .PTG textures, .MAD models.
      - 0x10003380 walks the scene by tiles and also draws objects, hogs, text and lines
        (calls afDrawObj, afDrawAnimModel, afDrawText, afDraw3dLine, 0x1000ea50 ...): next
        to read, with how it lights, fogs and textures a tile and whether it relies on the
        order of drawing (z-buffer or not)
- [ ] Interpolate the hogs and other moving objects (still at 25 a second): find their
      places (afSetObjPos / afDrawObj arguments, or the objects behind them)
- [ ] More speed: profile with `HOGS_SAMPLE=1` (DLL 0x10013f20, 0x10008080, 0x100085e0,
      rt_string; ~400 pages saved per extra picture: leave the back buffer and the engine's
      vertex buffers out of the transaction)
- [ ] A setting for the rate (registry, Preferences), as future_rust_cop's `FrameRate`
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
- [ ] Bink videos (`bink.rs`: BinkOpen fails, the game skips them): a Bink 1 decoder
- [ ] High resolution, widescreen, settings
