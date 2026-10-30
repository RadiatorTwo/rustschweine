# CLAUDE.md — Hogs of War (Frontschweine) in Rust

Hand-over for future sessions. What is done and what is open: **PLAN.md** (keep it current).

## Goal (the user's)

A native port of Hogs of War (German: Frontschweine, Infogrames 2000, PC version) in Rust,
built like the user's earlier port of Future Cop (`C:\!Code2\future_rust_cop`, also by
the user): the game's own x86 code is recompiled with Cranelift when the game starts, and the
Windows/DirectX functions it calls are implemented in Rust (OpenGL for the 3D renderer).
The game is hard to run on modern systems; it should run natively, later with high
resolution and widescreen.

Only code is ever shared; players supply their own game files. **Never commit anything
derived from the EXE or DLL** (translated code, extracted data). `game_files/` is git-ignored.

## The user

- New to reverse engineering; explain in plain words, give a recommendation, not a survey.
- Speaks German; works in short sessions and watches usage limits.
- Confirm before anything outward-facing or destructive (pushing, deleting).
- Prefers clean Rust over generated C; performance comes later.

## The game files

`game_files/` holds the user's CD image (`Hogs German.cue` + `Hogs German (Track NN).bin`:
track 1 MODE1/2352 data, tracks 2... CD audio, music) and its contents extracted to
`game_files/cd/` (2048-byte sectors from track 1, then 7z). What matters:

- `warhogs.exe` (SHA-256 `84f5afda...2a9c`, base 0x400000, 965 KB, not packed): the game.
  Imports kernel32, user32, gdi32, advapi32, ole32, comctl32, ddraw (DirectDrawCreateEx:
  DirectX 7), dsound, dinput (DirectInputCreateEx), winmm, binkw32 (videos): 171 imports.
  CD check through GetDriveTypeA ("Bitte legen Sie die Frontschweine-CD ein.").
- `Data/_d3d.dll` (SHA-256 `1932766f...9026`, base 0x10000000, has relocations): the 3D
  engine (map, models, sky, sort lists, text) on DirectDraw 7/Direct3D. The EXE loads it with
  LoadLibrary (`_D3D.DLL`) and calls ~90 exports (`afDrawObj`, `afSetMap`, `Begin3D`, ...).
  71 imports.
- `binkw32.dll` (Bink 'f', recompiled too), videos `Language/Streams/*.bik` (320x240, 25 fps); data in `Maps/`, `Chars/`, `Skys/`,
  `Audio/` (WAV), `Speech/`, `Language/`, `FEBmps/`, `FEText/`, `FESounds/`.
- Not needed: the installer (`SETUP.EXE`, `*.cab`, `_INST32I.EX_`), `FEProg.exe`/`wh32LIB.DLL`
  (launcher), `Redist/` (DirectX), `Extras/`.

## How it works

The core is taken from future_rust_cop (2026-10-28) and made independent of Future Cop:

- `src/image.rs`: a PE file's sections (`exec`: holds code), imports, exports, resources.
  `Image::combine` puts the EXE and the DLL (each at its own base) into one image so their
  code is found and compiled together. `data/symbols.txt`: what we know, by address.
- `src/game.rs`: loads `warhogs.exe` and `Data/_d3d.dll` from the game folder; seeds for
  finding the code: both entry points and the DLL's exports.
- `src/discover.rs`: `find_functions` finds function boundaries without a Ghidra export:
  recursive descent from the seeds through calls; then code addresses found only as numbers
  (in data sections, in undecoded words of code sections, as `imm32` operands: vtables,
  callbacks, window procedures) when `plausible` (decodes without running into the middle of
  known instructions, jumping into them is fine, and without instructions compiled code
  never has; data and jump tables decoded as code are full of those). Each function runs to
  the next entry. `Program::build` then decodes each function inside its range and adds
  extra entries where code jumps or calls into the middle of one, and for data pointing at
  decoded instructions. Result (2026-10-28): 2714 functions, 2874 entries. The bytes left
  undecoded are jump/index tables and some unreferenced C runtime code
  (`examples/functions.rs --gaps`). A missed function shows at run time as "call to unknown
  address" (it was a vtable entry once: fixed by the plausibility rule above).
- `src/lift.rs`, `src/lift/x87.rs`: x86 -> Cranelift IR (see future_rust_cop's CLAUDE.md for
  the details: registers as variables, one block per instruction, tail calls between
  functions, `rt_lookup` for indirect targets, x87 with rounding modes).
