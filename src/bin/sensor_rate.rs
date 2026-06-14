//! Sensor sampling-rate probe (run against a real pad).
//!
//! Enables calibrated sensor test mode on every connected pad, then measures how
//! many sensor samples per second arrive (a) while streaming light frames at 30Hz
//! like the game does, and (b) with no light traffic at all. Prints per-pad rates
//! so the light-contention effect (and any fix for it) is visible without
//! launching deadsync or running a song. Running with two pads connected also
//! shows whether their throughput is independent (each pad has its own pipeline).
//!
//! Run: cargo run --features sample --bin smx-sensor-rate [phase_secs] [lights_hz]

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

    println!("SMX sensor sampling-rate probe");
    println!("phase length: {phase_secs}s per phase, light rate: {lights_hz}Hz\n");

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

    let active = [
        CONNECTED[0].load(Ordering::Relaxed),
        CONNECTED[1].load(Ordering::Relaxed),
    ];
    for pad in 0..2 {
        if active[pad] {
            println!(" ... pad {pad} active.");
            mgr.set_test_mode(pad, SensorTestMode::CalibratedValues);
        }
    }
    std::thread::sleep(Duration::from_secs(1)); // settle

    let light = vec![32u8; LIGHTS_BYTES];

    run_phase(&mgr, "LIGHTS ON ", phase_secs, lights_hz, Some(&light), &active);
    run_phase(&mgr, "LIGHTS OFF", phase_secs, 0, None, &active);

    for pad in 0..2 {
        if active[pad] {
            mgr.set_test_mode(pad, SensorTestMode::Off);
        }
    }
}

fn run_phase(
    mgr: &SmxManager,
    label: &str,
    secs: u64,
    lights_hz: u64,
    light: Option<&[u8]>,
    active: &[bool; 2],
) {
    println!("Phase: {label} for {secs}s ...");
    if light.is_none() {
        // Hand lighting back to the pad's firmware so its idle animation resumes
        // during the off phase (streaming set_lights in the on phase takes that
        // over, leaving the panels frozen otherwise). This is one command, not
        // streamed frames, and the firmware drives the animation on-device, so the
        // no-contention baseline is unchanged.
        mgr.reenable_auto_lights();
    }
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
    while start.elapsed() < dur {
        if let Some(buf) = light {
            mgr.set_lights(buf);
        }
        std::thread::sleep(frame);
    }
    let elapsed = start.elapsed().as_secs_f64();
    for pad in 0..2 {
        if !active[pad] {
            continue;
        }
        let count = SAMPLES[pad].load(Ordering::Relaxed);
        println!("  pad {pad} -> {count} samples in {elapsed:.2}s = {:.1}/s", count as f64 / elapsed);
    }
}
