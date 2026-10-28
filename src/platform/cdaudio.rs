//! CD audio through MCI (`mciSendCommandA`, device type "cdaudio"): the game plays its music
//! from the CD's audio tracks. They are read straight from the player's CD image: a `.cue`
//! sheet in the game folder or the folder above it (`game_files/Hogs German.cue` next to
//! `game_files/cd`), with its `.bin` files (raw sectors of 2352 bytes: 588 frames of 16-bit
//! stereo at 44.1 kHz, little-endian). Nothing is extracted; a track is read when it is played
//! and mixed by `dsound.rs`.
//!
//! What the game does (0x45f880...): open the device, set the time format to TMSF (track,
//! minute, second, frame), ask whether a CD is there, the number of tracks, each track's type
//! (audio or data) and length, then play from a track to the end of a track, and poll the mode
//! to start the next piece when one ends.

use super::{arg, log, mem};
use crate::rt::Cpu;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const MCIERR_DEVICE_NOT_INSTALLED: u32 = 256 + 50;
const MCIERR_UNRECOGNIZED_COMMAND: u32 = 256 + 5;
const MCIERR_OUTOFRANGE: u32 = 256 + 26;

const MCI_OPEN: u32 = 0x803;
const MCI_CLOSE: u32 = 0x804;
const MCI_PLAY: u32 = 0x806;
const MCI_SEEK: u32 = 0x807;
const MCI_STOP: u32 = 0x808;
const MCI_PAUSE: u32 = 0x809;
const MCI_SET: u32 = 0x80d;
const MCI_STATUS: u32 = 0x814;
const MCI_RESUME: u32 = 0x855;

const MCI_FROM: u32 = 4;
const MCI_TO: u32 = 8;
const MCI_TRACK: u32 = 0x10;
const MCI_STATUS_ITEM: u32 = 0x100;
const MCI_SET_TIME_FORMAT: u32 = 0x400;

const MODE_STOP: u32 = 525;
const MODE_PLAY: u32 = 526;
const MODE_PAUSE: u32 = 529;

/// The time format TMSF: track | minute << 8 | second << 16 | frame << 24.
const FORMAT_TMSF: u32 = 10;
/// The time format MSF: minute | second << 8 | frame << 16.
const FORMAT_MSF: u32 = 2;

/// Frames (sectors) per second.
const FPS: u32 = 75;
const SECTOR: u64 = 2352;

/// The device ID we hand out.
const DEVICE: u32 = 1;

struct Track {
    file: PathBuf,
    /// where its sectors start in the file (INDEX 01), in bytes
    start: u64,
    /// its length in sectors
    sectors: u32,
    audio: bool,
}

/// The CD image's tracks (track 1 first), None without a cue sheet.
fn tracks() -> Option<&'static [Track]> {
    static T: OnceLock<Option<Vec<Track>>> = OnceLock::new();
    T.get_or_init(|| {
        let dir = super::game_dir();
        let cue = [Some(dir), dir.parent()].into_iter().flatten().find_map(find_cue)?;
        let t = parse_cue(&cue);
        log!("CD audio: {} ({} tracks)", cue.display(), t.len());
        (!t.is_empty()).then_some(t)
    })
    .as_deref()
}

fn find_cue(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("cue")))
}

