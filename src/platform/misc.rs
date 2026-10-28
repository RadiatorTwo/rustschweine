//! The small DLLs: WINMM timers, ole32 (COM start-up, CoCreateInstance), COMCTL32.

use super::{Import, arg, guest_call, mem};
use crate::rt::Cpu;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn time_get_time(_: &mut Cpu) -> u32 {
    super::kernel32::ms_since_start()
}

fn time_get_dev_caps(c: &mut Cpu) -> u32 {
    let p = arg(c, 0);
    mem().wr32(p, 1);
    mem().wr32(p + 4, 1_000_000);
    0
}

static TIMERS: Mutex<BTreeMap<u32, Arc<AtomicBool>>> = Mutex::new(BTreeMap::new());

/// timeSetEvent(delay, resolution, proc, user, flags): proc(id, 0, user, 0, 0) runs on its own
/// thread, once or (TIME_PERIODIC) every `delay` ms.
fn time_set_event(c: &mut Cpu) -> u32 {
    let (delay, proc_, user, flags) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 4));
    let run = Arc::new(AtomicBool::new(true));
    let id = {
        let mut t = TIMERS.lock().unwrap();
        let id = t.keys().next_back().map_or(1, |k| k + 1);
        t.insert(id, run.clone());
        id
    };
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            let mut tc = super::new_thread_cpu();
            let start = Instant::now();
            let mut n = 1u64;
            loop {
                let due = start + Duration::from_millis(delay.max(1) as u64 * n);
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                if !run.load(Ordering::Relaxed) {
                    break;
                }
                guest_call(&mut tc, proc_, &[id, 0, user, 0, 0]);
                if flags & 1 == 0 {
                    break;
                }
                n += 1;
            }
        })
        .expect("cannot start a timer thread");
    id
}

fn time_kill_event(c: &mut Cpu) -> u32 {
    match TIMERS.lock().unwrap().remove(&arg(c, 0)) {
        Some(r) => {
            r.store(false, Ordering::Relaxed);
            0
        }
        None => 97, // MMSYSERR_INVALPARAM
    }
}

fn ret0(_: &mut Cpu) -> u32 {
    0
}

/// CoCreateInstance(clsid, outer, context, iid, out): DirectPlay and its lobby.
fn co_create_instance(c: &mut Cpu) -> u32 {
    let (clsid, iid, out) = (arg(c, 0), arg(c, 3), arg(c, 4));
    let m = mem();
    let id: [u8; 16] = m.slice(clsid, 16).try_into().unwrap();
    if id == super::dplay::CLSID_DIRECTPLAY {
        return super::dplay::create(iid, out);
    }
    if id == super::dplay::CLSID_DIRECTPLAYLOBBY {
        return super::dplay::create_lobby(out);
    }
    let guid = format!("{:08x}-{:04x}-{:04x}", m.rd32(clsid), m.rd16(clsid + 4), m.rd16(clsid + 6));
    super::log!("CoCreateInstance({guid}): class not registered");
    if out != 0 {
        m.wr32(out, 0);
    }
    0x8004_0154 // REGDB_E_CLASSNOTREG
}

pub const IMPORTS: &[Import] = &[
    ("timeGetTime", 0, time_get_time),
    ("timeGetDevCaps", 2, time_get_dev_caps),
    ("timeBeginPeriod", 1, ret0),
    ("timeEndPeriod", 1, ret0),
    ("timeSetEvent", 5, time_set_event),
    ("timeKillEvent", 1, time_kill_event),
    ("CoInitialize", 1, ret0),
    ("CoUninitialize", 0, ret0),
    ("CoCreateInstance", 5, co_create_instance),
    ("#17", 0, ret0), // InitCommonControls
];