- `src/smc.rs`: fields of instructions the game writes (`mov [code address], x`); none found
  in Hogs of War so far. Future Cop's rewritten-instruction variants were dropped.
- `src/rt.rs`: the CPU state, guest memory (4 GB reserved; guest address a = base + a),
  dispatch, events, x87 helpers. `HOGS_COUNT=<file>`: functions count their calls.
- `src/jit.rs`: compiles all functions; the machine code can be cached (`compile_cached`).
- `src/iso.rs`: reading files from a CD image (.iso or raw .bin), for the setup later.
- `src/main.rs`: `rustschweine [game folder]` (default `game_files/cd`): finds, compiles
  (cached in `<folder>/compiled.bin`) and runs the game.

### Platform layer (`src/platform/`, from future_rust_cop, 2026-10-28)

How the game starts (`HOGS_TRACE_API=1` shows every import call):
1. C runtime start-up, then DirectDraw 7 is checked (LoadLibraryA("DDRAW.DLL") +
   GetProcAddress: `system_module`/`system_proc` give our own functions for system DLLs).
2. Registry: `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Hogs Of War` with
   InstallSource `D:\`, InstallLocation `C:\Hogs\`, LanguageLetter `G` (German), Footprint 2
   (full installation) are what the installer wrote (`advapi32.rs` puts them in;
   `registry.txt` in the game folder keeps changes). Without them the game quits.
3. CD check (0x47f7c0): GetDriveTypeA finds D: (CD-ROM), D:\PIGS.ID must exist, writing
   D:\temp.tmp must fail (`on_cd`: D: is read-only). Paths on C:\Hogs and D: both mean the
   game folder (`host_path`); the game sets the current folder to D:\.
4. The launcher, dialog LAUNCHERBOX (device, mode, detail; Start/Ende), shown by our dialog
   overlay (`dialog/`), which opens the window for it. Start writes `launch.bin`. Scripted:
   `HOGS_INPUT="1500:click@424,338"` (Start, in 640x480 overlay pixels). `dialog/extend.rs`
   puts our settings in: the display's resolutions instead of the game's modes (the game is
   always given 640x480: it keeps the mode from CBN_SELENDOK, its menus break at others) and
   a "Vollbild" box; saved as `RenderSize` and `Fullscreen` in the game's key (`display.rs`:
   a window gets that size, fullscreen is borderless with the 3D drawn at it).
5. Main window, `_d3d.dll` loaded (`kernel32::load_library`: its DllMain once; GetProcAddress
   from its exports), DirectDraw 7 + Direct3D 7 device (`d3d.rs`: one HAL device drawn with
   OpenGL into a framebuffer of the size the window shows the picture in, the game's
   coordinates staying 640x480 (`HOGS_LOWRES=1`: 640x480); the back buffer's 555 pixels are
   stretched into it when DirectDraw wrote them (`ddraw::Surface::stamp`), and written back
   shrunk before DirectDraw touches them (`d3d::flush`)), DirectPlay + lobby by CoCreateInstance (`dplay.rs`), DirectInput 7 (keyboard and
   mouse by state, `dinput.rs`), DirectSound (`dsound.rs`, ordinal #1), mixer/MCI (none).
   Videos: the CD's `binkw32.dll` is recompiled with the EXE and the engine (`game.rs`: moved
   to 0x12000000 by its relocations, its DllMain run before the EXE's entry). The registry's
   Footprint is forced to 1: the game reads it only for the videos, 2 skips them.
6. Title screen (bitmaps by LoadImageA + StretchBlt onto surfaces: `gdi32.rs`), "Taste
   drücken", then the front end: 2D BltFast onto the back buffer, 3D (the team screen's pig)
   by DrawPrimitive, Flip. The missions are all Direct3D (pre-transformed vertices, FVF 0x1c4,
   16-bit 1555 texture atlases, table fog by distance).

Guest memory: EXE at 0x400000, DLL at 0x10000000 (its .data is 30 MB), binkw32.dll at
0x12000000, heap 0x20000000-
0x70000000, thunks 0x7f000000. Other files: `ddraw.rs` (DirectDraw 1/2/7, DDSURFACEDESC2 by
size), `winmm.rs` (mmio RIFF reading), `cdaudio.rs` (MCI CD audio: the music
tracks straight from the `.cue`/`.bin` image in the game folder or above it; the mixer's CD
volume, which the game sets from master volume x fades; not kept between runs on purpose), `gdi32.rs` (bitmaps, DCs).
Widescreen in missions: `wide.rs` (what the engine cuts at 640 widened while a mission picture is
drawn), `d3d.rs` `margin`; `HOGS_WIDE=0|1` overrides the launcher's "Breitbild".
Antialiasing: the launcher's "Antialiasing" list (REG_DWORD `Antialiasing`, samples) multisamples
Direct3D's framebuffer (`d3d.rs` `resolve`); `HOGS_MSAA=<samples>` overrides it. The
shaders' outputs are `centroid` (else edge pixels sampled past a ground tile into the atlas's
transparent border: thin lines between far tiles).
Env: `HOGS_SHOT=file.png` (the shown frame; `{t}` in the name keeps all, `HOGS_SHOT_MS` apart), `HOGS_INPUT` (scripted input, see
`window.rs`; into the first mission: `"1500:click@424,338 8000:Enter 11000:Enter 14000:Enter
17000:Enter 20000:Enter 23000:Enter 26000:Enter 30000:Enter"`, ~45 s; `HOGS_INPUT=@file.txt`
reads a file), `HOGS_RECORD=file.txt` (the player's input written as HOGS_INPUT items; F11 a mark:
time, camera and the picture as `file.txt.<t>.png`), `HOGS_MONITOR=1` (the window opens on that display), `HOGS_TRACE_API` (with
arguments), `HOGS_D3D_DUMP=folder` (textures as PNG), `HOGS_D3D_LOG=<texture hex>` (only its
draws, all vertices), `HOGS_TRACE_MSG`, `HOGS_D3D_LOG`, `HOGS_DD_LOG`, `HOGS_WATCH`.

