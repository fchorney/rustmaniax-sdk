//! Sensor sampling-rate probe (run against a real pad).
//!
//! Enables calibrated sensor test mode on the first connected pad, then measures
//! how many sensor samples per second arrive (a) while streaming light frames at
//! 30Hz like the game does, and (b) with no light traffic at all. Prints both so
//! the light-contention effect (and any fix for it) is visible without launching
//! deadsync or running a song.
//!
//! Run: cargo run --features sample --bin smx-sensor-rate [phase_secs] [lights_hz]

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustmaniax_sdk::{SensorTestMode, SmxEvent, SmxManager};

static SAMPLES: AtomicU64 = AtomicU64::new(0);
static CONNECTED_PAD: AtomicI64 = AtomicI64::new(-1);

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
            let _ = CONNECTED_PAD.compare_exchange(
                -1,
                pad as i64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        SmxEvent::SensorTestData { .. } => {
            SAMPLES.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    })
    .expect("Failed to initialize HID");

    // We drive lights ourselves; make sure auto-animation is not also streaming.
    mgr.set_animation_auto(false);

    print!("Waiting for a pad");
    let deadline = Instant::now() + Duration::from_secs(15);
    while CONNECTED_PAD.load(Ordering::Relaxed) < 0 {
        if Instant::now() > deadline {
            println!(" ... none found, exiting.");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let pad = CONNECTED_PAD.load(Ordering::Relaxed) as usize;
    println!(" ... using pad {pad}.\n");

    // Enable calibrated sensor test mode and let it settle before measuring.
    mgr.set_test_mode(pad, SensorTestMode::CalibratedValues);
    std::thread::sleep(Duration::from_secs(1));

    let light = vec![32u8; LIGHTS_BYTES];

    let with = run_phase(&mgr, "LIGHTS ON ", phase_secs, lights_hz, Some(&light));
    let without = run_phase(&mgr, "LIGHTS OFF", phase_secs, 0, None);

    mgr.set_test_mode(pad, SensorTestMode::Off);

    println!("\n=== Results (pad {pad}) ===");
    println!("  lights ON  ({lights_hz}Hz): {with:.1} samples/s");
    println!("  lights OFF        : {without:.1} samples/s");
}

fn run_phase(
    mgr: &SmxManager,
    label: &str,
    secs: u64,
    lights_hz: u64,
    light: Option<&[u8]>,
) -> f64 {
    println!("Phase: {label} for {secs}s ...");
    SAMPLES.store(0, Ordering::Relaxed);
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
    let count = SAMPLES.load(Ordering::Relaxed);
    let rate = count as f64 / elapsed;
    println!("  -> {count} samples in {elapsed:.2}s = {rate:.1}/s");
    rate
}
