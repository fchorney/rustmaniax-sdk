# Rust Port: Design Differences from C++ SDK

This document explains the architectural and design decisions that differ between `stepmaniax-sdk-mp` (C++) and `rustmaniax-sdk` (Rust), and why.

## Threading: Split-Struct Connection

**C++:** `SMXDeviceConnection` is a single class accessed by two threads. The USB polling thread calls `PollUSBData()` and the main I/O thread calls `Update()`/`SendCommand()`. Thread safety is managed by careful documentation, atomics, and a mutex on the Report 6 buffer.

**Rust:** The connection is split into two separate types at construction time:
- `PollHandle` — owned by the USB polling thread; reads only (owns the device's *read* HID handle)
- `CommandHandle` — owned by the main I/O thread; sends commands and processes responses (owns the device's *write* HID handle)

They share input and coordination state through `Arc` (`AtomicU16` input state, `AtomicBool` flags, `Mutex<Vec<u8>>` Report 6 buffer), but **not** the HID handle — each owns its own.

**Why:** Rust's type system enforces thread safety at compile time. A single struct accessed from two threads requires `unsafe` or wrapping everything in `Arc<Mutex<>>`. The split design makes the threading contract explicit in the types — `PollHandle` physically cannot call `send_command()`, and `CommandHandle` physically cannot call `poll()`. This eliminates an entire class of bugs that the C++ version guards against with documentation and careful coding.

**Shared by both SDKs as of 1.4.0:** the *separate read/write HID handles* (so an input read never waits behind a blocking write) and keeping the USB poll thread *off the manager's state lock* are now present in both the Rust and C++ SDKs — those are no longer differences. What remains different here is purely structural: two distinct types vs one class holding two handles. See [ARCHITECTURE.md](ARCHITECTURE.md) for the lock and handle details.

## Callback System: Events with Data

**C++:** The update callback receives `(pad, reason, pUser)` and the application calls back into the SDK (`SMX_GetInfo`, `SMX_GetInputState`, etc.) to get the relevant data.

**Rust:** The callback receives an `SmxEvent` enum that carries all relevant data:
```rust
enum SmxEvent {
    Connected { pad: usize, info: SmxInfo },
    Disconnected { pad: usize },
    InputState { pad: usize, state: u16 },
    ConfigUpdated { pad: usize },
    SensorTestData { pad: usize, data: SensorTestData },
}
```

**Why:** In Rust, the callback cannot hold a reference to the manager that owns it (circular reference). Rather than work around this with `Arc` or global state, we pass the data directly. This is also more efficient (no lock acquisition in the callback) and simpler for consumers — everything needed is in the event.

## Error Handling: Result<T, E> Instead of Out-Parameters

**C++:** Functions like `SMX_GetConfig(pad, &config)` return `bool` and write to an out-parameter.

**Rust:** Functions return `Option<T>` or `Result<T, E>`:
```rust
fn get_config(&self, pad: usize) -> Option<SmxConfig>
```

**Why:** This is idiomatic Rust. The compiler forces callers to handle the None/Err case, preventing use of uninitialized data.

## HID Abstraction: Trait-Based Dependency Injection

**C++:** `IHIDDevice` and `IHIDEnumerator` are abstract classes with virtual methods. `SMX_StartWithEnumerator` is marked as test-only.

**Rust:** `HidDevice` and `HidEnumerator` are traits. Both `SmxManager::new()` (custom enumerator) and `SmxManager::start()` (real hidapi) are public API.

**Why:** The custom-enumerator constructor is useful beyond testing — it enables sharing a `HidApi` instance with the host application (critical on macOS where hidapi can only be initialized once per process). Making it public costs nothing and enables real use cases.

## Shared HidApi: `HidapiEnumerator::from_shared()`

**C++:** Each `SMXManager` creates and owns its own hidapi instance.

**Rust:** `HidapiEnumerator` can be constructed with `from_shared(Arc<hidapi::HidApi>)` to share a single HidApi instance across the process.

**Why:** On macOS, hidapi (via IOKit) cannot be safely initialized multiple times in the same process. If the host application already uses hidapi for other input devices, the SDK must share the same instance.

## No Recursive Mutex

**C++:** Uses `std::recursive_mutex` for the manager lock, allowing nested lock acquisition from callbacks and re-entrant calls.

**Rust:** Uses `std::sync::Mutex` (non-recursive). The manager locks, does work, drops the lock, then waits on a condvar.

**Why:** Recursive mutexes are generally considered a code smell — they mask design issues where lock boundaries are unclear. Rust's standard library doesn't provide one. The non-recursive design forces cleaner separation of locked and unlocked code paths.

## Logging: `log` Crate Instead of Custom Callback

**C++:** `SMX_SetLogCallback` lets the application provide a function pointer for log output.

**Rust:** Uses the standard `log` crate. The application chooses its own logger implementation (`env_logger`, `tracing`, etc.).

**Why:** The `log` crate is Rust's universal logging facade. Any Rust application already has a logger configured. Adding a custom callback API would be redundant and non-idiomatic.

## Packed Struct Access

**C++:** `#pragma pack(push, 1)` structs are accessed directly. The compiler handles unaligned access transparently on x86.

**Rust:** `#[repr(C, packed)]` structs require `std::ptr::read_unaligned()` / `write_unaligned()` for multi-byte fields. Direct field access can cause SIGILL on strict-alignment architectures.

**Why:** Rust treats unaligned access as undefined behavior and the compiler enforces this. The `bytemuck` crate provides safe zero-copy casting between byte slices and packed structs, but individual field access still requires care.

## Config Struct Padding

**C++:** `uint8_t padding[49]` works directly.

**Rust:** Split into `padding: [u8; 32]` + `padding2: [u8; 17]` because `bytemuck`'s `Pod`/`Zeroable` derive macros only support arrays up to size 48.

**Why:** A limitation of the `bytemuck` crate's const generics support. Functionally identical — the padding is opaque.

## No Global Singleton

**C++:** `SMX_Start`/`SMX_Stop` manage a global singleton. All `SMX_*` functions operate on it implicitly.

**Rust:** `SmxManager` is an owned value. The application holds it and calls methods on it. Multiple managers could theoretically coexist (though hidapi constraints prevent this in practice).

**Why:** Global mutable state is antithetical to Rust's ownership model. Explicit ownership makes lifetimes clear, enables testing, and prevents hidden coupling.

## Recording: Same Format, Different Integration

**C++:** Recording is triggered by the `SMX_CAPTURE_DIR` environment variable and wraps the enumerator internally.

**Rust:** Same behavior via `SmxManager::start()`, but `RecordingEnumerator` is also public API — applications can wrap any enumerator with recording explicitly.

**Why:** No reason to hide it. Useful for debugging in production.

## Test Infrastructure

**C++:** `FakeDevice` auto-responds to commands inline during `Write()`. Tests use `WaitFor()` polling with timeouts.

**Rust:** Same pattern — `FakeDevice::new_auto()` creates a device that auto-responds to device info and config requests. `ReplayDevice` gates reads by write count for deterministic replay. `wait_for()` provides the same polling timeout.

**Difference:** Hardware tests share a single `SmxManager` via `OnceLock` (static singleton for the test binary) because macOS hidapi can't be re-initialized. The C++ tests create/destroy managers per test.

## What Stayed the Same

- USB protocol (packet format, fragmentation, report IDs)
- Device info handshake flow
- Config wire format (250 bytes, packed)
- Legacy config conversion (all firmware versions supported)
- Sensor test data bit-interleaving algorithm
- Lights command structure (commands '2', '3', '4', color scaling)
- Animation GIF loading (14×15 and 23×24 formats)
- Firmware upload packet format and interleaving
- Panel test mode periodic refresh
- 30 FPS lights rate limiting
- Device ordering (P1 in slot 0, P2 in slot 1)
- `.smxhid` capture file format (binary compatible)
