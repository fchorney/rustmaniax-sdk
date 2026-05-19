//! Hardware integration tests requiring a physical StepManiaX pad.
//!
//! Run all tests:
//!   cargo test --test hardware -- --ignored --test-threads=1
//!
//! To record captures per-test (matching C++ SDK capture/ layout):
//!   SMX_CAPTURE_DIR=capture/connection cargo test --test hardware hardware_connection -- --ignored
//!   SMX_CAPTURE_DIR=capture/force_recalibration cargo test --test hardware hardware_force_recalibration -- --ignored
//!   SMX_CAPTURE_DIR=capture/panel_test_mode cargo test --test hardware hardware_panel_test_mode -- --ignored
//!   SMX_CAPTURE_DIR=capture/reenable_auto_lights cargo test --test hardware hardware_reenable_auto_lights -- --ignored
//!   SMX_CAPTURE_DIR=capture/config_get_set cargo test --test hardware hardware_config_get_set -- --ignored
//!   SMX_CAPTURE_DIR=capture/platform_lights cargo test --test hardware hardware_platform_lights -- --ignored
//!   SMX_CAPTURE_DIR=capture/sensor_test_mode cargo test --test hardware hardware_sensor_test_mode -- --ignored
//!   SMX_CAPTURE_DIR=capture/panel_lights cargo test --test hardware hardware_panel_lights -- --ignored
//!   SMX_CAPTURE_DIR=capture/panel_animation cargo test --test hardware hardware_panel_animation -- --ignored
//!   SMX_CAPTURE_DIR=capture/animation_upload cargo test --test hardware hardware_animation_upload -- --ignored
//!   SMX_CAPTURE_DIR=capture/factory_reset cargo test --test hardware hardware_zzz_factory_reset -- --ignored

use rustmaniax_sdk::{
    HidapiEnumerator, HidEnumerator, PanelTestMode, RecordingEnumerator, SensorTestMode,
    SmxEvent, SmxManager,
};

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

fn wait_for(cond: impl Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    while !cond() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Shared manager for all hardware tests.
static MANAGER: OnceLock<(SmxManager, Arc<Mutex<Vec<SmxEvent>>>)> = OnceLock::new();

fn get_manager() -> &'static (SmxManager, Arc<Mutex<Vec<SmxEvent>>>) {
    MANAGER.get_or_init(|| {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);

        let enumerator: Box<dyn HidEnumerator> = {
            let real = Box::new(HidapiEnumerator::new().expect("Failed to init HID"));
            match std::env::var("SMX_CAPTURE_DIR").ok().filter(|s| !s.is_empty()) {
                Some(dir) => Box::new(RecordingEnumerator::new(
                    real,
                    Path::new(&dir),
                    false,
                )),
                None => real,
            }
        };

        let mgr = SmxManager::new(enumerator, move |event| {
            events_clone.lock().unwrap().push(event);
        });

        (mgr, events)
    })
}

/// Returns indices of all connected pads.
fn connected_pads() -> Vec<usize> {
    let (mgr, _) = get_manager();
    wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 5000);
    (0..2).filter(|&i| mgr.get_info(i).connected).collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn hardware_connection() {
    let (mgr, events) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected. Is a pad connected?");

    let evts = events.lock().unwrap();

    for i in &pads {
        let info = mgr.get_info(*i);
        println!(
            "Slot {i}: fw={} p2={} serial={}",
            info.firmware_version, info.is_player2, info.serial
        );
        assert!(info.firmware_version > 0);
    }
}

#[test]
#[ignore]
fn hardware_force_recalibration() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    for &pad in &pads {
        mgr.force_recalibration(pad);
        println!("Sent force recalibration to pad {pad}");
    }
    std::thread::sleep(Duration::from_millis(500));

    for &pad in &pads {
        assert!(mgr.get_info(pad).connected, "Pad {pad} disconnected after recalibration");
    }
}

#[test]
#[ignore]
fn hardware_panel_test_mode() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    mgr.set_panel_test_mode(PanelTestMode::PressureTest);
    println!("Enabled pressure test mode");
    std::thread::sleep(Duration::from_secs(2));

    for &pad in &pads {
        assert!(mgr.get_info(pad).connected);
    }

    mgr.set_panel_test_mode(PanelTestMode::Off);
    println!("Disabled panel test mode");
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
#[ignore]
fn hardware_reenable_auto_lights() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    mgr.reenable_auto_lights();
    println!("Sent re-enable auto lights");
    std::thread::sleep(Duration::from_millis(500));

    for &pad in &pads {
        assert!(mgr.get_info(pad).connected);
    }
}

