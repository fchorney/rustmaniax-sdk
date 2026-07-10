# rustmaniax-sdk

Rust SDK for StepManiaX dance pad controllers.

This is a port of [stepmaniax-sdk-mp](https://github.com/fchorney/stepmaniax-sdk-mp) (C++) to Rust.

## Table of Contents

- [Quick Start](#quick-start)
- [Player Assignment](#player-assignment)
- [Dependencies](#dependencies)
- [Building](#building)
- [Running the Sample](#running-the-sample)
- [Testing](#testing)
- [Capture & Debugging Tools](#capture--debugging-tools)
- [Integration Example](#integration-example)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [Reporting Issues](#reporting-issues)
- [License](#license)
- [Acknowledgements](#acknowledgements)

## Quick Start

```rust
use rustmaniax_sdk::{SmxManager, SmxEvent};

let mgr = SmxManager::start(|event| match event {
    SmxEvent::Connected { pad, info } => {
        println!("Pad {pad}: P{}, fw={}", if info.is_player2 { 2 } else { 1 }, info.firmware_version);
    }
    SmxEvent::InputState { pad, state } => {
        println!("Pad {pad}: {state:04x}");
    }
    _ => {}
}).unwrap();
```

## Player Assignment

By default the two pad slots are ordered by each pad's hardware **P1/P2 jumper**
(slot 0 = P1, slot 1 = P2). When two pads share a jumper, are installed on the
wrong sides, or you want to play a single pad on a specific side, you can override
the ordering by serial:

```rust
// Pin which physical pad (by serial) is P1 vs P2. Overrides the jumper and
// re-orders the slots live.
//
// With two pads connected, the override engages only when both connected serials
// are the two given serials; otherwise ordering falls back to the jumper.
//
// A single connected pad follows a single-sided assignment: pass only the P2
// serial to place a lone pad on slot 1 (P2), or only the P1 serial to place it on
// slot 0 (P1), regardless of the pad's jumper.
mgr.set_player_assignment(Some(p1_serial), Some(p2_serial));

// Single pad played as P2 (its jumper is ignored):
mgr.set_player_assignment(None, Some(p2_serial));

// Clear the override (follow the jumper again):
mgr.set_player_assignment(None, None);
```

For lighting, use `set_lights` with a `2 * BYTES_PER_PAD_25`-byte buffer (the
hardware-shape constants — `NUM_PANELS`, `BYTES_PER_PAD_16/25`, `LEDS_PER_PANEL_*`,
`SMX_USB_VENDOR_ID`/`PRODUCT_ID`/`PRODUCT_STRING`, `SERIAL_SIZE` — are re-exported
from the crate root so callers don't hardcode sizes or device identity).

To light one pad and leave the other alone, use `set_lights_for_pads` with a
per-pad selector. The buffer still covers both pads; a deselected pad receives no
lights command, so its firmware auto-lighting resumes (a pad reverts once lights
stop arriving). Both pads' commands are queued together either way, so the two
never drift out of phase: driving one pad does not disturb the other's timing.

```rust
// Light P1, hand P2 back to its firmware.
mgr.set_lights_for_pads(&frame, [true, false]);

// Release a single pad now, instead of waiting out the firmware's timeout.
mgr.reenable_auto_lights_for_pad(1);
```

## Dependencies

- **Rust** 1.85+ (edition 2024)
- **hidapi** system library:
  - macOS: `brew install hidapi`
  - Linux: `sudo apt-get install libudev-dev`
  - Windows: no additional dependencies (uses `windows-native` feature)
- **Python 3** (optional, for `decode_smxhid.py` capture analysis)

## Building

```bash
cargo build
cargo test
cargo clippy
```

## Running the Sample

```bash
cargo run --features sample --bin smx-sample
cargo run --features sample --bin smx-sample -- --all-packets
cargo run --features sample --bin smx-sample -- --calibrated
cargo run --features sample --bin smx-sample -- --test-mode
cargo run --features sample --bin smx-sample -- 50 --all-packets
```

Flags:
- `[main_thread_ms]` — main-thread loop cadence (positional arg). Paces lifecycle work (enumeration, command writes, lights, config/sensor responses). Input reads are not paced by it — each pad's poll thread blocks on the device and wakes when a report arrives.
- `--all-packets` — fire input callback on every USB packet (not just changes)
- `--test-mode` — enable panel pressure test mode
- `--uncalibrated` / `--calibrated` / `--noise` / `--tare` — sensor test modes

Enable debug logging with `RUST_LOG=debug`.

There is also a `smx-sensor-rate` probe that measures how many sensor-test samples per second arrive from each connected pad, both while streaming light frames at 30Hz and with no light traffic:

```bash
cargo run --features sample --bin smx-sensor-rate -- [phase_secs] [lights_hz]
```

Lights and sensor-test polling share one per-pad command pipeline. The SDK schedules them fairly: light frames are coalesced and bounded to one un-sent frame at a time (last-writer-wins, so stale frames never back up), the sensor request is paced to ~30Hz and inserted ahead of a pending light frame for low latency, and the main loop wakes exactly when the next request is due. This keeps both ~30Hz sensor sampling and ~30Hz lights without either starving the other; under a tight pipeline the light frame rate degrades gracefully rather than backlogging. The probe streams a moving pattern so light lag is visible and reports per-pad sample rates, to confirm on real hardware.

There is also an `smx-input-timing` probe that verifies how fast input reports actually arrive. It runs in all-packets mode and reports each pad's arrival rate plus an inter-arrival histogram:

```bash
cargo run --features sample --bin smx-input-timing
# press r to reset, q to quit
```

Input reads are interrupt-driven: each pad's poll thread blocks on the device and wakes the instant a report arrives, so a connected pad streams at close to the USB frame rate. Full Speed USB delivers one HID report per 1ms frame -- the hard precision floor for step timing regardless of firmware sampling rate -- so a healthy pad shows ~1000 reports/sec and the histogram clusters at ~1ms.

## Testing

```bash
# Unit + integration + replay tests (no hardware needed)
cargo test

# Hardware tests (requires SMX pad connected via USB)
./scripts/test-hardware.sh

# Record new captures for replay regression tests
./scripts/capture.sh
```

## Capture & Debugging Tools

The SDK supports recording HID traffic to `.smxhid` files for debugging and regression testing. Set `SMX_CAPTURE_DIR` to enable:

```bash
SMX_CAPTURE_DIR=capture/my_session cargo run --bin smx-sample
```

To decode and inspect capture files:

```bash
python3 scripts/decode_smxhid.py capture/connection/device_0.smxhid
```

See the [C++ SDK's capture documentation](https://github.com/fchorney/stepmaniax-sdk-mp#capture-recording) for details on the `.smxhid` file format.

## Integration Example

If your application already owns a `HidApi` instance (e.g., [deadsync](https://github.com/pnn64/deadsync)), share it to avoid initializing hidapi twice:

```rust
use std::sync::Arc;

let hid_api = Arc::new(std::sync::Mutex::new(hidapi::HidApi::new().unwrap()));
let enumerator = rustmaniax_sdk::HidapiEnumerator::from_shared(hid_api);
let smx = rustmaniax_sdk::SmxManager::new(Box::new(enumerator), |event| { /* ... */ });
```

## Documentation

- [Design Differences from C++ SDK](docs/DESIGN_DIFFERENCES.md) — architectural decisions and rationale
- [Architecture & Code Paths](docs/ARCHITECTURE.md) — threading model and data flow
- [USB Protocol](https://github.com/fchorney/stepmaniax-sdk-mp/blob/main/docs/USB_PROTOCOL.md) — HID packet format (unchanged from C++ SDK)

## Contributing

### Code Style

- Run `cargo clippy` and `cargo test` before submitting
- Follow existing patterns — match the style of surrounding code
- Keep `unsafe` usage minimal and well-documented

### Branching

Use the format `initials/branch_name`:

```
fc/fix-reconnect-handling
fc/add-animation-upload
```

### Key Considerations

- All protocol changes must maintain compatibility with the C++ SDK's `.smxhid` capture format
- New features should include unit tests and, where applicable, hardware integration tests
- Packed struct fields require `read_unaligned`/`write_unaligned` — never take references to them

## Reporting Issues

When filing a bug report, please include:

- **OS and architecture** (e.g., macOS 14 ARM64, Ubuntu 24.04 x86_64)
- **Rust version** (`rustc --version`)
- **SMX pad firmware version** (printed on connection)
- **Steps to reproduce**
- **Capture file** if possible — run with `SMX_CAPTURE_DIR=capture/bug_report` and attach the `.smxhid` files
- **Log output** — run with `RUST_LOG=debug` and include relevant lines

## Acknowledgements

- [StepManiaX](https://stepmaniax.com/) — the original SDK and hardware
- [stepmaniax-sdk-mp](https://github.com/fchorney/stepmaniax-sdk-mp) — the cross-platform C++ port this is based on
- [deadsync](https://github.com/pnn64/deadsync) — the project that inspired this Rust port

## License

MIT — see [LICENSE](LICENSE) for details.
