use std::time::Instant;

use bytemuck::Zeroable;

use crate::config::SmxConfig;
use crate::connection::CommandHandle;
use crate::protocol::{
    CONFIG_WRITE_RATE_LIMIT_SECONDS, NUM_PANELS, SENSOR_TEST_TIMEOUT_SECONDS,
};

/// Sensor test modes for reading raw/calibrated sensor values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SensorTestMode {
    Off = 0,
    UncalibratedValues = b'0',
    CalibratedValues = b'1',
    Noise = b'2',
    Tare = b'3',
}

/// Callback reason flags for device state changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateReason {
    Updated,
    InputState,
    Connected,
    Disconnected,
    ConfigUpdated,
    SensorTestData,
}

/// Event emitted by the SDK with all relevant data attached.
/// The callback receives this directly — no need to call back into the manager.
#[derive(Clone, Debug)]
pub enum SmxEvent {
    Connected { pad: usize, info: SmxInfo },
    Disconnected { pad: usize },
    InputState { pad: usize, state: u16 },
    ConfigUpdated { pad: usize },
    SensorTestData { pad: usize, data: SensorTestData },
}

/// Public device info (equivalent to C++ SMXInfo).
#[derive(Clone, Debug, Default)]
pub struct SmxInfo {
    pub connected: bool,
    pub is_player2: bool,
    pub has_serial_number: bool,
    pub serial: String,
    pub firmware_version: u16,
}

/// Sensor test mode data returned per device.
#[derive(Clone, Debug, Default)]
pub struct SensorTestData {
    pub have_data_from_panel: [bool; NUM_PANELS],
    pub sensor_level: [[i16; 4]; NUM_PANELS],
    pub bad_sensor_input: [[bool; 4]; NUM_PANELS],
    pub dip_switch_per_panel: [u8; NUM_PANELS],
    pub bad_jumper: [[bool; 4]; NUM_PANELS],
}

/// High-level per-controller state and logic.
pub struct SmxDevice {
    pad_index: usize,
    update_callback: Option<Box<dyn Fn(SmxEvent) + Send>>,
    connection: Option<CommandHandle>,

    // Config state.
    config: SmxConfig,
    wanted_config: SmxConfig,
    have_config: bool,
    send_config: bool,
    sending_config: bool,
    delay_config_until: Option<Instant>,

    // Sensor test state.
    sensor_test_mode: SensorTestMode,
    waiting_for_sensor_response: SensorTestMode,
    sensor_request_sent_at: Option<Instant>,
    sensor_test_data: SensorTestData,
    have_sensor_test_data: bool,
}

impl SmxDevice {
    pub fn new(pad_index: usize) -> Self {
        Self {
            pad_index,
            update_callback: None,
            connection: None,
            config: SmxConfig::zeroed(),
            wanted_config: SmxConfig::zeroed(),
            have_config: false,
            send_config: false,
            sending_config: false,
            delay_config_until: None,
            sensor_test_mode: SensorTestMode::Off,
            waiting_for_sensor_response: SensorTestMode::Off,
            sensor_request_sent_at: None,
            sensor_test_data: SensorTestData::default(),
            have_sensor_test_data: false,
        }
    }

    pub fn set_update_callback(&mut self, cb: Box<dyn Fn(SmxEvent) + Send>) {
        self.update_callback = Some(cb);
    }

    pub fn set_connection(&mut self, conn: CommandHandle) {
        self.connection = Some(conn);
    }

    pub fn take_connection(&mut self) -> Option<CommandHandle> {
        self.connection.take()
    }

    pub fn has_connection(&self) -> bool {
        self.connection.is_some()
    }

    pub fn connection(&self) -> Option<&CommandHandle> {
        self.connection.as_ref()
    }

    pub fn connection_mut(&mut self) -> Option<&mut CommandHandle> {
        self.connection.as_mut()
    }

