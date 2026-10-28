//! WINMM beyond the timers (`misc`): mmio, the RIFF file reader the game loads its WAV files
//! with. The mixer (CD volume) and MCI (CD audio) are `cdaudio.rs`.

use super::{Import, alloc, arg, cstr, host_path, log, mem};
use crate::rt::Cpu;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Mutex, MutexGuard};

const MMSYSERR_INVALHANDLE: u32 = 5;
const MMIOERR_CHUNKNOTFOUND: u32 = 265;

const BUF_SIZE: u32 = 0x1_0000;

struct File {
    f: std::fs::File,
    /// the guest buffer of mmioGetInfo, and the file position its first byte came from
    buf: u32,
    buf_at: u64,
}

static FILES: Mutex<BTreeMap<u32, File>> = Mutex::new(BTreeMap::new());

fn files() -> MutexGuard<'static, BTreeMap<u32, File>> {
    FILES.lock().unwrap_or_else(|e| e.into_inner())
}

fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

/// mmioOpenA(name, info, flags): reading only.
fn mmio_open(c: &mut Cpu) -> u32 {
    let (name, flags) = (cstr(arg(c, 0)), arg(c, 2));
    if flags & 3 != 0 {
        log!("warn: mmioOpenA({name}) for writing not supported");
        return 0;
    }
    let Ok(f) = std::fs::File::open(host_path(&name)) else {
        if super::trace_api() {
            log!("mmioOpenA {name}: not found");
        }
        return 0;
    };
    let mut t = files();
    let h = t.keys().next_back().map_or(0x4d00, |k| k + 4);
    t.insert(h, File { f, buf: 0, buf_at: 0 });
    h
}

fn mmio_close(c: &mut Cpu) -> u32 {
    match files().remove(&arg(c, 0)) {
        Some(f) => {
            if f.buf != 0 {
                super::free(f.buf);
            }
            0
        }
        None => MMSYSERR_INVALHANDLE,
    }
}

fn read_into(f: &mut std::fs::File, dst: &mut [u8]) -> usize {
    let mut got = 0;
    while got < dst.len() {
        match f.read(&mut dst[got..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => got += k,
        }
    }
    got
}

fn mmio_read(c: &mut Cpu) -> u32 {
    let (h, buf, n) = (arg(c, 0), arg(c, 1), arg(c, 2));
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return 0xffff_ffff };
    read_into(&mut f.f, mem().slice(buf, n as usize)) as u32
}

fn mmio_seek(c: &mut Cpu) -> u32 {
    let (h, off, origin) = (arg(c, 0), arg(c, 1) as i32 as i64, arg(c, 2));
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return 0xffff_ffff };
    let to = match origin {
        0 => SeekFrom::Start(off.max(0) as u64),
        1 => SeekFrom::Current(off),
        _ => SeekFrom::End(off),
    };
    f.f.seek(to).map_or(0xffff_ffff, |p| p as u32)
}

/// MMCKINFO: ckid, cksize, fccType, dwDataOffset, dwFlags
fn read_chunk_header(f: &mut std::fs::File) -> Option<(u32, u32, u64)> {
    let mut h = [0u8; 8];
    (read_into(f, &mut h) == 8).then_some(())?;
    let pos = f.stream_position().ok()?;
    Some((u32::from_le_bytes(h[..4].try_into().unwrap()), u32::from_le_bytes(h[4..].try_into().unwrap()), pos))
}

/// mmioDescend(h, chunk, parent, flags): into the next chunk, or the next one of the id (or
/// RIFF/LIST type) asked for, before the parent's end.
fn mmio_descend(c: &mut Cpu) -> u32 {
    let (h, ck, parent, flags) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
    let m = mem();
    let end = if parent != 0 { m.rd32(parent + 12) as u64 + m.rd32(parent + 4) as u64 } else { u64::MAX };
    let (want_id, want_type) = match flags & 0x70 {
        0x10 => (Some(m.rd32(ck)), None),
        0x20 => (Some(fourcc(b"RIFF")), Some(m.rd32(ck + 8))),
        0x40 => (Some(fourcc(b"LIST")), Some(m.rd32(ck + 8))),
        _ => (None, None),
    };
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return MMSYSERR_INVALHANDLE };
    loop {
        let Some((id, size, data)) = read_chunk_header(&mut f.f) else { return MMIOERR_CHUNKNOTFOUND };
        if data > end {
            return MMIOERR_CHUNKNOTFOUND;
        }
        let list = id == fourcc(b"RIFF") || id == fourcc(b"LIST");
        let mut ty = 0;
        if list {
            let mut b = [0u8; 4];
            read_into(&mut f.f, &mut b);
            ty = u32::from_le_bytes(b);
        }
        if want_id.is_none_or(|w| w == id) && want_type.is_none_or(|w| w == ty) {
            m.wr32(ck, id);
            m.wr32(ck + 4, size);
            m.wr32(ck + 8, ty);
            m.wr32(ck + 12, data as u32);
            m.wr32(ck + 16, 0);
            return 0;
        }
        let next = data + size as u64 + (size & 1) as u64;
        if f.f.seek(SeekFrom::Start(next)).is_err() {
            return MMIOERR_CHUNKNOTFOUND;
        }
    }
}

