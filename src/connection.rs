use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::error::SmxError;
use crate::protocol::{
    self, HID_PACKET_SIZE, HID_REPORT_DATA, HID_REPORT_INPUT_STATE, COMMAND_TIMEOUT_SECONDS,
    SERIAL_SIZE,
};

// ─── HID Trait Abstraction ───────────────────────────────────────────────────

/// Abstract interface for a single HID device connection.
/// Production code uses the hidapi implementation; tests inject a fake.
pub trait HidDevice: Send {
    /// Non-blocking read. Returns number of bytes read, 0 if no data, or error.
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError>;
    /// Write a packet. Returns number of bytes written or error.
    fn write(&self, buf: &[u8]) -> Result<usize, SmxError>;
}

/// Information about a discovered HID device.
#[derive(Clone, Debug)]
pub struct HidDeviceInfo {
    pub path: String,
    pub product: String,
}

/// Abstract interface for HID device enumeration and opening.
pub trait HidEnumerator: Send {
    fn enumerate(&self, vid: u16, pid: u16) -> Vec<HidDeviceInfo>;
    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError>;
}

// ─── Real hidapi Implementation ──────────────────────────────────────────────

/// HID device backed by the `hidapi` crate.
pub struct HidapiDevice {
    dev: hidapi::HidDevice,
}

impl HidDevice for HidapiDevice {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError> {
        Ok(self.dev.read(buf)?)
    }

    fn write(&self, buf: &[u8]) -> Result<usize, SmxError> {
        Ok(self.dev.write(buf)?)
    }
}

/// HID enumerator backed by the `hidapi` crate.
pub struct HidapiEnumerator {
    api: std::sync::Arc<std::sync::Mutex<hidapi::HidApi>>,
}

impl HidapiEnumerator {
    /// Creates a new enumerator with its own `HidApi` instance.
    pub fn new() -> Result<Self, SmxError> {
        Ok(Self {
            api: std::sync::Arc::new(std::sync::Mutex::new(hidapi::HidApi::new()?)),
        })
    }

    /// Creates an enumerator sharing an existing `HidApi` instance.
    /// Use this when the host application (e.g., deadsync) already owns a `HidApi`.
    pub fn from_shared(api: std::sync::Arc<std::sync::Mutex<hidapi::HidApi>>) -> Self {
        Self { api }
    }
}

impl HidEnumerator for HidapiEnumerator {
    fn enumerate(&self, vid: u16, pid: u16) -> Vec<HidDeviceInfo> {
        let mut api = self.api.lock().unwrap();
        // Refresh to pick up newly connected/disconnected devices.
        let _ = api.refresh_devices();
        api.device_list()
            .filter(|d| d.vendor_id() == vid && d.product_id() == pid)
            .map(|d| HidDeviceInfo {
                path: d.path().to_string_lossy().into_owned(),
                product: d.product_string().unwrap_or_default().to_string(),
            })
            .collect()
    }

    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError> {
        let api = self.api.lock().unwrap();
        let dev = api.open_path(&std::ffi::CString::new(path).unwrap())?;
        dev.set_blocking_mode(false)?;
        Ok(Box::new(HidapiDevice { dev }))
    }
}

// ─── Device Info ─────────────────────────────────────────────────────────────

/// Immutable device information retrieved on connection.
#[derive(Clone, Debug, Default)]
pub struct SmxDeviceInfo {
    pub is_player2: bool,
    pub serial: String,
    pub firmware_version: u16,
}

/// Wire format for the device info response.
#[repr(C, packed)]
struct DataInfoPacket {
    cmd: u8,
    packet_size: u8,
    player: u8,
    _unused2: u8,
    serial: [u8; SERIAL_SIZE],
    firmware_version: u16,
    _unused3: u8,
}

