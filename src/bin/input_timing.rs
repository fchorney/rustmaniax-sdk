//! USB input-timing probe.
//!
//! Measures the native HID report interval from connected SMX pads -- the true
//! timing precision floor for step input. The pad firmware sends reports
//! continuously at a fixed USB-negotiated rate regardless of whether panels are
//! pressed, so this probe answers: "how finely can the system distinguish two
//! steps in time?"
//!
//! With always-fire input mode enabled, every HID report fires a callback.
//! Inter-callback gaps are split into two categories:
//!   Genuine gaps  (>= BURST_THRESHOLD_US): real inter-report intervals set by
//!                 the pad firmware / USB hardware. These determine precision.
//!   Burst gaps    (<  BURST_THRESHOLD_US): multiple packets drained from the
//!                 USB buffer in one poll() pass. Indicate that packets sat in
//!                 the buffer longer than one poll interval (latency, not rate).
//!
//! Up/Down arrows adjust the USB poll sleep (100-5000us in 50us steps) to
//! confirm that genuine report intervals are stable regardless of poll rate.
//! `r` resets stats, `q` quits and prints a session summary.
//!
//! Run: cargo run --features sample --bin smx-input-timing [initial_sleep_us]

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType};
use rustmaniax_sdk::{SmxEvent, SmxManager};

const WINDOW: usize = 200;
const MIN_US: i32 = 100;
const MAX_US: i32 = 5000;
const STEP_US: i32 = 50;
const MAIN_SLEEP_MS: i32 = 50;
// Gaps below this are burst artifacts (same poll() drain), not genuine
// inter-report intervals. Pad firmware intervals are typically >= 1ms.
const BURST_THRESHOLD_US: u64 = 200;

struct PadStats {
    connected: bool,
    total: u64,
    last_ts: Option<Instant>,

    // Genuine inter-report gaps (>= BURST_THRESHOLD_US).
    // These reflect the USB hardware report interval = timing precision.
    genuine: VecDeque<u64>,
    genuine_sum: u64,
    genuine_min_us: u64,
    genuine_max_us: u64,
    genuine_peak_hz: f64,

    // Burst tracking: packets drained back-to-back in one poll() pass.
    burst_current: u64,
    burst_max: u64,
    burst_total: u64,
    burst_size_sum: u64,
}

impl PadStats {
    fn new() -> Self {
        Self {
            connected: false,
            total: 0,
            last_ts: None,
            genuine: VecDeque::with_capacity(WINDOW + 1),
            genuine_sum: 0,
            genuine_min_us: u64::MAX,
            genuine_max_us: 0,
            genuine_peak_hz: 0.0,
            burst_current: 0,
            burst_max: 0,
            burst_total: 0,
            burst_size_sum: 0,
        }
    }

    fn reset(&mut self) {
        let connected = self.connected;
        *self = Self::new();
        self.connected = connected;
    }

    fn on_packet(&mut self) {
        let now = Instant::now();
        self.total += 1;
        if let Some(prev) = self.last_ts {
            let us = now.duration_since(prev).as_micros() as u64;
            if us >= BURST_THRESHOLD_US {
                // Genuine inter-report gap.
                self.genuine.push_back(us);
                self.genuine_sum += us;
                if self.genuine.len() > WINDOW {
                    self.genuine_sum -= self.genuine.pop_front().unwrap();
                }
                if us < self.genuine_min_us {
                    self.genuine_min_us = us;
                }
                if us > self.genuine_max_us {
                    self.genuine_max_us = us;
                }
                if let Some(hz) = self.mean_hz() {
                    if hz > self.genuine_peak_hz {
                        self.genuine_peak_hz = hz;
                    }
                }
                self.close_burst();
                self.burst_current = 1;
            } else {
                // Burst packet from the same poll() drain.
                self.burst_current += 1;
            }
        } else {
            self.burst_current = 1;
        }
        self.last_ts = Some(now);
    }

    fn close_burst(&mut self) {
        if self.burst_current > 0 {
            self.burst_total += 1;
            self.burst_size_sum += self.burst_current;
            if self.burst_current > self.burst_max {
                self.burst_max = self.burst_current;
            }
            self.burst_current = 0;
        }
    }

    fn mean_interval_us(&self) -> Option<f64> {
        (!self.genuine.is_empty())
            .then(|| self.genuine_sum as f64 / self.genuine.len() as f64)
    }

    fn mean_hz(&self) -> Option<f64> {
        self.mean_interval_us().map(|m| 1_000_000.0 / m)
    }

    fn jitter_us(&self) -> Option<u64> {
        (self.genuine_min_us < u64::MAX && self.genuine_max_us > 0)
            .then(|| self.genuine_max_us - self.genuine_min_us)
    }

