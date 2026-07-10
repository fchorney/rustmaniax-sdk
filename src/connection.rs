use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::SmxError;
use crate::protocol::{
    self, COMMAND_TIMEOUT_SECONDS, HID_PACKET_SIZE, HID_REPORT_DATA, HID_REPORT_INPUT_STATE,
    SERIAL_SIZE,
};

// ─── HID Trait Abstraction ───────────────────────────────────────────────────

/// Abstract interface for a single HID device connection.
/// Production code uses the hidapi implementation; tests inject a fake.
pub trait HidDevice: Send {
    /// Non-blocking read. Returns number of bytes read, 0 if no data, or error.
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError>;
    /// Blocking read that waits up to `timeout_ms` for a packet, then returns the
    /// bytes read (0 if it timed out with no data). A negative timeout blocks
    /// indefinitely. This is what makes input reads interrupt-driven: the kernel
    /// wakes the caller the moment the device delivers data instead of returning
    /// immediately to a poll-and-sleep loop.
    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, SmxError>;
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

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, SmxError> {
        Ok(self.dev.read_timeout(buf, timeout_ms)?)
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

/// State shared between the USB polling thread, the USB writer thread, and the
/// main I/O thread.
struct SharedState {
    /// Current panel press bitmask. Written by poll thread, read by main thread.
    input_state: AtomicU16,
    /// Whether to fire the input callback on every packet (not just changes).
    always_fire_input: AtomicBool,
    /// Set by poll thread if a read error occurs.
    had_read_error: AtomicBool,
    /// Set by the writer thread if a write fails; surfaced as a disconnect on
    /// the next `update()`, mirroring `had_read_error`.
    had_write_error: AtomicBool,
    /// A command's packets have been handed to the writer thread and are not
    /// fully on the wire yet. While set, the command stays un-`sent` (its
    /// response timeout hasn't started) and `wait_writes_idle` blocks.
    write_in_flight: AtomicBool,
    /// Report 6 packets buffered by poll thread, consumed by main thread.
    report6_buffer: Mutex<Vec<u8>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            input_state: AtomicU16::new(0),
            always_fire_input: AtomicBool::new(false),
            had_read_error: AtomicBool::new(false),
            had_write_error: AtomicBool::new(false),
            write_in_flight: AtomicBool::new(false),
            report6_buffer: Mutex::new(Vec::new()),
        }
    }
}

// ─── Writer thread ───────────────────────────────────────────────────────────

/// One command's pre-built HID packets, handed to the writer thread.
type WriteJob = Vec<[u8; HID_PACKET_SIZE]>;