fn parse_device_info(payload: &[u8]) -> SmxDeviceInfo {
    if payload.len() < size_of::<DataInfoPacket>() {
        return SmxDeviceInfo::default();
    }
    // Safe: we checked length, and the struct is packed POD.
    let packet: DataInfoPacket = unsafe { std::ptr::read_unaligned(payload.as_ptr().cast()) };
    SmxDeviceInfo {
        is_player2: packet.player == b'1',
        serial: hex_encode(&packet.serial),
        firmware_version: packet.firmware_version,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ─── Shared State (between PollHandle and CommandHandle) ─────────────────────

/// State shared between the USB polling thread and the main I/O thread.
struct SharedState {
    /// Current panel press bitmask. Written by poll thread, read by main thread.
    input_state: AtomicU16,
    /// Whether to fire the input callback on every packet (not just changes).
    always_fire_input: AtomicBool,
    /// Set by poll thread if a read error occurs.
    had_read_error: AtomicBool,
    /// Report 6 packets buffered by poll thread, consumed by main thread.
    report6_buffer: Mutex<Vec<u8>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            input_state: AtomicU16::new(0),
            always_fire_input: AtomicBool::new(false),
            had_read_error: AtomicBool::new(false),
            report6_buffer: Mutex::new(Vec::new()),
        }
    }
}

// ─── PollHandle (USB polling thread side) ────────────────────────────────────

/// Handle used by the USB polling thread. Reads raw HID data and dispatches it.
///
/// Report 3 (input state) is parsed inline and stored atomically.
/// Report 6 (command/config) is buffered for the main thread.
pub struct PollHandle {
    device: Box<dyn HidDevice>,
    shared: Arc<SharedState>,
    input_callback: Option<Box<dyn Fn(u16) + Send>>,
}

impl PollHandle {
    /// Polls for available USB data. Returns true if Report 6 data was buffered.
    pub fn poll(&self) -> bool {
        if self.shared.had_read_error.load(Ordering::Relaxed) {
            return false;
        }

        let mut report6_local: Vec<u8> = Vec::new();
        let mut buf = [0u8; HID_PACKET_SIZE];

        loop {
            let n = match self.device.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => {
                    self.shared.had_read_error.store(true, Ordering::Relaxed);
                    return false;
                }
            };

            if n < 1 {
                continue;
            }

            match buf[0] {
                HID_REPORT_INPUT_STATE => {
                    if n < 3 {
                        continue;
                    }
                    let new_state = u16::from_le_bytes([buf[1], buf[2]]);
                    let old_state = self.shared.input_state.load(Ordering::Relaxed);
                    let changed = old_state != new_state;
                    if changed {
                        self.shared.input_state.store(new_state, Ordering::Relaxed);
                    }
                    if (changed
                        || self.shared.always_fire_input.load(Ordering::Relaxed))
                        && let Some(cb) = self.input_callback.as_ref()
                    {
                        cb(new_state);
                    }
                }
                HID_REPORT_DATA => {
                    if n < 3 {
                        continue;
                    }
                    let payload_len = buf[2] as usize;
                    let packet_len = 3 + payload_len;
                    if n < packet_len {
                        continue;
                    }
                    report6_local.extend_from_slice(&buf[..packet_len]);
                }
                _ => {}
            }
        }

        if !report6_local.is_empty() {
            let mut locked = self.shared.report6_buffer.lock().unwrap();
            locked.extend_from_slice(&report6_local);
            true
        } else {
            false
        }
    }

    /// Returns the current input state.
    pub fn input_state(&self) -> u16 {
        self.shared.input_state.load(Ordering::Relaxed)
    }
}

// ─── CommandHandle (main I/O thread side) ────────────────────────────────────

/// Completion callback type for commands.
pub type CommandCallback = Box<dyn FnOnce(Vec<u8>) + Send>;

/// A command pending transmission or awaiting response.
struct PendingCommand {
    /// Pre-built HID packets (each 64 bytes) to send sequentially.
    packets: Vec<[u8; HID_PACKET_SIZE]>,
    /// Callback invoked with the response (or empty vec on cancel/error).
    callback: Option<CommandCallback>,
    /// True if this is a device info request.
    is_device_info: bool,
    /// True if packets have been sent and we're awaiting a response.
    sent: bool,
    /// When the command was sent (for timeout detection).
    sent_at: Option<Instant>,
}

