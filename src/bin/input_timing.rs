//! USB input timing probe.
//!
//! Tracks genuine input state changes and histograms inter-change intervals.
//!
//! Any gap below BURST_THRESHOLD_US is a drain artifact: two USB reports from
//! back-to-back 1ms frames that were buffered by the OS and drained together,
//! making them appear microseconds apart instead of ~1ms apart. These are
//! counted separately and excluded from the histogram.
//!
//! Genuine gaps cluster at the USB frame boundary (~1ms) and human step
//! timing above that. Since Full Speed USB delivers one report per 1ms frame,
//! 1ms is the absolute precision floor regardless of firmware sampling rate.
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

// Gaps below this are OS drain artifacts (two buffered reports read back-to-back),
// not genuine inter-event intervals. Full Speed USB frames are 1ms so no real
// gap can be shorter than that. 1100us gives headroom for OS HID delivery jitter
// that can make a ~1ms frame appear slightly under 1ms at the software layer.
const BURST_THRESHOLD_US: u64 = 1100;

// 100us buckets covering 0..20ms. TUI shows at most TUI_MAX_BUCKETS rows;
// the full 200-bucket table goes in the session summary.
const BUCKET_US: u64 = 100;
const NUM_BUCKETS: usize = 200;
const TUI_MAX_BUCKETS: usize = 50; // 0..5ms visible in the live view
const MIN_SHOW_BUCKETS: usize = 20; // always show at least 0..2ms
const BAR_WIDTH: usize = 28;

struct PadStats {
    connected: bool,
    changes: u64,
    last_ts: Option<Instant>,
    // Genuine intervals (>= BURST_THRESHOLD_US).
    buckets: [u64; NUM_BUCKETS],
    overflow: u64,
    min_us: u64,
    max_us: u64,
    sum_us: u64,
    interval_count: u64,
    // Drain artifacts (< BURST_THRESHOLD_US).
    burst_count: u64,
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
            burst_count: 0,
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
            if us < BURST_THRESHOLD_US {
                self.burst_count += 1;
            } else {
                let idx = (us / BUCKET_US) as usize;
                if idx < NUM_BUCKETS {
                    self.buckets[idx] += 1;
                } else {
                    self.overflow += 1;
                }
                if us < self.min_us { self.min_us = us; }
                if us > self.max_us { self.max_us = us; }
                self.sum_us += us;
                self.interval_count += 1;
            }
        }
        self.last_ts = Some(now);
    }

    fn mean_us(&self) -> Option<f64> {
        (self.interval_count > 0).then(|| self.sum_us as f64 / self.interval_count as f64)
    }

    // How many buckets to show in the TUI: enough to cover all populated data
    // plus a small margin, clamped to [MIN_SHOW_BUCKETS, TUI_MAX_BUCKETS].
    fn tui_show_buckets(&self) -> usize {
        let last = self.buckets[..TUI_MAX_BUCKETS]
            .iter()
            .rposition(|&c| c > 0)
            .unwrap_or(0);
        (last + 3).max(MIN_SHOW_BUCKETS).min(TUI_MAX_BUCKETS)
    }

    // Last populated bucket across the full table (for the session summary).
    fn summary_show_buckets(&self) -> usize {
        let last = self.buckets.iter().rposition(|&c| c > 0).unwrap_or(0);
        (last + 2).min(NUM_BUCKETS)
    }
}

fn render_histogram(stdout: &mut impl Write, p: &PadStats, show: usize) {
    let max_count = p.buckets[..show].iter().copied().max().unwrap_or(1).max(1);
    for i in 0..show {
        let ms = i as f64 * BUCKET_US as f64 / 1_000.0;
        let count = p.buckets[i];
        let bar_len = (count as usize * BAR_WIDTH) / max_count as usize;
        writeln!(
            stdout,
            "  {:5.2}ms [{:5}] {}\r",
            ms, count,
            "█".repeat(bar_len)
        )
        .ok();
    }
    let threshold_ms = show as f64 * BUCKET_US as f64 / 1_000.0;
    let overflow: u64 = p.buckets[show..].iter().sum::<u64>() + p.overflow;
    if overflow > 0 {
        writeln!(stdout, "  >{:.2}ms [{:5}]\r", threshold_ms, overflow).ok();
    }
}

fn print_pad(stdout: &mut impl Write, pad: usize, p: &PadStats) {
    writeln!(
        stdout,
        "Pad {pad}:  ({} state changes, {} genuine intervals, {} drain artifacts filtered)\r",
        p.changes, p.interval_count, p.burst_count
    )
    .ok();

    if p.interval_count < 2 {
        writeln!(stdout, "  Step rapidly on any panel to populate histogram.\r").ok();
        writeln!(stdout, "\r").ok();
        return;
    }

    if p.min_us < u64::MAX {
        writeln!(stdout, "  Min gap  : {:6} us\r", p.min_us).ok();
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
        "  Histogram ({}us buckets, showing 0..{:.1}ms):\r",
        BUCKET_US,
        p.tui_show_buckets() as f64 * BUCKET_US as f64 / 1_000.0
    )
    .ok();
    render_histogram(stdout, p, p.tui_show_buckets());
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
        writeln!(stdout, "SMX Input Timing Probe\r").ok();
        writeln!(
            stdout,
            "USB poll sleep: {us} us  [up/down: adjust, r: reset, q: quit]\r"
        )
        .ok();
        writeln!(
            stdout,
            "Precision floor: ~1ms (Full Speed USB frame). Gaps < {}us are drain artifacts.\r",
            BURST_THRESHOLD_US
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
            print_pad(&mut stdout, pad, p);
        }

        drop(s);
        stdout.flush().ok();
        std::thread::sleep(Duration::from_millis(100));
    }

    let final_us = sleep_us.load(Ordering::Relaxed);
    let mut summary: Vec<String> = Vec::new();
    {
        let s = stats.lock().unwrap();
        for pad in 0..2 {
            let p = &s[pad];
            if !p.connected || p.changes == 0 {
                continue;
            }
            summary.push(format!(
                "Pad {pad}:  ({} changes, {} genuine, {} drain artifacts)",
                p.changes, p.interval_count, p.burst_count
            ));
            if p.min_us < u64::MAX {
                summary.push(format!("  Min gap  : {} us", p.min_us));
            }
            if let Some(mean) = p.mean_us() {
                summary.push(format!("  Mean gap : {mean:.0} us"));
            }
            if p.max_us > 0 {
                summary.push(format!("  Max gap  : {} us", p.max_us));
            }
            let show = p.summary_show_buckets();
            summary.push(format!("  Histogram ({}us buckets):", BUCKET_US));
            for i in 0..show {
                if p.buckets[i] > 0 {
                    let ms = i as f64 * BUCKET_US as f64 / 1_000.0;
                    summary.push(format!("    {:6.2}ms  {}", ms, p.buckets[i]));
                }
            }
            if p.overflow > 0 {
                let t = show as f64 * BUCKET_US as f64 / 1_000.0;
                summary.push(format!("    >{t:.2}ms  {}", p.overflow));
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
