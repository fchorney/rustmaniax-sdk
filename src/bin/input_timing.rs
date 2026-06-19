//! USB input rate probe.
//!
//! Verifies how fast input reports actually reach the SDK now that reads are
//! interrupt-driven (each pad's poll thread blocks on the device and wakes the
//! instant a report arrives). Runs in all-packets mode so every Report 3 fires
//! an event, then reports per-pad arrival rate and an inter-arrival histogram.
//!
//! Full Speed USB delivers one report per 1ms frame, so a healthy connected pad
//! streams at ~1000 reports/sec and the histogram clusters at ~1ms. That 1ms
//! frame is the precision floor regardless of firmware sampling rate.
//!
//! Run: cargo run --features sample --bin smx-input-timing

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType};
use rustmaniax_sdk::{SmxEvent, SmxManager};

// 100us buckets covering 0..20ms. The TUI shows a window; the session summary
// dumps the full populated range.
const BUCKET_US: u64 = 100;
const NUM_BUCKETS: usize = 200;
const TUI_MAX_BUCKETS: usize = 50; // 0..5ms visible in the live view
const MIN_SHOW_BUCKETS: usize = 20; // always show at least 0..2ms
const BAR_WIDTH: usize = 28;

struct PadStats {
    connected: bool,
    reports: u64,
    last_ts: Option<Instant>,
    // Inter-arrival interval distribution.
    buckets: [u64; NUM_BUCKETS],
    overflow: u64,
    min_us: u64,
    max_us: u64,
    sum_us: u64,
    interval_count: u64,
    // Rolling rate window.
    window_start: Instant,
    window_reports: u64,
    rate_hz: f64,
}

impl PadStats {
    fn new() -> Self {
        Self {
            connected: false,
            reports: 0,
            last_ts: None,
            buckets: [0; NUM_BUCKETS],
            overflow: 0,
            min_us: u64::MAX,
            max_us: 0,
            sum_us: 0,
            interval_count: 0,
            window_start: Instant::now(),
            window_reports: 0,
            rate_hz: 0.0,
        }
    }

    fn reset(&mut self) {
        let connected = self.connected;
        *self = Self::new();
        self.connected = connected;
    }

    fn on_report(&mut self) {
        let now = Instant::now();
        self.reports += 1;
        self.window_reports += 1;
        if let Some(prev) = self.last_ts {
            let us = now.duration_since(prev).as_micros() as u64;
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
        self.last_ts = Some(now);

        // Refresh the rolling rate roughly twice a second.
        let elapsed = now.duration_since(self.window_start).as_secs_f64();
        if elapsed >= 0.5 {
            self.rate_hz = self.window_reports as f64 / elapsed;
            self.window_start = now;
            self.window_reports = 0;
        }
    }

    fn mean_us(&self) -> Option<f64> {
        (self.interval_count > 0).then(|| self.sum_us as f64 / self.interval_count as f64)
    }

    // How many buckets to show in the TUI: enough to cover populated data plus a
    // small margin, clamped to [MIN_SHOW_BUCKETS, TUI_MAX_BUCKETS].
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
            "  {:5.2}ms [{:7}] {}\r",
            ms, count,
            "█".repeat(bar_len)
        )
        .ok();
    }
    let threshold_ms = show as f64 * BUCKET_US as f64 / 1_000.0;
    let overflow: u64 = p.buckets[show..].iter().sum::<u64>() + p.overflow;
    if overflow > 0 {
        writeln!(stdout, "  >{:.2}ms [{:7}]\r", threshold_ms, overflow).ok();
    }
}

fn print_pad(stdout: &mut impl Write, pad: usize, p: &PadStats) {
    writeln!(
        stdout,
        "Pad {pad}:  {:.0} reports/sec  ({} reports total)\r",
        p.rate_hz, p.reports
    )
    .ok();

    if p.interval_count < 2 {
        writeln!(stdout, "  Waiting for input reports...\r").ok();
        writeln!(stdout, "\r").ok();
        return;
    }

    if p.min_us < u64::MAX {
        writeln!(stdout, "  Min gap  : {:6} us\r", p.min_us).ok();
    }
    if let Some(mean) = p.mean_us() {
        writeln!(stdout, "  Mean gap : {:6.0} us  ({:.0} Hz)\r", mean, 1_000_000.0 / mean).ok();
    }
    if p.max_us > 0 {
        writeln!(stdout, "  Max gap  : {:6} us\r", p.max_us).ok();
    }

    writeln!(stdout, "\r").ok();
    writeln!(
        stdout,
        "  Inter-arrival histogram ({}us buckets, showing 0..{:.1}ms):\r",
        BUCKET_US,
        p.tui_show_buckets() as f64 * BUCKET_US as f64 / 1_000.0
    )
    .ok();
    render_histogram(stdout, p, p.tui_show_buckets());
    writeln!(stdout, "\r").ok();
}

fn main() {
    env_logger::init();

    let running = Arc::new(AtomicBool::new(true));
    let stats: Arc<Mutex<[PadStats; 2]>> =
        Arc::new(Mutex::new([PadStats::new(), PadStats::new()]));

    let stats_cb = Arc::clone(&stats);
    let mgr = SmxManager::start(move |event| match event {
        SmxEvent::Connected { pad, .. } if pad < 2 => {
            stats_cb.lock().unwrap()[pad].connected = true;
        }
        SmxEvent::Disconnected { pad } if pad < 2 => {
            stats_cb.lock().unwrap()[pad] = PadStats::new();
        }
        SmxEvent::InputState { pad, .. } if pad < 2 => {
            stats_cb.lock().unwrap()[pad].on_report();
        }
        _ => {}
    })
    .expect("Failed to initialize HID");

    // Fire on every Report 3 (not just changes) so the histogram reflects the
    // raw report arrival rate, which is what we want to verify.
    mgr.set_input_state_mode(true);

    terminal::enable_raw_mode().expect("enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide).ok();

    let r = Arc::clone(&running);
    ctrlc::set_handler(move || r.store(false, Ordering::Relaxed)).ok();

    while running.load(Ordering::Relaxed) {
        while event::poll(Duration::ZERO).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                match key.code {
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

        let s = stats.lock().unwrap();

        execute!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All)).ok();
        writeln!(stdout, "SMX Input Rate Probe  [r: reset, q: quit]\r").ok();
        writeln!(
            stdout,
            "Interrupt-driven reads, all-packets mode. ~1000 reports/sec = 1ms USB frame (the floor).\r"
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

    let mut summary: Vec<String> = Vec::new();
    {
        let s = stats.lock().unwrap();
        for pad in 0..2 {
            let p = &s[pad];
            if !p.connected || p.reports == 0 {
                continue;
            }
            summary.push(format!(
                "Pad {pad}:  {:.0} reports/sec ({} reports total)",
                p.rate_hz, p.reports
            ));
            if p.min_us < u64::MAX {
                summary.push(format!("  Min gap  : {} us", p.min_us));
            }
            if let Some(mean) = p.mean_us() {
                summary.push(format!("  Mean gap : {mean:.0} us ({:.0} Hz)", 1_000_000.0 / mean));
            }
            if p.max_us > 0 {
                summary.push(format!("  Max gap  : {} us", p.max_us));
            }
            let show = p.summary_show_buckets();
            summary.push(format!("  Inter-arrival histogram ({}us buckets):", BUCKET_US));
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

    println!("--- Session summary ---");
    for line in &summary {
        println!("{line}");
    }
}