/// Handle used by the main I/O thread. Sends commands and processes responses.
pub struct CommandHandle {
    device: Box<dyn HidDevice>,
    shared: Arc<SharedState>,
    path: String,

    // Connection state.
    active: bool,
    got_info: bool,
    device_info: SmxDeviceInfo,

    // Packet reassembly.
    reassembler: protocol::PacketReassembler,
    read_buffers: VecDeque<Vec<u8>>,

    // Command queue.
    pending_commands: VecDeque<PendingCommand>,
    current_command: Option<PendingCommand>,
}

impl CommandHandle {
    /// Returns the HID device path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns true if device info has been received.
    pub fn is_connected_with_info(&self) -> bool {
        self.got_info
    }

    /// Returns the cached device info.
    pub fn device_info(&self) -> &SmxDeviceInfo {
        &self.device_info
    }

    /// Returns the current input state (reads the shared atomic).
    pub fn input_state(&self) -> u16 {
        self.shared.input_state.load(Ordering::Relaxed)
    }

    /// Sets whether the device is active (sending input updates).
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// Returns whether the device is active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Sets whether the input callback fires on every packet.
    pub fn set_always_fire_input(&self, always: bool) {
        self.shared.always_fire_input.store(always, Ordering::Relaxed);
    }

    /// Returns true if the USB polling thread encountered a read error.
    pub fn has_read_error(&self) -> bool {
        self.shared.had_read_error.load(Ordering::Relaxed)
    }

    /// Queues a command for transmission.
    pub fn send_command(&mut self, cmd: &[u8], callback: Option<CommandCallback>) {
        let packets = protocol::build_command_packets(cmd);
        self.pending_commands.push_back(PendingCommand {
            packets,
            callback,
            is_device_info: false,
            sent: false,
            sent_at: None,
        });
    }

    /// Like `send_command`, but inserts at the FRONT of the queue so it is sent
    /// ahead of already-queued commands (after any in-flight one finishes).
    ///
    /// Lights and sensor-test polling share this single command pipeline. Light
    /// frames enqueue several commands at 30Hz, so a FIFO sensor request waits
    /// behind that backlog (measured ~100ms request->response with lights vs
    /// ~16ms without). Latency-sensitive requests use this to stay prompt
    /// without reducing the light rate.
    pub fn send_command_priority(&mut self, cmd: &[u8], callback: Option<CommandCallback>) {
        let packets = protocol::build_command_packets(cmd);
        self.pending_commands.push_front(PendingCommand {
            packets,
            callback,
            is_device_info: false,
            sent: false,
            sent_at: None,
        });
    }

    /// Reads a completed response packet from the buffer.
    pub fn read_packet(&mut self) -> Option<Vec<u8>> {
        self.read_buffers.pop_front()
    }

    /// Processes I/O: consumes buffered Report 6 data and sends pending commands.
    /// Returns an error if the device should be disconnected.
    pub fn update(&mut self) -> Result<(), SmxError> {
        if self.shared.had_read_error.load(Ordering::Relaxed) {
            return Err(SmxError::NotConnected);
        }
        self.check_reads();
        self.check_writes()?;
        Ok(())
    }

    /// Cancels all pending commands, invoking callbacks with empty data.
    pub fn close(&mut self) {
        if let Some(cmd) = self.current_command.take()
            && let Some(cb) = cmd.callback
        {
            cb(Vec::new());
        }
        for cmd in self.pending_commands.drain(..) {
            if let Some(cb) = cmd.callback {
                cb(Vec::new());
            }
        }
        self.read_buffers.clear();
        self.active = false;
        self.got_info = false;
    }

    // ─── Private ─────────────────────────────────────────────────────────────

