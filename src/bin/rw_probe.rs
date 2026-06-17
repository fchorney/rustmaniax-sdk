//! De-risk probe for the read/write HID handle split.
//!
//! Question this answers on real hardware: can we open one SMX device path
//! TWICE (a dedicated read handle and a dedicated write handle), and if so, do
//! reads on the read handle keep flowing while blocking writes hammer the write
//! handle? Today both share one handle behind a mutex, so a read waits behind
//! every multi-millisecond write. If a second open succeeds and the read gap
//! stays small under write load, the handle split is viable.
//!
//! Run on the Mini with a pad connected:
//!   cargo run --features sample --bin smx-rw-probe
//!
//! It does NOT use SmxManager; it talks to hidapi directly so it measures the
//! raw platform behaviour, not the SDK's threading.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustmaniax_sdk::{SMX_USB_PRODUCT_ID, SMX_USB_PRODUCT_STRING, SMX_USB_VENDOR_ID};

const HID_PACKET_SIZE: usize = 64;
const HID_REPORT_COMMAND: u8 = 0x05;
const HID_REPORT_INPUT_STATE: u8 = 0x03;
const PACKET_FLAG_DEVICE_INFO: u8 = 0x80;
const RUN_SECONDS: u64 = 8;

fn device_info_request() -> [u8; HID_PACKET_SIZE] {
    let mut packet = [0u8; HID_PACKET_SIZE];
    packet[0] = HID_REPORT_COMMAND;
    packet[1] = PACKET_FLAG_DEVICE_INFO;
    packet
}

fn main() {
    let api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Failed to init hidapi: {e}");
            std::process::exit(1);
        }
    };

    let path = api
        .device_list()
        .find(|d| {
            d.vendor_id() == SMX_USB_VENDOR_ID
                && d.product_id() == SMX_USB_PRODUCT_ID
                && d.product_string()
                    .unwrap_or_default()
                    .contains(SMX_USB_PRODUCT_STRING)
        })
        .map(|d| d.path().to_owned());

    let Some(path) = path else {
        eprintln!("No StepManiaX device found. Plug a pad in and retry.");
        std::process::exit(1);
    };
    println!("Found SMX device: {path:?}");

    // First open: the read handle.
    let read_dev = match api.open_path(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("First open failed: {e}");
            std::process::exit(1);
        }
    };
    println!("First open OK (read handle).");

    // Second open of the SAME path: the write handle. This is the question.
    let write_dev = match api.open_path(&path) {
        Ok(d) => {
            println!("SECOND open OK (write handle). Double-open is allowed here.");
            d
        }
        Err(e) => {
            println!("SECOND open FAILED: {e}");
            println!("=> Double-open is NOT allowed on this platform; the handle split");
            println!("   would need a different approach (or a single-open fallback).");
            std::process::exit(2);
        }
    };

    if let Err(e) = read_dev.set_blocking_mode(false) {
        eprintln!("set_blocking_mode(false) on read handle failed: {e}");
        std::process::exit(1);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let input_reports = Arc::new(AtomicU64::new(0));
    let max_gap_us = Arc::new(AtomicU64::new(0));

    // Reader thread: tight non-blocking read loop on the read handle, tracking
    // the largest gap between two successful reads. That gap is the input
    // latency we care about; if the write load below does not inflate it, the
    // split works.
    let reader = {
        let stop = Arc::clone(&stop);
        let reads = Arc::clone(&reads);
        let input_reports = Arc::clone(&input_reports);
        let max_gap_us = Arc::clone(&max_gap_us);
        std::thread::spawn(move || {
            let mut buf = [0u8; HID_PACKET_SIZE];
            let mut last_read = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                match read_dev.read(&mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        let now = Instant::now();
                        let gap = now.duration_since(last_read).as_micros() as u64;
                        max_gap_us.fetch_max(gap, Ordering::Relaxed);
                        last_read = now;
                        reads.fetch_add(1, Ordering::Relaxed);
                        if n >= 1 && buf[0] == HID_REPORT_INPUT_STATE {
                            input_reports.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("read error: {e}");
                        break;
                    }
                }
                // Match the SDK's poll cadence so the gap reflects real behaviour.
                std::thread::sleep(Duration::from_micros(1000));
            }
        })
    };

    // Writer: hammer the write handle. Each iteration writes several packets
    // back to back to mimic a multi-packet light frame, then a short pause to
    // mimic the ~30Hz frame rate. We time the slowest single write.
    println!("Running concurrent read+write for {RUN_SECONDS}s (press the pad to generate input)...");
    let info_req = device_info_request();
    let mut writes: u64 = 0;
    let mut write_errors: u64 = 0;
    let mut max_write_us: u64 = 0;
    let deadline = Instant::now() + Duration::from_secs(RUN_SECONDS);
    while Instant::now() < deadline {
        // ~4 packets per "frame", like a lights '2'/'3'/'4' batch.
        for _ in 0..4 {
            let t = Instant::now();
            match write_dev.write(&info_req) {
                Ok(_) => {}
                Err(_) => write_errors += 1,
            }
            let us = t.elapsed().as_micros() as u64;
            max_write_us = max_write_us.max(us);
            writes += 1;
        }
        std::thread::sleep(Duration::from_millis(33));
    }

    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();

    let secs = RUN_SECONDS as f64;
    println!("\n=== rw-probe results ===");
    println!("second open allowed:  YES");
    println!(
        "writes:               {writes} ({:.0}/s), errors {write_errors}, slowest write {:.2}ms",
        writes as f64 / secs,
        max_write_us as f64 / 1000.0
    );
    println!(
        "reads:                {} ({:.0}/s), input-state reports {}",
        reads.load(Ordering::Relaxed),
        reads.load(Ordering::Relaxed) as f64 / secs,
        input_reports.load(Ordering::Relaxed)
    );
    println!(
        "max read gap:         {:.2}ms  <-- key number: should stay near the 1ms poll cadence,",
        max_gap_us.load(Ordering::Relaxed) as f64 / 1000.0
    );
    println!("                                   not track the slowest write, if the split helps.");
}
