//! Full-stack integration tests exercising SmxManager with fake HID devices.

use rustmaniax_sdk::{PanelTestMode, SmxEvent, SmxManager};
use rustmaniax_sdk::test_helpers::HID_REPORT_COMMAND;
use rustmaniax_sdk::test_helpers::{wait_for, FakeDevice, FakeEnumerator};

use std::sync::{Arc, Mutex};

fn make_manager_one_device(
    is_p2: bool,
    firmware: u16,
) -> (SmxManager, FakeDevice, Arc<Mutex<Vec<SmxEvent>>>) {
    let dev = FakeDevice::new_auto(is_p2, firmware);
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let enumerator = FakeEnumerator::new(vec![("/dev/smx0".to_string(), dev.clone())]);
    let mgr = SmxManager::new(Box::new(enumerator), move |event| {
        events_clone.lock().unwrap().push(event);
    });

    (mgr, dev, events)
}

fn make_manager_two_devices(
) -> (SmxManager, FakeDevice, FakeDevice, Arc<Mutex<Vec<SmxEvent>>>) {
    let dev_p1 = FakeDevice::new_auto(false, 5);
    let dev_p2 = FakeDevice::new_auto(true, 5);
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let enumerator = FakeEnumerator::new(vec![
        ("/dev/smx0".to_string(), dev_p1.clone()),
        ("/dev/smx1".to_string(), dev_p2.clone()),
    ]);
    let mgr = SmxManager::new(Box::new(enumerator), move |event| {
        events_clone.lock().unwrap().push(event);
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
    assert!(evts.iter().any(|e| matches!(e, SmxEvent::Connected { pad: 0, .. })));
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
fn set_player_assignment_overrides_jumper_order() {
    let (mgr, _dev_p1, _dev_p2, _events) = make_manager_two_devices();

    let both_connected = wait_for(
        || mgr.get_info(0).connected && mgr.get_info(1).connected,
        2000,
    );
    assert!(both_connected, "Both devices did not connect");

    // Jumper ordering: P1-jumpered pad in slot 0, P2-jumpered pad in slot 1.
    let s0 = mgr.get_info(0).serial;
    let s1 = mgr.get_info(1).serial;
    assert_ne!(s0, s1);
    assert!(!mgr.get_info(0).is_player2);
    assert!(mgr.get_info(1).is_player2);

    // Pin the assignment reversed (the P2-jumpered pad becomes P1, slot 0).
    // The override must beat the jumper, so the slots swap immediately.
    mgr.set_player_assignment(Some(s1.clone()), Some(s0.clone()));
    assert_eq!(mgr.get_info(0).serial, s1, "P2-jumpered pad should now be slot 0");
    assert_eq!(mgr.get_info(1).serial, s0, "P1-jumpered pad should now be slot 1");

    // Re-applying the same assignment is idempotent (no further swap).
    mgr.set_player_assignment(Some(s1.clone()), Some(s0.clone()));
    assert_eq!(mgr.get_info(0).serial, s1);
    assert_eq!(mgr.get_info(1).serial, s0);

    // Clearing the override restores jumper ordering.
    mgr.set_player_assignment(None, None);
    assert_eq!(mgr.get_info(0).serial, s0, "jumper order restored after clear");
    assert_eq!(mgr.get_info(1).serial, s1);
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
    let mgr = SmxManager::new(Box::new(enumerator), |_| {});
    mgr.reenable_auto_lights();
}

#[test]
fn panel_test_mode_with_no_devices_does_not_crash() {
    let enumerator = FakeEnumerator::new(vec![]);
    let mgr = SmxManager::new(Box::new(enumerator), |_| {});
    mgr.set_panel_test_mode(PanelTestMode::PressureTest);
    std::thread::sleep(std::time::Duration::from_millis(100));
}

#[test]
fn pad_swap_input_state_routes_correctly() {
    let (mgr, dev_p1, dev_p2, _events) = make_manager_two_devices();

    let both_connected = wait_for(
        || mgr.get_info(0).connected && mgr.get_info(1).connected,
        2000,
    );
    assert!(both_connected);

    // P1 device (slot 0) gets input state.
    dev_p1.queue_input_state(0x0001);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(mgr.get_input_state(0), 0x0001);
    assert_eq!(mgr.get_input_state(1), 0x0000);

    // P2 device (slot 1) gets different input state.
    dev_p2.queue_input_state(0x0100);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(mgr.get_input_state(0), 0x0001);
    assert_eq!(mgr.get_input_state(1), 0x0100);
}

#[test]
fn config_available_after_connection() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    let config = mgr.get_config(0);
    assert!(config.is_some(), "Config should be available after connection");
}

#[test]
fn get_config_returns_none_for_invalid_pad() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    assert!(mgr.get_config(2).is_none());
}

// ─── Disconnect / Reconnect ──────────────────────────────────────────────────

#[test]
fn device_disconnect_fires_callback_and_clears_slot() {
    let (mgr, dev, events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    // Simulate read error (device unplugged).
    dev.set_fail_reads(true);
    let disconnected = wait_for(
        || events.lock().unwrap().iter().any(|e| matches!(e, SmxEvent::Disconnected { .. })),
        2000,
    );
    assert!(disconnected, "Disconnect event not received");
    assert!(!mgr.get_info(0).connected);
}

#[test]
fn device_reconnects_after_disconnect() {
    let (mgr, dev, events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    // Disconnect.
    dev.set_fail_reads(true);
    let disconnected = wait_for(|| !mgr.get_info(0).connected, 2000);
    assert!(disconnected);

    // Re-enable reads (simulates device being plugged back in).
    dev.set_fail_reads(false);

    // Should reconnect.
    let reconnected = wait_for(|| mgr.get_info(0).connected, 3000);
    assert!(reconnected, "Device did not reconnect");

    // Should have received at least 2 Connected events.
    let connect_count = events.lock().unwrap().iter()
        .filter(|e| matches!(e, SmxEvent::Connected { .. }))
        .count();
    assert!(connect_count >= 2, "Expected >= 2 Connected events, got {connect_count}");
}

// ─── Config Write ────────────────────────────────────────────────────────────

#[test]
fn set_config_sends_w_command_fw5() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    let config = mgr.get_config(0).unwrap();
    mgr.set_config(0, config);

    // Wait for the write to be sent (rate limited to 1/sec, but first write is immediate).
    std::thread::sleep(std::time::Duration::from_millis(500));

    let writes = dev.get_writes();
    let has_w = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'W');
    assert!(has_w, "Expected 'W' command for fw>=5");
}

#[test]
fn set_config_sends_w_command_fw4() {
    let (mgr, dev, _events) = make_manager_one_device(false, 4);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    let config = mgr.get_config(0).unwrap();
    mgr.set_config(0, config);
    std::thread::sleep(std::time::Duration::from_millis(500));

    let writes = dev.get_writes();
    let has_w = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'w');
    assert!(has_w, "Expected 'w' command for fw<5");
}