    fn check_reads(&mut self) {
        // Check command timeout.
        if let Some(ref cmd) = self.current_command
            && cmd.sent
            && let Some(sent_at) = cmd.sent_at
            && sent_at.elapsed().as_secs_f64() > COMMAND_TIMEOUT_SECONDS
        {
            log::warn!("Command timed out, retrying");
            let mut cmd = self.current_command.take().unwrap();
            cmd.sent = false;
            cmd.sent_at = None;
            self.pending_commands.push_front(cmd);
        }

        // Swap out the report6 buffer.
        let data = {
            let mut locked = self.shared.report6_buffer.lock().unwrap();
            std::mem::take(&mut *locked)
        };

        // Process packets from the buffer.
        let mut offset = 0;
        while offset + 3 <= data.len() {
            let payload_len = data[offset + 2] as usize;
            let packet_len = 3 + payload_len;
            if offset + packet_len > data.len() {
                break;
            }

            self.handle_packet(&data[offset..offset + packet_len]);
            offset += packet_len;
        }

        // Put back any unprocessed remainder.
        if offset < data.len() {
            let mut locked = self.shared.report6_buffer.lock().unwrap();
            let remainder = &data[offset..];
            locked.splice(0..0, remainder.iter().copied());
        }
    }

    fn handle_packet(&mut self, raw: &[u8]) {
        let Some(parsed) = protocol::parse_report6(raw) else {
            return;
        };

        match &parsed {
            protocol::ParsedPacket::DeviceInfo(payload) => {
                // Only handle if we're expecting a device info response.
                let is_info_cmd = self
                    .current_command
                    .as_ref()
                    .is_some_and(|c| c.is_device_info);
                if !is_info_cmd {
                    return;
                }

                self.device_info = parse_device_info(payload);
                self.got_info = true;

                log::info!(
                    "Device info: fw={}, P{}, serial={}",
                    self.device_info.firmware_version,
                    if self.device_info.is_player2 { 2 } else { 1 },
                    self.device_info.serial
                );

                let cmd = self.current_command.take().unwrap();
                if let Some(cb) = cmd.callback {
                    cb(payload.clone());
                }
            }
            protocol::ParsedPacket::Fragment { .. } => {
                if !self.active {
                    return;
                }

                let host_cmd_finished = self.reassembler.push(&parsed);

                // Queue any fully reassembled packets for read_packet().
                for packet in self.reassembler.take_completed() {
                    self.read_buffers.push_back(packet);
                }

                // If host_cmd_finished, clear the current command and invoke callback.
                if host_cmd_finished
                    && let Some(cmd) = self.current_command.take()
                        && let Some(cb) = cmd.callback {
                            let response = self.read_buffers.back().cloned().unwrap_or_default();
                            cb(response);
                        }
            }
        }
    }

    fn check_writes(&mut self) -> Result<(), SmxError> {
        if self.current_command.is_some() {
            return Ok(());
        }
        if self.pending_commands.is_empty() {
            return Ok(());
        }

        let mut cmd = self.pending_commands.pop_front().unwrap();

        for packet in &cmd.packets {
            let written = self.device.write(packet)?;
            if written == 0 {
                // Write failed — cancel command.
                if let Some(cb) = cmd.callback {
                    cb(Vec::new());
                }
                return Err(SmxError::Hid(hidapi::HidError::IncompleteSendError {
                    sent: 0,
                    all: packet.len(),
                }));
            }
        }

        cmd.sent = true;
        cmd.sent_at = Some(Instant::now());
        self.current_command = Some(cmd);
        Ok(())
    }

    fn request_device_info(&mut self, callback: Option<CommandCallback>) {
        let packet = protocol::build_device_info_request();
        self.pending_commands.push_back(PendingCommand {
            packets: vec![packet],
            callback,
            is_device_info: true,
            sent: false,
            sent_at: None,
        });
    }
}

// ─── Connection Constructor ──────────────────────────────────────────────────