### Frame rate (`src/platform/fps.rs`)

Missions step every 40 ms (0x47f930: logic 0x49fea0, then the drawing 0x44e290). Between
steps the drawing runs again for extra pictures, as a transaction: the pages it writes of EXE
.data, engine DLL .data and the heap are put back afterwards, and the heap's bookkeeping. Guest
memory is reserved with `MEM_WRITE_WATCH`; `GetWriteWatch` names the written pages, their old
contents come from a copy of those ranges (`Shadow`, brought up to date before each extra
picture); no other guest thread runs meanwhile (`EXCLUSIVE`; `guest_call` takes the turn
before writing guest memory). Host code that writes guest memory in a picture should write
only changed values (each page written costs a copy). The camera ([0x537f98]: position floats
at [[+0x18]+4], angles +0x84/0x88/0x8c, 4096 a turn) is set between the last two steps; a
step's own picture shows the step before.
`HOGS_FPS` = cap (0: original), `HOGS_FPS_LOG=1` statistics, `HOGS_SAMPLE=1` a sampling
profiler (`sample.rs`: guest function / import / host helper the main thread is in).
The mission's 3D is drawn by our own shaders, not by the engine's CPU code (each checked
against the engine's vertices): ground and water (`ground.rs`), objects, trees, the sky and the
hogs with their bones (`objects.rs`, afDrawObj; models kept in a vertex buffer, the object's
or bones' matrices as uniforms; `HOGS_OBJECTS=engine|check`, `HOGS_GROUND=engine`; the module
comments describe the engine's data). The game's objects (list at [0x537fbc], next +4, the
position at [[obj+0x18]+4] as the camera's) are blended between the last two steps like the
camera (`fps.rs` `bodies_step`): hogs, names, shadows follow; the hogs' poses are blended
relative to their position (`objects.rs` `smooth`, `fps::blend`). `examples/pngdiff.rs` compares
two `HOGS_SHOT`s.
Direct3D draws are batched by state (`d3d.rs` `Pipe`); a flip of a Direct3D picture shows the
GL texture directly (`d3d::show`). x87 under 24 bits to nearest (Direct3D's control word
0x3f) is computed inline (`lift/x87.rs` `arith`), exact (falls back to `rt_fpu_round` on a
float's midpoint unless the double was exact, and outside the float's range).

Tools: `cargo run --release --example functions -- game_files/cd [--gaps]` (find + compile
both programs, statistics), `examples/dis.rs <folder> <addr|all>`, `examples/imports.rs
<exe or dll>`. `cargo test --release` needs no game files.
