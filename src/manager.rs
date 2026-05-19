use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::connection::{self, HidEnumerator, HidapiEnumerator, PollHandle};
use crate::device::{SmxDevice, SmxEvent, SmxInfo};
use crate::protocol::{
    BYTES_PER_PAD_16, BYTES_PER_PAD_25, ENUMERATION_INTERVAL_SECONDS, LED_COLOR_SCALE,
    LEGACY_LIGHTS_PAYLOAD_SIZE, LIGHTS_FRAME_INTERVAL, LIGHTS_LEGACY_COMMAND_DELAY, NUM_PANELS,
    PANEL_TEST_REFRESH_SECONDS, PLATFORM_STRIP_LEDS, SERIAL_SIZE, SMX_USB_PRODUCT_ID,
    SMX_USB_VENDOR_ID,
};

/// Panel-side diagnostic test modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PanelTestMode {
    Off = b'0',
    PressureTest = b'1',
}

/// A scheduled lights command for both pads.
struct PendingLightsCommand {
    send_at: Instant,
    pad_command: [Vec<u8>; 2],
}

/// Manages the lifecycle of all connected StepManiaX devices.
pub struct SmxManager {
    shared: Arc<ManagerShared>,
    main_thread: Option<thread::JoinHandle<()>>,
    usb_thread: Option<thread::JoinHandle<()>>,
}

/// State shared between the manager's threads and the public API.
struct ManagerShared {
    // Shutdown signal.
    shutdown: AtomicBool,
    wake: Condvar,
    // Protected state.
    state: Mutex<ManagerState>,
    // Polling rate config (atomics for lock-free access from threads).
    main_thread_sleep_ms: AtomicI32,
    usb_polling_sleep_us: AtomicI32,
}

struct ManagerState {
    devices: [SmxDevice; 2],
    poll_handles: [Option<PollHandle>; 2],
    enumerator: Box<dyn HidEnumerator>,
    callback: Box<dyn Fn(SmxEvent) + Send>,
    last_enumeration: Option<Instant>,

    // Panel test mode.
    panel_test_mode: PanelTestMode,
    last_sent_panel_test_mode: PanelTestMode,
    last_panel_test_sent_at: Option<Instant>,

    // Lights command queue.
    pending_lights: Vec<PendingLightsCommand>,
    delay_lights_until: Option<Instant>,
}

impl SmxManager {
    /// Creates a manager using the real hidapi enumerator.
    /// If `SMX_CAPTURE_DIR` is set, wraps it with a recording layer that writes
    /// `.smxhid` files directly into that directory (overwriting previous captures).
    pub fn start(callback: impl Fn(SmxEvent) + Send + 'static) -> Result<Self, crate::error::SmxError> {
        let enumerator: Box<dyn HidEnumerator> = {
            let real = Box::new(HidapiEnumerator::new()?);
            match std::env::var("SMX_CAPTURE_DIR") {
                Ok(dir) if !dir.is_empty() => {
                    Box::new(crate::recorder::RecordingEnumerator::new(
                        real,
                        std::path::Path::new(&dir),
                        false,
                    ))
                }
                _ => real,
            }
        };
        Ok(Self::new(enumerator, callback))
    }

    /// Creates a new manager with a custom enumerator and starts background threads.
    pub fn new(
        enumerator: Box<dyn HidEnumerator>,
        callback: impl Fn(SmxEvent) + Send + 'static,
    ) -> Self {
        let state = ManagerState {
            devices: [SmxDevice::new(0), SmxDevice::new(1)],
            poll_handles: [None, None],
            enumerator,
            callback: Box::new(callback),
            last_enumeration: None,
            panel_test_mode: PanelTestMode::Off,
            last_sent_panel_test_mode: PanelTestMode::Off,
            last_panel_test_sent_at: None,
            pending_lights: Vec::new(),
            delay_lights_until: None,
        };

        let shared = Arc::new(ManagerShared {
            shutdown: AtomicBool::new(false),
            wake: Condvar::new(),
            state: Mutex::new(state),
            main_thread_sleep_ms: AtomicI32::new(50),
            usb_polling_sleep_us: AtomicI32::new(1000),
        });

        let shared_main = Arc::clone(&shared);
        let main_thread = thread::spawn(move || main_thread_loop(shared_main));

        let shared_usb = Arc::clone(&shared);
        let usb_thread = thread::spawn(move || usb_polling_loop(shared_usb));

        Self {
            shared,
            main_thread: Some(main_thread),
            usb_thread: Some(usb_thread),
        }
    }

