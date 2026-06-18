//! Firmware sensor loop rate probe.
//!
//! Tracks genuine input state changes (always_fire_input OFF) and builds a
//! histogram of inter-change intervals to fingerprint the pad's sensor polling
//! period.
//!
//! Step rapidly on any panel. If the firmware samples sensors at a fixed rate
//! (e.g. every 2ms), detectable events are quantized to that grid and the
//! histogram clusters at multiples of the loop period (2ms, 4ms, 6ms, ...).
//! The minimum observed interval is an upper bound on the firmware loop period.
//!
//! Keep the USB poll sleep well below the expected firmware period (e.g. 250us)
//! so software polling jitter doesn't wash out the quantization signal.
//!
//! Run: cargo run --features sample --bin smx-input-timing [initial_sleep_us]

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType};
use rustmaniax_sdk::{SmxEvent, SmxManager};

const MIN_US: i32 = 100;
const MAX_US: i32 = 5000;
const STEP_US: i32 = 50;
const MAIN_SLEEP_MS: i32 = 50;

// Histogram config. 500us buckets covering 0..20ms, always display at least
// 0..5ms so the interesting sub-5ms region is always visible.
const BUCKET_US: u64 = 500;
const NUM_BUCKETS: usize = 40;
const MIN_SHOW_BUCKETS: usize = 10;
const BAR_WIDTH: usize = 28;

struct PadStats {
    connected: bool,
    changes: u64,
    last_ts: Option<Instant>,
    buckets: [u64; NUM_BUCKETS],
    overflow: u64,
    min_us: u64,
    max_us: u64,
    sum_us: u64,
    interval_count: u64,
}

impl PadStats {
    fn new() -> Self {
        Self {
            connected: false,
            changes: 0,
            last_ts: None,
            buckets: [0; NUM_BUCKETS],
            overflow: 0,
            min_us: u64::MAX,
            max_us: 0,
            sum_us: 0,
            interval_count: 0,
        }
    }

    fn reset(&mut self) {
        let connected = self.connected;
        *self = Self::new();
        self.connected = connected;
    }

    fn on_change(&mut self) {
        let now = Instant::now();
        self.changes += 1;
        if let Some(prev) = self.last_ts {
            let us = now.duration_since(prev).as_micros() as u64;
            let idx = (us / BUCKET_US) as usize;
            if idx < NUM_BUCKETS {
                self.buckets[idx] += 1;
            } else {
                self.overflow += 1;
            }
            if us < self.min_us {
                self.min_us = us;
            }
            if us > self.max_us {
                self.max_us = us;
            }
            self.sum_us += us;
            self.interval_count += 1;
        }
        self.last_ts = Some(now);
    }

    fn mean_us(&self) -> Option<f64> {
        (self.interval_count > 0).then(|| self.sum_us as f64 / self.interval_count as f64)
    }

    // How many buckets to display: enough to cover all populated data plus a
    // small margin, but always at least MIN_SHOW_BUCKETS.
    fn show_buckets(&self) -> usize {
        let last = self.buckets.iter().rposition(|&c| c > 0).unwrap_or(0);
        (last + 3).max(MIN_SHOW_BUCKETS).min(NUM_BUCKETS)
    }
}

fn render_histogram(stdout: &mut impl Write, p: &PadStats) {
    let show = p.show_buckets();
    let max_count = p.buckets[..show].iter().copied().max().unwrap_or(1).max(1);

    for i in 0..show {
        let ms = i as f64 * BUCKET_US as f64 / 1_000.0;
        let count = p.buckets[i];
        let bar_len = (count as usize * BAR_WIDTH) / max_count as usize;
        writeln!(
            stdout,
            "  {:5.1}ms [{:5}] {}\r",
            ms,
            count,
            "█".repeat(bar_len)
        )
        .ok();
    }
    let overflow_threshold_ms = show as f64 * BUCKET_US as f64 / 1_000.0;
    if p.overflow > 0 {
        writeln!(
            stdout,
            "  >{:.1}ms [{:5}]\r",
            overflow_threshold_ms, p.overflow
        )
        .ok();
    }
}