/// USB writer thread body: performs the blocking HID writes for one command at
/// a time, so no caller (in particular the manager main thread, which runs
/// under the manager state lock) ever blocks on the wire. `hid_write` to an
/// interrupt OUT endpoint takes on the order of milliseconds per packet; with
/// panel lights streaming those writes used to hold the manager state lock in
/// bursts, stalling every `get_info`-style query on other threads (measured as
/// visible frame drops in menus that poll pad state each frame).
///
/// The main thread hands over at most one command's packets at a time
/// (`write_in_flight`), preserving the one-command-in-flight pacing. A failed
/// write sets `had_write_error` and exits; the main thread turns that into a
/// disconnect on its next `update()`, exactly like a poll-thread read error.
fn writer_loop(
    device: Box<dyn HidDevice>,
    rx: Receiver<WriteJob>,
    shared: Arc<SharedState>,
    write_done_notify: Option<Box<dyn Fn() + Send>>,
) {
    while let Ok(packets) = rx.recv() {
        for packet in &packets {
            let failed = match device.write(packet) {
                Ok(0) => true,
                Ok(_) => false,
                Err(_) => true,
            };
            if failed {
                shared.had_write_error.store(true, Ordering::Release);
                break;
            }
        }
        shared.write_in_flight.store(false, Ordering::Release);
        if let Some(notify) = &write_done_notify {
            notify();
        }
        if shared.had_write_error.load(Ordering::Relaxed) {
            return;
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
    ///
    /// The first read blocks in the kernel for up to `first_read_timeout_ms`, so
    /// the caller is woken the instant the device delivers a packet (interrupt
    /// style) instead of spinning. Once data arrives, the rest of the OS read
    /// buffer is drained with non-blocking reads. With no data the first read
    /// times out and this returns `false`, having already provided the wait.
    pub fn poll(&self, first_read_timeout_ms: i32) -> bool {
        if self.shared.had_read_error.load(Ordering::Relaxed) {
            return false;
        }

        let mut report6_local: Vec<u8> = Vec::new();
        let mut buf = [0u8; HID_PACKET_SIZE];
        let mut first = true;

        loop {
            // Block only on the first read; drain the rest non-blocking.
            let result = if first {
                first = false;
                self.device.read_timeout(&mut buf, first_read_timeout_ms)
            } else {
                self.device.read(&mut buf)
            };
            let n = match result {
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
                    if (changed || self.shared.always_fire_input.load(Ordering::Relaxed))
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

    /// Returns true if this connection's read side hit an error (shared with the
    /// command side). Lets the USB poll thread flag a disconnect without reaching
    /// into the manager state.
    pub fn has_read_error(&self) -> bool {
        self.shared.had_read_error.load(Ordering::Relaxed)
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
    /// True if this is a panel-lights command (used to bound the light backlog).
    is_lights: bool,
    /// True if packets have been sent and we're awaiting a response.
    sent: bool,
    /// When the command was sent (for timeout detection).
    sent_at: Option<Instant>,
}

/// Handle used by the main I/O thread. Sends commands and processes responses.
///
/// The blocking HID writes themselves happen on a dedicated writer thread (see
/// `writer_loop`); this handle only queues packets to it, so `update()` never
/// blocks on the wire.
pub struct CommandHandle {
    writer_tx: Option<Sender<WriteJob>>,
    writer_join: Option<JoinHandle<()>>,
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
        self.shared
            .always_fire_input
            .store(always, Ordering::Relaxed);
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
            is_lights: false,
            sent: false,
            sent_at: None,
        });
    }

    /// Like `send_command`, but inserts at the FRONT of the queue so it is sent
    /// ahead of already-queued commands (after any in-flight one finishes).
    ///
    /// Used for latency-sensitive requests (sensor-test polling) so they don't
    /// wait behind a queued light frame. Lights are coalesced and bounded (see
    /// `send_command_lights` / `has_unsent_lights`) and the sensor request is
    /// paced, so this no longer starves the light stream.
    pub fn send_command_priority(&mut self, cmd: &[u8], callback: Option<CommandCallback>) {
        let packets = protocol::build_command_packets(cmd);
        self.pending_commands.push_front(PendingCommand {
            packets,
            callback,
            is_device_info: false,
            is_lights: false,
            sent: false,
            sent_at: None,
        });
    }

    /// Queue a panel-lights command. Tagged so the manager can keep at most one
    /// light frame queued at a time (`has_unsent_lights`), since lights are
    /// last-writer-wins state and stale frames must never back up behind a
    /// prioritized sensor request.
    pub fn send_command_lights(&mut self, cmd: &[u8]) {
        let packets = protocol::build_command_packets(cmd);
        self.pending_commands.push_back(PendingCommand {
            packets,
            callback: None,
            is_device_info: false,
            is_lights: true,
            sent: false,
            sent_at: None,
        });
    }

    /// True if any un-sent panel-lights command is queued (not counting one
    /// already in flight). Used to avoid piling new light frames onto the queue.
    pub fn has_unsent_lights(&self) -> bool {
        self.pending_commands.iter().any(|c| c.is_lights)
    }

    /// Drops any not-yet-dispatched panel-lights commands from the queue. A frame
    /// can already have been moved out of the manager's staging list and into this
    /// connection's own queue before a caller clears its share there, so re-enabling
    /// auto lights for this pad must also purge them here or a queued frame can still
    /// land and immediately re-disable the lighting. A command already handed to the
    /// writer thread (in flight) can't be recalled; that residual window is negligible.
    pub fn drop_queued_lights(&mut self) {
        self.pending_commands.retain(|c| !c.is_lights);
    }

    /// Reads a completed response packet from the buffer.
    pub fn read_packet(&mut self) -> Option<Vec<u8>> {
        self.read_buffers.pop_front()
    }

    /// Processes I/O: consumes buffered Report 6 data and sends pending commands.
    /// Returns an error if the device should be disconnected.
    pub fn update(&mut self) -> Result<(), SmxError> {
        if self.shared.had_read_error.load(Ordering::Relaxed)
            || self.shared.had_write_error.load(Ordering::Relaxed)
        {
            return Err(SmxError::NotConnected);
        }
        self.check_reads();
        self.check_writes()?;
        Ok(())
    }

    /// Block until the writer thread has no packets in flight, up to `timeout`.
    /// Returns false on timeout. Mainly a test aid: `update()` returns before
    /// the packets are on the wire, so tests that assert on written data (or
    /// rely on a fake device's write-triggered responses) settle through this.
    pub fn wait_writes_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.shared.write_in_flight.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        true
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
                    && let Some(cb) = cmd.callback
                {
                    let response = self.read_buffers.back().cloned().unwrap_or_default();
                    cb(response);
                }
            }
        }
    }

    fn check_writes(&mut self) -> Result<(), SmxError> {
        if let Some(cmd) = &mut self.current_command {
            // Promote the command to `sent` once the writer thread has all its
            // packets on the wire; the response timeout starts from there. If a
            // response beats this promotion (the poll thread can deliver it
            // before our next pass), the command completes with `sent` still
            // false, which is fine: `sent` only gates the timeout retry.
            if !cmd.sent && !self.shared.write_in_flight.load(Ordering::Acquire) {
                cmd.sent = true;
                cmd.sent_at = Some(Instant::now());
            }
            return Ok(());
        }
        if self.pending_commands.is_empty() {
            return Ok(());
        }

        let cmd = self.pending_commands.pop_front().unwrap();

        // Hand the packets to the writer thread and keep the command as the
        // in-flight one. The packets stay in the command too, so a timeout
        // retry can re-hand them later. A closed channel means the writer
        // exited (write error); surface the disconnect.
        self.shared.write_in_flight.store(true, Ordering::Release);
        let handed_off = self
            .writer_tx
            .as_ref()
            .is_some_and(|tx| tx.send(cmd.packets.clone()).is_ok());
        if !handed_off {
            self.shared.write_in_flight.store(false, Ordering::Release);
            if let Some(cb) = cmd.callback {
                cb(Vec::new());
            }
            return Err(SmxError::NotConnected);
        }
        self.current_command = Some(cmd);
        Ok(())
    }

    fn request_device_info(&mut self, callback: Option<CommandCallback>) {
        let packet = protocol::build_device_info_request();
        self.pending_commands.push_back(PendingCommand {
            packets: vec![packet],
            callback,
            is_device_info: true,
            is_lights: false,
            sent: false,
            sent_at: None,
        });
    }
}

