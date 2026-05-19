//! Hardware integration tests requiring a physical StepManiaX pad.
//!
//! These tests are ignored by default. Run with:
//!   cargo test --test hardware -- --ignored --test-threads=1
//!
//! To record HID traffic while running:
//!   SMX_CAPTURE_DIR=capture/hardware cargo test --test hardware -- --ignored --test-threads=1

use rustmaniax_sdk::{SmxManager, UpdateReason};

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

/// Shared manager instance across all hardware tests (avoids HID re-init issues).
static MANAGER: OnceLock<(SmxManager, Arc<Mutex<Vec<(usize, UpdateReason)>>>)> = OnceLock::new();

fn get_manager() -> &'static (SmxManager, Arc<Mutex<Vec<(usize, UpdateReason)>>>) {
    MANAGER.get_or_init(|| {
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_clone = Arc::clone(&events);
        let mgr = SmxManager::start(move |pad, reason| {
            events_clone.lock().unwrap().push((pad, reason));
        })
        .expect("Failed to initialize HID");
        (mgr, events)
    })
}

fn connected_pad() -> Option<usize> {
    let (mgr, _) = get_manager();
    let connected = wait_for(|| mgr.get_info(0).connected || mgr.get_info(1).connected, 5000);
    if !connected {
        return None;
    }
    if mgr.get_info(0).connected { Some(0) } else { Some(1) }
}

#[test]
#[ignore]
fn hardware_device_connects() {
    let (mgr, events) = get_manager();

    let pad = connected_pad();
    assert!(pad.is_some(), "No SMX device detected. Is a pad connected?");
    let pad = pad.unwrap();

    let evts = events.lock().unwrap();
    assert!(evts.iter().any(|(_, r)| *r == UpdateReason::Connected));

    let info = mgr.get_info(pad);
    println!(
        "Connected: P{}, fw={}, serial={}",
        if info.is_player2 { 2 } else { 1 },
        info.firmware_version,
        info.serial
    );
    assert!(info.firmware_version > 0);
    assert!(info.has_serial_number);
}

#[test]
#[ignore]
fn hardware_get_config() {
    let (mgr, _) = get_manager();
    let pad = connected_pad().expect("No SMX device detected.");

    let config = mgr.get_config(pad);
    assert!(config.is_some(), "Config not available");

    let cfg = config.unwrap();
    let mv = cfg.master_version;
    let cv = cfg.config_version;
    println!("Config: master_version={mv}, config_version={cv}");
}

#[test]
#[ignore]
fn hardware_input_state_changes() {
    let (mgr, _) = get_manager();
    let pad = connected_pad().expect("No SMX device detected.");

    println!("Monitoring input on pad {pad}. Step on a panel within 10 seconds...");
    let changed = wait_for(|| mgr.get_input_state(pad) != 0, 10000);
    if changed {
        println!("Input detected: 0x{:04x}", mgr.get_input_state(pad));
    } else {
        println!("No input detected (timeout).");
    }
}

#[test]
#[ignore]
fn hardware_force_recalibration() {
    let (mgr, _) = get_manager();
    let pad = connected_pad().expect("No SMX device detected.");

    mgr.force_recalibration(pad);
    std::thread::sleep(Duration::from_millis(500));
    println!("Force recalibration sent to pad {pad}.");
}

#[test]
#[ignore]
fn hardware_reenable_auto_lights() {
    let (mgr, _) = get_manager();
    let _pad = connected_pad().expect("No SMX device detected.");

    mgr.reenable_auto_lights();
    std::thread::sleep(Duration::from_millis(500));
    println!("Re-enabled auto lights.");
}
