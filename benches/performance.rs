#![allow(dead_code, unused_imports)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use criterion::{Criterion, Throughput};

#[path = "../src/lights.rs"]
mod lights;
#[path = "../src/protocol.rs"]
mod protocol;

struct CountingAlloc;

static TRACK_ALLOCS: AtomicBool = AtomicBool::new(false);
static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: all allocations and deallocations are forwarded unchanged to the
// system allocator. The relaxed counters are diagnostic only.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if TRACK_ALLOCS.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        // SAFETY: this forwards the allocator contract and layout unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` came from the forwarded system allocation.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

#[cfg(windows)]
mod thread_cycles {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut c_void;
        fn QueryThreadCycleTime(thread: *mut c_void, cycles: *mut u64) -> i32;
    }

    pub fn now() -> u64 {
        let mut cycles = 0;
        // SAFETY: `GetCurrentThread` returns a valid pseudo-handle for the
        // calling thread, and `cycles` is a writable `u64` for the full call.
        let success = unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
        assert_ne!(success, 0, "QueryThreadCycleTime failed");
        cycles
    }
}

#[derive(Clone, Copy)]
struct AllocStats {
    count: usize,
    bytes: usize,
}

fn count_allocs<T>(f: impl FnOnce() -> T) -> (T, AllocStats) {
    TRACK_ALLOCS.store(false, Ordering::SeqCst);
    ALLOC_COUNT.store(0, Ordering::SeqCst);
    ALLOC_BYTES.store(0, Ordering::SeqCst);
    TRACK_ALLOCS.store(true, Ordering::SeqCst);
    let value = f();
    TRACK_ALLOCS.store(false, Ordering::SeqCst);
    let stats = AllocStats {
        count: ALLOC_COUNT.load(Ordering::SeqCst),
        bytes: ALLOC_BYTES.load(Ordering::SeqCst),
    };
    (value, stats)
}

struct LegacyReport {
    payload: Vec<u8>,
    flags: u8,
}

fn legacy_parse_report6(raw: &[u8]) -> Option<LegacyReport> {
    if raw.len() < 3 || raw[0] != protocol::HID_REPORT_DATA {
        return None;
    }
    let payload_len = raw[2] as usize;
    if raw.len() < 3 + payload_len {
        return None;
    }
    Some(LegacyReport {
        payload: raw[3..3 + payload_len].to_vec(),
        flags: raw[1],
    })
}

fn report6_sample() -> [u8; protocol::HID_PACKET_SIZE] {
    let mut raw = [0; protocol::HID_PACKET_SIZE];
    raw[0] = protocol::HID_REPORT_DATA;
    raw[1] = protocol::PACKET_FLAG_START_OF_COMMAND
        | protocol::PACKET_FLAG_END_OF_COMMAND
        | protocol::PACKET_FLAG_HOST_CMD_FINISHED;
    raw[2] = protocol::HID_MAX_PAYLOAD_SIZE as u8;
    for (i, byte) in raw[3..].iter_mut().enumerate() {
        *byte = i as u8;
    }
    raw
}

fn legacy_report_checksum(raw: &[u8]) -> u64 {
    let parsed = legacy_parse_report6(raw).unwrap();
    parsed
        .payload
        .iter()
        .map(|&byte| u64::from(byte))
        .sum::<u64>()
        + u64::from(parsed.flags)
}

fn report_checksum(raw: &[u8]) -> u64 {
    (match protocol::parse_report6(raw).unwrap() {
        protocol::ParsedPacket::DeviceInfo(payload)
        | protocol::ParsedPacket::Fragment { payload, .. } => {
            payload.iter().map(|&byte| u64::from(byte)).sum::<u64>()
        }
    }) + u64::from(raw[1])
}

fn legacy_scale(c: u8) -> u8 {
    (c as f32 * protocol::LED_COLOR_SCALE) as u8
}

fn scale_frame(frame: &[u8], scale: fn(u8) -> u8) -> u64 {
    frame
        .iter()
        .fold(0, |sum, &value| sum + u64::from(scale(value)))
}

fn report_allocations() {
    const FRAME_BYTES: usize = 2 * protocol::BYTES_PER_PAD_25;
    let report = report6_sample();
    let (old_checksum, old_report_allocs) = count_allocs(|| legacy_report_checksum(&report));
    let (new_checksum, new_report_allocs) = count_allocs(|| report_checksum(&report));
    assert_eq!(old_checksum, new_checksum);

    let mut old_animation = lights::AnimationState::new();
    let mut new_animation = lights::AnimationState::new();
    let (old_frame, old_frame_allocs) = count_allocs(|| old_animation.build_frame([0, 0]));
    let mut new_frame = [0xFF; FRAME_BYTES];
    let (_, new_frame_allocs) =
        count_allocs(|| new_animation.build_frame_into([0, 0], &mut new_frame));
    assert_eq!(old_frame, new_frame);

    let scale_input: [u8; FRAME_BYTES] = std::array::from_fn(|i| i as u8);
    let (old_scale, old_scale_allocs) = count_allocs(|| scale_frame(&scale_input, legacy_scale));
    let (new_scale, new_scale_allocs) =
        count_allocs(|| scale_frame(&scale_input, lights::scale_color));
    assert_eq!(old_scale, new_scale);

    println!("\nAllocation comparison (one hot-path operation):");
    println!(
        "  parse Report 6    old {:>2} alloc / {:>4} B | new {:>2} alloc / {:>4} B",
        old_report_allocs.count,
        old_report_allocs.bytes,
        new_report_allocs.count,
        new_report_allocs.bytes
    );
    println!(
        "  animation frame  old {:>2} alloc / {:>4} B | new {:>2} alloc / {:>4} B",
        old_frame_allocs.count,
        old_frame_allocs.bytes,
        new_frame_allocs.count,
        new_frame_allocs.bytes
    );
    println!(
        "  scale LED frame  old {:>2} alloc / {:>4} B | new {:>2} alloc / {:>4} B\n",
        old_scale_allocs.count,
        old_scale_allocs.bytes,
        new_scale_allocs.count,
        new_scale_allocs.bytes
    );

    assert!(new_report_allocs.count < old_report_allocs.count);
    assert!(new_frame_allocs.count < old_frame_allocs.count);
    assert_eq!(new_scale_allocs.count, 0);
}