/// Opens a connection to an SMX device and returns the split handles.
///
/// `PollHandle` is meant for the USB polling thread.
/// `CommandHandle` is meant for the main I/O thread.
/// Both share atomic state for input and a mutex-protected buffer for Report 6 data.
pub fn open_connection(
    path: String,
    device: Box<dyn HidDevice>,
    input_callback: Option<Box<dyn Fn(u16) + Send>>,
) -> Result<(PollHandle, CommandHandle), SmxError> {
    // We need two device handles — one for each thread.
    // However, hidapi doesn't support cloning a device handle.
    // The C++ code uses a single handle from both threads (read is non-blocking).
    // In Rust, we need the HidDevice to be Send but we can't share a single
    // Box<dyn HidDevice> across two threads without Arc<Mutex<>>.
    //
    // Solution: The caller provides a single device. We wrap it in an Arc<Mutex<>>
    // internally so both handles can access it. The poll thread does short non-blocking
    // reads and the main thread does writes — contention is minimal.
    let shared_device = Arc::new(Mutex::new(device));
    let shared = Arc::new(SharedState::new());

    let poll_handle = PollHandle {
        device: Box::new(SharedHidDevice(Arc::clone(&shared_device))),
        shared: Arc::clone(&shared),
        input_callback,
    };

    let mut cmd_handle = CommandHandle {
        device: Box::new(SharedHidDevice(Arc::clone(&shared_device))),
        shared,
        path,
        active: false,
        got_info: false,
        device_info: SmxDeviceInfo::default(),
        reassembler: protocol::PacketReassembler::new(),
        read_buffers: VecDeque::new(),
        pending_commands: VecDeque::new(),
        current_command: None,
    };

    // Automatically request device info on open.
    cmd_handle.request_device_info(None);

    Ok((poll_handle, cmd_handle))
}

/// Wrapper that implements HidDevice by locking a shared Arc<Mutex<Box<dyn HidDevice>>>.
struct SharedHidDevice(Arc<Mutex<Box<dyn HidDevice>>>);

impl HidDevice for SharedHidDevice {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError> {
        self.0.lock().unwrap().read(buf)
    }

    fn write(&self, buf: &[u8]) -> Result<usize, SmxError> {
        self.0.lock().unwrap().write(buf)
    }
}

