//! Sound: DirectSound buffers and PlaySoundA, mixed into one output stream (cpal).
//!
//! Sample data stays in guest memory (the game writes it through `Lock`); the mixer reads it
//! from the audio thread. Play positions advance in real time with the output, so the music,
//! which the game streams into a looping buffer behind the play cursor, keeps going. Without
//! an audio device a timer thread advances them instead. A stream of host samples (`stream_*`)
//! carries the videos' sound, and `cd_*` the CD's music tracks (`cdaudio.rs`).

use super::{Import, ResId, alloc, arg, com_new, cstr, free, guest_call, host_path, log, mem, new_cstr, resource};
use crate::rt::Cpu;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const OK: u32 = 0;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DSERR_INVALIDPARAM: u32 = 0x8007_0057;

const DSBCAPS_PRIMARYBUFFER: u32 = 1;

#[derive(Clone, Copy)]
struct Format {
    rate: u32,
    channels: u32,
    bits: u32,
}

impl Format {
    fn block(&self) -> u32 {
        self.channels * self.bits / 8
    }

    fn read(p: u32) -> Format {
        let m = mem();
        Format { channels: (m.rd16(p + 2) as u32).max(1), rate: m.rd32(p + 4).max(1), bits: if m.rd16(p + 14) == 8 { 8 } else { 16 } }
    }

    fn write(&self, p: u32) {
        let m = mem();
        m.wr16(p, 1); // PCM
        m.wr16(p + 2, self.channels as u16);
        m.wr32(p + 4, self.rate);
        m.wr32(p + 8, self.rate * self.block());
        m.wr16(p + 12, self.block() as u16);
        m.wr16(p + 14, self.bits as u16);
        m.wr16(p + 16, 0);
    }
}

/// A DirectSound buffer, or the PlaySoundA sound.
#[derive(Clone)]
struct Voice {
    data: u32,
    bytes: u32,
    fmt: Format,
    /// SetFrequency (0: the format's rate)
    freq: u32,
    /// position in frames
    pos: f64,
    playing: bool,
    looping: bool,
    /// hundredths of a dB
    volume: i32,
    pan: i32,
    primary: bool,
    refs: u32,
}

impl Voice {
    fn frames(&self) -> u32 {
        self.bytes / self.fmt.block().max(1)
    }

    /// Play and write cursor in bytes.
    fn cursors(&self, out_rate: u32) -> (u32, u32) {
        if self.bytes == 0 {
            return (0, 0);
        }
        let block = self.fmt.block();
        let play = (self.pos as u32).min(self.frames().saturating_sub(1)) * block;
        // the write cursor is where data must already be: a little ahead of the play cursor
        let ahead = (self.rate() as u64 * 20 / 1000).max(out_rate as u64 / 100) as u32 * block;
        (play, if self.playing { (play + ahead) % self.bytes } else { play })
    }

    fn rate(&self) -> u32 {
        if self.freq == 0 { self.fmt.rate } else { self.freq }
    }

    /// The sample of channel `ch` at frame `i`, -1..1.
    fn sample(&self, i: u32, ch: u32) -> f32 {
        let ch = ch.min(self.fmt.channels - 1);
        let a = self.data + i * self.fmt.block() + ch * self.fmt.bits / 8;
        let m = mem();
        if self.fmt.bits == 8 { (m.rd8(a) as f32 - 128.0) / 128.0 } else { m.rd16(a) as i16 as f32 / 32768.0 }
    }