#[cfg(windows)]
fn best_cycles(iterations: u64, mut f: impl FnMut()) -> f64 {
    for _ in 0..1_000 {
        f();
    }
    let mut best = u64::MAX;
    for _ in 0..25 {
        let start = thread_cycles::now();
        for _ in 0..iterations {
            f();
        }
        best = best.min(thread_cycles::now() - start);
    }
    best as f64 / iterations as f64
}

#[cfg(windows)]
fn report_cpu_cycles() {
    const FRAME_BYTES: usize = 2 * protocol::BYTES_PER_PAD_25;
    let report = report6_sample();
    let parse_old = best_cycles(100_000, || {
        black_box(legacy_report_checksum(black_box(&report)));
    });
    let parse_new = best_cycles(100_000, || {
        black_box(report_checksum(black_box(&report)));
    });

    let mut old_animation = lights::AnimationState::new();
    let frame_old = best_cycles(100_000, || {
        black_box(old_animation.build_frame(black_box([0, 0])));
    });
    let mut new_animation = lights::AnimationState::new();
    let mut output = [0; FRAME_BYTES];
    let frame_new = best_cycles(100_000, || {
        new_animation.build_frame_into(black_box([0, 0]), black_box(&mut output));
        black_box(&output);
    });

    let frame: [u8; FRAME_BYTES] = std::array::from_fn(|i| i as u8);
    let scale_old = best_cycles(10_000, || {
        black_box(scale_frame(black_box(&frame), legacy_scale));
    });
    let scale_new = best_cycles(10_000, || {
        black_box(scale_frame(black_box(&frame), lights::scale_color));
    });

    println!("Best thread CPU cycles per operation (25 batches):");
    println!("  parse Report 6    old {parse_old:>8.1} | new {parse_new:>8.1}");
    println!("  animation frame  old {frame_old:>8.1} | new {frame_new:>8.1}");
    println!("  scale LED frame  old {scale_old:>8.1} | new {scale_new:>8.1}\n");
}

#[cfg(not(windows))]
fn report_cpu_cycles() {}

fn bench_report6_parse(c: &mut Criterion) {
    let report = report6_sample();
    let mut group = c.benchmark_group("report6_parse");
    group.throughput(Throughput::Bytes(report.len() as u64));
    group.bench_function("legacy_owned_payload", |b| {
        b.iter(|| black_box(legacy_report_checksum(black_box(&report))));
    });
    group.bench_function("borrowed_payload", |b| {
        b.iter(|| black_box(report_checksum(black_box(&report))));
    });
    group.finish();
}

fn bench_animation_frame(c: &mut Criterion) {
    const FRAME_BYTES: usize = 2 * protocol::BYTES_PER_PAD_25;
    let mut group = c.benchmark_group("animation_frame");
    group.throughput(Throughput::Bytes(FRAME_BYTES as u64));

    let mut legacy = lights::AnimationState::new();
    group.bench_function("legacy_allocate", |b| {
        b.iter(|| black_box(legacy.build_frame(black_box([0, 0]))));
    });

    let mut optimized = lights::AnimationState::new();
    let mut output = [0; FRAME_BYTES];
    group.bench_function("reuse_buffer", |b| {
        b.iter(|| {
            optimized.build_frame_into(black_box([0, 0]), black_box(&mut output));
            black_box(&output);
        });
    });
    group.finish();
}

fn bench_color_scale(c: &mut Criterion) {
    const FRAME_BYTES: usize = 2 * protocol::BYTES_PER_PAD_25;
    let frame: [u8; FRAME_BYTES] = std::array::from_fn(|i| i as u8);
    let mut group = c.benchmark_group("led_color_scale");
    group.throughput(Throughput::Elements(FRAME_BYTES as u64));
    group.bench_function("legacy_float", |b| {
        b.iter(|| black_box(scale_frame(black_box(&frame), legacy_scale)));
    });
    group.bench_function("lookup_table", |b| {
        b.iter(|| black_box(scale_frame(black_box(&frame), lights::scale_color)));
    });
    group.finish();
}

fn main() {
    report_allocations();
    report_cpu_cycles();
    let mut criterion = Criterion::default()
        .sample_size(100)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2))
        .configure_from_args();
    bench_report6_parse(&mut criterion);
    bench_animation_frame(&mut criterion);
    bench_color_scale(&mut criterion);
    criterion.final_summary();
}
