use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::connection::{self, HidEnumerator, HidapiEnumerator, PollHandle};
use crate::device::{SmxDevice, SmxEvent, SmxInfo};
use crate::protocol::{
    BYTES_PER_PAD_16, BYTES_PER_PAD_25, ENUMERATION_INTERVAL_SECONDS,
    LEGACY_LIGHTS_PAYLOAD_SIZE, LIGHTS_FRAME_INTERVAL, LIGHTS_LEGACY_COMMAND_DELAY, NUM_PANELS,
    PANEL_TEST_REFRESH_SECONDS, PLATFORM_STRIP_LEDS, SERIAL_SIZE, SMX_USB_PRODUCT_ID,
    SMX_USB_PRODUCT_STRING, SMX_USB_VENDOR_ID,
};

/// Main-loop poll interval (ms) used while sensor test mode is active, so a new
/// sensor request fires shortly after each response. Targets ~20Hz+ sampling.
const SENSOR_TEST_POLL_WAIT_MS: u64 = 20;

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
    pad_command: [[u8; 256]; 2], // max command size is 245 bytes
    pad_command_len: [usize; 2],
}

/// Manages the lifecycle of all connected StepManiaX devices.
pub struct SmxManager {
    shared: Arc<ManagerShared>,
    main_thread: Option<thread::JoinHandle<()>>,
    usb_thread: Option<thread::JoinHandle<()>>,
    anim_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

/// State shared between the manager's threads and the public API.
struct ManagerShared {
    // Shutdown signal.
    shutdown: AtomicBool,
    wake: Condvar,
    // Protected state.
    state: Mutex<ManagerState>,
    // Enumerator has its own lock so enumeration doesn't block USB polling.
    enumerator: Mutex<Box<dyn HidEnumerator>>,
    // Polling rate config (atomics for lock-free access from threads).
    main_thread_sleep_ms: AtomicI32,
    usb_polling_sleep_us: AtomicI32,
    // Animation state.
    animation: Mutex<crate::lights::AnimationState>,
    anim_running: AtomicBool,
    anim_pause_until: Mutex<Option<Instant>>,
}

struct ManagerState {
    devices: [SmxDevice; 2],
    poll_handles: [Option<PollHandle>; 2],
    callback: Arc<dyn Fn(SmxEvent) + Send + Sync>,
    last_enumeration: Option<Instant>,

    // Panel test mode.
    panel_test_mode: PanelTestMode,
    last_sent_panel_test_mode: PanelTestMode,
    last_panel_test_sent_at: Option<Instant>,

    // Lights command queue.
    pending_lights: Vec<PendingLightsCommand>,
    delay_lights_until: Option<Instant>,

    // Input state tracking for change detection.
    last_input_state: [u16; 2],
    always_fire_input: bool,

    // Paths that failed to open (removed when the device disappears from enumeration).
    failed_paths: Vec<String>,

