#!/usr/bin/env python3
"""Decode .smxhid capture files to human-readable output.

Usage: ./scripts/decode_smxhid.py <file.smxhid> [--data] [--commands] [--input]

Options:
  --data      Show raw packet data as an array of ints
  --commands  Only show commands/data packets (hide input state)
  --input     Only show input state packets (hide commands/data)

Format:
  Header: "SMXHID\x01" (7 bytes)
  Records: [type:1][timestamp_us:8][size:2][data:size]
    type: 'R' = read from device, 'W' = write to device
"""

import struct
import sys

# HID report IDs
REPORT_INPUT_STATE = 0x03
REPORT_COMMAND = 0x05
REPORT_DATA = 0x06

# Packet flags (for Report 5 outgoing and Report 6 incoming)
FLAG_START = 0x04
FLAG_END = 0x01
FLAG_HOST_CMD_FINISHED = 0x02
FLAG_DEVICE_INFO = 0x80


def decode_flags(flags, is_write):
    parts = []
    if flags & FLAG_DEVICE_INFO:
        parts.append("DEVICE_INFO")
    if flags & FLAG_START:
        parts.append("START")
    if flags & FLAG_END:
        parts.append("END")
    if flags & FLAG_HOST_CMD_FINISHED:
        parts.append("CMD_FINISHED")
    return "|".join(parts) if parts else "0"


def describe_packet(direction, data):
    """Return a human-readable description of a HID packet."""
    if not data:
        return "empty"

    report_id = data[0]

    if report_id == REPORT_INPUT_STATE:
        if len(data) >= 3:
            state = data[1] | (data[2] << 8)
            panels = []
            names = ["UL", "U", "UR", "L", "C", "R", "DL", "D", "DR"]
            for i in range(9):
                if state & (1 << i):
                    panels.append(names[i])
            panel_str = "+".join(panels) if panels else "none"
            return f"InputState: 0x{state:04x} [{panel_str}]"
        return "InputState: (truncated)"

    if report_id == REPORT_COMMAND and direction == "W":
        if len(data) < 3:
            return "Command: (truncated)"
        flags = data[1]
        size = data[2]
        payload = data[3:3 + size]
        flag_str = decode_flags(flags, True)

        if flags & FLAG_DEVICE_INFO:
            return f"Command: RequestDeviceInfo flags={flag_str}"

        # Continuation packet (no START flag) — just show raw info
        if not (flags & FLAG_START) and not (flags & FLAG_DEVICE_INFO):
            return f"Command: (continuation) flags={flag_str} size={size}"

        cmd_char = chr(payload[0]) if payload else "?"
        desc = f"Command: '{cmd_char}' flags={flag_str} size={size}"

        if cmd_char == "G":
            desc += " (GetConfig v5+)"
        elif cmd_char == "g":
            desc += " (GetConfig old)"
        elif cmd_char == "W" and len(payload) > 2:
            desc += f" (SetConfig, {payload[1]} bytes)"
        elif cmd_char == "w":
            desc += " (SetConfig old)"
        elif cmd_char == "f":
            desc += " (FactoryReset)"
        elif cmd_char == "C":
            desc += " (ForceRecalibration)"
        elif cmd_char == "S":
            desc += " (ReenableAutoLights)"
        elif cmd_char == "t":
            mode = chr(payload[2]) if len(payload) > 2 else "?"
            desc += f" (PanelTestMode={mode})"

        return desc

    if report_id == REPORT_DATA and direction == "R":
        if len(data) < 3:
            return "Data: (truncated)"
        flags = data[1]
        size = data[2]
        payload = data[3:3 + size]
        flag_str = decode_flags(flags, False)

        if flags & FLAG_DEVICE_INFO:
            if len(payload) >= 22:
                player = chr(payload[2]) if len(payload) > 2 else "?"
                fw = struct.unpack_from("<H", bytes(payload), 20)[0] if len(payload) >= 22 else 0
                serial_bytes = payload[4:20]
                serial = "".join(f"{b:02x}" for b in serial_bytes)
                return f"Data: DeviceInfo P{'2' if player == '1' else '1'} fw={fw} serial={serial}"
            return f"Data: DeviceInfo flags={flag_str}"

        # ACK packet: CMD_FINISHED with no payload
        if size == 0 and (flags & FLAG_HOST_CMD_FINISHED):
            return "Data: ACK"

        # Continuation packet (no START flag)
        if not (flags & FLAG_START) and not (flags & FLAG_DEVICE_INFO):
            if flags == 0:
                return f"Data: (continuation) size={size}"
            return f"Data: flags={flag_str} size={size}"

        desc = f"Data: flags={flag_str} size={size}"
        if payload:
            cmd_char = chr(payload[0])
            if cmd_char in ("G", "g") and len(payload) > 1:
                cfg_size = payload[1]
                desc = f"Data: '{cmd_char}' flags={flag_str} size={size} (ConfigResponse, {cfg_size} bytes)"
            else:
                desc = f"Data: '{cmd_char}' flags={flag_str} size={size}"

        return desc

    return f"Report 0x{report_id:02x}: {len(data)} bytes"


def decode_file(path, show_raw=False, filter_mode=None):
    with open(path, "rb") as f:
        magic = f.read(7)
        if magic != b"SMXHID\x01":
            print(f"Error: invalid magic: {magic!r}", file=sys.stderr)
            sys.exit(1)

        print(f"# {path}")
        print(f"# {'Time':>12}  {'Dir':>3}  {'Size':>4}  Description")
        print(f"# {'-'*12}  {'-'*3}  {'-'*4}  {'-'*40}")

        while True:
            header = f.read(11)  # type:1 + timestamp:8 + size:2
            if len(header) < 11:
                break

            rec_type = chr(header[0])
            timestamp_us = struct.unpack_from("<Q", header, 1)[0]
            size = struct.unpack_from("<H", header, 9)[0]
            data = list(f.read(size))

            if len(data) < size:
                print(f"# WARNING: truncated record", file=sys.stderr)
                break

            time_s = timestamp_us / 1_000_000
            direction = "R" if rec_type == "R" else "W"
            desc = describe_packet(direction, data)

            # Apply filter
            is_input = data and data[0] == REPORT_INPUT_STATE
            if filter_mode == "commands" and is_input:
                continue
            if filter_mode == "input" and not is_input:
                continue

            print(f"  {time_s:12.6f}  {direction:>3}  {size:>4}  {desc}")

            if show_raw:
                # Truncate trailing zeros
                last_nonzero = len(data) - 1
                while last_nonzero >= 0 and data[last_nonzero] == 0:
                    last_nonzero -= 1
                if last_nonzero < len(data) - 1:
                    print(f"  {'':12}       {'':>4}  {list(data[:last_nonzero+1]) + ['...']}")
                else:
                    print(f"  {'':12}       {'':>4}  {list(data)}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <file.smxhid> [--data] [--commands] [--input]", file=sys.stderr)
        sys.exit(1)

    show_raw = "--data" in sys.argv
    filter_mode = None
    if "--commands" in sys.argv:
        filter_mode = "commands"
    elif "--input" in sys.argv:
        filter_mode = "input"
    files = [a for a in sys.argv[1:] if not a.startswith("--")]

    for path in files:
        decode_file(path, show_raw, filter_mode)
        if path != files[-1]:
            print()
