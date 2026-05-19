//! Replay-based regression tests using captured .smxhid HID traffic.
//! These tests replay real device interactions through the SDK and verify
//! the same commands are produced.

use std::path::Path;

use rustmaniax_sdk::SmxEvent;
use rustmaniax_sdk::SmxManager;
use rustmaniax_sdk::HID_REPORT_COMMAND;
use rustmaniax_sdk::test_helpers::{wait_for, ReplayDevice};

use std::sync::{Arc, Mutex};

/// Path to the capture directory.
const CAPTURE_DIR: &str = "capture";

fn capture_path(name: &str) -> std::path::PathBuf {
    Path::new(CAPTURE_DIR).join(name)
}

fn make_replay_manager(
    capture_name: &str,
) -> (SmxManager, Vec<ReplayDevice>, Arc<Mutex<Vec<SmxEvent>>>) {
    let dir = capture_path(capture_name);

    // Load devices — these are shared between the enumerator and test assertions.
    let mut devices = Vec::new();
    let mut enum_devices = Vec::new();
    for i in 0..2 {
        let path = dir.join(format!("device_{i}.smxhid"));
        if path.exists() {
            let dev = ReplayDevice::from_file(&path);
            enum_devices.push((format!("/dev/smx_replay_{i}"), dev.clone()));
            devices.push(dev);
        }
    }

    let enumerator = rustmaniax_sdk::test_helpers::ReplayEnumerator::from_devices(enum_devices);
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let mgr = SmxManager::new(Box::new(enumerator), move |event| {
        events_clone.lock().unwrap().push(event);
    });

    (mgr, devices, events)
}

/// Check if a write contains a specific command byte at position 3.
fn has_command(writes: &[Vec<u8>], cmd: u8) -> bool {
    writes
        .iter()
        .any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == cmd)
}

/// Check if a write contains a specific command string starting at position 3.
fn has_command_str(writes: &[Vec<u8>], cmd: &[u8]) -> bool {
    writes.iter().any(|w| {
        w.len() >= 3 + cmd.len() && w[0] == HID_REPORT_COMMAND && &w[3..3 + cmd.len()] == cmd
    })
}

/// Count writes containing a specific command byte.
fn count_command(writes: &[Vec<u8>], cmd: u8) -> usize {
    writes
        .iter()
        .filter(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == cmd)
        .count()
}

// ─── Replay Tests ────────────────────────────────────────────────────────────

#[test]
fn replay_connection() {
    if !capture_path("connection").exists() {
        eprintln!("Skipping: capture/connection not found");
        return;
    }

    let (mgr, devices, events) = make_replay_manager("connection");

    let connected = wait_for(
        || mgr.get_info(0).connected || mgr.get_info(1).connected,
        2000,
    );
    assert!(connected, "No device connected from replay");

    // Should have fired Connected callback.
    let evts = events.lock().unwrap();
    assert!(
        evts.iter().any(|e| matches!(e, SmxEvent::Connected { .. })),
        "No Connected callback"
    );

    // Should have sent 'G' (config request) command.
    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    assert!(has_command(&all_writes, b'G'), "Expected 'G' command in writes");
}

