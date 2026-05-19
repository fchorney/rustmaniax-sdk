//! Shared test infrastructure: fake HID devices and helpers.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::connection::{HidDevice, HidDeviceInfo, HidEnumerator};
use crate::error::SmxError;
use crate::protocol::{
    HID_REPORT_COMMAND, HID_REPORT_DATA, HID_REPORT_INPUT_STATE, PACKET_FLAG_DEVICE_INFO,
    PACKET_FLAG_END_OF_COMMAND, PACKET_FLAG_HOST_CMD_FINISHED, PACKET_FLAG_START_OF_COMMAND,
    SERIAL_SIZE,
};

/// Thread-safe fake HID device for testing.
/// Supports auto-responding to commands (for full-stack manager tests).
#[derive(Clone)]
pub struct FakeDevice {
    inner: Arc<Mutex<FakeDeviceInner>>,
}

struct FakeDeviceInner {
    read_queue: VecDeque<Vec<u8>>,
    writes: Vec<Vec<u8>>,
    fail_reads: bool,
    fail_writes: bool,
    capture_writes: bool,
    /// If set, auto-respond to 'G'/'g' commands with this config response.
    config_response: Option<Vec<Vec<u8>>>,
    /// If set, auto-respond to device info requests.
    device_info_response: Option<Vec<u8>>,
    /// If true, auto-ACK all non-config commands with HOST_CMD_FINISHED.
    auto_ack: bool,
}

impl Default for FakeDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeDevice {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeDeviceInner {
                read_queue: VecDeque::new(),
                writes: Vec::new(),
                fail_reads: false,
                fail_writes: false,
                capture_writes: true,
                config_response: None,
                device_info_response: None,
                auto_ack: false,
            })),
        }
    }

    /// Create a device that auto-responds to device info and config requests.
    /// This is the standard setup for full-stack manager tests.
    pub fn new_auto(is_p2: bool, firmware: u16) -> Self {
        let dev = Self::new();
        let serial: [u8; SERIAL_SIZE] = if is_p2 {
            [0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7,
             0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD, 0xBE, 0xBF]
        } else {
            [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
             0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF]
        };
        let info_pkt = make_device_info_packet(is_p2, firmware, &serial);
        let config_pkts = make_config_response_packets(firmware);

        let mut inner = dev.inner.lock().unwrap();
        inner.device_info_response = Some(info_pkt);
        inner.config_response = Some(config_pkts);
        inner.auto_ack = true;
        drop(inner);
        dev
    }

    /// Queue a raw HID packet to be returned by the next read.
    pub fn queue_read(&self, data: Vec<u8>) {
        self.inner.lock().unwrap().read_queue.push_back(data);
    }

    /// Get all writes that have been sent to this device.
    pub fn get_writes(&self) -> Vec<Vec<u8>> {
        self.inner.lock().unwrap().writes.clone()
    }

    /// Clear captured writes.
    pub fn clear_writes(&self) {
        self.inner.lock().unwrap().writes.clear();
    }

    /// Set whether reads should fail.
    pub fn set_fail_reads(&self, fail: bool) {
        self.inner.lock().unwrap().fail_reads = fail;
    }

    /// Set whether writes should fail.
    pub fn set_fail_writes(&self, fail: bool) {
        self.inner.lock().unwrap().fail_writes = fail;
    }

    /// Queue a Report 3 (input state) packet.
    pub fn queue_input_state(&self, state: u16) {
        let mut pkt = vec![0u8; 3];
        pkt[0] = HID_REPORT_INPUT_STATE;
        pkt[1] = (state & 0xFF) as u8;
        pkt[2] = (state >> 8) as u8;
        self.queue_read(pkt);
    }

    /// Queue a device info response.
    pub fn queue_device_info_response(&self, is_p2: bool, firmware: u16, serial: &[u8; SERIAL_SIZE]) {
        let pkt = make_device_info_packet(is_p2, firmware, serial);
        // Wrap in Report 6 format.
        let payload_len = pkt.len();
        let mut report = vec![0u8; 3 + payload_len];
        report[0] = HID_REPORT_DATA;
        report[1] = PACKET_FLAG_DEVICE_INFO;
        report[2] = payload_len as u8;
        report[3..].copy_from_slice(&pkt);
        self.queue_read(report);
    }

    /// Queue a Report 6 response with given flags and payload.
    pub fn queue_report6(&self, flags: u8, payload: &[u8]) {
        let mut pkt = vec![0u8; 3 + payload.len()];
        pkt[0] = HID_REPORT_DATA;
        pkt[1] = flags;
        pkt[2] = payload.len() as u8;
        pkt[3..].copy_from_slice(payload);
        self.queue_read(pkt);
    }

    /// Queue a complete single-packet command response (START|END|HOST_CMD_FINISHED).
    pub fn queue_command_response(&self, payload: &[u8]) {
        self.queue_report6(
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND | PACKET_FLAG_HOST_CMD_FINISHED,
            payload,
        );
    }
}