    /// Returns true if fully connected (device info received + config available).
    pub fn is_connected(&self) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|c| c.is_connected_with_info() && self.have_config)
    }

    pub fn get_info(&self) -> SmxInfo {
        let Some(conn) = &self.connection else {
            return SmxInfo::default();
        };
        if !conn.is_connected_with_info() || !self.have_config {
            return SmxInfo::default();
        }
        let di = conn.device_info();
        let has_serial = di.serial.chars().any(|c| c != '0' && c != 'f');
        SmxInfo {
            connected: true,
            is_player2: di.is_player2,
            has_serial_number: has_serial,
            serial: di.serial.clone(),
            firmware_version: di.firmware_version,
        }
    }

    pub fn input_state(&self) -> u16 {
        self.connection.as_ref().map_or(0, |c| c.input_state())
    }

    pub fn get_config(&self) -> Option<SmxConfig> {
        if !self.is_connected() {
            return None;
        }
        Some(if self.send_config { self.wanted_config } else { self.config })
    }

    pub fn set_config(&mut self, config: SmxConfig) {
        if !self.is_connected() {
            return;
        }
        self.wanted_config = config;
        self.send_config = true;
    }

    pub fn set_sensor_test_mode(&mut self, mode: SensorTestMode) {
        self.sensor_test_mode = mode;
    }

    pub fn get_test_data(&self) -> Option<&SensorTestData> {
        if self.have_sensor_test_data {
            Some(&self.sensor_test_data)
        } else {
            None
        }
    }

    pub fn factory_reset(&mut self) {
        let Some(conn) = &mut self.connection else { return };
        if !conn.is_connected_with_info() {
            return;
        }
        let fw = conn.device_info().firmware_version;
        conn.send_command(b"f\n", None);
        conn.send_command(if fw >= 5 { b"G" } else { b"g\n" }, None);
    }

    pub fn force_recalibration(&mut self) {
        let Some(conn) = &mut self.connection else { return };
        conn.send_command(b"C\n", None);
    }

    pub fn close(&mut self) {
        if let Some(conn) = &mut self.connection {
            conn.close();
        }
        self.connection = None;
        self.have_config = false;
        self.call_update(UpdateReason::Disconnected);
    }

    /// Main update loop, called from the I/O thread.
    pub fn update(&mut self) -> Result<(), crate::error::SmxError> {
        let Some(conn) = &self.connection else {
            return Ok(());
        };
        if !conn.is_connected_with_info() {
            // Still need to process I/O for the handshake to complete.
            let conn = self.connection.as_mut().unwrap();
            conn.update()?;
            return Ok(());
        }

        self.check_active();
        self.send_config_if_needed();
        self.update_sensor_test_mode();

        let conn = self.connection.as_mut().unwrap();
        conn.update()?;
        self.handle_packets();
        Ok(())
    }

    // ─── Private ─────────────────────────────────────────────────────────────

    fn call_update(&self, reason: UpdateReason) {
        if let Some(cb) = &self.update_callback {
            let event = match reason {
                UpdateReason::Connected => SmxEvent::Connected {
                    pad: self.pad_index,
                    info: self.get_info(),
                },
                UpdateReason::Disconnected => SmxEvent::Disconnected { pad: self.pad_index },
                UpdateReason::ConfigUpdated => SmxEvent::ConfigUpdated { pad: self.pad_index },
                UpdateReason::SensorTestData => SmxEvent::SensorTestData {
                    pad: self.pad_index,
                    data: self.sensor_test_data.clone(),
                },
                UpdateReason::InputState => SmxEvent::InputState {
                    pad: self.pad_index,
                    state: self.input_state(),
                },
                UpdateReason::Updated => return,
            };
            cb(event);
        }
    }

    fn check_active(&mut self) {
        let conn = self.connection.as_mut().unwrap();
        if !conn.is_connected_with_info() || conn.is_active() {
            return;
        }
        conn.set_active(true);
        let fw = conn.device_info().firmware_version;
        conn.send_command(if fw >= 5 { b"G" } else { b"g\n" }, None);
    }

    fn handle_packets(&mut self) {
        // Collect all packets first to avoid borrow conflict.
        let mut packets = Vec::new();
        let conn = self.connection.as_mut().unwrap();
        while let Some(buf) = conn.read_packet() {
            if !buf.is_empty() {
                packets.push(buf);
            }
        }
        for buf in packets {
            match buf[0] {
                b'y' => self.handle_sensor_test_response(&buf),
                b'g' | b'G' => self.handle_config_packet(&buf),
                _ => {}
            }
        }
    }

    fn handle_config_packet(&mut self, buf: &[u8]) {
        if buf.len() < 2 {
            return;
        }
        let size = buf[1] as usize;
        if buf.len() < size + 2 {
            return;
        }
        let payload = &buf[2..2 + size];

        if buf[0] == b'g' {
            // Legacy config — convert to new format.
            let mut padded = payload.to_vec();
            padded.resize(size_of::<OldSmxConfig>(), 0);
            convert_to_new_config(&padded, &mut self.config);
        } else {
            // New config — copy directly.
            let copy_len = size.min(size_of::<SmxConfig>());
            let config_bytes: &mut [u8] = bytemuck::bytes_of_mut(&mut self.config);
            config_bytes[..copy_len].copy_from_slice(&payload[..copy_len]);
        }

        self.have_config = true;
        self.call_update(UpdateReason::ConfigUpdated);
    }

    fn send_config_if_needed(&mut self) {
        if !self.is_connected() || !self.send_config || self.sending_config {
            return;
        }

        // Rate limit.
        let now = Instant::now();
        if let Some(delay_until) = self.delay_config_until
            && now < delay_until
        {
            return;
        }
        self.delay_config_until =
            Some(now + std::time::Duration::from_secs_f64(CONFIG_WRITE_RATE_LIMIT_SECONDS));

        let conn = self.connection.as_mut().unwrap();
        let fw = conn.device_info().firmware_version;

        let cmd_data = if fw >= 5 {
            let config_bytes = bytemuck::bytes_of(&self.wanted_config);
            let mut data = Vec::with_capacity(2 + config_bytes.len());
            data.push(b'W');
            data.push(config_bytes.len() as u8);
            data.extend_from_slice(config_bytes);
            data
        } else {
            let config_bytes = bytemuck::bytes_of(&self.config);
            let mut old_data = config_bytes.to_vec();
            convert_to_old_config(&self.wanted_config, &mut old_data);
            let mut data = Vec::with_capacity(2 + old_data.len());
            data.push(b'w');
            data.push(old_data.len() as u8);
            data.extend_from_slice(&old_data);
            data
        };

        self.sending_config = true;
        // We can't capture `&mut self` in the callback, so we just clear the flag
        // when we process the next update cycle. The C++ version uses a raw pointer
        // capture which we avoid here.
        conn.send_command(&cmd_data, None);
        self.send_config = false;
        self.sending_config = false;

        // Update cached config optimistically.
        self.config = self.wanted_config;

        // Read back to verify.
        conn.send_command(if fw >= 5 { b"G" } else { b"g\n" }, None);
    }

    fn update_sensor_test_mode(&mut self) {
        if self.sensor_test_mode == SensorTestMode::Off {
            return;
        }

        if self.waiting_for_sensor_response != SensorTestMode::Off
            && let Some(sent_at) = self.sensor_request_sent_at
            && sent_at.elapsed().as_secs_f64() < SENSOR_TEST_TIMEOUT_SECONDS
        {
            return;
        }

        self.waiting_for_sensor_response = self.sensor_test_mode;
        self.sensor_request_sent_at = Some(Instant::now());

        let cmd = [b'y', self.sensor_test_mode as u8, b'\n'];
        let conn = self.connection.as_mut().unwrap();
        conn.send_command(&cmd, None);
    }

    fn handle_sensor_test_response(&mut self, buf: &[u8]) {
        if buf.len() < 3 {
            return;
        }

        let mode_byte = buf[1];
        let size = buf[2] as usize;
        if buf.len() < size * 2 + 3 {
            return;
        }

        let mode = match mode_byte {
            b'0' => SensorTestMode::UncalibratedValues,
            b'1' => SensorTestMode::CalibratedValues,
            b'2' => SensorTestMode::Noise,
            b'3' => SensorTestMode::Tare,
            _ => return,
        };

        if self.waiting_for_sensor_response == SensorTestMode::Off {
            return;
        }
        if mode != self.waiting_for_sensor_response {
            return;
        }
        self.waiting_for_sensor_response = SensorTestMode::Off;

        if mode != self.sensor_test_mode {
            return;
        }

        // Parse interleaved u16 data.
        let mut data = Vec::with_capacity(size);
        for i in (3..size * 2 + 3).step_by(2) {
            let value = u16::from_le_bytes([buf[i], buf[i + 1]]);
            data.push(value);
        }

        let fw = self.connection.as_ref().unwrap().device_info().firmware_version;
        self.sensor_test_data = parse_sensor_test_data(&data, fw);
        self.have_sensor_test_data = true;
        self.call_update(UpdateReason::SensorTestData);
    }
}

