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
    api: hidapi::HidApi,
}

impl HidapiEnumerator {
    pub fn new() -> Result<Self, SmxError> {
        Ok(Self {
            api: hidapi::HidApi::new()?,
        })
    }
}

impl HidEnumerator for HidapiEnumerator {
    fn enumerate(&self, vid: u16, pid: u16) -> Vec<HidDeviceInfo> {
        self.api
            .device_list()
            .filter(|d| d.vendor_id() == vid && d.product_id() == pid)
            .map(|d| HidDeviceInfo {
                path: d.path().to_string_lossy().into_owned(),
                product: d.product_string().unwrap_or_default().to_string(),
            })
            .collect()
    }

    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError> {
        let dev = self.api.open_path(&std::ffi::CString::new(path).unwrap())?;
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
    input_callback: Option<Box<dyn Fn() + Send>>,
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
                        && self.input_callback.is_some()
                    {
                        (self.input_callback.as_ref().unwrap())();
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

                if host_cmd_finished {
                    if let Some(cmd) = self.current_command.take() {
                        // Give the callback the current reassembled data.
                        let completed = self.reassembler.take_completed();
                        let response = completed.into_iter().last().unwrap_or_default();
                        if let Some(cb) = cmd.callback {
                            cb(response);
                        }
                    }
                } else {
                    // Queue any fully reassembled packets for read_packet().
                    for packet in self.reassembler.take_completed() {
                        self.read_buffers.push_back(packet);
                    }
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
    input_callback: Option<Box<dyn Fn() + Send>>,
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
unsafe impl Send for SharedHidDevice {}