/// Read a cue sheet: FILE "name" BINARY, TRACK nn AUDIO|MODE1/2352, INDEX 01 mm:ss:ff. A track
/// ends where the next one in the same file begins (its INDEX 00 or 01), or at the file's end.
fn parse_cue(cue: &Path) -> Vec<Track> {
    let Ok(text) = std::fs::read_to_string(cue) else { return Vec::new() };
    let dir = cue.parent().unwrap_or(Path::new("."));
    let msf = |s: &str| -> Option<u64> {
        let v: Vec<u64> = s.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
        (v.len() == 3).then(|| (v[0] * 60 + v[1]) * FPS as u64 + v[2])
    };
    // (file, audio, first sector of INDEX 00 or 01, sector of INDEX 01)
    let mut raw: Vec<(PathBuf, bool, Option<u64>, Option<u64>)> = Vec::new();
    let mut file = PathBuf::new();
    for line in text.lines() {
        let line = line.trim();
        let word = line.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        match word.as_str() {
            "FILE" => {
                let name = line.split('"').nth(1).map(str::to_string).or_else(|| line.split_whitespace().nth(1).map(str::to_string));
                file = dir.join(name.unwrap_or_default());
            }
            "TRACK" => {
                let audio = line.to_ascii_uppercase().contains("AUDIO");
                raw.push((file.clone(), audio, None, None));
            }
            "INDEX" => {
                let mut it = line.split_whitespace().skip(1);
                let (Some(n), Some(at)) = (it.next(), it.next().and_then(msf)) else { continue };
                if let Some(t) = raw.last_mut() {
                    if t.2.is_none() {
                        t.2 = Some(at);
                    }
                    if n == "01" {
                        t.3 = Some(at);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (k, (file, audio, first, start)) in raw.iter().enumerate() {
        let start = start.or(*first).unwrap_or(0);
        let end = match raw.get(k + 1) {
            Some((f, _, Some(next), _)) if f == file => *next,
            _ => std::fs::metadata(file).map_or(start, |m| m.len() / SECTOR),
        };
        out.push(Track { file: file.clone(), start: start * SECTOR, sectors: end.saturating_sub(start) as u32, audio: *audio });
    }
    out
}

/// What MCI was told and what plays.
struct State {
    open: bool,
    format: u32,
    /// the track playing (1-based) and the sector of it where the samples start
    from: (u32, u32),
}

static STATE: Mutex<State> = Mutex::new(State { open: false, format: FORMAT_MSF, from: (1, 0) });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn msf(sectors: u32) -> u32 {
    let (m, s, f) = (sectors / FPS / 60, sectors / FPS % 60, sectors % FPS);
    m | s << 8 | f << 16
}

/// A position in the time format as (track, sector of that track).
fn position(format: u32, v: u32, tracks: &[Track]) -> (u32, u32) {
    let b = |k: u32| (v >> (8 * k)) & 0xff;
    if format == FORMAT_TMSF {
        (b(0).max(1), (b(1) * 60 + b(2)) * FPS + b(3))
    } else {
        // MSF or milliseconds on the whole disc: find the track
        let mut at = if format == FORMAT_MSF { (b(0) * 60 + b(1)) * FPS + b(2) } else { v * FPS / 1000 };
        for (k, t) in tracks.iter().enumerate() {
            if at < t.sectors {
                return (k as u32 + 1, at);
            }
            at -= t.sectors;
        }
        (tracks.len() as u32 + 1, 0)
    }
}

/// The samples from (track, sector) to (track, sector), audio tracks only.
fn read(tracks: &[Track], from: (u32, u32), to: (u32, u32)) -> Vec<i16> {
    let mut out = Vec::new();
    for t in from.0..=to.0.min(tracks.len() as u32) {
        let tr = &tracks[t as usize - 1];
        let first = if t == from.0 { from.1 } else { 0 };
        let last = if t == to.0 { to.1.min(tr.sectors) } else { tr.sectors };
        if !tr.audio || first >= last {
            continue;
        }
        use std::io::{Read, Seek, SeekFrom};
        let mut buf = vec![0u8; (last - first) as usize * SECTOR as usize];
        let ok = std::fs::File::open(&tr.file).and_then(|mut f| {
            f.seek(SeekFrom::Start(tr.start + first as u64 * SECTOR))?;
            f.read_exact(&mut buf)
        });
        if let Err(e) = ok {
            log!("CD audio: track {t}: {}: {e}", tr.file.display());
            continue;
        }
        out.extend(buf.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
    }
    out
}

/// mciSendCommandA(device, message, flags, parameters)
pub fn mci_send_command(c: &mut Cpu) -> u32 {
    let (msg, flags, p) = (arg(c, 1), arg(c, 2), arg(c, 3));
    let Some(tracks) = tracks() else {
        if super::trace_api() {
            log!("mciSendCommandA(message {msg:#x}, flags {flags:#x}): no CD image");
        }
        return MCIERR_DEVICE_NOT_INSTALLED;
    };
    let m = mem();
    let mut st = state();
    match msg {
        MCI_OPEN => {
            st.open = true;
            st.format = FORMAT_MSF;
            m.wr32(p + 4, DEVICE);
        }
        MCI_CLOSE => {
            st.open = false;
            super::dsound::cd_stop();
        }
        MCI_SET => {
            if flags & MCI_SET_TIME_FORMAT != 0 {
                st.format = m.rd32(p + 4);
            }
        }
        MCI_PLAY => {
            let from = if flags & MCI_FROM != 0 {
                position(st.format, m.rd32(p + 4), tracks)
            } else {
                // on from where it is
                let played = super::dsound::cd_state().map_or(0, |(pos, _, _)| pos as u32 / 588);
                (st.from.0, st.from.1 + played)
            };
            let to = if flags & MCI_TO != 0 { position(st.format, m.rd32(p + 8), tracks) } else { (tracks.len() as u32, u32::MAX) };
            if from.0 == 0 || from.0 as usize > tracks.len() {
                return MCIERR_OUTOFRANGE;
            }
            log!("CD audio: play track {} at {} to track {} at {}", from.0, from.1, to.0, to.1);
            st.from = from;
            super::dsound::cd_play(Arc::new(read(tracks, from, to)));
        }
        MCI_SEEK | MCI_STOP => super::dsound::cd_stop(),
        MCI_PAUSE => super::dsound::cd_pause(true),
        MCI_RESUME => super::dsound::cd_pause(false),
        MCI_STATUS if flags & MCI_STATUS_ITEM != 0 => {
            let track = m.rd32(p + 12);
            let tr = (flags & MCI_TRACK != 0).then(|| tracks.get((track as usize).wrapping_sub(1))).flatten();
            let playing = super::dsound::cd_state();
            let v = match m.rd32(p + 8) {
                // length: of a track, or of the disc
                1 => msf(tr.map_or_else(|| tracks.iter().map(|t| t.sectors).sum(), |t| t.sectors)),
                // position: where a track starts, or where it plays
                2 => match tr {
                    Some(_) => track,
                    None => {
                        let at = st.from.1 + playing.map_or(0, |(pos, _, _)| pos as u32 / 588);
                        let (mm, s, f) = (at / FPS / 60, at / FPS % 60, at % FPS);
                        st.from.0 | mm << 8 | s << 16 | f << 24
                    }
                },
                3 => tracks.len() as u32,
                4 => match playing {
                    Some((pos, len, paused)) if pos < len => {
                        if paused {
                            MODE_PAUSE
                        } else {
                            MODE_PLAY
                        }
                    }
                    _ => MODE_STOP,
                },
                // media present, ready
                5 | 7 => 1,
                6 => st.format,
                8 => st.from.0,
                // track type: audio or other
                0x4001 => {
                    if tr.is_some_and(|t| t.audio) {
                        0x440
                    } else {
                        0x441
                    }
                }
                item => {
                    log!("warn: MCI status item {item:#x} not supported");
                    0
                }
            };
            m.wr32(p + 4, v);
        }
        _ => {
            log!("warn: MCI message {msg:#x} (flags {flags:#x}) not supported");
            return MCIERR_UNRECOGNIZED_COMMAND;
        }
    }
    0
}

// ---- the mixer: one line, CD audio, with a volume control ---------------------------------------
//
// The game (0x45fce0...) opens mixer 0, finds the line of component type "compact disc",
// takes its volume control (0...65535) and sets it from its music volume option.

const MIXERR_INVALLINE: u32 = 1024;
const MIXERR_INVALCONTROL: u32 = 1025;
const MIXERLINE_COMPONENTTYPE_SRC_COMPACTDISC: u32 = 0x1005;
const MIXERCONTROL_CONTROLTYPE_VOLUME: u32 = 0x5003_0001;
const MIXER_HANDLE: u32 = 0x4d58;
const LINE: u32 = 1;
const CONTROL: u32 = 1;

/// The volume control's value, 0...65535. It starts full and is not kept: the game reads it
/// at start as the music's share (0x4398e0: value / 655 as a percentage), then sets it to
/// master volume x that share x its fades (0x43e060: fading out to 0 while a mission loads),
/// and puts the start value back when it quits. A kept value would keep a fade (or a crash's
/// last value) and mute the music from then on.
static VOLUME: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xffff);

fn volume() -> u32 {
    VOLUME.load(std::sync::atomic::Ordering::Relaxed)
}

fn put_str(p: u32, max: u32, s: &str) {
    let m = mem();
    m.slice(p, max as usize).fill(0);
    m.slice(p, s.len().min(max as usize - 1)).copy_from_slice(&s.as_bytes()[..s.len().min(max as usize - 1)]);
}

pub fn mixer_get_num_devs(_: &mut Cpu) -> u32 {
    1
}

/// mixerOpen(out handle, id, callback, instance, flags)
pub fn mixer_open(c: &mut Cpu) -> u32 {
    if arg(c, 0) != 0 {
        mem().wr32(arg(c, 0), MIXER_HANDLE);
    }
    0
}

pub fn mixer_close(_: &mut Cpu) -> u32 {
    0
}

/// mixerGetDevCapsA(id, MIXERCAPSA*, size): one destination.
pub fn mixer_get_dev_caps(c: &mut Cpu) -> u32 {
    let (p, size) = (arg(c, 1), arg(c, 2));
    let m = mem();
    m.slice(p, size.min(48) as usize).fill(0);
    if size >= 48 {
        put_str(p + 8, 32, "Mixer");
        m.wr32(p + 44, 1);
    }
    0
}

/// mixerGetLineInfoA(handle, MIXERLINEA*, flags): the CD audio line, whatever is asked.
pub fn mixer_get_line_info(c: &mut Cpu) -> u32 {
    let (p, flags) = (arg(c, 1), arg(c, 2));
    let m = mem();
    // by component type (3): only the CD's
    if flags & 0xf == 3 && m.rd32(p + 0x18) != MIXERLINE_COMPONENTTYPE_SRC_COMPACTDISC {
        return MIXERR_INVALLINE;
    }
    m.slice(p + 4, 0xa8 - 4).fill(0);
    m.wr32(p + 0xc, LINE);
    m.wr32(p + 0x10, 1); // active
    m.wr32(p + 0x18, MIXERLINE_COMPONENTTYPE_SRC_COMPACTDISC);
    m.wr32(p + 0x1c, 2); // channels
    m.wr32(p + 0x24, 1); // controls
    put_str(p + 0x28, 16, "CD");
    put_str(p + 0x38, 64, "CD Audio");
    0
}

/// mixerGetLineControlsA(handle, MIXERLINECONTROLSA*, flags): the volume control.
pub fn mixer_get_line_controls(c: &mut Cpu) -> u32 {
    let (p, flags) = (arg(c, 1), arg(c, 2));
    let m = mem();
    // one by type (2): only the volume
    if flags & 0xf == 2 && m.rd32(p + 8) != MIXERCONTROL_CONTROLTYPE_VOLUME {
        return MIXERR_INVALCONTROL;
    }
    let ctl = m.rd32(p + 0x14);
    if ctl == 0 {
        return MIXERR_INVALCONTROL;
    }
    m.slice(ctl, 0x94).fill(0);
    m.wr32(ctl, 0x94);
    m.wr32(ctl + 4, CONTROL);
    m.wr32(ctl + 8, MIXERCONTROL_CONTROLTYPE_VOLUME);
    m.wr32(ctl + 0xc, 1); // uniform
    put_str(ctl + 0x14, 16, "Volume");
    put_str(ctl + 0x24, 64, "CD Volume");
    m.wr32(ctl + 0x68, 0xffff); // maximum
    m.wr32(ctl + 0x7c, 0x10000); // steps
    0
}

/// mixerGetControlDetailsA(handle, MIXERCONTROLDETAILS*, flags): the volume, per channel.
pub fn mixer_get_control_details(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let m = mem();
    let (n, values) = (m.rd32(p + 8).max(1), m.rd32(p + 0x14));
    let v = volume();
    for k in 0..n.min(2) {
        m.wr32(values + 4 * k, v);
    }
    0
}

/// mixerSetControlDetails(handle, MIXERCONTROLDETAILS*, flags): the volume (both channels'
/// average when given two).
pub fn mixer_set_control_details(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let m = mem();
    let (n, values) = (m.rd32(p + 8).clamp(1, 2), m.rd32(p + 0x14));
    let v = ((0..n).map(|k| m.rd32(values + 4 * k).min(0xffff)).sum::<u32>() / n).min(0xffff);
    VOLUME.store(v, std::sync::atomic::Ordering::Relaxed);
    if super::trace_api() {
        log!("CD volume {v:#x}");
    }
    super::dsound::cd_volume(v as f32 / 65535.0);
    0
}
