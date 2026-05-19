# rustmaniax-sdk

Rust SDK for StepManiaX dance pad controllers.

This is a port of [stepmaniax-sdk-mp](https://github.com/fchorney/stepmaniax-sdk-mp) (C++) to Rust.

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

## Building

```bash
cargo build
cargo test
```

### macOS

```bash
brew install hidapi
```

## Running the Sample

```bash
cargo run --bin smx-sample
cargo run --bin smx-sample -- --all-packets
cargo run --bin smx-sample -- --calibrated
cargo run --bin smx-sample -- 50 500 --all-packets
```

## Hardware Tests

Requires a physical SMX pad connected via USB:

```bash
./scripts/test-hardware.sh
```

To record captures:

```bash
./scripts/capture.sh
```

## Documentation

- [Design Differences from C++ SDK](docs/DESIGN_DIFFERENCES.md) — architectural decisions and rationale
- [Architecture & Code Paths](docs/ARCHITECTURE.md) — threading model and data flow
- [USB Protocol](https://github.com/fchorney/stepmaniax-sdk-mp/blob/main/docs/USB_PROTOCOL.md) — HID packet format (unchanged from C++ SDK)

## Integration Example

If your application already owns a `HidApi` instance (e.g., [deadsync](https://github.com/pnn64/deadsync)), share it to avoid initializing hidapi twice:

```rust
use std::sync::Arc;

// Share deadsync's existing HidApi instance:
let hid_api = Arc::new(std::sync::Mutex::new(hidapi::HidApi::new().unwrap()));
let enumerator = rustmaniax_sdk::HidapiEnumerator::from_shared(hid_api);
let smx = rustmaniax_sdk::SmxManager::new(Box::new(enumerator), |event| { /* ... */ });
```