    /// Mix into interleaved stereo frames at `out_rate`, advancing the position.
    fn mix(&mut self, out: &mut [[f32; 2]], out_rate: u32) {
        let frames = self.frames();
        if !self.playing || self.primary || frames == 0 {
            return;
        }
        let gain = 10f32.powf(self.volume.clamp(-10000, 0) as f32 / 2000.0);
        let (mut gl, mut gr) = (gain, gain);
        if self.pan > 0 {
            gl *= 10f32.powf(-self.pan.min(10000) as f32 / 2000.0);
        } else if self.pan < 0 {
            gr *= 10f32.powf(self.pan.max(-10000) as f32 / 2000.0);
        }
        let step = self.rate() as f64 / out_rate as f64;
        for o in out.iter_mut() {
            let i = self.pos as u32;
            let frac = (self.pos - i as f64) as f32;
            let next = if i + 1 < frames { i + 1 } else if self.looping { 0 } else { i };
            for (ch, g) in [(0, gl), (1, gr)] {
                let (a, b) = (self.sample(i, ch), self.sample(next, ch));
                o[ch as usize] += (a + (b - a) * frac) * g;
            }
            self.pos += step;
            if self.pos >= frames as f64 {
                if self.looping {
                    self.pos -= frames as f64;
                } else {
                    self.playing = false;
                    self.pos = 0.0;
                    break;
                }
            }
        }
    }
}

struct Mixer {
    voices: BTreeMap<u32, Voice>,
    /// sample data blocks and how many buffers (duplicates included) use each
    data_refs: BTreeMap<u32, u32>,
    /// the PlaySoundA sound
    wave: Option<Voice>,
    /// a video's sound
    stream: Option<Stream>,
    /// CD audio, and its volume (0...1, the mixer's CD volume control)
    cd: Option<Cd>,
    cd_gain: f32,
    out_rate: u32,
    started: bool,
}

static MIX: Mutex<Mixer> =
    Mutex::new(Mixer { voices: BTreeMap::new(), data_refs: BTreeMap::new(), wave: None, stream: None, cd: None, cd_gain: 1.0, out_rate: 44100, started: false });

/// Samples the host gives (a video's sound), played once.
struct Stream {
    queue: VecDeque<[f32; 2]>,
    rate: u32,
    /// position between the first two queued frames
    frac: f64,
    /// frames taken from the queue
    played: u64,
    gain: f32,
    paused: bool,
}

impl Stream {
    fn mix(&mut self, out: &mut [[f32; 2]], out_rate: u32) {
        if self.paused {
            return;
        }
        let step = self.rate as f64 / out_rate as f64;
        for o in out.iter_mut() {
            let (Some(a), b) = (self.queue.front().copied(), self.queue.get(1).copied()) else { break };
            let b = b.unwrap_or(a);
            let f = self.frac as f32;
            o[0] += (a[0] + (b[0] - a[0]) * f) * self.gain;
            o[1] += (a[1] + (b[1] - a[1]) * f) * self.gain;
            self.frac += step;
            while self.frac >= 1.0 && !self.queue.is_empty() {
                self.frac -= 1.0;
                self.queue.pop_front();
                self.played += 1;
            }
        }
    }
}

/// CD audio being played: 44.1 kHz 16-bit stereo, interleaved.
struct Cd {
    samples: std::sync::Arc<Vec<i16>>,
    /// position in frames
    pos: f64,
    paused: bool,
}

impl Cd {
    fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    fn mix(&mut self, out: &mut [[f32; 2]], out_rate: u32, gain: f32) {
        let n = self.frames();
        if self.paused || n == 0 {
            return;
        }
        let step = 44100.0 / out_rate as f64;
        let s = |i: usize, ch: usize| self.samples[2 * i.min(n - 1) + ch] as f32 / 32768.0;
        for o in out.iter_mut() {
            let i = self.pos as usize;
            if i >= n {
                break;
            }
            let f = (self.pos - i as f64) as f32;
            for (ch, o) in o.iter_mut().enumerate() {
                let (a, b) = (s(i, ch), s(i + 1, ch));
                *o += (a + (b - a) * f) * gain;
            }
            self.pos += step;
        }
    }
}

/// Play CD audio samples (replaces what plays).
pub fn cd_play(samples: std::sync::Arc<Vec<i16>>) {
    mixer().cd = Some(Cd { samples, pos: 0.0, paused: false });
}