    // Optional user-pinned serial->slot assignment, index = desired slot
    // (0 = P1, 1 = P2). When both connected pads' serials are covered by this,
    // `correct_device_order` orders strictly by it (overriding the P1/P2 jumper);
    // otherwise it falls back to jumper-based ordering. `[None, None]` = no override.
    player_assignment: [Option<String>; 2],
}

/// Normalize a raw `SMX_CAPTURE_DIR` value: trim surrounding whitespace and
/// treat empty/whitespace-only as unset. On Windows `set VAR=path && app`
/// captures the trailing space before `&&` into the value, which otherwise
/// lands mid-path (".../dir \device_0.smxhid") and makes the capture file
/// unresolvable (os error 3, path not found).
fn capture_dir_from_value(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

impl SmxManager {
    /// Creates a manager using the real hidapi enumerator.
    /// If `SMX_CAPTURE_DIR` is set, wraps it with a recording layer that writes
    /// `.smxhid` files directly into that directory (overwriting previous captures).
    pub fn start(callback: impl Fn(SmxEvent) + Send + Sync + 'static) -> Result<Self, crate::error::SmxError> {
        let enumerator: Box<dyn HidEnumerator> = {
            let real = Box::new(HidapiEnumerator::new()?);
            match std::env::var("SMX_CAPTURE_DIR") {
                Ok(raw) => match capture_dir_from_value(&raw) {
                    Some(dir) => Box::new(crate::recorder::RecordingEnumerator::new(
                        real,
                        std::path::Path::new(dir),
                        false,
                    )),
                    None => real,
                },
                Err(_) => real,
            }
        };
        Ok(Self::new(enumerator, callback))
    }

    /// Creates a new manager with a custom enumerator and starts background threads.
    pub fn new(
        enumerator: Box<dyn HidEnumerator>,
        callback: impl Fn(SmxEvent) + Send + Sync + 'static,
    ) -> Self {
        let callback: Arc<dyn Fn(SmxEvent) + Send + Sync> = Arc::new(callback);

        let mut devices = [SmxDevice::new(0), SmxDevice::new(1)];
        for device in &mut devices {
            let cb = Arc::clone(&callback);
            device.set_update_callback(Box::new(move |event| cb(event)));
        }

        let state = ManagerState {
            devices,
            poll_handles: [None, None],
            callback: Arc::clone(&callback),
            last_enumeration: None,
            panel_test_mode: PanelTestMode::Off,
            last_sent_panel_test_mode: PanelTestMode::Off,
            last_panel_test_sent_at: None,
            pending_lights: Vec::new(),
            delay_lights_until: None,
            last_input_state: [0; 2],
            always_fire_input: false,
            failed_paths: Vec::new(),
            player_assignment: [None, None],
        };

        let shared = Arc::new(ManagerShared {
            shutdown: AtomicBool::new(false),
            wake: Condvar::new(),
            state: Mutex::new(state),
            enumerator: Mutex::new(enumerator),
            main_thread_sleep_ms: AtomicI32::new(50),
            usb_polling_sleep_us: AtomicI32::new(1000),
            animation: Mutex::new(crate::lights::AnimationState::new()),
            anim_running: AtomicBool::new(false),
            anim_pause_until: Mutex::new(None),
        });

        let shared_main = Arc::clone(&shared);
        let main_thread = thread::spawn(move || main_thread_loop(shared_main));

        let shared_usb = Arc::clone(&shared);
        let usb_thread = thread::spawn(move || usb_polling_loop(shared_usb));

        Self {
            shared,
            main_thread: Some(main_thread),
            usb_thread: Some(usb_thread),
            anim_thread: Mutex::new(None),
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
        let mut state = self.shared.state.lock().unwrap();
        state.always_fire_input = always_fire;
        for device in &state.devices {
            if let Some(conn) = device.connection() {
                conn.set_always_fire_input(always_fire);
            }
        }
    }

    /// Pin pad serials to player slots (`p1_serial` -> slot 0, `p2_serial` -> slot 1),
    /// overriding the hardware P1/P2 jumper when ordering the two slots.
    ///
    /// The override only takes effect when both connected pads' serials are the two
    /// given serials; otherwise (no override, a single pad, or an unrecognized serial)
    /// ordering falls back to the jumper. Passing `None, None` clears the override.
    ///
    /// Applies immediately: if the two connected pads are in the wrong slots they are
    /// swapped now, and `Connected`/`ConfigUpdated` events are fired for the affected
    /// slots so listeners re-read per-slot identity — no reconnect required.
    pub fn set_player_assignment(&self, p1_serial: Option<String>, p2_serial: Option<String>) {
        let mut state = self.shared.state.lock().unwrap();
        state.player_assignment = [p1_serial, p2_serial];
        if correct_device_order(&mut state) {
            for i in 0..2 {
                if state.devices[i].is_connected() {
                    let info = state.devices[i].get_info();
                    (state.callback)(SmxEvent::Connected { pad: i, info });
                    (state.callback)(SmxEvent::ConfigUpdated { pad: i });
                }
            }
        }
        drop(state);
        self.shared.wake.notify_all();
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
        // Pause auto-animation when lights are set directly.
        if self.shared.anim_running.load(Ordering::Relaxed) {
            *self.shared.anim_pause_until.lock().unwrap() =
                Some(Instant::now() + Duration::from_secs_f64(crate::protocol::ANIMATION_PAUSE_DURATION));
        }
        let mut state = self.shared.state.lock().unwrap();
        set_lights_inner(&mut state, light_data);
        drop(state);
        self.shared.wake.notify_all();
    }

    /// Loads a GIF animation for a pad.
    pub fn load_animation(
        &self,
        gif_data: &[u8],
        pad: usize,
        lights_type: crate::lights::LightsType,
    ) -> Result<(), &'static str> {
        let mut anim = self.shared.animation.lock().unwrap();
        anim.load(gif_data, pad, lights_type)
    }

    /// Enables or disables automatic animation playback at 30 FPS.
    pub fn set_animation_auto(&self, enable: bool) {
        if enable {
            if self.shared.anim_running.load(Ordering::Relaxed) {
                return; // Already running.
            }
            self.shared.anim_running.store(true, Ordering::Relaxed);
            let shared = Arc::clone(&self.shared);
            *self.anim_thread.lock().unwrap() = Some(thread::spawn(move || animation_thread_loop(shared)));
        } else {
            if !self.shared.anim_running.load(Ordering::Relaxed) {
                return; // Not running.
            }
            self.shared.anim_running.store(false, Ordering::Relaxed);
            if let Some(t) = self.anim_thread.lock().unwrap().take() {
                let _ = t.join();
            }
        }
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
        self.shared.anim_running.store(false, Ordering::Relaxed);
        self.shared.wake.notify_all();
        if let Some(t) = self.anim_thread.lock().unwrap().take() {
            let _ = t.join();
        }
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
        let mut should_wake = false;
        {
            let state = shared.state.lock().unwrap();
            for (i, poll_handle) in state.poll_handles.iter().enumerate() {
                if let Some(ph) = poll_handle
                    && ph.poll()
                {
                    should_wake = true;
                }
                // Also wake if a device has a read error (for prompt disconnect).
                if let Some(conn) = state.devices[i].connection()
                    && conn.has_read_error()
                {
                    should_wake = true;
                }
            }
        }
        if should_wake {
            shared.wake.notify_all();
        }
        let us = shared.usb_polling_sleep_us.load(Ordering::Relaxed).max(100);
        thread::sleep(Duration::from_micros(us as u64));
    }
}

fn animation_thread_loop(shared: Arc<ManagerShared>) {
    let frame_duration = Duration::from_millis(33); // ~30 FPS

    while shared.anim_running.load(Ordering::Relaxed) && !shared.shutdown.load(Ordering::Relaxed) {
        let frame_start = Instant::now();

        // Check if paused (direct set_lights was called).
        let paused = shared.anim_pause_until.lock().unwrap()
            .is_some_and(|until| Instant::now() < until);
        if paused {
            thread::sleep(frame_duration);
            continue;
        }

        // Get input states and build frame.
        let (input_states, has_animation) = {
            let state = shared.state.lock().unwrap();
            let inputs = [state.devices[0].input_state(), state.devices[1].input_state()];
            let has_anim = state.devices[0].is_connected() || state.devices[1].is_connected();
            (inputs, has_anim)
        };

        if !has_animation {
            thread::sleep(frame_duration);
            continue;
        }

        let frame = {
            let mut anim = shared.animation.lock().unwrap();
            anim.build_frame(input_states)
        };

        // Send lights (bypasses the pause logic by going directly to set_lights_inner).
        {
            let mut state = shared.state.lock().unwrap();
            set_lights_inner(&mut state, &frame);
        }
        shared.wake.notify_all();

        // Sleep for remainder of frame.
        let elapsed = frame_start.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
    }
}

fn main_thread_loop(shared: Arc<ManagerShared>) {
    while !shared.shutdown.load(Ordering::Relaxed) {
        // Attempt connections (releases state lock during enumeration).
        attempt_connections(&shared);

        let wait_ms = {
            let mut state = shared.state.lock().unwrap();

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
                    (state.callback)(SmxEvent::ConfigUpdated { pad: i });
                }
            }

            // Detect input state changes (fired from USB thread via callback,
            // but also check here for devices that connected without the callback wired).
            for i in 0..2 {
                if state.devices[i].is_connected() {
                    let new_state = state.devices[i].input_state();
                    if new_state != state.last_input_state[i] {
                        state.last_input_state[i] = new_state;
                        // Already fired by PollHandle callback, skip duplicate.
                    }
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
            // While sensor test mode is active, poll faster so the next request
            // fires promptly after each response instead of waiting out the full
            // idle interval (the 50ms default otherwise caps the rate at ~15Hz).
            if state.devices[0].is_sensor_test_active() || state.devices[1].is_sensor_test_active()
            {
                wait = wait.min(SENSOR_TEST_POLL_WAIT_MS);
            }
            wait
        };

        // Wait with condvar (releases lock implicitly since we dropped state above).
        let state = shared.state.lock().unwrap();
        let _ = shared.wake.wait_timeout(state, Duration::from_millis(wait_ms));
    }
}

// ─── Device Discovery ────────────────────────────────────────────────────────

fn attempt_connections(shared: &ManagerShared) {
    // Check if we should enumerate (rate limit + slot availability).
    {
        let state = shared.state.lock().unwrap();
        let has_slot = state.devices[0].connection().is_none()
            || state.devices[1].connection().is_none();
        if !has_slot {
            return;
        }
        let now = Instant::now();
        if let Some(last) = state.last_enumeration
            && now.duration_since(last).as_secs_f64() < ENUMERATION_INTERVAL_SECONDS
        {
            return;
        }
    }
    // State lock is released here — USB polling thread can run during enumeration.

    // Enumerate (potentially slow syscall, no lock held).
    let devs = {
        let enumerator = shared.enumerator.lock().unwrap();
        enumerator.enumerate(SMX_USB_VENDOR_ID, SMX_USB_PRODUCT_ID)
    };

    // Re-acquire state lock for connection setup.
    let mut state = shared.state.lock().unwrap();
    state.last_enumeration = Some(Instant::now());

    // Clear failed paths that are no longer in the enumeration (device was unplugged).
    state.failed_paths.retain(|p| devs.iter().any(|d| d.path == *p));

    for dev_info in devs {
        if dev_info.path.is_empty() {
            continue;
        }

        if !dev_info.product.contains(SMX_USB_PRODUCT_STRING) {
            continue;
        }

        if state.failed_paths.contains(&dev_info.path) {
            continue;
        }

        let already_open = state.devices.iter().any(|d| {
            d.connection().is_some_and(|c| c.path() == dev_info.path)
        });
        if already_open {
            continue;
        }

        let slot = state.devices.iter().position(|d| d.connection().is_none());
        let Some(slot_idx) = slot else {
            break;
        };

        log::info!("Opening SMX device: {}", dev_info.path);
        let device = {
            let enumerator = shared.enumerator.lock().unwrap();
            match enumerator.open(&dev_info.path) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("Error opening device {}: {e}", dev_info.path);
                    state.failed_paths.push(dev_info.path.clone());
                    continue;
                }
            }
        };

        let input_cb = {
            let callback = Arc::clone(&state.callback);
            let pad_index = state.devices[slot_idx].pad_index_shared();
            Some(Box::new(move |new_state: u16| {
                let pad = pad_index.load(std::sync::atomic::Ordering::Relaxed);
                (callback)(SmxEvent::InputState { pad, state: new_state });
            }) as Box<dyn Fn(u16) + Send>)
        };

        match connection::open_connection(dev_info.path, device, input_cb) {
            Ok((poll_handle, cmd_handle)) => {
                if state.always_fire_input {
                    cmd_handle.set_always_fire_input(true);
                }
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

    let should_swap = match override_should_swap(&state.player_assignment, &info0, &info1) {
        // User-pinned serial->slot assignment covers both pads: order by it,
        // ignoring the jumper (this is the only path that can order two pads
        // that share a jumper).
        Some(swap) => swap,
        // No applicable override: fall back to jumper-based ordering.
        None => {
            // If both connected with same player setting, can't determine order.
            if info0.connected && info1.connected && info0.is_player2 == info1.is_player2 {
                return false;
            }
            (info0.connected && info0.is_player2) || (info1.connected && !info1.is_player2)
        }
    };

    if should_swap {
        state.devices.swap(0, 1);
        state.poll_handles.swap(0, 1);
        // Update pad indices so events report the correct pad number.
        state.devices[0].set_pad_index(0);
        state.devices[1].set_pad_index(1);
    }
    should_swap
}

/// Decide ordering from a user-pinned serial->slot assignment.
///
/// Returns `Some(should_swap)` when the assignment determines the order, or
/// `None` to defer to jumper ordering (no assignment set, or a connected pad's
/// serial isn't covered by the assignment).
///
/// - **Two pads connected:** only a full two-serial assignment forces an order
///   (the one case that can separate two pads sharing a jumper).
/// - **One pad connected:** a single-sided assignment relocates it to its pinned
///   slot, so a lone stage pinned to P2 moves to slot 1 (and one pinned to P1
///   moves to slot 0), regardless of its hardware jumper. This is how a single
///   pad is played as P2.
fn override_should_swap(
    assignment: &[Option<String>; 2],
    info0: &SmxInfo,
    info1: &SmxInfo,
) -> Option<bool> {
    let p1 = assignment[0].as_deref();
    let p2 = assignment[1].as_deref();
    if p1.is_none() && p2.is_none() {
        return None; // no assignment at all
    }
    match (info0.connected, info1.connected) {
        // Two pads: require the complete pair to force an order; a single pinned
        // side is ambiguous with two pads present, so defer to the jumper.
        (true, true) => {
            let (Some(p1), Some(p2)) = (p1, p2) else {
                return None;
            };
            let (s0, s1) = (info0.serial.as_str(), info1.serial.as_str());
            if s0 == p1 && s1 == p2 {
                Some(false) // already in the desired order
            } else if s0 == p2 && s1 == p1 {
                Some(true) // pads are swapped relative to the assignment
            } else {
                None // a serial isn't covered by the assignment -> jumper fallback
            }
        }
        // Lone pad in slot 0: keep it there if it's the P1 pad, move it to slot 1
        // if it's the P2 pad. An unassigned serial defers to the jumper.
        (true, false) => match info0.serial.as_str() {
            s if p1 == Some(s) => Some(false),
            s if p2 == Some(s) => Some(true),
            _ => None,
        },
        // Lone pad in slot 1: keep it there if it's the P2 pad, move it to slot 0
        // if it's the P1 pad.
        (false, true) => match info1.serial.as_str() {
            s if p2 == Some(s) => Some(false),
            s if p1 == Some(s) => Some(true),
            _ => None,
        },
        (false, false) => None,
    }
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



#[allow(clippy::needless_range_loop)]
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

    // Build 3 commands per pad into stack buffers.
    // cmd '4' (inner 3x3): max 1 + 9*9*3 + 1 = 245 bytes
    // cmd '2' (top half):  max 1 + 9*8*3 + 1 = 218 bytes
    // cmd '3' (bottom half): same as '2'
    let mut cmds = [[[0u8; 256]; 2]; 3];
    let mut cmd_lens = [[0usize; 2]; 3];

    for pad in 0..2 {
        let pad_data = &light_data[pad * bytes_per_pad..(pad + 1) * bytes_per_pad];

        let mut len4 = 0usize;
        let mut len2 = 0usize;
        let mut len3 = 0usize;

        cmds[0][pad][len4] = b'4'; len4 += 1;
        cmds[1][pad][len2] = b'2'; len2 += 1;
        cmds[2][pad][len3] = b'3'; len3 += 1;

        let mut input_idx = 0;
        for _panel in 0..NUM_PANELS {
            for byte_idx in 0..4 * 4 * 3 {
                let color = crate::lights::scale_color(pad_data[input_idx]);
                input_idx += 1;
                if byte_idx < 4 * 2 * 3 {
                    cmds[1][pad][len2] = color; len2 += 1;
                } else {
                    cmds[2][pad][len3] = color; len3 += 1;
                }
            }
            if bytes_per_pad == BYTES_PER_PAD_25 {
                for _ in 0..3 * 3 * 3 {
                    cmds[0][pad][len4] = crate::lights::scale_color(pad_data[input_idx]);
                    len4 += 1;
                    input_idx += 1;
                }
            } else {
                for _ in 0..3 * 3 * 3 {
                    cmds[0][pad][len4] = 0;
                    len4 += 1;
                }
            }
        }

        cmds[0][pad][len4] = b'\n'; len4 += 1;
        cmds[1][pad][len2] = b'\n'; len2 += 1;
        cmds[2][pad][len3] = b'\n'; len3 += 1;

        cmd_lens[0][pad] = len4;
        cmd_lens[1][pad] = len2;
        cmd_lens[2][pad] = len3;
    }

    // Rate limiting: replace last 3 pending if full, otherwise append.
    let now = Instant::now();

    if state.pending_lights.len() < 3 {
        let send_at = state.delay_lights_until.unwrap_or(now).max(now);

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
                pad_command: [[0; 256]; 2],
                pad_command_len: [0; 2],
            });
        }

        let base = state.pending_lights.len() - 3;
        for pad in 0..2 {
            if !has_config[pad] {
                continue;
            }
            let master_v = configs[pad].map_or(0, |c| c.master_version);
            if master_v >= 4 {
                state.pending_lights[base].pad_command[pad][..cmd_lens[0][pad]]
                    .copy_from_slice(&cmds[0][pad][..cmd_lens[0][pad]]);
                state.pending_lights[base].pad_command_len[pad] = cmd_lens[0][pad];
            }
            state.pending_lights[base + 1].pad_command[pad][..cmd_lens[1][pad]]
                .copy_from_slice(&cmds[1][pad][..cmd_lens[1][pad]]);
            state.pending_lights[base + 1].pad_command_len[pad] = cmd_lens[1][pad];
            state.pending_lights[base + 2].pad_command[pad][..cmd_lens[2][pad]]
                .copy_from_slice(&cmds[2][pad][..cmd_lens[2][pad]]);
            state.pending_lights[base + 2].pad_command_len[pad] = cmd_lens[2][pad];
        }
    } else {
        // Replace last 3.
        let base = state.pending_lights.len() - 3;
        for pad in 0..2 {
            let Some(cfg) = state.devices[pad].get_config() else { continue };
            if cfg.master_version >= 4 {
                state.pending_lights[base].pad_command[pad][..cmd_lens[0][pad]]
                    .copy_from_slice(&cmds[0][pad][..cmd_lens[0][pad]]);
                state.pending_lights[base].pad_command_len[pad] = cmd_lens[0][pad];
            } else {
                state.pending_lights[base].pad_command_len[pad] = 0;
            }
            state.pending_lights[base + 1].pad_command[pad][..cmd_lens[1][pad]]
                .copy_from_slice(&cmds[1][pad][..cmd_lens[1][pad]]);
            state.pending_lights[base + 1].pad_command_len[pad] = cmd_lens[1][pad];
            state.pending_lights[base + 2].pad_command[pad][..cmd_lens[2][pad]]
                .copy_from_slice(&cmds[2][pad][..cmd_lens[2][pad]]);
            state.pending_lights[base + 2].pad_command_len[pad] = cmd_lens[2][pad];
        }
    }
}