// ─── Sensor Test Data Parsing ────────────────────────────────────────────────

/// Wire format for per-panel sensor test data (extracted bit-by-bit).
#[repr(C, packed)]
struct DetailData {
    // Byte 0: sig1:1, sig2:1, sig3:1, bad_sensor[4]:4, dummy:1
    flags: u8,
    // Bytes 1-8: sensors[4] as i16
    sensors: [i16; 4],
    // Byte 9: dip:4, bad_sensor_dip[4]:4
    dip_flags: u8,
}

const DETAIL_DATA_SIZE: usize = size_of::<DetailData>();

fn parse_sensor_test_data(data: &[u16], firmware_version: u16) -> SensorTestData {
    let mut output = SensorTestData::default();

    for panel in 0..NUM_PANELS {
        // Extract bits for this panel from interleaved data.
        let mut raw = [0u8; DETAIL_DATA_SIZE];
        for (i, byte) in raw.iter_mut().enumerate() {
            let mut result = 0u8;
            for j in 0..8 {
                let bit_index = i * 8 + j;
                if bit_index < data.len() {
                    result |= (((data[bit_index] >> panel) & 1) as u8) << j;
                }
            }
            *byte = result;
        }

        // Parse the extracted bytes.
        let sig1 = raw[0] & 1;
        let sig2 = (raw[0] >> 1) & 1;
        let sig3 = (raw[0] >> 2) & 1;

        if sig1 != 0 || sig2 != 1 || sig3 != 0 {
            output.have_data_from_panel[panel] = false;
            continue;
        }
        output.have_data_from_panel[panel] = true;

        output.bad_sensor_input[panel][0] = (raw[0] >> 3) & 1 != 0;
        output.bad_sensor_input[panel][1] = (raw[0] >> 4) & 1 != 0;
        output.bad_sensor_input[panel][2] = (raw[0] >> 5) & 1 != 0;
        output.bad_sensor_input[panel][3] = (raw[0] >> 6) & 1 != 0;

        // Parse sensor values (i16 little-endian, bytes 1-8).
        for s in 0..4 {
            let offset = 1 + s * 2;
            output.sensor_level[panel][s] =
                i16::from_le_bytes([raw[offset], raw[offset + 1]]);
        }

        // DIP switch and bad jumper flags (byte 9).
        output.dip_switch_per_panel[panel] = raw[9] & 0x0F;
        output.bad_jumper[panel][0] = (raw[9] >> 4) & 1 != 0;
        output.bad_jumper[panel][1] = (raw[9] >> 5) & 1 != 0;
        output.bad_jumper[panel][2] = (raw[9] >> 6) & 1 != 0;
        output.bad_jumper[panel][3] = (raw[9] >> 7) & 1 != 0;

        // Disable bad sensor flags for FSRs (firmware v5+, activates spuriously).
        if firmware_version >= 5 {
            output.bad_sensor_input[panel] = [false; 4];
        }
    }

    output
}