// ─── Connection Constructor ──────────────────────────────────────────────────

impl Drop for CommandHandle {
    /// Stop the writer thread: closing the channel ends its `recv` loop; the
    /// join waits out at most the packet it is mid-write on (a few ms).
    fn drop(&mut self) {
        self.writer_tx = None;
        if let Some(join) = self.writer_join.take() {
            let _ = join.join();
        }
    }
}

/// Opens a connection to an SMX device and returns the split handles.
///
/// `PollHandle` (USB polling thread) owns `read_device`; the writer thread
/// spawned here owns `write_device` (the `CommandHandle` only queues packets
/// to it). These are two independent device handles to the same physical
/// device, and reads, writes, and the manager's own bookkeeping all happen on
/// separate threads: a read never waits behind a blocking write, and the
/// manager state lock is never held across one either (that lock used to be,
/// via `CommandHandle::update()`; measured on hardware, menu-thread pad-state
/// queries stalled for whole write bursts while lights streamed).
///
/// `write_done_notify` is invoked by the writer thread after each command's
/// packets are on the wire (and on a write error), so the manager can wake
/// promptly instead of sleeping out its loop interval.
///
/// The two handles must address the same physical device: hidapi opens are
/// per-handle, and on macOS this requires the `macos-shared-device` feature
/// (non-exclusive open); Linux and Windows already allow shared opens.
pub fn open_connection(
    path: String,
    read_device: Box<dyn HidDevice>,
    write_device: Box<dyn HidDevice>,
    input_callback: Option<Box<dyn Fn(u16) + Send>>,
    write_done_notify: Option<Box<dyn Fn() + Send>>,
) -> Result<(PollHandle, CommandHandle), SmxError> {
    let shared = Arc::new(SharedState::new());

    let poll_handle = PollHandle {
        device: read_device,
        shared: Arc::clone(&shared),
        input_callback,
    };

    let (writer_tx, writer_rx) = mpsc::channel::<WriteJob>();
    let writer_shared = Arc::clone(&shared);
    let writer_join = std::thread::Builder::new()
        .name("smx-writer".to_owned())
        .spawn(move || writer_loop(write_device, writer_rx, writer_shared, write_done_notify))
        .map_err(|_| SmxError::NotConnected)?;

    let mut cmd_handle = CommandHandle {
        writer_tx: Some(writer_tx),
        writer_join: Some(writer_join),
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

/// Test aid: run one I/O pass, then wait for the async writer thread to put
/// the packets on the wire. Tests assert on written data and rely on the fake
/// device's write-triggered auto-responses, both of which need the write to
/// have actually happened, which `update()` alone no longer guarantees.
#[cfg(test)]
fn settle(cmd: &mut CommandHandle) {
    cmd.update().unwrap();
    assert!(cmd.wait_writes_idle(Duration::from_secs(2)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        HID_REPORT_COMMAND, PACKET_FLAG_END_OF_COMMAND, PACKET_FLAG_HOST_CMD_FINISHED,
        PACKET_FLAG_START_OF_COMMAND, SERIAL_SIZE,
    };
    use crate::test_helpers::FakeDevice;
    use std::sync::atomic::{AtomicU32, Ordering};

    // Read and write handles are two clones of one FakeDevice; the clones share
    // inner state, mirroring how two real handles address one physical device.
    fn open_fake(device: FakeDevice) -> (PollHandle, CommandHandle) {
        open_connection(
            "/dev/fake".to_string(),
            Box::new(device.clone()),
            Box::new(device.clone()),
            None,
            None,
        )
        .unwrap()
    }

    fn open_fake_with_callback(
        device: FakeDevice,
        cb: Box<dyn Fn(u16) + Send>,
    ) -> (PollHandle, CommandHandle) {
        open_connection(
            "/dev/fake".to_string(),
            Box::new(device.clone()),
            Box::new(device.clone()),
            Some(cb),
            None,
        )
        .unwrap()
    }

    #[test]
    fn input_state_updated_from_report3() {
        let dev = FakeDevice::new();
        dev.queue_input_state(0x0010);
        let (poll, cmd) = open_fake(dev);
        poll.poll(0);
        assert_eq!(cmd.input_state(), 0x0010);
    }

    #[test]
    fn input_state_multiple_panels() {
        let dev = FakeDevice::new();
        dev.queue_input_state(0x0111);
        let (poll, cmd) = open_fake(dev);
        poll.poll(0);
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
            Box::new(move |_| {
                count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        );
        poll.poll(0);
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
            Box::new(move |_| {
                count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        );
        cmd.set_always_fire_input(true);
        poll.poll(0);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn poll_returns_false_when_no_data() {
        let dev = FakeDevice::new();
        let (poll, _cmd) = open_fake(dev.clone());
        assert!(!poll.poll(0));
        assert!(dev.clone().read(&mut [0; 64]).is_ok());
    }

    #[test]
    fn read_error_propagates() {
        let dev = FakeDevice::new();
        let (poll, cmd) = open_fake(dev.clone());
        dev.set_fail_reads(true);
        poll.poll(0);
        assert!(cmd.has_read_error());
    }

    #[test]
    fn close_resets_state() {
        let dev = FakeDevice::new();
        dev.queue_input_state(0x00FF);
        let (poll, mut cmd) = open_fake(dev);
        poll.poll(0);
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
        poll.poll(0);
        settle(&mut cmd);
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
        poll.poll(0);
        settle(&mut cmd);
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
        poll.poll(0);
        settle(&mut cmd);
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
        poll.poll(0);
        settle(&mut cmd);
        assert!(cmd.read_packet().is_none());
    }

    #[test]
    fn device_info_handshake() {
        let dev = FakeDevice::new();
        let serial = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        let (poll, mut cmd) = open_fake(dev.clone());
        assert!(!cmd.is_connected_with_info());

        // First update sends the device info request.
        settle(&mut cmd);

        // Now queue the response (simulating device replying).
        dev.queue_device_info_response(false, 7, &serial);
        poll.poll(0);
        settle(&mut cmd);

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
        settle(&mut cmd);

        dev.queue_device_info_response(true, 3, &serial);
        poll.poll(0);
        settle(&mut cmd);

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
        settle(&mut cmd);
        poll.poll(0);
        settle(&mut cmd);

        cmd.send_command(b"hello", None);
        settle(&mut cmd);

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
        settle(&mut cmd);
        poll.poll(0);
        settle(&mut cmd);
        cmd.set_active(true);

        let response = Arc::new(Mutex::new(Vec::new()));
        let resp_clone = Arc::clone(&response);
        cmd.send_command(
            b"G",
            Some(Box::new(move |data| {
                *resp_clone.lock().unwrap() = data;
            })),
        );
        settle(&mut cmd); // sends command

        // Queue response with HOST_CMD_FINISHED.
        dev.queue_report6(
            PACKET_FLAG_START_OF_COMMAND
                | PACKET_FLAG_END_OF_COMMAND
                | PACKET_FLAG_HOST_CMD_FINISHED,
            b"Gcfg",
        );
        poll.poll(0);
        settle(&mut cmd);

        assert_eq!(*response.lock().unwrap(), b"Gcfg");
    }

    #[test]
    fn commands_serialized() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev.clone());
        settle(&mut cmd);
        poll.poll(0);
        settle(&mut cmd);

        // Queue two commands.
        cmd.send_command(b"A", None);
        cmd.send_command(b"B", None);
        settle(&mut cmd); // sends first

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
        settle(&mut cmd);
        poll.poll(0);
        settle(&mut cmd);

        let called = Arc::new(AtomicU32::new(0));
        let called_clone = Arc::clone(&called);
        cmd.send_command(
            b"X",
            Some(Box::new(move |data| {
                if data.is_empty() {
                    called_clone.fetch_add(1, Ordering::Relaxed);
                }
            })),
        );
        settle(&mut cmd); // sends it (now it's current_command)

        cmd.close();
        assert_eq!(called.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn write_error_returns_error() {
        let dev = FakeDevice::new();
        let serial = [0u8; SERIAL_SIZE];
        dev.queue_device_info_response(false, 5, &serial);

        let (poll, mut cmd) = open_fake(dev.clone());
        settle(&mut cmd);
        poll.poll(0);
        settle(&mut cmd);

        dev.set_fail_writes(true);
        cmd.send_command(b"X", None);
        // The hand-off itself succeeds; the writer thread hits the failure and
        // flags it, and the NEXT update surfaces it as a disconnect (same
        // deferred shape as a poll-thread read error).
        cmd.update().unwrap();
        assert!(cmd.wait_writes_idle(Duration::from_secs(2)));
        let result = cmd.update();
        assert!(result.is_err());
    }

    #[test]
    fn lights_command_is_tagged_others_are_not() {
        let (_poll, mut cmd) = open_fake(FakeDevice::new());
        cmd.pending_commands.clear(); // drop the auto device-info request
        assert!(!cmd.has_unsent_lights());

        cmd.send_command_lights(b"4aaa\n");
        assert!(cmd.has_unsent_lights());

        cmd.pending_commands.clear();
        cmd.send_command(b"y1\n", None);
        cmd.send_command_priority(b"y1\n", None);
        assert!(
            !cmd.has_unsent_lights(),
            "sensor/config commands must not be tagged as lights"
        );
    }

    #[test]
    fn priority_command_jumps_ahead_of_queued_lights() {
        let (_poll, mut cmd) = open_fake(FakeDevice::new());
        cmd.pending_commands.clear();

        cmd.send_command_lights(b"4aaa\n");
        cmd.send_command_priority(b"y1\n", None);

        // Sensor request at the front, the light frame behind it.
        assert!(!cmd.pending_commands.front().unwrap().is_lights);
        assert!(cmd.pending_commands.back().unwrap().is_lights);
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
            Box::new(dev.clone()),
            Box::new(dev),
            None,
            None,
        )
        .unwrap();

        // Cycle: update sends request, write triggers auto-response, poll reads it, update processes.
        settle(&mut cmd);
        poll.poll(0);
        settle(&mut cmd);
        assert!(cmd.is_connected_with_info(), "device info not received");
        assert_eq!(cmd.device_info().firmware_version, 5);
    }
}