pub fn cd_pause(paused: bool) {
    if let Some(c) = &mut mixer().cd {
        c.paused = paused;
    }
}

/// The CD audio's volume, 0...1.
pub fn cd_volume(v: f32) {
    mixer().cd_gain = v.clamp(0.0, 1.0);
}

pub fn cd_stop() {
    mixer().cd = None;
}

/// The CD audio's position and length in frames, and whether it is paused; None when nothing
/// plays.
pub fn cd_state() -> Option<(usize, usize, bool)> {
    mixer().cd.as_ref().map(|c| ((c.pos as usize).min(c.frames()), c.frames(), c.paused))
}

/// Start a stream of host samples at `rate` (replaces one playing).
pub fn stream_start(rate: u32) {
    mixer().stream = Some(Stream { queue: VecDeque::new(), rate: rate.max(1), frac: 0.0, played: 0, gain: 1.0, paused: false });
}

/// Queue samples for the stream.
pub fn stream_push(frames: &[[f32; 2]]) {
    if let Some(s) = &mut mixer().stream {
        s.queue.extend(frames);
    }
}

/// Frames queued and not played yet, and frames played since the start.
pub fn stream_state() -> (usize, u64) {
    mixer().stream.as_ref().map_or((0, 0), |s| (s.queue.len(), s.played))
}

/// The stream's volume in hundredths of a dB (0 full, -10000 silent).
pub fn stream_volume(v: i32) {
    if let Some(s) = &mut mixer().stream {
        s.gain = 10f32.powf(v.clamp(-10000, 0) as f32 / 2000.0);
    }
}

pub fn stream_pause(paused: bool) {
    if let Some(s) = &mut mixer().stream {
        s.paused = paused;
    }
}

pub fn stream_stop() {
    mixer().stream = None;
}

fn mixer() -> MutexGuard<'static, Mixer> {
    let mut m = MIX.lock().unwrap_or_else(|e| e.into_inner());
    if !m.started {
        m.started = true;
        // the output thread takes the lock itself: start it without holding it
        drop(m);
        start_output();
        m = MIX.lock().unwrap_or_else(|e| e.into_inner());
    }
    m
}

impl Mixer {
    fn mix(&mut self, out: &mut [[f32; 2]]) {
        out.fill([0.0; 2]);
        let rate = self.out_rate;
        for v in self.voices.values_mut() {
            v.mix(out, rate);
        }
        if let Some(w) = &mut self.wave {
            w.mix(out, rate);
            if !w.playing {
                self.wave = None;
            }
        }
        if let Some(s) = &mut self.stream {
            s.mix(out, rate);
        }
        if let Some(c) = &mut self.cd {
            c.mix(out, rate, self.cd_gain);
        }
    }
}