fn send_pending_lights(state: &mut ManagerState) {
    // Bound the light backlog: don't hand a new frame to the per-connection
    // command queue while a previous frame is still unsent there. Lights are
    // last-writer-wins state, so the newest frame stays in pending_lights (which
    // coalesces) until the connections drain. Without this, light frames pile up
    // behind the prioritized sensor request and flush late (panels go dark mid
    // song, then dump on exit).
    let lights_in_flight = (0..2).any(|pad| {
        state.devices[pad]
            .connection()
            .is_some_and(|c| c.has_unsent_lights())
    });
    if lights_in_flight {
        return;
    }

    let now = Instant::now();
    let mut consumed = 0;

    while consumed < state.pending_lights.len() {
        if state.pending_lights[consumed].send_at > now {
            break;
        }
        let cmd = &state.pending_lights[consumed];
        for pad in 0..2 {
            if cmd.pad_command_len[pad] > 0
                && let Some(conn) = state.devices[pad].connection_mut()
            {
                conn.send_command_lights(&cmd.pad_command[pad][..cmd.pad_command_len[pad]]);
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
    // Use system time as entropy source for serial assignment.
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

#[cfg(test)]
mod tests {
    use super::{capture_dir_from_value, override_should_swap};
    use crate::device::SmxInfo;

    #[test]
    fn capture_dir_trims_and_rejects_empty() {
        // The whole point: a trailing space (the `set VAR=path && app` gotcha)
        // is stripped so the capture path stays resolvable.
        assert_eq!(
            capture_dir_from_value("C:\\tmp\\smx-capture "),
            Some("C:\\tmp\\smx-capture")
        );
        assert_eq!(capture_dir_from_value("  /tmp/cap  "), Some("/tmp/cap"));
        assert_eq!(capture_dir_from_value("/tmp/cap"), Some("/tmp/cap"));
        // Empty or whitespace-only means "not set": no recording.
        assert_eq!(capture_dir_from_value(""), None);
        assert_eq!(capture_dir_from_value("   "), None);
    }

    fn info(connected: bool, is_player2: bool, serial: &str) -> SmxInfo {
        SmxInfo {
            connected,
            is_player2,
            has_serial_number: true,
            serial: serial.to_string(),
            firmware_version: 5,
        }
    }

    fn assignment(p1: &str, p2: &str) -> [Option<String>; 2] {
        [Some(p1.to_string()), Some(p2.to_string())]
    }

    #[test]
    fn no_override_defers_to_jumper() {
        let none: [Option<String>; 2] = [None, None];
        let a = info(true, false, "aaaa");
        let b = info(true, true, "bbbb");
        assert_eq!(override_should_swap(&none, &a, &b), None);
    }

    #[test]
    fn override_already_in_order_no_swap() {
        let asn = assignment("aaaa", "bbbb");
        // slot0 = P1 serial, slot1 = P2 serial -> already correct.
        let a = info(true, false, "aaaa");
        let b = info(true, true, "bbbb");
        assert_eq!(override_should_swap(&asn, &a, &b), Some(false));
    }

    #[test]
    fn override_reversed_requests_swap() {
        let asn = assignment("aaaa", "bbbb");
        // slot0 holds the P2 serial, slot1 the P1 serial -> swap.
        let a = info(true, true, "bbbb");
        let b = info(true, false, "aaaa");
        assert_eq!(override_should_swap(&asn, &a, &b), Some(true));
    }

    #[test]
    fn override_orders_two_same_jumper_pads() {
        // The key new capability: both pads report the same jumper (both P1),
        // which the jumper path can't order, but the override still does.
        let asn = assignment("aaaa", "bbbb");
        let reversed_a = info(true, false, "bbbb");
        let reversed_b = info(true, false, "aaaa");
        assert_eq!(override_should_swap(&asn, &reversed_a, &reversed_b), Some(true));

        let ordered_a = info(true, false, "aaaa");
        let ordered_b = info(true, false, "bbbb");
        assert_eq!(override_should_swap(&asn, &ordered_a, &ordered_b), Some(false));
    }

    #[test]
    fn lone_pad_unassigned_serial_defers_to_jumper() {
        // A lone pad whose serial isn't in the assignment follows its jumper.
        let asn = assignment("aaaa", "bbbb");
        let other = info(true, false, "cccc");
        let empty = info(false, false, "");
        assert_eq!(override_should_swap(&asn, &other, &empty), None);
        assert_eq!(override_should_swap(&asn, &empty, &other), None);
    }

    #[test]
    fn lone_pad_pinned_to_p2_relocates_to_slot1() {
        // The single-pad-as-P2 case: a lone P1-jumpered pad assigned to P2 (only
        // the P2 serial set) must move from slot 0 to slot 1.
        let p2_only: [Option<String>; 2] = [None, Some("aaaa".to_string())];
        let a = info(true, false, "aaaa"); // jumper P1, connected at slot 0
        let empty = info(false, false, "");
        assert_eq!(override_should_swap(&p2_only, &a, &empty), Some(true));
        // Already at slot 1 (e.g. after a prior relocate): stay put.
        assert_eq!(override_should_swap(&p2_only, &empty, &a), Some(false));
    }

    #[test]
    fn lone_pad_pinned_to_p1_stays_at_slot0() {
        let p1_only: [Option<String>; 2] = [Some("aaaa".to_string()), None];
        let a = info(true, true, "aaaa"); // jumper P2, but pinned to P1
        let empty = info(false, false, "");
        assert_eq!(override_should_swap(&p1_only, &a, &empty), Some(false));
        // Lone pad at slot 1 pinned to P1 -> relocate to slot 0.
        assert_eq!(override_should_swap(&p1_only, &empty, &a), Some(true));
    }

    #[test]
    fn unknown_serial_defers_to_jumper() {
        // One connected pad's serial isn't part of the assignment (e.g. a freshly
        // swapped-in pad) -> fall back to jumper ordering.
        let asn = assignment("aaaa", "bbbb");
        let a = info(true, false, "aaaa");
        let other = info(true, true, "cccc");
        assert_eq!(override_should_swap(&asn, &a, &other), None);
    }

    #[test]
    fn partial_assignment_defers_to_jumper() {
        // Only one slot pinned -> not a complete override, defer to jumper.
        let half: [Option<String>; 2] = [Some("aaaa".to_string()), None];
        let a = info(true, false, "aaaa");
        let b = info(true, true, "bbbb");
        assert_eq!(override_should_swap(&half, &a, &b), None);
    }
}
