//! Full-stack integration tests exercising SmxManager with fake HID devices.

use rustmaniax_sdk::UpdateReason;
use rustmaniax_sdk::{PanelTestMode, SmxManager};
use rustmaniax_sdk::HID_REPORT_COMMAND;
use rustmaniax_sdk::test_helpers::{wait_for, FakeDevice, FakeEnumerator};

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

fn make_manager_one_device(
    is_p2: bool,
    firmware: u16,
) -> (SmxManager, FakeDevice, Arc<Mutex<Vec<(usize, UpdateReason)>>>) {
    let dev = FakeDevice::new_auto(is_p2, firmware);
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let enumerator = FakeEnumerator::new(vec![("/dev/smx0".to_string(), dev.clone())]);
    let mgr = SmxManager::new(Box::new(enumerator), move |pad, reason| {
        events_clone.lock().unwrap().push((pad, reason));
    });

    (mgr, dev, events)
}

fn make_manager_two_devices(
) -> (SmxManager, FakeDevice, FakeDevice, Arc<Mutex<Vec<(usize, UpdateReason)>>>) {
    let dev_p1 = FakeDevice::new_auto(false, 5);
    let dev_p2 = FakeDevice::new_auto(true, 5);
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let enumerator = FakeEnumerator::new(vec![
        ("/dev/smx0".to_string(), dev_p1.clone()),
        ("/dev/smx1".to_string(), dev_p2.clone()),
    ]);
    let mgr = SmxManager::new(Box::new(enumerator), move |pad, reason| {
        events_clone.lock().unwrap().push((pad, reason));
    });

    (mgr, dev_p1, dev_p2, events)
}

// ─── Discovery & Connection ──────────────────────────────────────────────────

#[test]
fn single_p1_device_discovered_and_connected() {
    let (mgr, _dev, events) = make_manager_one_device(false, 5);

    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected, "Device did not connect within timeout");

    let info = mgr.get_info(0);
    assert!(!info.is_player2);
    assert_eq!(info.firmware_version, 5);

    // Should have received a Connected callback for pad 0.
    let evts = events.lock().unwrap();
    assert!(evts.iter().any(|(pad, reason)| *pad == 0 && *reason == UpdateReason::Connected));
}

#[test]
fn single_p2_device_placed_in_slot_1() {
    let (mgr, _dev, _events) = make_manager_one_device(true, 5);

    let connected = wait_for(|| mgr.get_info(1).connected, 2000);
    assert!(connected, "P2 device did not connect in slot 1");

    assert!(!mgr.get_info(0).connected);
    assert!(mgr.get_info(1).is_player2);
}

#[test]
fn two_devices_ordered_p1_slot0_p2_slot1() {
    let (mgr, _dev_p1, _dev_p2, _events) = make_manager_two_devices();

    let both_connected = wait_for(
        || mgr.get_info(0).connected && mgr.get_info(1).connected,
        2000,
    );
    assert!(both_connected, "Both devices did not connect");

    assert!(!mgr.get_info(0).is_player2);
    assert!(mgr.get_info(1).is_player2);
}

#[test]
fn get_input_state_through_full_stack() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);

    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.queue_input_state(0x0155);
    std::thread::sleep(std::time::Duration::from_millis(100));

    assert_eq!(mgr.get_input_state(0), 0x0155);
}

#[test]
fn get_info_disconnected_pad() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    // Pad 1 has no device.
    let info = mgr.get_info(1);
    assert!(!info.connected);
    assert_eq!(info.firmware_version, 0);
}

// ─── Commands ────────────────────────────────────────────────────────────────

#[test]
fn factory_reset_sends_f_and_g_commands() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.factory_reset(0);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_f = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'f');
    let has_g = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'G');
    assert!(has_f, "Expected 'f' command");
    assert!(has_g, "Expected 'G' command");
}

#[test]
fn force_recalibration_sends_c_command() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.force_recalibration(0);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_c = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'C');
    assert!(has_c, "Expected 'C' command");
}

#[test]
fn reenable_auto_lights_sends_command() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.reenable_auto_lights();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_s = writes.iter().any(|w| {
        w.len() > 6 && w[0] == HID_REPORT_COMMAND && w[3] == b'S' && w[4] == b' ' && w[5] == b'1'
    });
    assert!(has_s, "Expected 'S 1' command");
}

#[test]
fn panel_test_mode_sends_t_command() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.set_panel_test_mode(PanelTestMode::PressureTest);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_t = writes.iter().any(|w| {
        w.len() > 5 && w[0] == HID_REPORT_COMMAND && w[3] == b't' && w[5] == b'1'
    });
    assert!(has_t, "Expected 't 1' command");
}

#[test]
fn set_serial_numbers_sends_s_command() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.set_serial_numbers();
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_s = writes.iter().any(|w| {
        w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b's' && w[2] >= 18
    });
    assert!(has_s, "Expected 's' command with serial data");
}

// ─── Edge Cases ──────────────────────────────────────────────────────────────

#[test]
fn factory_reset_on_disconnected_pad_does_not_crash() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    // Pad 1 is not connected.
    mgr.factory_reset(1);
}

#[test]
fn force_recalibration_on_disconnected_pad_does_not_crash() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    mgr.force_recalibration(1);
}

#[test]
fn reenable_auto_lights_with_no_devices_does_not_crash() {
    let enumerator = FakeEnumerator::new(vec![]);
    let mgr = SmxManager::new(Box::new(enumerator), |_, _| {});
    mgr.reenable_auto_lights();
}

#[test]
fn panel_test_mode_with_no_devices_does_not_crash() {
    let enumerator = FakeEnumerator::new(vec![]);
    let mgr = SmxManager::new(Box::new(enumerator), |_, _| {});
    mgr.set_panel_test_mode(PanelTestMode::PressureTest);
    std::thread::sleep(std::time::Duration::from_millis(100));
}
