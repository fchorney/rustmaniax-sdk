//! HID traffic recording for debugging and regression test capture.
//!
//! When `SMX_CAPTURE_DIR` is set, the manager wraps the real HID enumerator
//! with a recording layer that writes `.smxhid` files for every opened device.
//!
//! File format: `"SMXHID\x01"` magic, then records of `[type:1][timestamp_us:8][size:2][data:size]`.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::connection::{HidDevice, HidDeviceInfo, HidEnumerator};
use crate::error::SmxError;

const HID_CAPTURE_MAGIC: &[u8; 7] = b"SMXHID\x01";

/// One capture file plus the shared clock for its records. Shared (via `Arc`)
/// between the read and write handles of a single physical device so both record
/// into the same interleaved `.smxhid` stream, preserving the one-file-per-device
/// format even though the device is now opened twice.
struct RecordingSink {
    file: Mutex<File>,
    start: Instant,
}

impl RecordingSink {
    fn create(output_path: &Path) -> std::io::Result<Self> {
        let mut file = File::create(output_path)?;
        file.write_all(HID_CAPTURE_MAGIC)?;
        Ok(Self {
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

/// Wraps a HidDevice and records all reads/writes to a shared `.smxhid` sink.
pub struct RecordingDevice {
    device: Box<dyn HidDevice>,
    sink: Arc<RecordingSink>,
}

impl HidDevice for RecordingDevice {
    fn read(&self, buf: &mut [u8]) -> Result<usize, SmxError> {
        let n = self.device.read(buf)?;
        if n > 0 {
            self.sink.write_record(b'R', &buf[..n]);
        }
        Ok(n)
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ms: i32) -> Result<usize, SmxError> {
        let n = self.device.read_timeout(buf, timeout_ms)?;
        if n > 0 {
            self.sink.write_record(b'R', &buf[..n]);
        }
        Ok(n)
    }

    fn write(&self, buf: &[u8]) -> Result<usize, SmxError> {
        let n = self.device.write(buf)?;
        if n > 0 {
            self.sink.write_record(b'W', buf);
        }
        Ok(n)
    }
}

// RecordingDevice is Send because the inner device is Send and Arc<RecordingSink>
// is Send + Sync.

/// Wraps a HidEnumerator and records traffic for every opened device.
pub struct RecordingEnumerator {
    inner: Box<dyn HidEnumerator>,
    output_dir: PathBuf,
    // One sink per distinct device path. The manager opens each path twice (read
    // + write handle); both opens resolve to the same sink, so a device's reads
    // and writes land in one interleaved file (device_0, device_1, ... numbered
    // per distinct path, not per open).
    sinks: Mutex<Vec<(String, Arc<RecordingSink>)>>,
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
            sinks: Mutex::new(Vec::new()),
        }
    }

    /// Return the sink for `path`, creating its capture file on first use.
    /// `None` if the file can't be created (caller records nothing).
    fn sink_for(&self, path: &str) -> Option<Arc<RecordingSink>> {
        let mut sinks = self.sinks.lock().unwrap();
        if let Some((_, sink)) = sinks.iter().find(|(p, _)| p == path) {
            return Some(Arc::clone(sink));
        }
        let file_path = self.output_dir.join(format!("device_{}.smxhid", sinks.len()));
        match RecordingSink::create(&file_path) {
            Ok(sink) => {
                let sink = Arc::new(sink);
                sinks.push((path.to_string(), Arc::clone(&sink)));
                Some(sink)
            }
            Err(e) => {
                log::error!("Failed to create capture file {}: {e}", file_path.display());
                None
            }
        }
    }
}

impl HidEnumerator for RecordingEnumerator {
    fn enumerate(&self, vid: u16, pid: u16) -> Vec<HidDeviceInfo> {
        self.inner.enumerate(vid, pid)
    }

    fn open(&self, path: &str) -> Result<Box<dyn HidDevice>, SmxError> {
        let device = self.inner.open(path)?;
        match self.sink_for(path) {
            Some(sink) => Ok(Box::new(RecordingDevice { device, sink })),
            // Couldn't open a capture file; pass the device through unrecorded.
            None => Ok(device),
        }
    }
}

fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Raw epoch seconds as a simple unique directory name.
    // For human-readable timestamps you'd use chrono, but this avoids the dependency.
    format!("{secs}")
}
