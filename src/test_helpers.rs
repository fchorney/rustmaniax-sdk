//! Shared test infrastructure: fake HID devices and helpers.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::connection::{HidDevice, HidDeviceInfo, HidEnumerator};
use crate::error::SmxError;
use crate::protocol::{
    HID_REPORT_DATA, HID_REPORT_INPUT_STATE, PACKET_FLAG_DEVICE_INFO,
    PACKET_FLAG_END_OF_COMMAND, PACKET_FLAG_HOST_CMD_FINISHED, PACKET_FLAG_START_OF_COMMAND,
    SERIAL_SIZE,
};

/// Thread-safe fake HID device for testing.
#[derive(Clone)]
pub struct FakeDevice {
    inner: Arc<Mutex<FakeDeviceInner>>,
}

struct FakeDeviceInner {
    read_queue: VecDeque<Vec<u8>>,
    writes: Vec<Vec<u8>>,
    fail_reads: bool,
    fail_writes: bool,
}

impl FakeDevice {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeDeviceInner {
                read_queue: VecDeque::new(),
                writes: Vec::new(),
                fail_reads: false,
                fail_writes: false,
            })),
        }
    }

    /// Queue a raw HID packet to be returned by the next read.
    pub fn queue_read(&self, data: Vec<u8>) {
        self.inner.lock().unwrap().read_queue.push_back(data);
    }

    /// Get all writes that have been sent to this device.
    pub fn get_writes(&self) -> Vec<Vec<u8>> {
        self.inner.lock().unwrap().writes.clone()
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
        // DataInfoPacket layout: cmd(1) + packet_size(1) + player(1) + unused(1) + serial(16) + fw(2) + unused(1) = 23
        let mut payload = vec![0u8; 23];
        payload[0] = 0; // cmd
        payload[1] = 23; // packet_size
        payload[2] = if is_p2 { b'1' } else { b'0' }; // player
        payload[3] = 0; // unused
        payload[4..4 + SERIAL_SIZE].copy_from_slice(serial);
        let fw_bytes = firmware.to_le_bytes();
        payload[20] = fw_bytes[0];
        payload[21] = fw_bytes[1];
        payload[22] = 0; // unused

        let payload_len = payload.len();
        let mut pkt = vec![0u8; 3 + payload_len];
        pkt[0] = HID_REPORT_DATA;
        pkt[1] = PACKET_FLAG_DEVICE_INFO;
        pkt[2] = payload_len as u8;
        pkt[3..].copy_from_slice(&payload);
        self.queue_read(pkt);
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

    /// Queue a config response (for firmware v5+: 'G' + size + config bytes).
    pub fn queue_config_response(&self, config_bytes: &[u8]) {
        let mut payload = Vec::with_capacity(2 + config_bytes.len());
        payload.push(b'G');
        payload.push(config_bytes.len() as u8);
        payload.extend_from_slice(config_bytes);
        self.queue_report6(
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND,
            &payload,
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
        inner.writes.push(buf.to_vec());
        Ok(buf.len())
    }
}

/// Fake HID enumerator for testing.
pub struct FakeEnumerator {
    devices: Vec<(String, FakeDevice)>,
}

impl FakeEnumerator {
    pub fn new(devices: Vec<(String, FakeDevice)>) -> Self {
        Self { devices }
    }
}

impl HidEnumerator for FakeEnumerator {
    fn enumerate(&self, _vid: u16, _pid: u16) -> Vec<HidDeviceInfo> {
        self.devices
            .iter()
            .map(|(path, _)| HidDeviceInfo {
                path: path.clone(),
                product: "StepManiaX".to_string(),
            })
            .collect()
    }

    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError> {
        for (p, dev) in &self.devices {
            if p == path {
                return Ok(Box::new(dev.clone()));
            }
        }
        Err(SmxError::NotConnected)
    }
}
