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
- [ ] Comparing function boundaries (a one-time check of `find_functions`): when a crash or
      `rt_lookup` miss points to a wrongly split or missed function
- [ ] Names of the C runtime (Ghidra's Function ID: memcpy, _ftol, malloc, sprintf ...) for
      `data/symbols.txt`: at the latest when hooking runtime functions (step 3)
- [ ] Decompiler for understanding the game: when working on the renderer in `_d3d.dll`
      (step 4: resolution, widescreen, how it uses Direct3D) or on crashes in game logic

How: `warhogs.exe` and `Data/_d3d.dll` as two programs in one Ghidra project, auto-analysis
with defaults (Function ID on), export each as JSON (addr, size, name; the format of
future_rust_cop's `data/functions.json`) to `data/warhogs_functions.json` and
`data/d3d_functions.json` (boundaries and names only: fine to commit). Then: a comparison
tool, names into `data/symbols.txt`.

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
