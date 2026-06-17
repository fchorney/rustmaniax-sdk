//! De-risk probe for the read/write HID handle split.
//!
//! Question this answers on real hardware: can we open one SMX device path
//! TWICE (a dedicated read handle and a dedicated write handle), and if so, does
//! a read() call on the read handle stay fast while blocking writes hammer the
//! write handle? Today both share one handle behind a mutex, so a read waits
//! behind every multi-millisecond write. If the second open succeeds and the
//! slowest read() call stays in microseconds under write load (not tracking the
//! slowest write), the handle split is viable.
//!
//! Note: pads emit input only on change plus a ~10Hz heartbeat, so most reads
//! return no data. That is why we time the read() CALL, not the gap between
//! data-bearing reads (which would just measure the pad's output cadence).
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
    let read_attempts = Arc::new(AtomicU64::new(0));
    let reads_with_data = Arc::new(AtomicU64::new(0));
    let input_reports = Arc::new(AtomicU64::new(0));
    // The key metric: the longest a single read() CALL took. Pads emit data only
    // on change (plus a ~10Hz heartbeat), so reads mostly return no-data; what we
    // care about is whether the read() call itself ever STALLS while a blocking
    // write is in flight on the other handle. With two independent handles and no
    // shared lock it should always return in microseconds, never tracking the
    // slowest write.
    let max_read_call_us = Arc::new(AtomicU64::new(0));

    // Reader thread: tight non-blocking read loop on the read handle, timing each
    // read() call under the concurrent write load below.
    let reader = {
        let stop = Arc::clone(&stop);
        let read_attempts = Arc::clone(&read_attempts);
        let reads_with_data = Arc::clone(&reads_with_data);
        let input_reports = Arc::clone(&input_reports);
        let max_read_call_us = Arc::clone(&max_read_call_us);
        std::thread::spawn(move || {
            let mut buf = [0u8; HID_PACKET_SIZE];
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                let result = read_dev.read(&mut buf);
                let call_us = t.elapsed().as_micros() as u64;
                max_read_call_us.fetch_max(call_us, Ordering::Relaxed);
                read_attempts.fetch_add(1, Ordering::Relaxed);
                match result {
                    Ok(0) => {}
                    Ok(n) => {
                        reads_with_data.fetch_add(1, Ordering::Relaxed);
                        if n >= 1 && buf[0] == HID_REPORT_INPUT_STATE {
                            input_reports.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        eprintln!("read error: {e}");
                        break;
                    }
                }
            }
        })
    };

    // Writer: hammer the write handle continuously (no pause) for maximum write
    // pressure, so a large share of the reader's read() calls overlap an in-flight
    // write. We time the slowest single write to show writes really do block.
    println!("Running concurrent read+write for {RUN_SECONDS}s (press the pad to generate input)...");
    let info_req = device_info_request();
    let mut writes: u64 = 0;
    let mut write_errors: u64 = 0;
    let mut max_write_us: u64 = 0;
    let deadline = Instant::now() + Duration::from_secs(RUN_SECONDS);
    while Instant::now() < deadline {
        let t = Instant::now();
        match write_dev.write(&info_req) {
            Ok(_) => {}
            Err(_) => write_errors += 1,
        }
        let us = t.elapsed().as_micros() as u64;
        max_write_us = max_write_us.max(us);
        writes += 1;
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
        "read attempts:        {} ({:.0}/s), with data {}, input-state reports {}",
        read_attempts.load(Ordering::Relaxed),
        read_attempts.load(Ordering::Relaxed) as f64 / secs,
        reads_with_data.load(Ordering::Relaxed),
        input_reports.load(Ordering::Relaxed)
    );
    println!(
        "slowest read() call:  {:.3}ms  <-- KEY: should be ~microseconds (well under the slowest",
        max_read_call_us.load(Ordering::Relaxed) as f64 / 1000.0
    );
    println!("                                    write). If it approaches the slowest write, reads are");
    println!("                                    still coupled to writes and the split would not help.");
}