#[test]
#[ignore]
fn hardware_config_get_set() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    for &pad in &pads {
        let original = mgr.get_config(pad).expect("Config not available");
        let orig_debounce = unsafe { std::ptr::addr_of!(original.panel_debounce_us).read_unaligned() };
        println!("Pad {pad} original panelDebounceMicroseconds: {orig_debounce}");

        let mut modified = original;
        let new_val: u16 = if orig_debounce == 4000 { 5000 } else { 4000 };
        unsafe { std::ptr::addr_of_mut!(modified.panel_debounce_us).write_unaligned(new_val) };
        mgr.set_config(pad, modified);
        std::thread::sleep(Duration::from_secs(2));

        let readback = mgr.get_config(pad).unwrap();
        let rb_debounce = unsafe { std::ptr::addr_of!(readback.panel_debounce_us).read_unaligned() };
        println!("Pad {pad} read back: {rb_debounce}");
        assert_eq!(rb_debounce, new_val);

        // Restore.
        mgr.set_config(pad, original);
        std::thread::sleep(Duration::from_secs(2));

        let restored = mgr.get_config(pad).unwrap();
        let res_debounce = unsafe { std::ptr::addr_of!(restored.panel_debounce_us).read_unaligned() };
        assert_eq!(res_debounce, orig_debounce);
        println!("Pad {pad} restored: {res_debounce}");
    }
}

#[test]
#[ignore]
fn hardware_platform_lights() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    let any_v4 = pads.iter().any(|&p| mgr.get_info(p).firmware_version >= 4);
    if !any_v4 {
        println!("No pad with firmware v4+, skipping platform lights");
        return;
    }

    // Red.
    let mut data = vec![0u8; 264];
    for i in 0..88 { data[i * 3] = 255; }
    mgr.set_platform_lights(&data);
    println!("Set platform lights to RED");
    std::thread::sleep(Duration::from_secs(2));

    // Blue.
    data.fill(0);
    for i in 0..88 { data[i * 3 + 2] = 255; }
    mgr.set_platform_lights(&data);
    println!("Set platform lights to BLUE");
    std::thread::sleep(Duration::from_secs(2));

    mgr.reenable_auto_lights();
    std::thread::sleep(Duration::from_millis(200));
}

#[test]
#[ignore]
fn hardware_sensor_test_mode() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    let modes = [
        (SensorTestMode::UncalibratedValues, "Uncalibrated"),
        (SensorTestMode::CalibratedValues, "Calibrated"),
        (SensorTestMode::Noise, "Noise"),
        (SensorTestMode::Tare, "Tare"),
    ];

    for &pad in &pads {
        println!("Testing pad {pad}:");
        for (mode, name) in &modes {
            mgr.set_test_mode(pad, *mode);
            let got_data = wait_for(|| mgr.get_test_data(pad).is_some(), 5000);
            if got_data {
                let data = mgr.get_test_data(pad).unwrap();
                let panels_with_data = data.have_data_from_panel.iter().filter(|&&b| b).count();
                println!("  {name}: {panels_with_data} panels responded");
                assert!(panels_with_data > 0);
            } else {
                println!("  {name}: no data received (timeout)");
            }
            mgr.set_test_mode(pad, SensorTestMode::Off);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

#[test]
#[ignore]
fn hardware_panel_lights() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    println!("Running 3-second rainbow sweep on {} pad(s)", pads.len());

    let duration = Duration::from_secs(3);
    let frame_interval = Duration::from_millis(33);
    let start = std::time::Instant::now();
    let mut frame_count = 0;

    while start.elapsed() < duration {
        let progress = start.elapsed().as_secs_f32() / duration.as_secs_f32();
        let mut light_data = vec![0u8; 1350];

        for pad in 0..2 {
            for panel in 0..9 {
                let base_hue = (progress * 360.0 + panel as f32 * 40.0 + pad as f32 * 180.0) % 360.0;
                for led in 0..25 {
                    let hue = (base_hue + led as f32 * 2.0) % 360.0;
                    let (r, g, b) = hsv_to_rgb(hue);
                    let offset = (pad * 9 * 25 + panel * 25 + led) * 3;
                    light_data[offset] = r;
                    light_data[offset + 1] = g;
                    light_data[offset + 2] = b;
                }
            }
        }

        mgr.set_lights(&light_data);
        frame_count += 1;
        std::thread::sleep(frame_interval);
    }

    println!("Sent {frame_count} frames (~{} FPS)", frame_count / 3);
    assert!(frame_count >= 80);

    mgr.reenable_auto_lights();
    std::thread::sleep(Duration::from_millis(500));
}

#[test]
#[ignore]
fn hardware_panel_animation() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    let gif = generate_test_animation_gif();
    println!("Generated {}-byte animated GIF", gif.len());

    let mut anim_state = rustmaniax_sdk::AnimationState::new();
    for &pad in &pads {
        anim_state
            .load(&gif, pad, rustmaniax_sdk::LightsType::Released)
            .expect("Failed to load animation");
        println!("Loaded animation for pad {pad}");
    }

    println!("Playing animation for 3 seconds...");
    let duration = Duration::from_secs(3);
    let start = std::time::Instant::now();
    while start.elapsed() < duration {
        let input = [mgr.get_input_state(0), mgr.get_input_state(1)];
        let frame = anim_state.build_frame(input);
        mgr.set_lights(&frame);
        std::thread::sleep(Duration::from_millis(33));
    }

    mgr.reenable_auto_lights();
    std::thread::sleep(Duration::from_millis(500));
    println!("Animation test complete");
}

