//! `HOGS_SAMPLE=1`: a sampling profiler for finding what is slow. A thread stops the game's
//! main thread 1000 times a second, reads where it is (its host instruction pointer), and
//! counts the recompiled guest function there, or the import being run, or the host helper;
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
                "host other".into()
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

#[cfg(not(windows))]
pub fn start() {}
