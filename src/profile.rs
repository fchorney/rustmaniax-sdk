//! Opt-in lock-hold instrumentation, enabled with `DEADSYNC_SMX_LOCK_PROFILE=1`.
//!
//! Times how long each background thread holds the shared `state` mutex, to find
//! the multi-millisecond holds that stall per-frame readers (e.g. a USB write or
//! device open performed while the lock is held). Logs a per-site summary
//! (avg / max / count over a one-second window) at `warn` so it shows at the
//! default log level. Zero cost when disabled: `hold` returns `None` and records
//! nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// A point where a background thread holds the shared state lock to do work.
#[derive(Clone, Copy)]
pub enum Site {
    UsbPoll,
    MainUpdate,
    AnimInputs,
    AnimLights,
    ConnectCheck,
    ConnectSetup,
}

const SITE_COUNT: usize = 6;
const SITE_NAMES: [&str; SITE_COUNT] = [
    "usb_poll",
    "main_update",
    "anim_inputs",
    "anim_lights",
    "connect_check",
    "connect_setup",
];

struct Bucket {
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
    count: AtomicU64,
}

impl Bucket {
    const fn new() -> Self {
        Self {
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn record(&self, ns: u64) {
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    // Average (µs), max (µs), and count over the window, resetting it.
    fn take(&self) -> (f64, f64, u64) {
        let sum = self.sum_ns.swap(0, Ordering::Relaxed);
        let max = self.max_ns.swap(0, Ordering::Relaxed);
        let count = self.count.swap(0, Ordering::Relaxed);
        let avg_us = if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64 / 1000.0
        };
        (avg_us, max as f64 / 1000.0, count)
    }
}

static BUCKETS: [Bucket; SITE_COUNT] = [
    Bucket::new(),
    Bucket::new(),
    Bucket::new(),
    Bucket::new(),
    Bucket::new(),
    Bucket::new(),
];

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("DEADSYNC_SMX_LOCK_PROFILE").is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// Times the holder's critical section. Create right after acquiring the lock;
/// it records the elapsed hold when dropped (at the end of the locked scope).
pub struct HoldGuard {
    start: Instant,
    site: usize,
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        BUCKETS[self.site].record(self.start.elapsed().as_nanos() as u64);
    }
}

/// Begin timing a lock hold. Returns `None` (and records nothing) when disabled.
pub fn hold(site: Site) -> Option<HoldGuard> {
    enabled().then(|| HoldGuard {
        start: Instant::now(),
        site: site as usize,
    })
}

/// Log the per-site summary once a second. Call from a periodic spot (e.g. the
/// main loop). Cheap no-op when disabled.
pub fn maybe_report() {
    if !enabled() {
        return;
    }
    static LAST: OnceLock<Mutex<Instant>> = OnceLock::new();
    let clock = LAST.get_or_init(|| Mutex::new(Instant::now()));
    let mut last = clock.lock().unwrap();
    if last.elapsed().as_secs_f32() < 1.0 {
        return;
    }
    *last = Instant::now();
    drop(last);

    let mut line = String::from("smx-lock-hold:");
    let mut any = false;
    for (i, name) in SITE_NAMES.iter().enumerate() {
        let (avg_us, max_us, n) = BUCKETS[i].take();
        if n == 0 {
            continue;
        }
        any = true;
        line.push_str(&format!(" {name}[avg={avg_us:.1}us max={max_us:.1}us n={n}]"));
    }
    if any {
        log::warn!("{line}");
    }
}