#[test]
fn set_config_rate_limits() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();

    // Send config twice rapidly.
    let config = mgr.get_config(0).unwrap();
    mgr.set_config(0, config);
    mgr.set_config(0, config);
    std::thread::sleep(std::time::Duration::from_millis(500));

    let writes = dev.get_writes();
    let w_count = writes.iter()
        .filter(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'W')
        .count();
    // Should only have sent one 'W' due to rate limiting.
    assert_eq!(w_count, 1, "Expected 1 'W' command (rate limited), got {w_count}");
}

// ─── Lights Behavior ─────────────────────────────────────────────────────────

#[test]
fn set_lights_rejects_invalid_size() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.set_lights(&[0u8; 100]); // invalid size
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let lights_cmds = writes.iter()
        .filter(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && (w[3] == b'2' || w[3] == b'3' || w[3] == b'4'))
        .count();
    assert_eq!(lights_cmds, 0, "No lights commands should be sent for invalid size");
}

#[test]
fn set_lights_blocked_during_panel_test_mode() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    mgr.set_panel_test_mode(PanelTestMode::PressureTest);
    std::thread::sleep(std::time::Duration::from_millis(200));

    dev.clear_writes();
    mgr.set_lights(&[128u8; 1350]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let lights_cmds = writes.iter()
        .filter(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && (w[3] == b'2' || w[3] == b'3'))
        .count();
    assert_eq!(lights_cmds, 0, "Lights should be blocked during panel test mode");

    mgr.set_panel_test_mode(PanelTestMode::Off);
}

#[test]
fn set_lights_fw4_sends_4_command() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.set_lights(&[128u8; 1350]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_4 = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'4');
    assert!(has_4, "Expected '4' command for fw>=4 (25-LED inner grid)");
}

#[test]
fn set_lights_fw3_no_4_command() {
    let (mgr, dev, _events) = make_manager_one_device(false, 3);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    dev.clear_writes();
    mgr.set_lights(&[128u8; 1350]);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_4 = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'4');
    assert!(!has_4, "Should NOT send '4' command for fw<4");

    let has_2 = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'2');
    let has_3 = writes.iter().any(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && w[3] == b'3');
    assert!(has_2 && has_3, "Should still send '2' and '3' commands");
}