// ─── Legacy Config Conversion ────────────────────────────────────────────────

/// Old config format used by firmware < v5.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct OldSmxConfig {
    unused1: u8, unused2: u8,
    unused3: u8, unused4: u8,
    unused5: u8, unused6: u8,
    master_debounce_ms: u16,
    panel_threshold_7_low: u8, panel_threshold_7_high: u8,
    panel_threshold_4_low: u8, panel_threshold_4_high: u8,
    panel_threshold_2_low: u8, panel_threshold_2_high: u8,
    panel_debounce_us: u16,
    auto_calibration_period_ms: u16,
    auto_calibration_max_deviation: u8,
    bad_sensor_minimum_delay_seconds: u8,
    auto_calibration_averages_per_update: u16,
    unused7: u8, unused8: u8,
    panel_threshold_1_low: u8, panel_threshold_1_high: u8,
    enabled_sensors: [u8; 5],
    auto_lights_timeout: u8,
    step_color: [u8; 27],
    panel_rotation: u8,
    auto_calibration_samples_per_average: u16,
    master_version: u8,
    config_version: u8,
    unused9: [u8; 10],
    panel_threshold_0_low: u8, panel_threshold_0_high: u8,
    panel_threshold_3_low: u8, panel_threshold_3_high: u8,
    panel_threshold_5_low: u8, panel_threshold_5_high: u8,
    panel_threshold_6_low: u8, panel_threshold_6_high: u8,
    panel_threshold_8_low: u8, panel_threshold_8_high: u8,
    debounce_delay_ms: u16,
    padding: [u8; 164],
}

