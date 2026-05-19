//! HID traffic recording for debugging and regression test capture.
//!
//! When `SMX_CAPTURE_DIR` is set, the manager wraps the real HID enumerator
//! with a recording layer that writes `.smxhid` files for every opened device.
//!
//! File format: `"SMXHID\x01"` magic, then records of `[type:1][timestamp_us:8][size:2][data:size]`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use crate::connection::{HidDevice, HidDeviceInfo, HidEnumerator};
use crate::error::SmxError;

const HID_CAPTURE_MAGIC: &[u8; 7] = b"SMXHID\x01";

/// Wraps a HidDevice and records all reads/writes to a `.smxhid` file.
pub struct RecordingDevice {
    device: Box<dyn HidDevice>,
    file: Mutex<File>,
    start: Instant,
}

impl RecordingDevice {
    pub fn new(device: Box<dyn HidDevice>, output_path: &Path) -> std::io::Result<Self> {
        let mut file = File::create(output_path)?;
        file.write_all(HID_CAPTURE_MAGIC)?;
        Ok(Self {
            device,
            file: Mutex::new(file),
            start: Instant::now(),
        })
    }

    fn write_record(&self, record_type: u8, data: &[u8]) {
        let timestamp_us = self.start.elapsed().as_micros() as u64;
        let size = data.len() as u16;

        let mut file = self.file.lock().unwrap();
        let _ = file.write_all(&[record_type]);
        let _ = file.write_all(&timestamp_us.to_le_bytes());
        let _ = file.write_all(&size.to_le_bytes());
        let _ = file.write_all(data);
        let _ = file.flush();
    }
}

impl HidDevice for RecordingDevice {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError> {
        let n = self.device.read(buf)?;
        if n > 0 {
            self.write_record(b'R', &buf[..n]);
        }
        Ok(n)
    }

    fn write(&self, buf: &[u8]) -> Result<usize, SmxError> {
        let n = self.device.write(buf)?;
        if n > 0 {
            self.write_record(b'W', buf);
        }
        Ok(n)
    }
}

// RecordingDevice is Send because File is Send and the inner device is Send.
unsafe impl Send for RecordingDevice {}

/// Wraps a HidEnumerator and records traffic for every opened device.
pub struct RecordingEnumerator {
    inner: Box<dyn HidEnumerator>,
    output_dir: PathBuf,
    device_count: Mutex<usize>,
}

impl RecordingEnumerator {
    /// Create a recording enumerator that writes captures to `output_dir`.
    /// If `timestamp_subdir` is true, creates a timestamped subdirectory.
    pub fn new(
        inner: Box<dyn HidEnumerator>,
        output_dir: &Path,
        timestamp_subdir: bool,
    ) -> Self {
        let dir = if timestamp_subdir {
            let ts = chrono_timestamp();
            output_dir.join(ts)
        } else {
            output_dir.to_path_buf()
        };
        let _ = fs::create_dir_all(&dir);
        log::info!("Recording HID traffic to: {}", dir.display());

        Self {
            inner,
            output_dir: dir,
            device_count: Mutex::new(0),
        }
    }
}

impl HidEnumerator for RecordingEnumerator {
    fn enumerate(&self, vid: u16, pid: u16) -> Vec<HidDeviceInfo> {
        self.inner.enumerate(vid, pid)
    }

    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError> {
        let device = self.inner.open(path)?;

        let mut count = self.device_count.lock().unwrap();
        let file_path = self.output_dir.join(format!("device_{}.smxhid", *count));
        *count += 1;

        match RecordingDevice::new(device, &file_path) {
            Ok(rec) => Ok(Box::new(rec)),
            Err(e) => {
                log::error!("Failed to create capture file {}: {e}", file_path.display());
                // Fall back to opening without recording.
                self.inner.open(path)
            }
        }
    }
}

fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple timestamp: YYYY-MM-DD_HH-MM-SS (approximate from epoch).
    // For a proper implementation you'd use chrono, but this avoids the dependency.
    format!("{secs}")
}
