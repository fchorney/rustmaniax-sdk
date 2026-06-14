//! Sensor sampling-rate probe (run against a real pad).
//!
//! Enables calibrated sensor test mode on every connected pad, then measures how
//! many sensor samples per second arrive (a) while streaming light frames at 30Hz
//! like the game does, and (b) with no light traffic at all. Prints per-pad rates
//! so the light-contention effect (and any fix for it) is visible without
//! launching deadsync or running a song. Running with two pads connected also
//! shows whether their throughput is independent (each pad has its own pipeline).
//!
//! Run: cargo run --features sample --bin smx-sensor-rate [phase_secs] [lights_hz] [pad]
//!
//! `pad` (0 or 1) limits sensor test mode to a single pad. The other connected
//! pad is then left untouched and acts as a visual control: light frames still
//! reach it during the on phase (set_lights covers both pads), but in the off
//! phase it reverts to its firmware idle animation, showing that the host stops
//! driving lights. A pad that is itself being measured stays in sensor test mode
//! the whole run and holds its last frame, so it will not show the idle animation.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustmaniax_sdk::{SensorTestMode, SmxEvent, SmxManager};

static SAMPLES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static CONNECTED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

// Two pads worth of 25-LED light data (BYTES_PER_PAD_25 = 675). Matches the
// frame size the game streams; exact LED values do not matter for timing.
const LIGHTS_BYTES: usize = 2 * 675;

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let phase_secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(6);
    let lights_hz: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let target_pad: Option<usize> = args.get(3).and_then(|s| s.parse().ok()).filter(|&p| p < 2);

    println!("SMX sensor sampling-rate probe");
    println!("phase length: {phase_secs}s per phase, light rate: {lights_hz}Hz");
    match target_pad {
        Some(p) => println!("measuring pad {p} only (other connected pad is a visual control)\n"),
        None => println!("measuring all connected pads\n"),
    }

    let mgr = SmxManager::start(|event| match event {
        SmxEvent::Connected { pad, info } => {
            println!(
                "Pad {pad} connected (P{}, fw {})",
                if info.is_player2 { 2 } else { 1 },
                info.firmware_version
            );
            if pad < 2 {
                CONNECTED[pad].store(true, Ordering::Relaxed);
            }
        }
        SmxEvent::Disconnected { pad } => {
            if pad < 2 {
                CONNECTED[pad].store(false, Ordering::Relaxed);
            }
        }
        SmxEvent::SensorTestData { pad, .. } => {
            if pad < 2 {
                SAMPLES[pad].fetch_add(1, Ordering::Relaxed);
            }
        }
        _ => {}
    })
    .expect("Failed to initialize HID");

    // We drive lights ourselves; make sure auto-animation is not also streaming.
    mgr.set_animation_auto(false);

    print!("Waiting for a pad");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !CONNECTED[0].load(Ordering::Relaxed) && !CONNECTED[1].load(Ordering::Relaxed) {
        if Instant::now() > deadline {
            println!(" ... none found, exiting.");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300)); // let a second pad enumerate

    // A pad is measured if it is connected and not excluded by the pad selector.
    let measure = [
        CONNECTED[0].load(Ordering::Relaxed) && target_pad.map_or(true, |p| p == 0),
        CONNECTED[1].load(Ordering::Relaxed) && target_pad.map_or(true, |p| p == 1),
    ];
    for pad in 0..2 {
        if measure[pad] {
            println!(" ... measuring pad {pad}.");
            mgr.set_test_mode(pad, SensorTestMode::CalibratedValues);
        }
    }
    std::thread::sleep(Duration::from_secs(1)); // settle

    run_phase(&mgr, "LIGHTS ON ", phase_secs, lights_hz, true, &measure);
    run_phase(&mgr, "LIGHTS OFF", phase_secs, 0, false, &measure);

    for pad in 0..2 {
        if measure[pad] {
            mgr.set_test_mode(pad, SensorTestMode::Off);
        }
    }
}

// A frame that lights one panel (moving each tick) on both pads. The motion is
// what makes light lag/backlog visible: if frames are starved, the lit panel
// stutters or keeps moving after the phase ends instead of tracking in real time.
fn moving_frame(tick: usize) -> Vec<u8> {
    const BYTES_PER_PAD: usize = 675; // 9 panels * 25 LEDs * 3 (RGB)
    const BYTES_PER_PANEL: usize = 75;
    let mut buf = vec![0u8; LIGHTS_BYTES];
    let panel = tick % 9;
    for pad in 0..2 {
        let base = pad * BYTES_PER_PAD + panel * BYTES_PER_PANEL;
        for b in 0..BYTES_PER_PANEL {
            buf[base + b] = 96;
        }
    }
    buf
}

fn run_phase(
    mgr: &SmxManager,
    label: &str,
    secs: u64,
    lights_hz: u64,
    lights: bool,
    active: &[bool; 2],
) {
    println!("Phase: {label} for {secs}s ...");
    for s in &SAMPLES {
        s.store(0, Ordering::Relaxed);
    }
    let start = Instant::now();
    let dur = Duration::from_secs(secs);
    let frame = if lights_hz > 0 {
        Duration::from_secs_f64(1.0 / lights_hz as f64)
    } else {
        Duration::from_millis(50)
    };
    let mut tick = 0usize;
    let mut frames_sent = 0u64;
    let mut next = Instant::now();
    while start.elapsed() < dur {
        if lights {
            mgr.set_lights(&moving_frame(tick));
            tick += 1;
            frames_sent += 1;
        }
        // Deadline-based pacing so set_lights cost doesn't drag the rate below
        // the requested Hz.
        next += frame;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    if lights {
        println!("  (streamed {} light frames = {:.1}/s requested)", frames_sent, frames_sent as f64 / elapsed);
    }
    for pad in 0..2 {
        if !active[pad] {
            continue;
        }
        let count = SAMPLES[pad].load(Ordering::Relaxed);
        println!("  pad {pad} -> {count} samples in {elapsed:.2}s = {:.1}/s", count as f64 / elapsed);
    }
}