fn print_pad(stdout: &mut impl Write, pad: usize, p: &PadStats, has_data: bool) {
    writeln!(
        stdout,
        "Pad {pad}:  ({} state changes, {} intervals)\r",
        p.changes, p.interval_count
    )
    .ok();

    if !has_data {
        writeln!(stdout, "  Step rapidly on any panel to populate histogram.\r").ok();
        writeln!(stdout, "\r").ok();
        return;
    }

    if p.min_us < u64::MAX {
        let min_hz = 1_000_000.0 / p.min_us as f64;
        writeln!(
            stdout,
            "  Min gap  : {:6} us  =>  firmware loop <= ~{min_hz:.0} Hz\r",
            p.min_us
        )
        .ok();
    }
    if let Some(mean) = p.mean_us() {
        writeln!(stdout, "  Mean gap : {:6.0} us\r", mean).ok();
    }
    if p.max_us > 0 {
        writeln!(stdout, "  Max gap  : {:6} us\r", p.max_us).ok();
    }

    writeln!(stdout, "\r").ok();
    writeln!(
        stdout,
        "  Inter-change interval histogram ({}us buckets):\r",
        BUCKET_US
    )
    .ok();
    render_histogram(stdout, p);
    writeln!(stdout, "\r").ok();
}

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let initial_us: i32 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(250)
        .clamp(MIN_US, MAX_US);

    let sleep_us = Arc::new(AtomicI32::new(initial_us));
    let running = Arc::new(AtomicBool::new(true));
    let stats: Arc<Mutex<[PadStats; 2]>> =
        Arc::new(Mutex::new([PadStats::new(), PadStats::new()]));

    let stats_cb = Arc::clone(&stats);
    let mgr = SmxManager::start(move |event| match event {
        SmxEvent::Connected { pad, .. } if pad < 2 => {
            stats_cb.lock().unwrap()[pad].connected = true;
        }
        SmxEvent::Disconnected { pad } if pad < 2 => {
            let mut s = stats_cb.lock().unwrap();
            s[pad] = PadStats::new();
        }
        SmxEvent::InputState { pad, .. } if pad < 2 => {
            stats_cb.lock().unwrap()[pad].on_change();
        }
        _ => {}
    })
    .expect("Failed to initialize HID");

    // always_fire_input stays OFF -- only genuine state changes fire the callback.
    mgr.set_polling_rate(MAIN_SLEEP_MS, initial_us);

    terminal::enable_raw_mode().expect("enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide).ok();

    let r = Arc::clone(&running);
    ctrlc::set_handler(move || r.store(false, Ordering::Relaxed)).ok();

    while running.load(Ordering::Relaxed) {
        while event::poll(Duration::ZERO).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match key.code {
                    KeyCode::Up => {
                        let v = (sleep_us.load(Ordering::Relaxed) + STEP_US).min(MAX_US);
                        sleep_us.store(v, Ordering::Relaxed);
                        mgr.set_polling_rate(MAIN_SLEEP_MS, v);
                    }
                    KeyCode::Down => {
                        let v = (sleep_us.load(Ordering::Relaxed) - STEP_US).max(MIN_US);
                        sleep_us.store(v, Ordering::Relaxed);
                        mgr.set_polling_rate(MAIN_SLEEP_MS, v);
                    }
                    KeyCode::Char('r') => {
                        let mut s = stats.lock().unwrap();
                        s[0].reset();
                        s[1].reset();
                    }
                    KeyCode::Char('q') => {
                        running.store(false, Ordering::Relaxed);
                        break;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        running.store(false, Ordering::Relaxed);
                        break;
                    }
                    _ => {}
                }
            }
        }

        let us = sleep_us.load(Ordering::Relaxed);
        let s = stats.lock().unwrap();

        execute!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All)).ok();
        writeln!(stdout, "SMX Firmware Rate Probe\r").ok();
        writeln!(
            stdout,
            "USB poll sleep: {us} us  [up/down: adjust, r: reset, q: quit]\r"
        )
        .ok();
        writeln!(
            stdout,
            "Clustering at multiples of N ms = firmware sensor loop at 1000/N Hz.\r"
        )
        .ok();
        writeln!(stdout, "\r").ok();

        for pad in 0..2 {
            let p = &s[pad];
            if !p.connected {
                writeln!(stdout, "Pad {pad}: not connected\r").ok();
                writeln!(stdout, "\r").ok();
                continue;
            }
            let has_data = p.interval_count >= 2;
            print_pad(&mut stdout, pad, p, has_data);
        }

        drop(s);
        stdout.flush().ok();
        std::thread::sleep(Duration::from_millis(100));
    }

    // Collect summary before tearing down the terminal.
    let final_us = sleep_us.load(Ordering::Relaxed);
    let mut summary: Vec<String> = Vec::new();
    {
        let s = stats.lock().unwrap();
        for pad in 0..2 {
            let p = &s[pad];
            if !p.connected || p.interval_count == 0 {
                continue;
            }
            summary.push(format!(
                "Pad {pad}:  ({} state changes, {} intervals)",
                p.changes, p.interval_count
            ));
            if p.min_us < u64::MAX {
                let hz = 1_000_000.0 / p.min_us as f64;
                summary.push(format!(
                    "  Min gap  : {} us  =>  firmware loop <= ~{hz:.0} Hz",
                    p.min_us
                ));
            }
            if let Some(mean) = p.mean_us() {
                summary.push(format!("  Mean gap : {mean:.0} us"));
            }
            if p.max_us > 0 {
                summary.push(format!("  Max gap  : {} us", p.max_us));
            }
            let show = p.show_buckets();
            summary.push(format!("  Histogram ({}us buckets):", BUCKET_US));
            for i in 0..show {
                let ms = i as f64 * BUCKET_US as f64 / 1_000.0;
                summary.push(format!("    {:5.1}ms  {}", ms, p.buckets[i]));
            }
            if p.overflow > 0 {
                let t = show as f64 * BUCKET_US as f64 / 1_000.0;
                summary.push(format!("    >{t:.1}ms  {}", p.overflow));
            }
        }
    }

    execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show).ok();
    terminal::disable_raw_mode().ok();

    println!("--- Session summary (final poll sleep: {final_us} us) ---");
    for line in &summary {
        println!("{line}");
    }
}