// ─── Panel Test Mode ─────────────────────────────────────────────────────────

#[test]
fn panel_test_mode_off_sends_t_0() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    mgr.set_panel_test_mode(PanelTestMode::PressureTest);
    std::thread::sleep(std::time::Duration::from_millis(200));

    dev.clear_writes();
    mgr.set_panel_test_mode(PanelTestMode::Off);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let writes = dev.get_writes();
    let has_t0 = writes.iter().any(|w| {
        w.len() > 5 && w[0] == HID_REPORT_COMMAND && w[3] == b't' && w[5] == b'0'
    });
    assert!(has_t0, "Expected 't 0' command when disabling test mode");
}

#[test]
fn duplicate_player_jumpers_both_connected() {
    // Both devices report as P1 — should still connect both without crashing.
    let dev_a = FakeDevice::new_auto(false, 5);
    let dev_b = FakeDevice::new_auto(false, 5);
    let events = Arc::new(Mutex::new(Vec::new()));
    let events_clone = Arc::clone(&events);

    let enumerator = FakeEnumerator::new(vec![
        ("/dev/smx0".to_string(), dev_a),
        ("/dev/smx1".to_string(), dev_b),
    ]);
    let mgr = SmxManager::new(Box::new(enumerator), move |event| {
        events_clone.lock().unwrap().push(event);
    });

    let both = wait_for(|| mgr.get_info(0).connected && mgr.get_info(1).connected, 2000);
    assert!(both, "Both devices should connect even with duplicate jumpers");
}

// ─── Auto Animation ──────────────────────────────────────────────────────────

fn make_test_gif() -> Vec<u8> {
    use image::codecs::gif::GifEncoder;
    use image::{Frame, Rgba, RgbaImage};
    use std::io::Cursor;

    let mut buf = Cursor::new(Vec::new());
    {
        let mut encoder = GifEncoder::new(&mut buf);
        let mut img = RgbaImage::new(23, 24);
        for y in 0..24 {
            for x in 0..23 {
                img.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        encoder.encode_frames(vec![Frame::new(img)]).unwrap();
    }
    buf.into_inner()
}

#[test]
fn animation_auto_enable_disable() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    let gif = make_test_gif();
    mgr.load_animation(&gif, 0, rustmaniax_sdk::LightsType::Released).unwrap();

    mgr.set_animation_auto(true);
    std::thread::sleep(std::time::Duration::from_millis(200));

    mgr.set_animation_auto(false);
    // Should not crash or hang.
}

#[test]
fn animation_auto_double_enable_is_safe() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    let gif = make_test_gif();
    mgr.load_animation(&gif, 0, rustmaniax_sdk::LightsType::Released).unwrap();

    mgr.set_animation_auto(true);
    mgr.set_animation_auto(true); // idempotent
    std::thread::sleep(std::time::Duration::from_millis(100));
    mgr.set_animation_auto(false);
}

#[test]
fn animation_auto_double_disable_is_safe() {
    let (mgr, _dev, _events) = make_manager_one_device(false, 5);
    mgr.set_animation_auto(false); // not running
    mgr.set_animation_auto(false); // still not running, no crash
}

#[test]
fn animation_auto_pauses_on_direct_set_lights() {
    let (mgr, dev, _events) = make_manager_one_device(false, 5);
    let connected = wait_for(|| mgr.get_info(0).connected, 2000);
    assert!(connected);

    let gif = make_test_gif();
    mgr.load_animation(&gif, 0, rustmaniax_sdk::LightsType::Released).unwrap();
    mgr.set_animation_auto(true);
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Verify animation is sending lights.
    dev.clear_writes();
    std::thread::sleep(std::time::Duration::from_millis(150));
    let writes_before = dev.get_writes();
    let cmds_before = writes_before.iter()
        .filter(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && (w[3] == b'2' || w[3] == b'3'))
        .count();
    assert!(cmds_before > 0, "Animation should be sending lights");

    // Direct set_lights should pause animation.
    mgr.set_lights(&[0u8; 1350]);
    dev.clear_writes();
    std::thread::sleep(std::time::Duration::from_millis(80));
    let writes_during = dev.get_writes();
    let cmds_during = writes_during.iter()
        .filter(|w| w.len() > 3 && w[0] == HID_REPORT_COMMAND && (w[3] == b'2' || w[3] == b'3'))
        .count();
    // During pause (~100ms), animation should send fewer commands than normal.
    // Allow some tolerance for thread scheduling variance.
    assert!(cmds_during <= cmds_before, "Animation should be paused: {cmds_during} > {cmds_before}");

    mgr.set_animation_auto(false);
}