fn convert_to_new_config(old_bytes: &[u8], config: &mut SmxConfig) {
    assert!(old_bytes.len() >= size_of::<OldSmxConfig>());
    let old: OldSmxConfig = unsafe { std::ptr::read_unaligned(old_bytes.as_ptr().cast()) };

    config.debounce_nodelay_ms = old.master_debounce_ms;

    config.panel_settings[7].load_cell_low_threshold = old.panel_threshold_7_low;
    config.panel_settings[7].load_cell_high_threshold = old.panel_threshold_7_high;
    config.panel_settings[4].load_cell_low_threshold = old.panel_threshold_4_low;
    config.panel_settings[4].load_cell_high_threshold = old.panel_threshold_4_high;
    config.panel_settings[2].load_cell_low_threshold = old.panel_threshold_2_low;
    config.panel_settings[2].load_cell_high_threshold = old.panel_threshold_2_high;

    config.panel_debounce_us = old.panel_debounce_us;
    config.auto_calibration_max_deviation = old.auto_calibration_max_deviation;
    config.bad_sensor_minimum_delay_seconds = old.bad_sensor_minimum_delay_seconds;
    config.auto_calibration_averages_per_update = old.auto_calibration_averages_per_update;

    config.panel_settings[1].load_cell_low_threshold = old.panel_threshold_1_low;
    config.panel_settings[1].load_cell_high_threshold = old.panel_threshold_1_high;

    config.enabled_sensors = old.enabled_sensors;
    config.auto_lights_timeout = old.auto_lights_timeout;
    config.step_color = old.step_color;
    config.panel_rotation = old.panel_rotation;
    config.auto_calibration_samples_per_average = old.auto_calibration_samples_per_average;

    if old.config_version == 0xFF {
        return;
    }

    config.master_version = old.master_version;
    config.config_version = old.config_version;

    if old.config_version < 2 {
        return;
    }

    config.panel_settings[0].load_cell_low_threshold = old.panel_threshold_0_low;
    config.panel_settings[0].load_cell_high_threshold = old.panel_threshold_0_high;
    config.panel_settings[3].load_cell_low_threshold = old.panel_threshold_3_low;
    config.panel_settings[3].load_cell_high_threshold = old.panel_threshold_3_high;
    config.panel_settings[5].load_cell_low_threshold = old.panel_threshold_5_low;
    config.panel_settings[5].load_cell_high_threshold = old.panel_threshold_5_high;
    config.panel_settings[6].load_cell_low_threshold = old.panel_threshold_6_low;
    config.panel_settings[6].load_cell_high_threshold = old.panel_threshold_6_high;
    config.panel_settings[8].load_cell_low_threshold = old.panel_threshold_8_low;
    config.panel_settings[8].load_cell_high_threshold = old.panel_threshold_8_high;

    if old.config_version < 3 {
        return;
    }

    config.debounce_delay_ms = old.debounce_delay_ms;
}

