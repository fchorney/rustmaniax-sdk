//! Rust SDK for StepManiaX dance pad controllers.
//!
//! # Quick Start
//! ```no_run
//! use rustmaniax_sdk::{SmxManager, UpdateReason};
//!
//! let mgr = SmxManager::start(|pad, reason| {
//!     if reason == UpdateReason::Connected {
//!         println!("Pad {pad} connected!");
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
pub use device::{SensorTestData, SensorTestMode, SmxInfo, UpdateReason};
pub use error::SmxError;
pub use manager::{PanelTestMode, SmxManager};

// Lights and animation.
pub use lights::{AnimationState, LightsType, UploadCommand, UploadData};
pub use lights::prepare_upload;

// Connection layer (for custom HID backends or testing).
pub use connection::{CommandCallback, HidDevice, HidDeviceInfo, HidEnumerator, HidapiEnumerator};

// Recording (for debugging/capture).
pub use recorder::{RecordingDevice, RecordingEnumerator};

// Protocol constants (useful for low-level inspection/testing).
pub use protocol::HID_REPORT_COMMAND;