#[test]
#[ignore]
fn hardware_animation_upload() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    for &pad in &pads {
        let info = mgr.get_info(pad);
        if info.firmware_version < 4 {
            println!("Pad {pad}: firmware v4+ required for upload, skipping (fw={})", info.firmware_version);
            continue;
        }

        let gif = generate_test_animation_gif();
        let upload = rustmaniax_sdk::prepare_upload(&gif, pad, rustmaniax_sdk::LightsType::Released)
            .expect("Failed to prepare upload");

        let total = upload.commands.len();
        println!("Pad {pad}: sending {} upload commands", total);

        for (i, cmd) in upload.commands.iter().enumerate() {
            match cmd {
                rustmaniax_sdk::UploadCommand::Packet(data) => {
                    mgr.send_command(pad, data, None);
                }
                rustmaniax_sdk::UploadCommand::Delay(ms) => {
                    std::thread::sleep(Duration::from_millis(*ms as u64));
                }
            }
            if (i + 1) % 50 == 0 || i + 1 == total {
                println!("  progress: {}/{}", i + 1, total);
            }
        }
        println!("Pad {pad}: upload complete");
    }
}

#[test]
#[ignore]
fn hardware_zzz_factory_reset() {
    let (mgr, _) = get_manager();
    let pads = connected_pads();
    assert!(!pads.is_empty(), "No SMX device detected.");

    for &pad in &pads {
        mgr.factory_reset(pad);
        println!("Sent factory reset to pad {pad}");
    }
    std::thread::sleep(Duration::from_secs(2));

    for &pad in &pads {
        assert!(mgr.get_info(pad).connected, "Pad {pad} disconnected after factory reset");
        assert!(mgr.get_config(pad).is_some(), "Pad {pad} config not available after factory reset");
    }
    println!("All pads reset and still connected");
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn hsv_to_rgb(h: f32) -> (u8, u8, u8) {
    let c = 1.0f32;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

fn generate_test_animation_gif() -> Vec<u8> {
    use image::codecs::gif::GifEncoder;
    use image::{Frame, Rgba, RgbaImage};
    use std::io::Cursor;

    let colors: [(u8, u8, u8); 6] = [
        (255, 0, 0),
        (255, 255, 0),
        (0, 255, 0),
        (0, 255, 255),
        (0, 0, 255),
        (255, 0, 255),
    ];

    let mut buf = Cursor::new(Vec::new());
    {
        let mut encoder = GifEncoder::new(&mut buf);
        let frames: Vec<Frame> = colors
            .iter()
            .map(|&(r, g, b)| {
                let mut img = RgbaImage::new(23, 24);
                for y in 0..24 {
                    for x in 0..23 {
                        img.put_pixel(x, y, Rgba([r, g, b, 255]));
                    }
                }
                Frame::new(img)
            })
            .collect();
        encoder.encode_frames(frames.into_iter()).unwrap();
    }
    buf.into_inner()
}
