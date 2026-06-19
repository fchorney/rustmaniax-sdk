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
│   - Spawns one poll thread per connected pad + main I/O thread          │
│   - Handles device discovery and ordering                               │
│   - Routes API calls to the correct SmxDevice                           │
│   - Poll-thread + enumerator locks kept off the state lock (see below)  │
└───────────┬─────────────────────────────────┬───────────────────────────┘
            │                                 │
            ▼                                 ▼
┌───────────────────────────┐   ┌───────────────────────────────────────┐
│   Per-Pad Poll Thread      │   │   Main I/O Thread                     │
│   (one per pad,            │   │   (~50ms cycle)                       │
│    interrupt-driven)       │   │                                       │
│                            │   │   attempt_connections()               │
│   PollHandle::poll(t)      │   │   SmxDevice::update() per device      │
│   ├─ Blocking first read   │   │   ├─ CommandHandle::update()          │
│   │  (wakes on a report)   │   │   │  ├─ check_reads() [Report 6]     │
│   ├─ Drain rest non-block  │   │   │  └─ check_writes() [send cmds]   │
│   ├─ Report 3 → atomic     │   │   ├─ handle_packets() [config/data]   │
│   │  input_state update    │   │   ├─ send_config_if_needed()         │
│   │  → fires SmxEvent::    │   │   └─ update_sensor_test_mode()       │
│   │    InputState callback │   │   correct_device_order()              │
│   └─ Report 6 → mutex      │   │   send_pending_lights()               │
│      buffer for main thread │   │   Fire Connected/ConfigUpdated events │
│   Wakes main thread on     │   │                                       │
│   Report 6 data or errors  │   │                                       │
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
│   PollHandle (poll thread only):                                        │
│   - poll(t) → blocks for a report, drains, updates atomics, buffers R6  │
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

The SDK runs one main I/O thread plus one poll thread per connected pad.

### Per-Pad Poll Thread (interrupt-driven, one per pad)

Each connected pad gets its own poll thread, spawned on connect and reaped on read error or shutdown (`pad_poll_loop` in `manager.rs`). The thread owns its connection's `PollHandle` outright, so it touches no shared poll lock during a read — see [Lock Hierarchy](#lock-hierarchy).

- Calls `PollHandle::poll(timeout_ms)`, whose first read blocks in the kernel (`hid_read_timeout`) until the device delivers a packet, so the thread wakes the instant input arrives instead of polling on a sleep. Once data is ready, the rest of the OS buffer is drained with non-blocking reads.
- Parses Report 3 (input state) inline — updates `AtomicU16`, fires `SmxEvent::InputState` callback
- Buffers Report 6 (command responses) in a `Mutex<Vec<u8>>` for the main thread
- Wakes the main thread via `Condvar` when Report 6 data arrives or a read error occurs

Each pad reads independently: one pad's blocking read never delays the other's, and an idle/silent pad just re-blocks (the `POLL_READ_TIMEOUT_MS` timeout only bounds how quickly a parked thread notices a stop/shutdown request, not input latency). This replaces the earlier single USB-polling thread that read both pads on a fixed ~1ms sleep cycle.

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
ManagerShared::state (Mutex)        — manager/connection state, device updates, API calls
ManagerShared::poll_threads (Mutex) — the per-pad poll thread handles + stop flags
ManagerShared::enumerator (Mutex)   — held only during enumerate/open calls
SharedState::report6_buffer (Mutex) — held briefly for the buffer handoff
```

Two design choices keep input reads off the write path, so a blocking USB write never stalls them:

- **Separate read/write HID handles.** Each device is opened twice. `PollHandle` owns a read handle (used only by `poll()`); `CommandHandle` owns a write handle (used only by `update()` / `send_command()`). Independent OS handles let a read and a write run concurrently instead of serializing on one `Arc<Mutex<HidDevice>>`. On macOS the `macos-shared-device` hidapi feature is enabled so the second open is allowed; Linux and Windows already permit shared opens.
- **Each poll thread owns its read handle.** A poll thread holds no shared lock during its (blocking) read — it owns the `PollHandle` for its pad's lifetime. `poll_threads` holds only the thread/stop bookkeeping; the main thread takes it (lock order `state → poll_threads`) only briefly to spawn on connect, reap on read error, swap on reorder, or join on shutdown. A reorder just swaps the two `poll_threads` entries to stay index-aligned with `state.devices`; the threads keep reading their own devices, and events still report the right pad because the input callback reads the shared pad-index atomic. The poll threads never take `state`, so the ordering is deadlock-free.

The `SmxEvent::InputState` callback fires from `poll()` on the pad's poll thread; per the event-based design (see [DESIGN_DIFFERENCES.md](DESIGN_DIFFERENCES.md)) the callback must not call back into the manager.

The enumerator has its own lock so that HID enumeration (potentially slow on some platforms) doesn't block the poll threads or API calls.

## Event Flow

```
Panel pressed on hardware
    → USB device sends Report 3 packet
    → the pad's poll thread, blocked in PollHandle::poll(), wakes on the packet
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
- **Separate enumerator lock** so enumeration doesn't block the poll threads
- **No global singleton** — `SmxManager` is an owned value
