# Performance

The latency-sensitive paths are HID response parsing and 30 FPS light-frame
streaming. Run their old-vs-new benchmarks with:

```sh
cargo bench --bench performance -- --noplot
```

The harness keeps the replaced algorithms as benchmark-only baselines. It
reports Criterion wall time and throughput, counts heap allocations and bytes,
and uses `QueryThreadCycleTime` for per-thread CPU cycles on Windows. Release
benchmarks retain debug symbols for profiler use.

## Baseline

Measured on 2026-08-21 with rustc 1.98.0, Windows x86-64, and an Intel Xeon
E5-2696 v4. Values are the middle Criterion estimate and the best of 25 batched
thread-cycle samples. Results vary by machine; compare implementations within
the same run.

| Hot path | Implementation | Time | Throughput | CPU cycles | Allocations |
|---|---|---:|---:|---:|---:|
| Report 6 parse, 64 B | owned payload | 70.98 ns | 859.94 MiB/s | 139.1 | 1 / 61 B |
| Report 6 parse, 64 B | borrowed payload | 14.50 ns | 4.11 GiB/s | 29.3 | 0 / 0 B |
| Animation frame, 1,350 B | allocate output | 117.98 ns | 10.66 GiB/s | 243.7 | 1 / 1,350 B |
| Animation frame, 1,350 B | reuse output | 36.52 ns | 34.43 GiB/s | 71.6 | 0 / 0 B |
| Scale 1,350 LED bytes | float multiply | 1.75 us | 771.73 Melem/s | 3,636.8 | 0 / 0 B |
| Scale 1,350 LED bytes | lookup table | 428.76 ns | 3.15 Gelem/s | 891.0 | 0 / 0 B |

## Optimized paths

- Report 6 parsing borrows the payload from the received packet. Reassembly
  copies it once into the owned response instead of first allocating a temporary
  payload and then copying it again.
- `AnimationState::build_frame_into` lets the animation thread retain one
  1,350-byte buffer for its lifetime. `build_frame` remains as the allocating
  convenience API.
- LED scaling uses a compile-time 256-byte lookup table. The table is tested
  exhaustively against the prior floating-point expression for every `u8`.

Protocol unit tests cover packet flags, truncation, fragmentation, and borrowed
storage. Animation tests compare allocating and reusable APIs across released
and pressed states. The full integration and `.smxhid` replay suites protect
wire behavior.
