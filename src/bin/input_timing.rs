//! USB input-timing probe.
//!
//! Enables always-fire input mode on every connected pad and measures
//! inter-packet arrival time so you can see the effective input rate. Up/Down
//! arrow keys adjust the USB poll sleep timer live; `r` resets the rolling
//! stats; `q` quits.
//!
//! CPU note: the USB poll thread does one non-blocking HID read per sleep
//! interval. At 100us (~10k wakes/sec) you will see measurable single-core
//! CPU use on that thread. The game/main thread is unaffected because no lock
//! is held across the sleep.
//!
//! Burst detection: two callbacks within BURST_THRESHOLD_US of each other are
//! counted as one drain burst (the poll loop drained multiple queued packets in
//! a single pass). A large max burst means packets are piling up between polls,
//! which adds latency even if the mean rate looks healthy.
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
// Packets arriving within this threshold are counted as the same drain burst.
// Genuine inter-packet gaps from the pad firmware are several hundred us at
// minimum; back-to-back reads in one poll() pass are typically under 10us.
const BURST_THRESHOLD_US: u64 = 50;

struct PadStats {
    connected: bool,
    total: u64,
    last_ts: Option<Instant>,
    window: VecDeque<u64>,
    window_sum: u64,
    all_min_us: u64,
    all_max_us: u64,
    // Peak rate tracking: best mean Hz seen over any rolling window snapshot.
    peak_rate_hz: f64,
    // Burst tracking.
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
            window: VecDeque::with_capacity(WINDOW + 1),
            window_sum: 0,
            all_min_us: u64::MAX,
            all_max_us: 0,
            peak_rate_hz: 0.0,
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
            self.window.push_back(us);
            self.window_sum += us;
            if self.window.len() > WINDOW {
                self.window_sum -= self.window.pop_front().unwrap();
            }
            if us < self.all_min_us {
                self.all_min_us = us;
            }
            if us > self.all_max_us {
                self.all_max_us = us;
            }

            // Update peak rate from the current rolling window.
            if let Some(hz) = self.rate_hz() {
                if hz > self.peak_rate_hz {
                    self.peak_rate_hz = hz;
                }
            }

            if us < BURST_THRESHOLD_US {
                self.burst_current += 1;
            } else {
                self.close_burst();
                self.burst_current = 1;
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

    fn mean_us(&self) -> Option<f64> {
        (!self.window.is_empty())
            .then(|| self.window_sum as f64 / self.window.len() as f64)
    }

    fn rate_hz(&self) -> Option<f64> {
        self.mean_us().map(|m| 1_000_000.0 / m)
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

        // Close any in-progress burst before reading it for display so the
        // count is current rather than lagging until the next packet arrives.
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
            writeln!(stdout, "Pad {pad}:\r").ok();
            writeln!(stdout, "  Total packets : {}\r", p.total).ok();
            match (p.rate_hz(), p.mean_us()) {
                (Some(rate), Some(mean)) => {
                    writeln!(
                        stdout,
                        "  Rolling rate  : {rate:.1} Hz  (mean gap: {mean:.0} us, {} samples)\r",
                        p.window.len()
                    )
                    .ok();
                }
                _ => {
                    writeln!(stdout, "  Rolling rate  : waiting for data...\r").ok();
                }
            }
            if p.peak_rate_hz > 0.0 {
                writeln!(stdout, "  Peak rate     : {:.1} Hz\r", p.peak_rate_hz).ok();
            }
            if p.all_min_us < u64::MAX {
                writeln!(stdout, "  Min gap (all) : {} us\r", p.all_min_us).ok();
                writeln!(stdout, "  Max gap (all) : {} us\r", p.all_max_us).ok();
            }
            writeln!(stdout, "  Drain bursts  : {} total\r", p.burst_total).ok();
            writeln!(stdout, "  Max burst     : {} packets\r", p.burst_max).ok();
            if let Some(mean) = p.mean_burst() {
                writeln!(stdout, "  Mean burst    : {mean:.1} packets\r").ok();
            }
            if let Some(ts) = p.last_ts {
                let ms = ts.elapsed().as_millis();
                writeln!(stdout, "  Last packet   : {ms} ms ago\r").ok();
            }
            writeln!(stdout, "\r").ok();
        }

        drop(s);
        stdout.flush().ok();

        std::thread::sleep(Duration::from_millis(100));
    }

    // Collect summary before tearing down the terminal.
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
            summary.push(format!("Pad {pad}:"));
            summary.push(format!("  Total packets : {}", p.total));
            if let (Some(rate), Some(mean)) = (p.rate_hz(), p.mean_us()) {
                summary.push(format!("  Rolling rate  : {rate:.1} Hz  (mean gap: {mean:.0} us)"));
            }
            if p.peak_rate_hz > 0.0 {
                summary.push(format!("  Peak rate     : {:.1} Hz", p.peak_rate_hz));
            }
            if p.all_min_us < u64::MAX {
                summary.push(format!("  Min gap (all) : {} us", p.all_min_us));
                summary.push(format!("  Max gap (all) : {} us", p.all_max_us));
            }
            summary.push(format!("  Max burst     : {} packets", p.burst_max));
            if let Some(mean) = p.mean_burst() {
                summary.push(format!("  Mean burst    : {mean:.1} packets"));
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