/// Open the audio device and mix into it; without one, advance the positions in real time.
fn start_output() {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    fn open<T: cpal::SizedSample + cpal::FromSample<f32>>(dev: &cpal::Device, cfg: cpal::StreamConfig) -> Option<cpal::Stream> {
        let ch = cfg.channels as usize;
        let mut buf: Vec<[f32; 2]> = Vec::new();
        let stream = dev
            .build_output_stream(
                cfg,
                move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                    buf.resize(out.len() / ch, [0.0; 2]);
                    MIX.lock().unwrap_or_else(|e| e.into_inner()).mix(&mut buf);
                    for (frame, s) in out.chunks_mut(ch).zip(&buf) {
                        for (k, o) in frame.iter_mut().enumerate() {
                            let v = if k < 2 { s[k] } else { 0.0 };
                            *o = T::from_sample_(v.clamp(-1.0, 1.0));
                        }
                    }
                },
                |e| log!("warn: audio: {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;
        Some(stream)
    }
    let host = cpal::default_host();
    let stream = host.default_output_device().and_then(|dev| {
        let sup = dev.default_output_config().ok()?;
        let cfg = sup.config();
        MIX.lock().unwrap_or_else(|e| e.into_inner()).out_rate = cfg.sample_rate;
        log!("audio device: {} Hz {} ch", cfg.sample_rate, cfg.channels);
        match sup.sample_format() {
            cpal::SampleFormat::F32 => open::<f32>(&dev, cfg),
            cpal::SampleFormat::I16 => open::<i16>(&dev, cfg),
            cpal::SampleFormat::U16 => open::<u16>(&dev, cfg),
            cpal::SampleFormat::I32 => open::<i32>(&dev, cfg),
            f => {
                log!("warn: audio: sample format {f:?} not supported");
                None
            }
        }
    });
    match stream {
        // the stream plays as long as it exists: for the whole run
        Some(s) => std::mem::forget(s),
        None => {
            log!("warn: no audio output: sound runs silently");
            MIX.lock().unwrap_or_else(|e| e.into_inner()).out_rate = 44100;
            std::thread::spawn(|| {
                let mut buf = vec![[0.0f32; 2]; 441];
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    MIX.lock().unwrap_or_else(|e| e.into_inner()).mix(&mut buf);
                }
            });
        }
    }
}

// ---- DirectSound ------------------------------------------------------------------------------

fn direct_sound_create(c: &mut Cpu) -> u32 {
    drop(mixer());
    mem().wr32(arg(c, 1), com_new(DSOUND, 0));
    OK
}

fn direct_sound_enumerate(c: &mut Cpu) -> u32 {
    let (cb, ctx) = (arg(c, 0), arg(c, 1));
    static NAMES: OnceLock<(u32, u32)> = OnceLock::new();
    let &(desc, module) = NAMES.get_or_init(|| (new_cstr("Primary Sound Driver"), new_cstr("")));
    guest_call(c, cb, &[0, desc, module, ctx]);
    OK
}

fn add_ref(_: &mut Cpu) -> u32 {
    1
}
fn ok(_: &mut Cpu) -> u32 {
    OK
}

fn ds_query_interface(c: &mut Cpu) -> u32 {
    let id = mem().rd32(arg(c, 1));
    log!("warn: IDirectSound::QueryInterface({id:#010x}) not supported");
    mem().wr32(arg(c, 2), 0);
    E_NOINTERFACE
}

fn ds_create_sound_buffer(c: &mut Cpu) -> u32 {
    let (desc, out) = (arg(c, 1), arg(c, 2));
    let m = mem();
    let (flags, bytes, wfx) = (m.rd32(desc + 4), m.rd32(desc + 8), m.rd32(desc + 16));
    let primary = flags & DSBCAPS_PRIMARYBUFFER != 0;
    let fmt = if wfx != 0 { Format::read(wfx) } else { Format { rate: 22050, channels: 2, bits: 16 } };
    if !primary && (bytes == 0 || wfx == 0) {
        return DSERR_INVALIDPARAM;
    }
    let data = if primary { 0 } else { alloc(bytes) };
    if !primary && fmt.bits == 8 {
        m.slice(data, bytes as usize).fill(0x80);
    }
    let obj = com_new(BUFFER, 0);
    let mut mx = mixer();
    if data != 0 {
        mx.data_refs.insert(data, 1);
    }
    mx.voices.insert(
        obj,
        Voice { data, bytes, fmt, freq: 0, pos: 0.0, playing: false, looping: false, volume: 0, pan: 0, primary, refs: 1 },
    );
    m.wr32(out, obj);
    OK
}

fn ds_duplicate_sound_buffer(c: &mut Cpu) -> u32 {
    let (orig, out) = (arg(c, 1), arg(c, 2));
    let mut mx = mixer();
    let Some(v) = mx.voices.get(&orig).cloned() else { return DSERR_INVALIDPARAM };
    let obj = com_new(BUFFER, 0);
    *mx.data_refs.entry(v.data).or_default() += 1;
    mx.voices.insert(obj, Voice { pos: 0.0, playing: false, refs: 1, ..v });
    mem().wr32(out, obj);
    OK
}