    /// Gets info for a pad (0 or 1).
    pub fn get_info(&self, pad: usize) -> SmxInfo {
        if pad > 1 {
            return SmxInfo::default();
        }
        let state = self.shared.state.lock().unwrap();
        state.devices[pad].get_info()
    }

    /// Gets the current input state for a pad.
    pub fn get_input_state(&self, pad: usize) -> u16 {
        if pad > 1 {
            return 0;
        }
        let state = self.shared.state.lock().unwrap();
        state.devices[pad].input_state()
    }

    /// Gets the current config for a pad.
    pub fn get_config(&self, pad: usize) -> Option<crate::config::SmxConfig> {
        if pad > 1 {
            return None;
        }
        let state = self.shared.state.lock().unwrap();
        state.devices[pad].get_config()
    }

    /// Sets config for a pad.
    pub fn set_config(&self, pad: usize, config: crate::config::SmxConfig) {
        if pad > 1 {
            return;
        }
        let mut state = self.shared.state.lock().unwrap();
        state.devices[pad].set_config(config);
    }

    /// Sets polling rates.
    pub fn set_polling_rate(&self, main_thread_ms: i32, usb_polling_us: i32) {
        self.shared.main_thread_sleep_ms.store(main_thread_ms, Ordering::Relaxed);
        self.shared.usb_polling_sleep_us.store(usb_polling_us, Ordering::Relaxed);
    }

    /// Re-enables automatic panel lighting on both pads.
    pub fn reenable_auto_lights(&self) {
        let mut state = self.shared.state.lock().unwrap();
        for device in &mut state.devices {
            if let Some(conn) = device.connection_mut() {
                conn.send_command(b"S 1\n", None);
            }
        }
    }