/// mmioAscend(h, chunk, flags): to the end of the chunk (padded to a word).
fn mmio_ascend(c: &mut Cpu) -> u32 {
    let (h, ck) = (arg(c, 0), arg(c, 1));
    let (size, data) = (mem().rd32(ck + 4), mem().rd32(ck + 12));
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return MMSYSERR_INVALHANDLE };
    let _ = f.f.seek(SeekFrom::Start(data as u64 + size as u64 + (size & 1) as u64));
    0
}

// MMIOINFO offsets
const CCH_BUFFER: u32 = 20;
const PCH_BUFFER: u32 = 24;
const PCH_NEXT: u32 = 28;
const PCH_END_READ: u32 = 32;
const PCH_END_WRITE: u32 = 36;
const BUF_OFFSET: u32 = 40;
const DISK_OFFSET: u32 = 44;
const HMMIO: u32 = 68;

/// Fill the file's buffer from file position `at` and describe it in the MMIOINFO at `info`.
fn fill(f: &mut File, h: u32, info: u32, at: u64) {
    let m = mem();
    if f.buf == 0 {
        f.buf = alloc(BUF_SIZE);
    }
    let _ = f.f.seek(SeekFrom::Start(at));
    let n = read_into(&mut f.f, m.slice(f.buf, BUF_SIZE as usize)) as u32;
    f.buf_at = at;
    m.slice(info, 72).fill(0);
    m.wr32(info + CCH_BUFFER, BUF_SIZE);
    m.wr32(info + PCH_BUFFER, f.buf);
    m.wr32(info + PCH_NEXT, f.buf);
    m.wr32(info + PCH_END_READ, f.buf + n);
    m.wr32(info + PCH_END_WRITE, f.buf + BUF_SIZE);
    m.wr32(info + BUF_OFFSET, at as u32);
    m.wr32(info + DISK_OFFSET, (at + n as u64) as u32);
    m.wr32(info + HMMIO, h);
}

/// mmioGetInfo(h, info, flags): the buffer, filled from the current position.
fn mmio_get_info(c: &mut Cpu) -> u32 {
    let (h, info) = (arg(c, 0), arg(c, 1));
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return MMSYSERR_INVALHANDLE };
    let at = f.f.stream_position().unwrap_or(0);
    fill(f, h, info, at);
    0
}

/// Where the game has read up to in the buffer, as a file position.
fn consumed(f: &File, info: u32) -> u64 {
    f.buf_at + mem().rd32(info + PCH_NEXT).saturating_sub(f.buf) as u64
}

/// mmioAdvance(h, info, flags): the next part of the file into the buffer.
fn mmio_advance(c: &mut Cpu) -> u32 {
    let (h, info) = (arg(c, 0), arg(c, 1));
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return MMSYSERR_INVALHANDLE };
    let at = consumed(f, info);
    fill(f, h, info, at);
    0
}

/// mmioSetInfo(h, info, flags): the file position follows what was read from the buffer.
fn mmio_set_info(c: &mut Cpu) -> u32 {
    let (h, info) = (arg(c, 0), arg(c, 1));
    let mut t = files();
    let Some(f) = t.get_mut(&h) else { return MMSYSERR_INVALHANDLE };
    let at = consumed(f, info);
    let _ = f.f.seek(SeekFrom::Start(at));
    0
}



pub const IMPORTS: &[Import] = &[
    ("mmioOpenA", 3, mmio_open),
    ("mmioClose", 2, mmio_close),
    ("mmioRead", 3, mmio_read),
    ("mmioSeek", 3, mmio_seek),
    ("mmioDescend", 4, mmio_descend),
    ("mmioAscend", 3, mmio_ascend),
    ("mmioGetInfo", 3, mmio_get_info),
    ("mmioSetInfo", 3, mmio_set_info),
    ("mmioAdvance", 3, mmio_advance),
    ("mixerGetNumDevs", 0, super::cdaudio::mixer_get_num_devs),
    ("mixerOpen", 5, super::cdaudio::mixer_open),
    ("mixerClose", 1, super::cdaudio::mixer_close),
    ("mixerGetDevCapsA", 3, super::cdaudio::mixer_get_dev_caps),
    ("mixerGetLineInfoA", 3, super::cdaudio::mixer_get_line_info),
    ("mixerGetLineControlsA", 3, super::cdaudio::mixer_get_line_controls),
    ("mixerGetControlDetailsA", 3, super::cdaudio::mixer_get_control_details),
    ("mixerSetControlDetails", 3, super::cdaudio::mixer_set_control_details),
    ("mciSendCommandA", 4, super::cdaudio::mci_send_command),
];