fn ds_get_caps(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    let m = mem();
    let size = m.rd32(p).clamp(4, 96);
    m.slice(p + 4, size as usize - 4).fill(0);
    m.wr32(p + 4, 0xf | 0xf00); // primary and secondary: mono, stereo, 8 and 16 bits
    m.wr32(p + 8, 100);
    m.wr32(p + 12, 100_000);
    OK
}

fn ds_get_speaker_config(c: &mut Cpu) -> u32 {
    mem().wr32(arg(c, 1), 4); // DSSPEAKER_STEREO
    OK
}

static DSOUND: &[Import] = &[
    ("IDirectSound::QueryInterface", 3, ds_query_interface),
    ("IDirectSound::AddRef", 1, add_ref),
    ("IDirectSound::Release", 1, ok),
    ("IDirectSound::CreateSoundBuffer", 4, ds_create_sound_buffer),
    ("IDirectSound::GetCaps", 2, ds_get_caps),
    ("IDirectSound::DuplicateSoundBuffer", 3, ds_duplicate_sound_buffer),
    ("IDirectSound::SetCooperativeLevel", 3, ok),
    ("IDirectSound::Compact", 1, ok),
    ("IDirectSound::GetSpeakerConfig", 2, ds_get_speaker_config),
    ("IDirectSound::SetSpeakerConfig", 2, ok),
    ("IDirectSound::Initialize", 2, ok),
];

// ---- IDirectSoundBuffer ---------------------------------------------------------------------------

/// Run `f` on the buffer `this` (the first argument); DSERR_INVALIDPARAM if there is none.
fn with_voice(c: &Cpu, f: impl FnOnce(&mut Voice, u32) -> u32) -> u32 {
    let mut mx = mixer();
    let rate = mx.out_rate;
    match mx.voices.get_mut(&arg(c, 0)) {
        Some(v) => f(v, rate),
        None => DSERR_INVALIDPARAM,
    }
}

fn b_add_ref(c: &mut Cpu) -> u32 {
    with_voice(c, |v, _| {
        v.refs += 1;
        v.refs
    })
}

fn b_release(c: &mut Cpu) -> u32 {
    let this = arg(c, 0);
    let mut mx = mixer();
    let Some(v) = mx.voices.get_mut(&this) else { return 0 };
    v.refs = v.refs.saturating_sub(1);
    let r = v.refs;
    if r == 0 {
        let v = mx.voices.remove(&this).unwrap();
        if let Some(n) = mx.data_refs.get_mut(&v.data) {
            *n -= 1;
            if *n == 0 {
                mx.data_refs.remove(&v.data);
                free(v.data);
            }
        }
    }
    r
}

fn b_get_caps(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        let m = mem();
        m.wr32(p + 4, if v.primary { DSBCAPS_PRIMARYBUFFER } else { 0xe0 });
        m.wr32(p + 8, v.bytes);
        m.wr32(p + 12, 0);
        m.wr32(p + 16, 0);
        OK
    })
}

fn b_get_current_position(c: &mut Cpu) -> u32 {
    let (pp, wp) = (arg(c, 1), arg(c, 2));
    with_voice(c, |v, rate| {
        let (p, w) = v.cursors(rate);
        if pp != 0 {
            mem().wr32(pp, p);
        }
        if wp != 0 {
            mem().wr32(wp, w);
        }
        OK
    })
}

fn b_get_format(c: &mut Cpu) -> u32 {
    let (p, size, written) = (arg(c, 1), arg(c, 2), arg(c, 3));
    with_voice(c, |v, _| {
        if p != 0 && size >= 16 {
            v.fmt.write(p);
        }
        if written != 0 {
            mem().wr32(written, 18);
        }
        OK
    })
}

