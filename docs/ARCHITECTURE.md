# Architecture & Code Paths

This document describes the internal architecture of the Rust SDK. For the USB protocol (HID packet format, report types, fragmentation), see the [C++ SDK's USB Protocol doc](https://github.com/fchorney/stepmaniax-sdk-mp/blob/main/docs/USB_PROTOCOL.md) — the wire protocol is identical.

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         Application Layer                                │
│                                                                         │
│   SmxManager::start() / ::new()                                         │
│   mgr.get_info() / mgr.get_input_state() / mgr.set_config() / ...      │
│   Callback receives SmxEvent { Connected, InputState, ... }             │
└────────────────────────────────┬────────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                         SmxManager                                       │
│                                                                         │
│   - Owns SmxDevice[2] (one per pad slot)                                │
│   - Spawns USB polling thread + main I/O thread                         │
│   - Handles device discovery and ordering                               │
│   - Routes API calls to the correct SmxDevice                           │
│   - Enumerator lock separate from state lock (non-blocking enumeration) │
└───────────┬─────────────────────────────────┬───────────────────────────┘
            │                                 │
            ▼                                 ▼
┌───────────────────────────┐   ┌───────────────────────────────────────┐
│   USB Polling Thread       │   │   Main I/O Thread                     │
│   (~1ms cycle)             │   │   (~50ms cycle)                       │
│                            │   │                                       │
│   PollHandle::poll()       │   │   attempt_connections()               │
│   ├─ Read HID packets      │   │   SmxDevice::update() per device      │
│   ├─ Report 3 → atomic     │   │   ├─ CommandHandle::update()          │
│   │  input_state update    │   │   │  ├─ check_reads() [Report 6]     │
│   │  → fires SmxEvent::    │   │   │  └─ check_writes() [send cmds]   │
│   │    InputState callback │   │   ├─ handle_packets() [config/data]   │
│   └─ Report 6 → mutex      │   │   ├─ send_config_if_needed()         │
│      buffer for main thread │   │   └─ update_sensor_test_mode()       │
│                            │   │   correct_device_order()              │
│   Wakes main thread on     │   │   send_pending_lights()               │
│   Report 6 data or errors  │   │   Fire Connected/ConfigUpdated events │
└───────────────────────────┘   └───────────────────────────────────────┘
            │                                 │
            └────────────────┬────────────────┘
                             ▼
┌─────────────────────────────────────────────────────────────────────────┐
│              Split Connection: PollHandle + CommandHandle                │
│                                                                         │
│   Shared via Arc:                                                       │
│   - AtomicU16 input_state (USB thread writes, anyone reads)             │
│   - AtomicBool always_fire_input                                        │
│   - AtomicBool had_read_error                                           │
│   - Mutex<Vec<u8>> report6_buffer (USB thread → main thread)            │
│                                                                         │
│   PollHandle (USB thread only):                                         │
│   - poll() → reads HID, updates atomics, buffers Report 6              │
│                                                                         │
│   CommandHandle (main thread only):                                     │
│   - send_command() → queues commands                                    │
│   - update() → sends pending, processes responses                       │
│   - read_packet() → returns reassembled packets                         │
└────────────────────────────────┬────────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                    HidDevice trait (Box<dyn HidDevice>)                  │
│                                                                         │
│   Implementations:                                                      │
│   - HidapiDevice (real hardware via hidapi crate)                       │
│   - RecordingDevice (wraps any HidDevice, writes .smxhid captures)      │
│   - FakeDevice (test infrastructure, auto-responds to commands)          │
│   - ReplayDevice (replays captured .smxhid traffic for regression)       │
└─────────────────────────────────────────────────────────────────────────┘
```

## Threading Model

The SDK uses two background threads, matching the C++ SDK's design:

### USB Polling Thread (~1ms cycle)

- Calls `PollHandle::poll()` for each connected device
- Parses Report 3 (input state) inline — updates `AtomicU16`, fires `SmxEvent::InputState` callback
- Buffers Report 6 (command responses) in a `Mutex<Vec<u8>>` for the main thread
- Wakes the main thread via `Condvar` when Report 6 data arrives or a read error occurs

### Main I/O Thread (~50ms cycle)

- Discovers new devices via `HidEnumerator::enumerate()` (lock released during syscall)
- Calls `SmxDevice::update()` for each device:
  - Processes Report 6 data (device info handshake, config, sensor test responses)
  - Sends queued commands (fragmented into 64-byte HID packets)
  - Handles command timeouts and retries
- Corrects device ordering (P1 in slot 0, P2 in slot 1)
- Sends rate-limited lights commands (30 FPS)
- Fires `SmxEvent::Connected` / `SmxEvent::Disconnected` events

### Lock Hierarchy

```
ManagerShared::enumerator (Mutex)  — held only during enumerate/open calls
ManagerShared::state (Mutex)       — held during device updates and API calls
SharedState::report6_buffer (Mutex) — held briefly for buffer swap
```

The enumerator has its own lock so that HID enumeration (potentially slow on some platforms) doesn't block the USB polling thread or API calls.

## Event Flow

```
Panel pressed on hardware
    → USB device sends Report 3 packet
    → USB polling thread reads it in PollHandle::poll()
    → AtomicU16 updated, input callback fires
    → Application receives SmxEvent::InputState { pad, state }
```

```
Application calls mgr.set_config(pad, config)
    → Queued in SmxDevice.wanted_config
    → Main thread: send_config_if_needed() (rate-limited to 1/sec)
    → CommandHandle::send_command() fragments into HID packets
    → Device responds with new config
    → Main thread: handle_packets() updates cached config
    → Application receives SmxEvent::ConfigUpdated { pad }
```

## Module Map

| Module | Responsibility |
|--------|---------------|
| `manager.rs` | Orchestration, threading, device discovery, lights scheduling |
| `device.rs` | Per-pad state machine, config conversion, sensor test parsing |
| `connection.rs` | HID traits, split-struct connection, command queue |
| `protocol.rs` | Constants, packet framing, reassembly |
| `config.rs` | Packed config structs matching firmware layout |
| `lights.rs` | GIF animation loading, playback, firmware upload |
| `recorder.rs` | HID traffic recording for debugging |
| `error.rs` | Error types |

## Key Differences from C++ Architecture

See [DESIGN_DIFFERENCES.md](DESIGN_DIFFERENCES.md) for the full list. The most significant:

- **Split connection** instead of single class with threading documentation
- **Event-based callbacks** with data attached (no calling back into the manager)
- **`Arc<AtomicUsize>` pad index** that updates dynamically after device reordering
- **Separate enumerator lock** so enumeration doesn't block USB polling
- **No global singleton** — `SmxManager` is an owned value