fn convert_to_old_config(new_config: &SmxConfig, old_data: &mut Vec<u8>) {
    if old_data.len() < 128 {
        old_data.resize(128, 0xFF);
    }

    // Ensure we have enough space for the full struct.
    if old_data.len() < size_of::<OldSmxConfig>() {
        old_data.resize(size_of::<OldSmxConfig>(), 0);
    }

    let old: &mut OldSmxConfig = unsafe { &mut *(old_data.as_mut_ptr().cast()) };

    old.master_debounce_ms = new_config.debounce_nodelay_ms;

    old.panel_threshold_7_low = new_config.panel_settings[7].load_cell_low_threshold;
    old.panel_threshold_7_high = new_config.panel_settings[7].load_cell_high_threshold;
    old.panel_threshold_4_low = new_config.panel_settings[4].load_cell_low_threshold;
    old.panel_threshold_4_high = new_config.panel_settings[4].load_cell_high_threshold;
    old.panel_threshold_2_low = new_config.panel_settings[2].load_cell_low_threshold;
    old.panel_threshold_2_high = new_config.panel_settings[2].load_cell_high_threshold;

    old.panel_debounce_us = new_config.panel_debounce_us;
    old.auto_calibration_max_deviation = new_config.auto_calibration_max_deviation;
    old.bad_sensor_minimum_delay_seconds = new_config.bad_sensor_minimum_delay_seconds;
    old.auto_calibration_averages_per_update = new_config.auto_calibration_averages_per_update;

    old.panel_threshold_1_low = new_config.panel_settings[1].load_cell_low_threshold;
    old.panel_threshold_1_high = new_config.panel_settings[1].load_cell_high_threshold;

    old.enabled_sensors = new_config.enabled_sensors;
    old.auto_lights_timeout = new_config.auto_lights_timeout;
    old.step_color = new_config.step_color;
    old.panel_rotation = new_config.panel_rotation;
    old.auto_calibration_samples_per_average = new_config.auto_calibration_samples_per_average;

    old.master_version = new_config.master_version;
    old.config_version = new_config.config_version;

    old.panel_threshold_0_low = new_config.panel_settings[0].load_cell_low_threshold;
    old.panel_threshold_0_high = new_config.panel_settings[0].load_cell_high_threshold;
    old.panel_threshold_3_low = new_config.panel_settings[3].load_cell_low_threshold;
    old.panel_threshold_3_high = new_config.panel_settings[3].load_cell_high_threshold;
    old.panel_threshold_5_low = new_config.panel_settings[5].load_cell_low_threshold;
    old.panel_threshold_5_high = new_config.panel_settings[5].load_cell_high_threshold;
    old.panel_threshold_6_low = new_config.panel_settings[6].load_cell_low_threshold;
    old.panel_threshold_6_high = new_config.panel_settings[6].load_cell_high_threshold;
    old.panel_threshold_8_low = new_config.panel_settings[8].load_cell_low_threshold;
    old.panel_threshold_8_high = new_config.panel_settings[8].load_cell_high_threshold;

    old.debounce_delay_ms = new_config.debounce_delay_ms;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn sensor_test_signature_check() {
        // All zeros → sig2 should be 1 for valid data, so all-zero means invalid.
        let data = vec![0u16; 80];
        let result = parse_sensor_test_data(&data, 5);
        for panel in 0..NUM_PANELS {
            assert!(!result.have_data_from_panel[panel]);
        }
    }

    #[test]
    fn old_config_roundtrip() {
        let mut config = SmxConfig::zeroed();
        config.config_version = 3;
        config.master_version = 4;
        config.debounce_nodelay_ms = 100;
        config.panel_settings[7].load_cell_low_threshold = 42;
        config.panel_settings[0].load_cell_high_threshold = 99;
        config.debounce_delay_ms = 200;

        let mut old_data = Vec::new();
        convert_to_old_config(&config, &mut old_data);

        let mut result = SmxConfig::zeroed();
        convert_to_new_config(&old_data, &mut result);

        let debounce_nodelay = result.debounce_nodelay_ms;
        let threshold_7_low = result.panel_settings[7].load_cell_low_threshold;
        let threshold_0_high = result.panel_settings[0].load_cell_high_threshold;
        let debounce_delay = result.debounce_delay_ms;
        let version = result.config_version;

        assert_eq!(debounce_nodelay, 100);
        assert_eq!(threshold_7_low, 42);
        assert_eq!(threshold_0_high, 99);
        assert_eq!(debounce_delay, 200);
        assert_eq!(version, 3);
    }

    #[test]
    fn convert_version_0xff_copies_base_only() {
        let mut old = vec![0u8; size_of::<OldSmxConfig>()];
        // Set config_version to 0xFF (uninitialized).
        let old_struct: &mut OldSmxConfig = unsafe { &mut *(old.as_mut_ptr().cast()) };
        old_struct.config_version = 0xFF;
        old_struct.master_debounce_ms = 500;
        old_struct.panel_threshold_7_low = 77;
        old_struct.panel_threshold_0_low = 99; // v2 field — should NOT be copied

        let mut config = SmxConfig::zeroed();
        convert_to_new_config(&old, &mut config);

        let debounce = config.debounce_nodelay_ms;
        let t7 = config.panel_settings[7].load_cell_low_threshold;
        let t0 = config.panel_settings[0].load_cell_low_threshold;
        // masterVersion should remain as zeroed (not copied from 0xFF version).
        let mv = config.master_version;

        assert_eq!(debounce, 500);
        assert_eq!(t7, 77);
        assert_eq!(t0, 0); // v2 field not copied
        assert_eq!(mv, 0); // not copied for 0xFF version
    }

    #[test]
    fn convert_version_1_skips_v2_panels() {
        let mut old = vec![0u8; size_of::<OldSmxConfig>()];
        let old_struct: &mut OldSmxConfig = unsafe { &mut *(old.as_mut_ptr().cast()) };
        old_struct.config_version = 1;
        old_struct.master_version = 2;
        old_struct.panel_threshold_7_low = 50;
        old_struct.panel_threshold_0_low = 88; // v2 field

        let mut config = SmxConfig::zeroed();
        convert_to_new_config(&old, &mut config);

        let t7 = config.panel_settings[7].load_cell_low_threshold;
        let t0 = config.panel_settings[0].load_cell_low_threshold;
        let cv = config.config_version;
        let mv = config.master_version;

        assert_eq!(t7, 50);
        assert_eq!(t0, 0); // v2 field not copied for version 1
        assert_eq!(cv, 1);
        assert_eq!(mv, 2);
    }

    #[test]
    fn convert_version_2_copies_all_panels_but_not_debounce_delay() {
        let mut old = vec![0u8; size_of::<OldSmxConfig>()];
        let old_struct: &mut OldSmxConfig = unsafe { &mut *(old.as_mut_ptr().cast()) };
        old_struct.config_version = 2;
        old_struct.master_version = 3;
        old_struct.panel_threshold_0_low = 10;
        old_struct.panel_threshold_3_low = 30;
        old_struct.panel_threshold_5_low = 50;
        old_struct.panel_threshold_6_low = 60;
        old_struct.panel_threshold_8_low = 80;
        old_struct.debounce_delay_ms = 999; // v3 field

        let mut config = SmxConfig::zeroed();
        convert_to_new_config(&old, &mut config);

        assert_eq!(config.panel_settings[0].load_cell_low_threshold, 10);
        assert_eq!(config.panel_settings[3].load_cell_low_threshold, 30);
        assert_eq!(config.panel_settings[5].load_cell_low_threshold, 50);
        assert_eq!(config.panel_settings[6].load_cell_low_threshold, 60);
        assert_eq!(config.panel_settings[8].load_cell_low_threshold, 80);
        let dd = config.debounce_delay_ms;
        assert_eq!(dd, 0); // v3 field not copied for version 2
    }

    #[test]
    fn convert_version_3_copies_debounce_delay() {
        let mut old = vec![0u8; size_of::<OldSmxConfig>()];
        let old_struct: &mut OldSmxConfig = unsafe { &mut *(old.as_mut_ptr().cast()) };
        old_struct.config_version = 3;
        old_struct.master_version = 4;
        old_struct.debounce_delay_ms = 250;
        old_struct.panel_threshold_0_low = 11;

        let mut config = SmxConfig::zeroed();
        convert_to_new_config(&old, &mut config);

        let dd = config.debounce_delay_ms;
        assert_eq!(dd, 250);
        assert_eq!(config.panel_settings[0].load_cell_low_threshold, 11);
    }

    #[test]
    fn convert_enabled_sensors_and_step_color() {
        let mut old = vec![0u8; size_of::<OldSmxConfig>()];
        let old_struct: &mut OldSmxConfig = unsafe { &mut *(old.as_mut_ptr().cast()) };
        old_struct.config_version = 1;
        old_struct.master_version = 1;
        old_struct.enabled_sensors = [0x1F, 0x0A, 0x00, 0xFF, 0x55];
        old_struct.step_color[0] = 100;
        old_struct.step_color[26] = 200;

        let mut config = SmxConfig::zeroed();
        convert_to_new_config(&old, &mut config);

        assert_eq!(config.enabled_sensors, [0x1F, 0x0A, 0x00, 0xFF, 0x55]);
        assert_eq!(config.step_color[0], 100);
        assert_eq!(config.step_color[26], 200);
    }

    #[test]
    fn convert_to_old_extends_small_buffer() {
        let config = SmxConfig::zeroed();
        let mut old_data = vec![0u8; 10]; // too small
        convert_to_old_config(&config, &mut old_data);
        assert!(old_data.len() >= 128);
    }

    #[test]
    fn get_config_returns_none_when_not_connected() {
        let device = SmxDevice::new(0);
        assert!(device.get_config().is_none());
    }

    #[test]
    fn set_config_does_nothing_when_not_connected() {
        let mut device = SmxDevice::new(0);
        device.set_config(SmxConfig::zeroed()); // should not panic
        assert!(device.get_config().is_none());
    }

    #[test]
    fn get_config_returns_pending_after_set() {
        let fake = crate::test_helpers::FakeDevice::new_auto(false, 5);
        let (poll, cmd) = crate::connection::open_connection(
            "/dev/test".to_string(),
            Box::new(fake),
            None,
        ).unwrap();

        let mut device = SmxDevice::new(0);
        device.set_connection(cmd);

        // Connect.
        for _ in 0..20 {
            device.update().unwrap();
            poll.poll();
            if device.is_connected() { break; }
        }
        assert!(device.is_connected());

        // Set a modified config.
        let mut cfg = device.get_config().unwrap();
        cfg.auto_lights_timeout = 99;
        device.set_config(cfg);

        // get_config should return the pending (optimistic) value.
        let readback = device.get_config().unwrap();
        assert_eq!(readback.auto_lights_timeout, 99);
    }

    #[test]
    fn factory_reset_does_nothing_when_not_connected() {
        let mut device = SmxDevice::new(0);
        device.factory_reset(); // should not panic
    }

    #[test]
    fn force_recalibration_does_nothing_when_not_connected() {
        let mut device = SmxDevice::new(0);
        device.force_recalibration(); // should not panic
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::connection;
    use crate::test_helpers::FakeDevice;

    #[test]
    fn device_connects_with_auto_fake() {
        let fake = FakeDevice::new_auto(false, 5);
        let (poll, cmd) = connection::open_connection(
            "/dev/test".to_string(),
            Box::new(fake),
            None,
        )
        .unwrap();

        let mut device = SmxDevice::new(0);
        device.set_connection(cmd);

        // Simulate the manager loop: update then poll, repeat.
        for _ in 0..20 {
            device.update().unwrap();
            poll.poll();
            if device.is_connected() {
                break;
            }
        }
        assert!(device.is_connected(), "Device did not connect");
        let info = device.get_info();
        assert_eq!(info.firmware_version, 5);
        assert!(!info.is_player2);
    }
}