fn b_get_volume(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        mem().wr32(p, v.volume as u32);
        OK
    })
}

fn b_get_pan(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        mem().wr32(p, v.pan as u32);
        OK
    })
}

fn b_get_frequency(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        mem().wr32(p, v.rate());
        OK
    })
}

fn b_get_status(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        mem().wr32(p, if v.playing { 1 | if v.looping { 4 } else { 0 } } else { 0 });
        OK
    })
}

fn b_lock(c: &mut Cpu) -> u32 {
    let (off, bytes, p1, n1, p2, n2, flags) = (arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7));
    with_voice(c, |v, rate| {
        if v.bytes == 0 {
            return DSERR_INVALIDPARAM;
        }
        let off = if flags & 1 != 0 { v.cursors(rate).1 } else { off % v.bytes };
        let bytes = if flags & 2 != 0 { v.bytes } else { bytes.min(v.bytes) };
        let first = bytes.min(v.bytes - off);
        let m = mem();
        m.wr32(p1, v.data + off);
        m.wr32(n1, first);
        if p2 != 0 {
            m.wr32(p2, if bytes > first { v.data } else { 0 });
        }
        if n2 != 0 {
            m.wr32(n2, bytes - first);
        }
        OK
    })
}

fn b_play(c: &mut Cpu) -> u32 {
    let flags = arg(c, 3);
    with_voice(c, |v, _| {
        v.playing = true;
        v.looping = flags & 1 != 0;
        OK
    })
}

fn b_set_current_position(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        v.pos = (p / v.fmt.block().max(1)) as f64;
        if v.pos >= v.frames() as f64 {
            v.pos = 0.0;
        }
        OK
    })
}

fn b_set_format(c: &mut Cpu) -> u32 {
    let p = arg(c, 1);
    with_voice(c, |v, _| {
        if v.primary && p != 0 {
            v.fmt = Format::read(p);
        }
        OK
    })
}

fn b_set_volume(c: &mut Cpu) -> u32 {
    let x = arg(c, 1) as i32;
    with_voice(c, |v, _| {
        v.volume = x;
        OK
    })
}

fn b_set_pan(c: &mut Cpu) -> u32 {
    let x = arg(c, 1) as i32;
    with_voice(c, |v, _| {
        v.pan = x;
        OK
    })
}

fn b_set_frequency(c: &mut Cpu) -> u32 {
    let x = arg(c, 1);
    with_voice(c, |v, _| {
        v.freq = x;
        OK
    })
}

fn b_stop(c: &mut Cpu) -> u32 {
    with_voice(c, |v, _| {
        v.playing = false;
        OK
    })
}

fn b_query_interface(c: &mut Cpu) -> u32 {
    let id = mem().rd32(arg(c, 1));
    log!("warn: IDirectSoundBuffer::QueryInterface({id:#010x}) not supported");
    mem().wr32(arg(c, 2), 0);
    E_NOINTERFACE
}

static BUFFER: &[Import] = &[
    ("IDirectSoundBuffer::QueryInterface", 3, b_query_interface),
    ("IDirectSoundBuffer::AddRef", 1, b_add_ref),
    ("IDirectSoundBuffer::Release", 1, b_release),
    ("IDirectSoundBuffer::GetCaps", 2, b_get_caps),
    ("IDirectSoundBuffer::GetCurrentPosition", 3, b_get_current_position),
    ("IDirectSoundBuffer::GetFormat", 4, b_get_format),
    ("IDirectSoundBuffer::GetVolume", 2, b_get_volume),
    ("IDirectSoundBuffer::GetPan", 2, b_get_pan),
    ("IDirectSoundBuffer::GetFrequency", 2, b_get_frequency),
    ("IDirectSoundBuffer::GetStatus", 2, b_get_status),
    ("IDirectSoundBuffer::Initialize", 3, ok),
    ("IDirectSoundBuffer::Lock", 8, b_lock),
    ("IDirectSoundBuffer::Play", 4, b_play),
    ("IDirectSoundBuffer::SetCurrentPosition", 2, b_set_current_position),
    ("IDirectSoundBuffer::SetFormat", 2, b_set_format),
    ("IDirectSoundBuffer::SetVolume", 2, b_set_volume),
    ("IDirectSoundBuffer::SetPan", 2, b_set_pan),
    ("IDirectSoundBuffer::SetFrequency", 2, b_set_frequency),
    ("IDirectSoundBuffer::Stop", 1, b_stop),
    ("IDirectSoundBuffer::Unlock", 5, ok),
    ("IDirectSoundBuffer::Restore", 1, ok),
];