// SharedHidDevice contains Arc<Mutex<..>> which is Send, so this is safe.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        HID_REPORT_COMMAND, PACKET_FLAG_END_OF_COMMAND, PACKET_FLAG_HOST_CMD_FINISHED,
        PACKET_FLAG_START_OF_COMMAND, SERIAL_SIZE,
    };
    use crate::test_helpers::FakeDevice;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn open_fake(device: FakeDevice) -> (PollHandle, CommandHandle) {
        open_connection(
            "/dev/fake".to_string(),
            Box::new(device.clone()),
            None,
        )
        .unwrap()
    }

    fn open_fake_with_callback(
        device: FakeDevice,
        cb: Box<dyn Fn(u16) + Send>,
    ) -> (PollHandle, CommandHandle) {
        open_connection("/dev/fake".to_string(), Box::new(device.clone()), Some(cb)).unwrap()
    }

    #[test]
    fn input_state_updated_from_report3() {
        let dev = FakeDevice::new();
        dev.queue_input_state(0x0010);
        let (poll, cmd) = open_fake(dev);
        poll.poll();
        assert_eq!(cmd.input_state(), 0x0010);
    }

    #[test]
    fn input_state_multiple_panels() {
        let dev = FakeDevice::new();
        dev.queue_input_state(0x0111);
        let (poll, cmd) = open_fake(dev);
        poll.poll();
        assert_eq!(cmd.input_state(), 0x0111);
    }

    #[test]
    fn input_callback_fires_on_change() {
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&count);
        let dev = FakeDevice::new();
        dev.queue_input_state(0x0001);
        dev.queue_input_state(0x0001); // duplicate

        let (poll, _cmd) = open_fake_with_callback(
            dev,
            Box::new(move |_| { count_clone.fetch_add(1, Ordering::Relaxed); }),
        );
        poll.poll();
        // Only fires once for the change from 0 to 0x0001, not for duplicate.
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn input_callback_always_fire_mode() {
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = Arc::clone(&count);
        let dev = FakeDevice::new();
        dev.queue_input_state(0x0001);
        dev.queue_input_state(0x0001); // duplicate

        let (poll, cmd) = open_fake_with_callback(
            dev,
            Box::new(move |_| { count_clone.fetch_add(1, Ordering::Relaxed); }),
        );
        cmd.set_always_fire_input(true);
        poll.poll();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn poll_returns_false_when_no_data() {
        let dev = FakeDevice::new();
        let (poll, _cmd) = open_fake(dev.clone());
        assert!(!poll.poll());
        assert!(dev.clone().read(&mut [0; 64]).is_ok());
    }

    #[test]
    fn read_error_propagates() {
        let dev = FakeDevice::new();
        let (poll, cmd) = open_fake(dev.clone());
        dev.set_fail_reads(true);
        poll.poll();
        assert!(cmd.has_read_error());
    }

    #[test]
    fn close_resets_state() {
        let dev = FakeDevice::new();
        dev.queue_input_state(0x00FF);
        let (poll, mut cmd) = open_fake(dev);
        poll.poll();
        assert_eq!(cmd.input_state(), 0x00FF);
        cmd.close();
        // Input state is in shared atomic — still readable but device is closed.
        assert!(!cmd.is_connected_with_info());
    }

    #[test]
    fn report6_single_packet_queued() {
        let dev = FakeDevice::new();
        dev.queue_report6(
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND,
            b"AB",
        );
        let (poll, mut cmd) = open_fake(dev);
        cmd.set_active(true);
        poll.poll();
        cmd.update().unwrap();
        let pkt = cmd.read_packet().unwrap();
        assert_eq!(pkt, b"AB");
    }

    #[test]
    fn report6_multi_fragment_reassembly() {
        let dev = FakeDevice::new();
        dev.queue_report6(PACKET_FLAG_START_OF_COMMAND, b"Hello");
        dev.queue_report6(PACKET_FLAG_END_OF_COMMAND, b" W");
        let (poll, mut cmd) = open_fake(dev);
        cmd.set_active(true);
        poll.poll();
        cmd.update().unwrap();
        let pkt = cmd.read_packet().unwrap();
        assert_eq!(pkt, b"Hello W");
    }

    #[test]
    fn report6_start_clears_partial() {
        let dev = FakeDevice::new();
        // Partial packet (no END).
        dev.queue_report6(PACKET_FLAG_START_OF_COMMAND, b"old");
        // New START discards partial, then END completes.
        dev.queue_report6(PACKET_FLAG_START_OF_COMMAND, b"new");
        dev.queue_report6(PACKET_FLAG_END_OF_COMMAND, b"!");
        let (poll, mut cmd) = open_fake(dev);
        cmd.set_active(true);
        poll.poll();
        cmd.update().unwrap();
        let pkt = cmd.read_packet().unwrap();
        assert_eq!(pkt, b"new!");
    }

    #[test]
    fn report6_not_queued_when_inactive() {
        let dev = FakeDevice::new();
        dev.queue_report6(
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND,
            b"data",
        );
        let (poll, mut cmd) = open_fake(dev);
        // Don't set active.
        poll.poll();
        cmd.update().unwrap();
        assert!(cmd.read_packet().is_none());
    }

    #[test]
    fn device_info_handshake() {
        let dev = FakeDevice::new();
        let serial = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        let (poll, mut cmd) = open_fake(dev.clone());
        assert!(!cmd.is_connected_with_info());

        // First update sends the device info request.
        cmd.update().unwrap();

        // Now queue the response (simulating device replying).
        dev.queue_device_info_response(false, 7, &serial);
        poll.poll();
        cmd.update().unwrap();

        assert!(cmd.is_connected_with_info());
        let info = cmd.device_info();
        assert!(!info.is_player2);
        assert_eq!(info.firmware_version, 7);
        assert_eq!(info.serial, "0102030405060708090a0b0c0d0e0f10");
    }

    #[test]
    fn device_info_player2() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];

        let (poll, mut cmd) = open_fake(dev.clone());
        cmd.update().unwrap();

        dev.queue_device_info_response(true, 3, &serial);
        poll.poll();
        cmd.update().unwrap();

        let info = cmd.device_info();
        assert!(info.is_player2);
        assert_eq!(info.firmware_version, 3);
    }

    #[test]
    fn send_command_writes_packets() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev.clone());
        // Complete handshake first.
        cmd.update().unwrap();
        poll.poll();
        cmd.update().unwrap();

        cmd.send_command(b"hello", None);
        cmd.update().unwrap();

        let writes = dev.get_writes();
        // First write is device info request, second is our command.
        assert!(writes.len() >= 2);
        let cmd_pkt = &writes[1];
        assert_eq!(cmd_pkt[0], HID_REPORT_COMMAND);
        assert_eq!(
            cmd_pkt[1],
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND
        );
        assert_eq!(cmd_pkt[2], 5);
        assert_eq!(&cmd_pkt[3..8], b"hello");
    }

    #[test]
    fn command_callback_fires_on_response() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev.clone());
        cmd.update().unwrap();
        poll.poll();
        cmd.update().unwrap();
        cmd.set_active(true);

        let response = Arc::new(Mutex::new(Vec::new()));
        let resp_clone = Arc::clone(&response);
        cmd.send_command(b"G", Some(Box::new(move |data| {
            *resp_clone.lock().unwrap() = data;
        })));
        cmd.update().unwrap(); // sends command

        // Queue response with HOST_CMD_FINISHED.
        dev.queue_report6(
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND | PACKET_FLAG_HOST_CMD_FINISHED,
            b"Gcfg",
        );
        poll.poll();
        cmd.update().unwrap();

        assert_eq!(*response.lock().unwrap(), b"Gcfg");
    }

    #[test]
    fn commands_serialized() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev.clone());
        cmd.update().unwrap();
        poll.poll();
        cmd.update().unwrap();

        // Queue two commands.
        cmd.send_command(b"A", None);
        cmd.send_command(b"B", None);
        cmd.update().unwrap(); // sends first

        let writes = dev.get_writes();
        // Should only have device info + first command so far.
        let cmd_writes: Vec<_> = writes.iter().skip(1).collect();
        assert_eq!(cmd_writes.len(), 1);
        assert_eq!(cmd_writes[0][3], b'A');
    }

    #[test]
    fn close_invokes_pending_callbacks() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev);
        cmd.update().unwrap();
        poll.poll();
        cmd.update().unwrap();

        let called = Arc::new(AtomicU32::new(0));
        let called_clone = Arc::clone(&called);
        cmd.send_command(b"X", Some(Box::new(move |data| {
            if data.is_empty() {
                called_clone.fetch_add(1, Ordering::Relaxed);
            }
        })));
        cmd.update().unwrap(); // sends it (now it's current_command)

        cmd.close();
        assert_eq!(called.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn write_error_returns_error() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev.clone());
        cmd.update().unwrap();
        poll.poll();
        cmd.update().unwrap();

        dev.set_fail_writes(true);
        cmd.send_command(b"X", None);
        let result = cmd.update();
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod auto_tests {
    use super::*;
    use crate::test_helpers::FakeDevice;

    #[test]
    fn auto_device_connects() {
        let dev = FakeDevice::new_auto(false, 5);
        let (poll, mut cmd) = open_connection(
            "/dev/test".to_string(),
            Box::new(dev),
            None,
        ).unwrap();

        // Cycle: update sends request, write triggers auto-response, poll reads it, update processes.
        cmd.update().unwrap();
        poll.poll();
        cmd.update().unwrap();
        assert!(cmd.is_connected_with_info(), "device info not received");
        assert_eq!(cmd.device_info().firmware_version, 5);
    }
}