    fn mean_burst(&self) -> Option<f64> {
        (self.burst_total > 0)
            .then(|| self.burst_size_sum as f64 / self.burst_total as f64)
    }
}

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let initial_us: i32 = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
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
            stats_cb.lock().unwrap()[pad].on_packet();
        }
        _ => {}
    })
    .expect("Failed to initialize HID");

    mgr.set_input_state_mode(true);
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
        let mut s = stats.lock().unwrap();
        for p in s.iter_mut() {
            p.close_burst();
        }

        execute!(stdout, cursor::MoveTo(0, 0), Clear(ClearType::All)).ok();
        writeln!(stdout, "SMX Input Timing Probe\r").ok();
        writeln!(
            stdout,
            "USB poll sleep: {us} us  [up: slower, down: faster, r: reset, q: quit]\r"
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
            writeln!(stdout, "Pad {pad}:  ({} packets total)\r", p.total).ok();

            writeln!(stdout, "\r").ok();
            writeln!(stdout, "  -- Report interval (timing precision) --\r").ok();
            if p.genuine_min_us < u64::MAX {
                let min_hz = 1_000_000.0 / p.genuine_min_us as f64;
                writeln!(
                    stdout,
                    "  Min  : {} us  (~{min_hz:.0} Hz)  <- precision floor\r",
                    p.genuine_min_us
                )
                .ok();
            } else {
                writeln!(stdout, "  Min  : waiting...\r").ok();
            }
            match (p.mean_interval_us(), p.mean_hz()) {
                (Some(mean), Some(hz)) => {
                    writeln!(
                        stdout,
                        "  Mean : {mean:.0} us  (~{hz:.1} Hz)  [{} samples]\r",
                        p.genuine.len()
                    )
                    .ok();
                }
                _ => {
                    writeln!(stdout, "  Mean : waiting...\r").ok();
                }
            }
            if p.genuine_max_us > 0 {
                let max_hz = 1_000_000.0 / p.genuine_max_us as f64;
                writeln!(
                    stdout,
                    "  Max  : {} us  (~{max_hz:.0} Hz)\r",
                    p.genuine_max_us
                )
                .ok();
            }
            if let Some(jitter) = p.jitter_us() {
                writeln!(stdout, "  Jitter (max-min): {jitter} us\r").ok();
            }
            if p.genuine_peak_hz > 0.0 {
                writeln!(stdout, "  Peak rate seen  : {:.1} Hz\r", p.genuine_peak_hz).ok();
            }

            writeln!(stdout, "\r").ok();
            writeln!(stdout, "  -- Drain bursts (poll latency indicator) --\r").ok();
            writeln!(stdout, "  Total bursts : {}\r", p.burst_total).ok();
            writeln!(
                stdout,
                "  Max burst    : {} packets  (>1 = stale backlog)\r",
                p.burst_max
            )
            .ok();
            if let Some(mean) = p.mean_burst() {
                writeln!(stdout, "  Mean burst   : {mean:.1} packets\r").ok();
            }

            if let Some(ts) = p.last_ts {
                writeln!(stdout, "\r").ok();
                writeln!(stdout, "  Last packet  : {} ms ago\r", ts.elapsed().as_millis()).ok();
            }
            writeln!(stdout, "\r").ok();
        }

        drop(s);
        stdout.flush().ok();
        std::thread::sleep(Duration::from_millis(100));
    }

    let final_us = sleep_us.load(Ordering::Relaxed);
    let mut summary: Vec<String> = Vec::new();
    {
        let mut s = stats.lock().unwrap();
        for p in s.iter_mut() {
            p.close_burst();
        }
        for pad in 0..2 {
            let p = &s[pad];
            if !p.connected || p.total == 0 {
                continue;
            }
            summary.push(format!("Pad {pad}:  ({} packets total)", p.total));
            if p.genuine_min_us < u64::MAX {
                let hz = 1_000_000.0 / p.genuine_min_us as f64;
                summary.push(format!(
                    "  Report interval min  : {} us  (~{hz:.0} Hz)  <- precision floor",
                    p.genuine_min_us
                ));
            }
            if let (Some(mean), Some(hz)) = (p.mean_interval_us(), p.mean_hz()) {
                summary.push(format!("  Report interval mean : {mean:.0} us  (~{hz:.1} Hz)"));
            }
            if p.genuine_max_us > 0 {
                let hz = 1_000_000.0 / p.genuine_max_us as f64;
                summary.push(format!(
                    "  Report interval max  : {} us  (~{hz:.0} Hz)",
                    p.genuine_max_us
                ));
            }
            if let Some(jitter) = p.jitter_us() {
                summary.push(format!("  Jitter (max-min)     : {jitter} us"));
            }
            if p.genuine_peak_hz > 0.0 {
                summary.push(format!("  Peak rate seen       : {:.1} Hz", p.genuine_peak_hz));
            }
            summary.push(format!(
                "  Max drain burst      : {} packets",
                p.burst_max
            ));
            if let Some(mean) = p.mean_burst() {
                summary.push(format!("  Mean drain burst     : {mean:.1} packets"));
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