#[test]
fn replay_force_recalibration() {
    if !capture_path("force_recalibration").exists() {
        eprintln!("Skipping: capture/force_recalibration not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("force_recalibration");
    let connected = wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 2000);
    assert!(connected);

    for i in 0..2 {
        if mgr.get_info(i).connected {
            mgr.force_recalibration(i);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    assert!(has_command_str(&all_writes, b"C\n"), "Expected 'C\\n' command");
}

#[test]
fn replay_reenable_auto_lights() {
    if !capture_path("reenable_auto_lights").exists() {
        eprintln!("Skipping: capture/reenable_auto_lights not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("reenable_auto_lights");
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    mgr.reenable_auto_lights();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = devices[0].get_actual_writes();
    assert!(has_command_str(&writes, b"S 1\n"), "Expected 'S 1\\n' command");
}

#[test]
fn replay_config_get_set() {
    if !capture_path("config_get_set").exists() {
        eprintln!("Skipping: capture/config_get_set not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("config_get_set");
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    // Config should have been read during connection.
    let config = mgr.get_config(0);
    assert!(config.is_some(), "Config not available");

    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    assert!(has_command(&all_writes, b'G'), "Expected 'G' command");
}

#[test]
fn replay_factory_reset() {
    if !capture_path("factory_reset").exists() {
        eprintln!("Skipping: capture/factory_reset not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("factory_reset");
    let connected = wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 2000);
    assert!(connected);

    let pad = if mgr.get_info(0).connected { 0 } else { 1 };
    mgr.factory_reset(pad);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    assert!(has_command_str(&all_writes, b"f\n"), "Expected 'f\\n' command");
    assert!(has_command(&all_writes, b'G'), "Expected 'G' command after reset");
}

#[test]
fn replay_sensor_test_mode() {
    if !capture_path("sensor_test_mode").exists() {
        eprintln!("Skipping: capture/sensor_test_mode not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("sensor_test_mode");
    let connected = wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 2000);
    assert!(connected);

    for i in 0..2 {
        if mgr.get_info(i).connected {
            mgr.set_test_mode(i, rustmaniax_sdk::SensorTestMode::CalibratedValues);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    assert!(has_command(&all_writes, b'y'), "Expected 'y' command");
}

#[test]
fn replay_panel_test_mode() {
    if !capture_path("panel_test_mode").exists() {
        eprintln!("Skipping: capture/panel_test_mode not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("panel_test_mode");
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    mgr.set_panel_test_mode(rustmaniax_sdk::PanelTestMode::PressureTest);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = devices[0].get_actual_writes();
    assert!(has_command_str(&writes, b"t "), "Expected 't ' command");
}

#[test]
fn replay_platform_lights() {
    if !capture_path("platform_lights").exists() {
        eprintln!("Skipping: capture/platform_lights not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("platform_lights");
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    // Send platform lights data.
    let light_data = vec![128u8; 264]; // 2 pads × 44 LEDs × 3 RGB
    mgr.set_platform_lights(&light_data);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = devices[0].get_actual_writes();
    assert!(has_command(&writes, b'L'), "Expected 'L' command");
}

#[test]
fn replay_panel_lights() {
    if !capture_path("panel_lights").exists() {
        eprintln!("Skipping: capture/panel_lights not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("panel_lights");
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    // Send panel lights (25-LED mode = 1350 bytes).
    let light_data = vec![128u8; 1350];
    mgr.set_lights(&light_data);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = devices[0].get_actual_writes();
    let lights_count = count_command(&writes, b'2') + count_command(&writes, b'3');
    assert!(lights_count >= 2, "Expected lights commands ('2' and '3'), got {lights_count}");

    // Verify color scaling: no byte in lights payload should exceed 170.
    for w in &writes {
        if w.len() > 4 && w[0] == HID_REPORT_COMMAND && (w[3] == b'2' || w[3] == b'3') {
            for &byte in &w[4..] {
                if byte > 0 {
                    assert!(byte <= 170, "Color byte {byte} exceeds scaled max 170");
                }
            }
        }
    }
}

#[test]
fn replay_panel_animation() {
    if !capture_path("panel_animation").exists() {
        eprintln!("Skipping: capture/panel_animation not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("panel_animation");
    let connected = wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 2000);
    assert!(connected);

    // Send lights frames (simulating animation playback).
    let light_data = vec![128u8; 1350];
    for _ in 0..30 {
        mgr.set_lights(&light_data);
        std::thread::sleep(std::time::Duration::from_millis(33));
    }

    mgr.reenable_auto_lights();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    let lights_count = count_command(&all_writes, b'2') + count_command(&all_writes, b'3');
    assert!(lights_count >= 30, "Expected >= 30 lights commands, got {lights_count}");
    assert!(has_command_str(&all_writes, b"S 1\n"), "Expected 'S 1\\n' at end");
}

#[test]
fn replay_animation_upload() {
    if !capture_path("animation_upload").exists() {
        eprintln!("Skipping: capture/animation_upload not found");
        return;
    }

    let (mgr, devices, _events) = make_replay_manager("animation_upload");
    let connected = wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 2000);
    assert!(connected);

    let pad = if mgr.get_info(0).connected { 0 } else { 1 };

    // Generate and send upload commands (same as hardware test).
    let gif = generate_test_animation_gif();
    let upload = rustmaniax_sdk::prepare_upload(&gif, pad, rustmaniax_sdk::LightsType::Released);
    if let Ok(upload) = upload {
        for cmd in &upload.commands {
            match cmd {
                rustmaniax_sdk::UploadCommand::Packet(data) => {
                    mgr.send_command(pad, data, None);
                }
                rustmaniax_sdk::UploadCommand::Delay(ms) => {
                    std::thread::sleep(std::time::Duration::from_millis(*ms as u64));
                }
            }
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let all_writes: Vec<Vec<u8>> = devices.iter().flat_map(|d| d.get_actual_writes()).collect();
    let upload_count = count_command(&all_writes, b'm');
    assert!(upload_count > 0, "Expected 'm' upload commands, got 0");
}

fn generate_test_animation_gif() -> Vec<u8> {
    use image::codecs::gif::GifEncoder;
    use image::{Frame, Rgba, RgbaImage};
    use std::io::Cursor;

    let colors: [(u8, u8, u8); 3] = [(255, 0, 0), (0, 255, 0), (0, 0, 255)];
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