    /// Sets panel test mode on all pads.
    pub fn set_panel_test_mode(&self, mode: PanelTestMode) {
        let mut state = self.shared.state.lock().unwrap();
        state.panel_test_mode = mode;
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Sets whether input callbacks fire on every packet.
    pub fn set_input_state_mode(&self, always_fire: bool) {
        let state = self.shared.state.lock().unwrap();
        for device in &state.devices {
            if let Some(conn) = device.connection() {
                conn.set_always_fire_input(always_fire);
            }
        }
    }

    /// Assigns random serial numbers to devices that don't have one.
    pub fn set_serial_numbers(&self) {
        let mut state = self.shared.state.lock().unwrap();
        for device in &mut state.devices {
            let Some(conn) = device.connection_mut() else { continue };
            let mut cmd = Vec::with_capacity(1 + SERIAL_SIZE + 1);
            cmd.push(b's');
            cmd.extend_from_slice(&generate_serial());
            cmd.push(b'\n');
            conn.send_command(&cmd, None);
        }
    }

    /// Sets platform edge LED strip colors (264 bytes: 2 pads × 44 LEDs × 3 RGB).
    pub fn set_platform_lights(&self, light_data: &[u8]) {
        if light_data.len() < PLATFORM_STRIP_LEDS * 3 * 2 {
            return;
        }
        let mut state = self.shared.state.lock().unwrap();
        for pad in 0..2 {
            if !state.devices[pad].is_connected() {
                continue;
            }
            let Some(config) = state.devices[pad].get_config() else { continue };
            if config.master_version < 4 {
                continue;
            }
            let offset = pad * PLATFORM_STRIP_LEDS * 3;
            let mut cmd = Vec::with_capacity(3 + PLATFORM_STRIP_LEDS * 3);
            cmd.push(b'L');
            cmd.push(0); // strip index
            cmd.push(PLATFORM_STRIP_LEDS as u8);
            cmd.extend_from_slice(&light_data[offset..offset + PLATFORM_STRIP_LEDS * 3]);
            let conn = state.devices[pad].connection_mut().unwrap();
            conn.send_command(&cmd, None);
        }
    }

    /// Sets panel LED colors for both pads.
    pub fn set_lights(&self, light_data: &[u8]) {
        let mut state = self.shared.state.lock().unwrap();
        set_lights_inner(&mut state, light_data);
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Forces recalibration on a pad.
    pub fn force_recalibration(&self, pad: usize) {
        if pad > 1 {
            return;
        }
        let mut state = self.shared.state.lock().unwrap();
        state.devices[pad].force_recalibration();
    }

    /// Factory resets a pad.
    pub fn factory_reset(&self, pad: usize) {
        if pad > 1 {
            return;
        }
        let mut state = self.shared.state.lock().unwrap();
        state.devices[pad].factory_reset();
    }

    /// Sends a raw command to a pad. Used for animation upload and other low-level operations.
    /// This is not part of the stable public API.
    #[doc(hidden)]
    pub fn send_command(&self, pad: usize, cmd: &[u8], callback: Option<crate::connection::CommandCallback>) {
        if pad > 1 {
            return;
        }
        let mut state = self.shared.state.lock().unwrap();
        if let Some(conn) = state.devices[pad].connection_mut() {
            conn.send_command(cmd, callback);
        }
    }

    /// Sets sensor test mode for a pad.
    pub fn set_test_mode(&self, pad: usize, mode: crate::device::SensorTestMode) {
        if pad > 1 {
            return;
        }
        let mut state = self.shared.state.lock().unwrap();
        state.devices[pad].set_sensor_test_mode(mode);
    }

    /// Gets sensor test data for a pad.
    pub fn get_test_data(&self, pad: usize) -> Option<crate::device::SensorTestData> {
        if pad > 1 {
            return None;
        }
        let state = self.shared.state.lock().unwrap();
        state.devices[pad].get_test_data().cloned()
    }
}

impl Drop for SmxManager {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
        if let Some(t) = self.main_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.usb_thread.take() {
            let _ = t.join();
        }
    }
}

// ─── Threading ───────────────────────────────────────────────────────────────

fn usb_polling_loop(shared: Arc<ManagerShared>) {
    while !shared.shutdown.load(Ordering::Relaxed) {
        let mut has_report6 = false;
        {
            let state = shared.state.lock().unwrap();
            for poll_handle in state.poll_handles.iter().flatten() {
                if poll_handle.poll() {
                    has_report6 = true;
                }
            }
        }
        if has_report6 {
            shared.wake.notify_all();
        }
        let us = shared.usb_polling_sleep_us.load(Ordering::Relaxed).max(100);
        thread::sleep(Duration::from_micros(us as u64));
    }
}

fn main_thread_loop(shared: Arc<ManagerShared>) {
    while !shared.shutdown.load(Ordering::Relaxed) {
        let wait_ms = {
            let mut state = shared.state.lock().unwrap();

            attempt_connections(&mut state);

            let was_connected = [state.devices[0].is_connected(), state.devices[1].is_connected()];

            // Update devices.
            for i in 0..2 {
                if let Err(e) = state.devices[i].update() {
                    log::error!("Device {i} error: {e}");
                    state.devices[i].close();
                    state.poll_handles[i] = None;
                }
            }

            // Detect new connections and correct ordering.
            let just_connected_any = (!was_connected[0] && state.devices[0].is_connected())
                || (!was_connected[1] && state.devices[1].is_connected());

            let swapped = just_connected_any && correct_device_order(&mut state);

            let mut just_connected = [
                !was_connected[0] && state.devices[0].is_connected(),
                !was_connected[1] && state.devices[1].is_connected(),
            ];
            if swapped {
                just_connected = [
                    !was_connected[1] && state.devices[0].is_connected(),
                    !was_connected[0] && state.devices[1].is_connected(),
                ];
            }

            for (i, &connected) in just_connected.iter().enumerate() {
                if connected {
                    let info = state.devices[i].get_info();
                    (state.callback)(SmxEvent::Connected { pad: i, info });
                }
            }

            update_panel_test_mode(&mut state);
            send_pending_lights(&mut state);

            // Determine wait time.
            let mut wait = shared.main_thread_sleep_ms.load(Ordering::Relaxed).max(1) as u64;
            if let Some(next) = state.pending_lights.first() {
                let until = next.send_at.saturating_duration_since(Instant::now());
                let ms = until.as_millis() as u64 + 1;
                wait = wait.min(ms);
            }
            wait
        };

        // Wait with condvar (releases lock implicitly since we dropped state above).
        let state = shared.state.lock().unwrap();
        let _ = shared.wake.wait_timeout(state, Duration::from_millis(wait_ms));
    }
}

// ─── Device Discovery ────────────────────────────────────────────────────────

fn attempt_connections(state: &mut ManagerState) {
    // Skip if both slots occupied.
    let has_slot = state.devices[0].connection().is_none()
        || state.devices[1].connection().is_none();
    if !has_slot {
        return;
    }

    // Rate limit enumeration.
    let now = Instant::now();
    if let Some(last) = state.last_enumeration
        && now.duration_since(last).as_secs_f64() < ENUMERATION_INTERVAL_SECONDS
    {
        return;
    }
    state.last_enumeration = Some(now);

    let devs = state.enumerator.enumerate(SMX_USB_VENDOR_ID, SMX_USB_PRODUCT_ID);

    for dev_info in devs {
        if dev_info.path.is_empty() {
            continue;
        }

        // Skip if already open.
        let already_open = state.devices.iter().any(|d| {
            d.connection().is_some_and(|c| c.path() == dev_info.path)
        });
        if already_open {
            continue;
        }

        // Find empty slot.
        let slot = state.devices.iter().position(|d| d.connection().is_none());
        let Some(slot_idx) = slot else {
            break;
        };

        log::info!("Opening SMX device: {}", dev_info.path);
        let device = match state.enumerator.open(&dev_info.path) {
            Ok(d) => d,
            Err(e) => {
                log::error!("Error opening device {}: {e}", dev_info.path);
                continue;
            }
        };

        // Create the split connection.
        let cb = {
            // Input state callbacks will be wired up via the shared state's atomic.
            // The PollHandle fires the callback directly when input changes.
            None::<Box<dyn Fn() + Send>>
        };

        match connection::open_connection(dev_info.path, device, cb) {
            Ok((poll_handle, cmd_handle)) => {
                state.devices[slot_idx].set_connection(cmd_handle);
                state.poll_handles[slot_idx] = Some(poll_handle);
            }
            Err(e) => {
                log::error!("Error setting up connection: {e}");
            }
        }
    }
}

fn correct_device_order(state: &mut ManagerState) -> bool {
    let info0 = state.devices[0].get_info();
    let info1 = state.devices[1].get_info();

    // If both connected with same player setting, can't determine order.
    if info0.connected && info1.connected && info0.is_player2 == info1.is_player2 {
        return false;
    }

    let should_swap = (info0.connected && info0.is_player2)
        || (info1.connected && !info1.is_player2);

    if should_swap {
        state.devices.swap(0, 1);
        state.poll_handles.swap(0, 1);
    }
    should_swap
}

// ─── Panel Test Mode ─────────────────────────────────────────────────────────

fn update_panel_test_mode(state: &mut ManagerState) {
    let mode = state.panel_test_mode;
    let last = state.last_sent_panel_test_mode;

    if mode == last {
        if mode == PanelTestMode::Off {
            return;
        }
        if let Some(sent_at) = state.last_panel_test_sent_at
            && sent_at.elapsed().as_secs_f64() < PANEL_TEST_REFRESH_SECONDS
        {
            return;
        }
    }

    // When transitioning to active, send lights-off first.
    if last == PanelTestMode::Off && mode != PanelTestMode::Off {
        let mut cmd = Vec::with_capacity(2 + LEGACY_LIGHTS_PAYLOAD_SIZE);
        cmd.push(b'l');
        cmd.resize(1 + LEGACY_LIGHTS_PAYLOAD_SIZE, 0);
        cmd.push(b'\n');
        for device in &mut state.devices {
            if let Some(conn) = device.connection_mut() {
                conn.send_command(&cmd, None);
            }
        }
    }

    state.last_panel_test_sent_at = Some(Instant::now());
    state.last_sent_panel_test_mode = mode;

    let cmd = [b't', b' ', mode as u8, b'\n'];
    for device in &mut state.devices {
        if let Some(conn) = device.connection_mut() {
            conn.send_command(&cmd, None);
        }
    }
}

// ─── Lights ──────────────────────────────────────────────────────────────────

/// Precomputed color scaling table.
fn scale_color(c: u8) -> u8 {
    (c as f32 * LED_COLOR_SCALE) as u8
}

fn set_lights_inner(state: &mut ManagerState, light_data: &[u8]) {
    if state.panel_test_mode != PanelTestMode::Off {
        return;
    }

    let bytes_per_pad = if light_data.len() == 2 * BYTES_PER_PAD_16 {
        BYTES_PER_PAD_16
    } else if light_data.len() == 2 * BYTES_PER_PAD_25 {
        BYTES_PER_PAD_25
    } else {
        return;
    };

    // Build 3 commands per pad: '4' (inner 3x3), '2' (top half), '3' (bottom half).
    let mut cmds: [[Vec<u8>; 2]; 3] = Default::default();

    for pad in 0..2 {
        let pad_data = &light_data[pad * bytes_per_pad..(pad + 1) * bytes_per_pad];

        cmds[0][pad] = Vec::with_capacity(1 + NUM_PANELS * 9 * 3 + 1);
        cmds[1][pad] = Vec::with_capacity(1 + NUM_PANELS * 8 * 3 + 1);
        cmds[2][pad] = Vec::with_capacity(1 + NUM_PANELS * 8 * 3 + 1);

        cmds[0][pad].push(b'4');
        cmds[1][pad].push(b'2');
        cmds[2][pad].push(b'3');

        let mut input_idx = 0;
        for _panel in 0..NUM_PANELS {
            // Outer 4x4: top 2 rows → cmd '2', bottom 2 rows → cmd '3'.
            for byte_idx in 0..4 * 4 * 3 {
                let color = scale_color(pad_data[input_idx]);
                input_idx += 1;
                if byte_idx < 4 * 2 * 3 {
                    cmds[1][pad].push(color);
                } else {
                    cmds[2][pad].push(color);
                }
            }
            // Inner 3x3 → cmd '4'.
            if bytes_per_pad == BYTES_PER_PAD_25 {
                for _ in 0..3 * 3 * 3 {
                    cmds[0][pad].push(scale_color(pad_data[input_idx]));
                    input_idx += 1;
                }
            } else {
                cmds[0][pad].extend_from_slice(&[0u8; 3 * 3 * 3]);
            }
        }

        cmds[0][pad].push(b'\n');
        cmds[1][pad].push(b'\n');
        cmds[2][pad].push(b'\n');
    }

    // Rate limiting: replace last 3 pending if full, otherwise append.
    let now = Instant::now();

    if state.pending_lights.len() < 3 {
        let send_at = state.delay_lights_until.unwrap_or(now).max(now);

        // Check firmware version for timing.
        let mut is_v4 = false;
        let mut any_connected = false;
        let mut has_config = [false; 2];
        let mut configs = [None, None];
        for pad in 0..2 {
            if let Some(cfg) = state.devices[pad].get_config() {
                has_config[pad] = true;
                any_connected = true;
                if cfg.master_version >= 4 {
                    is_v4 = true;
                }
                configs[pad] = Some(cfg);
            }
        }

        if !any_connected {
            return;
        }

        let mut times = [send_at; 3];
        if !is_v4 {
            times[1] = send_at;
            times[2] = send_at + Duration::from_secs_f64(LIGHTS_LEGACY_COMMAND_DELAY);
        }

        state.delay_lights_until = Some(send_at + Duration::from_secs_f64(LIGHTS_FRAME_INTERVAL));

        for time in times {
            state.pending_lights.push(PendingLightsCommand {
                send_at: time,
                pad_command: [Vec::new(), Vec::new()],
            });
        }

        let base = state.pending_lights.len() - 3;
        for pad in 0..2 {
            if !has_config[pad] {
                continue;
            }
            let master_v = configs[pad].map_or(0, |c| c.master_version);
            if master_v >= 4 {
                state.pending_lights[base].pad_command[pad] = cmds[0][pad].clone();
            }
            state.pending_lights[base + 1].pad_command[pad] = cmds[1][pad].clone();
            state.pending_lights[base + 2].pad_command[pad] = cmds[2][pad].clone();
        }
    } else {
        // Replace last 3.
        let base = state.pending_lights.len() - 3;
        for pad in 0..2 {
            let Some(cfg) = state.devices[pad].get_config() else { continue };
            if cfg.master_version >= 4 {
                state.pending_lights[base].pad_command[pad] = cmds[0][pad].clone();
            } else {
                state.pending_lights[base].pad_command[pad].clear();
            }
            state.pending_lights[base + 1].pad_command[pad] = cmds[1][pad].clone();
            state.pending_lights[base + 2].pad_command[pad] = cmds[2][pad].clone();
        }
    }
}

fn send_pending_lights(state: &mut ManagerState) {
    let now = Instant::now();
    let mut consumed = 0;

    while consumed < state.pending_lights.len() {
        if state.pending_lights[consumed].send_at > now {
            break;
        }
        let cmd = &state.pending_lights[consumed];
        for pad in 0..2 {
            if !cmd.pad_command[pad].is_empty()
                && let Some(conn) = state.devices[pad].connection_mut()
            {
                conn.send_command(&cmd.pad_command[pad], None);
            }
        }
        consumed += 1;
    }

    if consumed > 0 {
        state.pending_lights.drain(..consumed);
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn generate_serial() -> [u8; SERIAL_SIZE] {
    let mut serial = [0u8; SERIAL_SIZE];
    // Use system time + thread ID as entropy source for serial assignment.
    // This doesn't need to be cryptographically secure — just unique per device.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut state = seed as u64;
    for byte in &mut serial {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    serial
}
