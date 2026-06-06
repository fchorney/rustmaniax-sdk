//! Rust SDK for StepManiaX dance pad controllers.
//!
//! # Quick Start
//! ```no_run
//! use rustmaniax_sdk::{SmxManager, SmxEvent};
//!
//! let mgr = SmxManager::start(|event| {
//!     if let SmxEvent::Connected { pad, info } = &event {
//!         println!("Pad {pad} connected! fw={}", info.firmware_version);
//!     }
//! }).unwrap();
//!
//! // Query device state
//! let info = mgr.get_info(0);
//! let input = mgr.get_input_state(0);
//! ```

#![allow(dead_code)]

// Internal modules (not part of public API).
mod config;
mod connection;
mod device;
mod error;
mod lights;
mod manager;
mod protocol;
mod recorder;

// Public test infrastructure (hidden from docs, available for integration tests).
#[doc(hidden)]
pub mod test_helpers;

// ─── Public API ──────────────────────────────────────────────────────────────

// Core types.
pub use config::{ConfigFlags, PackedSensorSettings, SmxConfig};
pub use device::{SensorTestData, SensorTestMode, SmxEvent, SmxInfo, UpdateReason};
pub use error::SmxError;
pub use manager::{PanelTestMode, SmxManager};

// Lights and animation.
pub use lights::{AnimationState, LightsType, UploadCommand, UploadData};
pub use lights::prepare_upload;

// Connection layer (for custom HID backends or testing).
pub use connection::{CommandCallback, HidDevice, HidDeviceInfo, HidEnumerator, HidapiEnumerator};

// Recording (for debugging/capture).
pub use recorder::{RecordingDevice, RecordingEnumerator};

// Protocol constant (useful for low-level inspection/testing).
pub use protocol::HID_REPORT_COMMAND;

// Hardware-shape constants — stable facts about the pads that consumers need to
// size buffers, enumerate devices, or read serials without hardcoding them.
// (Wire/protocol internals — packet flags, timeouts, report IDs — stay private.)
pub use protocol::{
    BYTES_PER_PAD_16, BYTES_PER_PAD_25, LEDS_PER_PANEL_16, LEDS_PER_PANEL_25, NUM_PANELS,
    PLATFORM_STRIP_LEDS, SERIAL_SIZE, SMX_USB_PRODUCT_ID, SMX_USB_VENDOR_ID,
};