impl HidDevice for FakeDevice {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_reads {
            return Err(SmxError::InvalidPacket);
        }
        match inner.read_queue.pop_front() {
            Some(data) => {
                let len = data.len().min(buf.len());
                buf[..len].copy_from_slice(&data[..len]);
                Ok(len)
            }
            None => Ok(0),
        }
    }

    fn write(&self, buf: &[u8]) -> Result<usize, SmxError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_writes {
            return Err(SmxError::InvalidPacket);
        }
        if inner.capture_writes {
            inner.writes.push(buf.to_vec());
        }

        // Auto-respond logic (for full-stack tests).
        if buf.len() >= 4 && buf[0] == HID_REPORT_COMMAND {
            let flags = buf[1];

            // Device info request.
            if flags & PACKET_FLAG_DEVICE_INFO != 0 {
                if let Some(ref info_pkt) = inner.device_info_response {
                    let payload_len = info_pkt.len();
                    let mut report = vec![0u8; 3 + payload_len];
                    report[0] = HID_REPORT_DATA;
                    report[1] = PACKET_FLAG_DEVICE_INFO;
                    report[2] = payload_len as u8;
                    report[3..].copy_from_slice(info_pkt);
                    inner.read_queue.push_back(report);
                }
                return Ok(buf.len());
            }

            // Regular command — check payload.
            if buf[2] >= 1 {
                let cmd_byte = buf[3];
                if (cmd_byte == b'G' || cmd_byte == b'g') && inner.config_response.is_some() {
                    let pkts = inner.config_response.clone().unwrap();
                    for pkt in pkts {
                        inner.read_queue.push_back(pkt);
                    }
                } else if inner.auto_ack {
                    // ACK with HOST_CMD_FINISHED.
                    let ack = vec![HID_REPORT_DATA, PACKET_FLAG_HOST_CMD_FINISHED, 0];
                    inner.read_queue.push_back(ack);
                }
            }
        }

        Ok(buf.len())
    }
}

/// Fake HID enumerator for testing.
pub struct FakeEnumerator {
    devices: Arc<Mutex<Vec<(String, FakeDevice, bool)>>>, // (path, device, opened)
}

impl FakeEnumerator {
    pub fn new(devices: Vec<(String, FakeDevice)>) -> Self {
        let entries = devices.into_iter().map(|(p, d)| (p, d, false)).collect();
        Self {
            devices: Arc::new(Mutex::new(entries)),
        }
    }
}

impl HidEnumerator for FakeEnumerator {
    fn enumerate(&self, _vid: u16, _pid: u16) -> Vec<HidDeviceInfo> {
        let devices = self.devices.lock().unwrap();
        devices
            .iter()
            .map(|(path, _, _)| HidDeviceInfo {
                path: path.clone(),
                product: "StepManiaX".to_string(),
            })
            .collect()
    }

    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError> {
        let mut devices = self.devices.lock().unwrap();
        for (p, dev, opened) in devices.iter_mut() {
            if p == path && !*opened {
                *opened = true;
                return Ok(Box::new(dev.clone()));
            }
        }
        Err(SmxError::NotConnected)
    }
}

// ─── Packet Building Helpers ─────────────────────────────────────────────────

/// Build raw device info payload (23 bytes matching DataInfoPacket).
fn make_device_info_packet(is_p2: bool, firmware: u16, serial: &[u8; SERIAL_SIZE]) -> Vec<u8> {
    let mut payload = vec![0u8; 23];
    payload[0] = 0; // cmd
    payload[1] = 23; // packet_size
    payload[2] = if is_p2 { b'1' } else { b'0' };
    payload[3] = 0; // unused
    payload[4..4 + SERIAL_SIZE].copy_from_slice(serial);
    let fw_bytes = firmware.to_le_bytes();
    payload[20] = fw_bytes[0];
    payload[21] = fw_bytes[1];
    payload[22] = 0; // unused
    payload
}

/// Build config response packets for auto-response.
/// Produces properly-sized packets (max 64 bytes each, matching real HID behavior).
fn make_config_response_packets(firmware: u16) -> Vec<Vec<u8>> {
    let cmd_byte = if firmware >= 5 { b'G' } else { b'g' };
    // Use a minimal config size that fits in a single HID packet payload.
    // Real configs are 250 bytes and would be fragmented, but for testing
    // we use a small config that proves the handshake works.
    let config_size: u8 = 40;

    let mut payload = Vec::with_capacity(2 + config_size as usize);
    payload.push(cmd_byte);
    payload.push(config_size);
    payload.resize(2 + config_size as usize, 0);

    // Single packet that fits within HID_MAX_PAYLOAD_SIZE (61 bytes).
    let mut pkt = vec![0u8; 3 + payload.len()];
    pkt[0] = HID_REPORT_DATA;
    pkt[1] = PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND | PACKET_FLAG_HOST_CMD_FINISHED;
    pkt[2] = payload.len() as u8;
    pkt[3..].copy_from_slice(&payload);
    vec![pkt]
}

// ─── Test Utility ────────────────────────────────────────────────────────────

/// Polls a condition until true or timeout (default 2s).
pub fn wait_for(cond: impl Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while !cond() {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    true
}
