//! `HOGS_SAMPLE=1`: a sampling profiler for finding what is slow. A thread stops the game's
//! main thread 1000 times a second, reads where it is (its host instruction pointer), and
//! counts the recompiled guest function there, or the import being run, or the host helper (else
//! the module it is in and the first of our functions found on its stack: a driver, ntdll waiting);
//! the top of the list is logged every 2 s.

use super::log;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The import being run (its thunk index + 1), 0 in guest code.
pub static IN_IMPORT: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
pub fn start() {
    use windows_sys::Win32::System::Diagnostics::Debug::{CONTEXT, GetThreadContext};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT, THREAD_SUSPEND_RESUME,
    };
    if std::env::var_os("HOGS_SAMPLE").is_none() {
        return;
    }
    let h = unsafe { OpenThread(THREAD_GET_CONTEXT | THREAD_SUSPEND_RESUME, 0, GetCurrentThreadId()) } as usize;
    std::thread::spawn(move || {
        let Some(d) = crate::rt::DISPATCH.get() else { return };
        let mut fns: Vec<(usize, u32)> = d.table.iter().map(|&(g, p)| (p, g)).collect();
        fns.sort_unstable();
        let lo = fns.first().map_or(0, |f| f.0);
        let hi = fns.last().map_or(0, |f| f.0) + 0x10000;
        let helpers: Vec<(usize, &str)> = {
            use crate::rt::*;
            let mut v = vec![
                (rt_lookup as *const () as usize, "rt_lookup"),
                (rt_x87 as *const () as usize, "rt_x87"),
                (rt_fpu_round as *const () as usize, "rt_fpu_round"),
                (rt_string as *const () as usize, "rt_string"),
                (rt_hook as *const () as usize, "rt_hook"),
                (rt_yield as *const () as usize, "rt_yield"),
            ];
            v.sort_unstable();
            v
        };
        let mut counts: HashMap<String, u32> = HashMap::new();
        let mut t = std::time::Instant::now();
        let mut total = 0u32;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1));
            let mut ctx: CONTEXT = unsafe { std::mem::zeroed() };
            ctx.ContextFlags = 0x0010_0001; // CONTEXT_CONTROL (AMD64)
            let rip = unsafe {
                if SuspendThread(h as _) == u32::MAX {
                    return;
                }
                let ok = GetThreadContext(h as _, &mut ctx);
                let imp = IN_IMPORT.load(Ordering::Relaxed);
                ResumeThread(h as _);
                if ok == 0 {
                    continue;
                }
                (ctx.Rip as usize, imp)
            };
            let stack = ctx.Rsp as usize;
            let key = if rip.1 != 0 {
                format!("import {}", super::thunk_name(rip.1 - 1))
            } else if (lo..hi).contains(&rip.0) {
                let k = fns.partition_point(|f| f.0 <= rip.0);
                let g = fns[k.saturating_sub(1)].1;
                format!("guest {}", crate::image::named(g))
            } else if let Some(&(a, n)) = helpers.iter().rev().find(|h| h.0 <= rip.0).filter(|h| rip.0 - h.0 < 0x4000) {
                let _ = a;
                format!("host {n}")
            } else {
                format!("host other {}", outside(rip.0, stack))
            };
            *counts.entry(key).or_default() += 1;
            total += 1;
            if t.elapsed().as_secs() >= 2 {
                let mut v: Vec<_> = counts.drain().collect();
                v.sort_by_key(|e| std::cmp::Reverse(e.1));
                log!("sample: {total} samples");
                for (k, n) in v.iter().take(25) {
                    log!("sample: {:5.1}% {k}", 100.0 * *n as f64 / total as f64);
                }
                total = 0;
                t = std::time::Instant::now();
            }
        }
    });
}

/// Where host code outside the known helpers is: its module (a driver, ntdll waiting, ...) and
/// the first of our own functions found as return addresses on the stack (a guess: any value
/// on the stack that points into our code counts).
#[cfg(windows)]
fn outside(rip: usize, rsp: usize) -> String {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW};
    let module = |a: usize| {
        let mut h = std::ptr::null_mut();
        let mut buf = [0u16; 260];
        unsafe {
            // FROM_ADDRESS | UNCHANGED_REFCOUNT
            if GetModuleHandleExW(6, a as *const u16, &mut h) == 0 {
                return "?".to_string();
            }
            let n = GetModuleFileNameW(h, buf.as_mut_ptr(), 260) as usize;
            String::from_utf16_lossy(&buf[..n]).rsplit('\\').next().unwrap_or("?").to_string()
        }
    };
    let (lo, hi) = unsafe {
        let base = GetModuleHandleW(std::ptr::null()) as usize;
        let nt = base + *((base + 0x3c) as *const u32) as usize;
        (base, base + *((nt + 0x50) as *const u32) as usize)
    };
    let mut ours = Vec::new();
    for k in 0..2048 {
        let v = unsafe { *((rsp + 8 * k) as *const usize) };
        if (lo..hi).contains(&v) {
            ours.push(symbol(v));
            ours.dedup();
            if ours.len() == 3 {
                break;
            }
        }
    }
    format!("[{}] <- {}", module(rip), ours.join(" <- "))
}

/// The name of our function at `a` (from the PDB, by dbghelp).
#[cfg(windows)]
fn symbol(a: usize) -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::{SYMBOL_INFOW, SymFromAddrW, SymInitializeW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static NAMES: std::sync::Mutex<Option<HashMap<usize, String>>> = std::sync::Mutex::new(None);
    let mut names = NAMES.lock().unwrap();
    let names = names.get_or_insert_with(|| {
        let dir: Vec<u16> = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_string_lossy().into_owned()))
            .unwrap_or_default()
            .encode_utf16()
            .chain([0])
            .collect();
        if unsafe { SymInitializeW(GetCurrentProcess(), dir.as_ptr(), 1) } == 0 {
            log!("sample: SymInitialize failed ({})", std::io::Error::last_os_error());
        }
        HashMap::new()
    });
    names
        .entry(a)
        .or_insert_with(|| unsafe {
            let mut buf = vec![0u64; (size_of::<SYMBOL_INFOW>() + 512 * 2) / 8 + 1];
            let info = buf.as_mut_ptr() as *mut SYMBOL_INFOW;
            (*info).SizeOfStruct = size_of::<SYMBOL_INFOW>() as u32;
            (*info).MaxNameLen = 500;
            let mut disp = 0u64;
            if SymFromAddrW(GetCurrentProcess(), a as u64, &mut disp, info) == 0 {
                return format!("{a:#x} ({})", std::io::Error::last_os_error());
            }
            let name = std::slice::from_raw_parts((*info).Name.as_ptr(), (*info).NameLen as usize);
            String::from_utf16_lossy(name)
        })
        .clone()
}

#[cfg(not(windows))]
pub fn start() {}