// ---- PlaySoundA -----------------------------------------------------------------------------------

/// A RIFF/WAVE file at `p` (`n` bytes) as a voice.
fn parse_wave(p: u32, n: u32) -> Option<Voice> {
    let m = mem();
    if n < 12 || m.slice(p, 4) != b"RIFF" || m.slice(p + 8, 4) != b"WAVE" {
        return None;
    }
    let (mut at, end) = (p + 12, p + n);
    let (mut fmt, mut data) = (None, None);
    while at + 8 <= end {
        let (id, len) = (m.slice(at, 4).to_vec(), m.rd32(at + 4));
        match &id[..] {
            b"fmt " => fmt = Some(Format::read(at + 8)),
            b"data" => data = Some((at + 8, len.min(end - at - 8))),
            _ => {}
        }
        at += 8 + ((len + 1) & !1);
    }
    let (fmt, (data, bytes)) = (fmt?, data?);
    Some(Voice { data, bytes, fmt, freq: 0, pos: 0.0, playing: true, looping: false, volume: 0, pan: 0, primary: false, refs: 1 })
}

/// PlaySoundA(sound, module, flags): a WAVE resource, a file or a sound in memory.
fn play_sound(c: &mut Cpu) -> u32 {
    let (s, flags) = (arg(c, 0), arg(c, 2));
    const SND_ASYNC: u32 = 1;
    const SND_MEMORY: u32 = 4;
    const SND_LOOP: u32 = 8;
    const SND_NOSTOP: u32 = 0x10;
    const SND_FILENAME: u32 = 0x2_0000;
    const SND_RESOURCE: u32 = 0x4_0004;
    if s == 0 {
        mixer().wave = None;
        return 1;
    }
    if flags & SND_NOSTOP != 0 && mixer().wave.is_some() {
        return 0;
    }
    let found = if flags & SND_RESOURCE == SND_RESOURCE {
        resource(ResId::Name("WAVE"), ResId::from_guest(s))
    } else if flags & SND_MEMORY != 0 {
        Some((s, 1 << 24))
    } else if flags & SND_FILENAME != 0 {
        std::fs::read(host_path(&cstr(s))).ok().map(|b| {
            let p = alloc(b.len() as u32);
            mem().slice(p, b.len()).copy_from_slice(&b);
            (p, b.len() as u32)
        })
    } else {
        None
    };
    let Some(mut v) = found.and_then(|(p, n)| parse_wave(p, n)) else {
        log!("warn: PlaySoundA({s:#x}, flags {flags:#x}): sound not found");
        return 0;
    };
    v.looping = flags & SND_LOOP != 0;
    let secs = v.frames() as f64 / v.fmt.rate as f64;
    mixer().wave = Some(v);
    if flags & SND_ASYNC == 0 && flags & SND_LOOP == 0 {
        std::thread::sleep(Duration::from_secs_f64(secs));
    }
    1
}

pub const IMPORTS: &[Import] = &[
    ("DirectSoundCreate", 3, direct_sound_create),
    ("#1", 3, direct_sound_create), // DirectSoundCreate by ordinal
    ("DirectSoundEnumerateA", 2, direct_sound_enumerate),
    ("PlaySoundA", 3, play_sound),
];
