use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustmaniax_sdk::{PanelTestMode, SensorTestMode, SmxEvent, SmxManager};

static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let start = Instant::now();

    println!("SMX SDK Rust Sample");

    let mgr = SmxManager::start(move |event| {
        let elapsed = start.elapsed().as_secs_f64();
        match event {
            SmxEvent::Connected { pad, info } => {
                println!(
                    "Pad {pad} connected (jumper: P{}, serial: {}, fw: {})",
                    if info.is_player2 { 2 } else { 1 },
                    if info.has_serial_number { &info.serial } else { "(none)" },
                    info.firmware_version
                );
                if !info.has_serial_number {
                    println!("  Warning: Pad {pad} has no serial number.");
                }
            }
            SmxEvent::Disconnected { pad } => {
                println!("Pad {pad}: disconnected");
            }
            SmxEvent::InputState { pad, state } => {
                println!("{elapsed:8.3}: Pad {pad}: input state {state:04x}");
            }
            SmxEvent::ConfigUpdated { pad } => {
                println!("{elapsed:8.3}: Pad {pad}: config updated");
            }
            SmxEvent::SensorTestData { pad, data } => {
                print!("{elapsed:8.3}: Pad {pad} sensors:");
                for panel in 0..9 {
                    if !data.have_data_from_panel[panel] {
                        print!(" [--]");
                    } else {
                        print!(
                            " [{},{},{},{}]",
                            data.sensor_level[panel][0],
                            data.sensor_level[panel][1],
                            data.sensor_level[panel][2],
                            data.sensor_level[panel][3]
                        );
                    }
                }
                println!();
            }
        }
    })
    .expect("Failed to initialize HID");

    // Parse flags.
    let mut all_packets = false;
    let mut test_mode = false;
    let mut sensor_mode = SensorTestMode::Off;
    let mut main_ms: Option<i32> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--all-packets" => all_packets = true,
            "--test-mode" => test_mode = true,
            "--uncalibrated" => sensor_mode = SensorTestMode::UncalibratedValues,
            "--calibrated" => sensor_mode = SensorTestMode::CalibratedValues,
            "--noise" => sensor_mode = SensorTestMode::Noise,
            "--tare" => sensor_mode = SensorTestMode::Tare,
            s if !s.starts_with('-') && main_ms.is_none() => main_ms = s.parse().ok(),
            _ => {}
        }
        i += 1;
    }

    if let Some(ms) = main_ms {
        mgr.set_main_thread_sleep_ms(ms);
        println!("Main thread sleep: {ms}ms");
    }

    if all_packets {
        mgr.set_input_state_mode(true);
        println!("Input state mode: fire on every Report 3 packet");
    }

    if test_mode {
        mgr.set_panel_test_mode(PanelTestMode::PressureTest);
        println!("Panel test mode: pressure test enabled");
    }

    if sensor_mode != SensorTestMode::Off {
        let name = match sensor_mode {
            SensorTestMode::UncalibratedValues => "uncalibrated",
            SensorTestMode::CalibratedValues => "calibrated",
            SensorTestMode::Noise => "noise",
            SensorTestMode::Tare => "tare",
            _ => "",
        };
        println!("Sensor test mode: {name}");
        for pad in 0..2 {
            mgr.set_test_mode(pad, sensor_mode);
        }
    }

    println!("Scanning for StepManiaX devices... Press Ctrl+C to quit.");
    println!(
        "Usage: {} [main_thread_ms] [--all-packets] [--test-mode]",
        args[0]
    );
    println!("       [--uncalibrated] [--calibrated] [--noise] [--tare]");

    ctrlc::set_handler(|| {
        SHOULD_EXIT.store(true, Ordering::Relaxed);
    })
    .expect("Failed to set Ctrl+C handler");

    while !SHOULD_EXIT.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }

    // Cleanup.
    if sensor_mode != SensorTestMode::Off {
        for pad in 0..2 {
            mgr.set_test_mode(pad, SensorTestMode::Off);
        }
    }
    if test_mode {
        mgr.set_panel_test_mode(PanelTestMode::Off);
    }
}
