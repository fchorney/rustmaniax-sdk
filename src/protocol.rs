// USB device identification.
pub const SMX_USB_VENDOR_ID: u16 = 0x2341;
pub const SMX_USB_PRODUCT_ID: u16 = 0x8037;

// USB report flags for packet fragmentation.
pub const PACKET_FLAG_START_OF_COMMAND: u8 = 0x04;
pub const PACKET_FLAG_END_OF_COMMAND: u8 = 0x01;
pub const PACKET_FLAG_HOST_CMD_FINISHED: u8 = 0x02;
pub const PACKET_FLAG_DEVICE_INFO: u8 = 0x80;

// HID report IDs.
pub const HID_REPORT_INPUT_STATE: u8 = 0x03;
pub const HID_REPORT_COMMAND: u8 = 0x05;
pub const HID_REPORT_DATA: u8 = 0x06;

// HID packet sizing.
pub const HID_PACKET_SIZE: usize = 64;
pub const HID_MAX_PAYLOAD_SIZE: usize = 61; // 64 - 3 byte header

// Panel and LED geometry.
pub const NUM_PANELS: usize = 9;
pub const LEDS_PER_PANEL_16: usize = 16;
pub const LEDS_PER_PANEL_25: usize = 25;
pub const PLATFORM_STRIP_LEDS: usize = 44;
pub const BYTES_PER_PAD_16: usize = NUM_PANELS * LEDS_PER_PANEL_16 * 3; // 432
pub const BYTES_PER_PAD_25: usize = NUM_PANELS * LEDS_PER_PANEL_25 * 3; // 675

// Color scaling applied to LED values before sending.
pub const LED_COLOR_SCALE: f32 = 0.6666;

// Legacy lights-off command payload size (9 panels × 4 LEDs × 3 RGB).
pub const LEGACY_LIGHTS_PAYLOAD_SIZE: usize = 108;

// Timing constants (seconds).
pub const COMMAND_TIMEOUT_SECONDS: f64 = 2.0;
pub const CONFIG_WRITE_RATE_LIMIT_SECONDS: f64 = 1.0;
pub const SENSOR_TEST_TIMEOUT_SECONDS: f64 = 2.0;
pub const PANEL_TEST_REFRESH_SECONDS: f64 = 1.0;
pub const ENUMERATION_INTERVAL_SECONDS: f64 = 1.0;
pub const LIGHTS_FRAME_INTERVAL: f64 = 1.0 / 30.0;
pub const LIGHTS_LEGACY_COMMAND_DELAY: f64 = 1.0 / 60.0;
pub const ANIMATION_PAUSE_DURATION: f64 = 0.1;

// Serial number size in bytes (raw binary, before hex encoding).
pub const SERIAL_SIZE: usize = 16;

/// Fragments a command payload into HID packets for transmission.
///
/// Each packet is 64 bytes: [report_id=0x05][flags][payload_len][payload...][zero padding].
/// The first packet has START_OF_COMMAND set, the last has END_OF_COMMAND set.
/// A single-packet command has both flags set.
pub fn build_command_packets(cmd: &[u8]) -> Vec<[u8; HID_PACKET_SIZE]> {
    if cmd.is_empty() {
        // Empty command still needs one packet with both flags.
        let mut packet = [0u8; HID_PACKET_SIZE];
        packet[0] = HID_REPORT_COMMAND;
        packet[1] = PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND;
        packet[2] = 0;
        return vec![packet];
    }

    let num_packets = cmd.len().div_ceil(HID_MAX_PAYLOAD_SIZE);
    let mut packets = Vec::with_capacity(num_packets);

    for (i, chunk) in cmd.chunks(HID_MAX_PAYLOAD_SIZE).enumerate() {
        let mut packet = [0u8; HID_PACKET_SIZE];
        packet[0] = HID_REPORT_COMMAND;

        let mut flags = 0u8;
        if i == 0 {
            flags |= PACKET_FLAG_START_OF_COMMAND;
        }
        if i == num_packets - 1 {
            flags |= PACKET_FLAG_END_OF_COMMAND;
        }
        packet[1] = flags;
        packet[2] = chunk.len() as u8;
        packet[3..3 + chunk.len()].copy_from_slice(chunk);

        packets.push(packet);
    }

    packets
}

/// Builds a device info request packet.
pub fn build_device_info_request() -> [u8; HID_PACKET_SIZE] {
    let mut packet = [0u8; HID_PACKET_SIZE];
    packet[0] = HID_REPORT_COMMAND;
    packet[1] = PACKET_FLAG_DEVICE_INFO;
    packet[2] = 0;
    packet
}

/// Result of parsing a single incoming Report 6 packet.
#[derive(Debug)]
pub enum ParsedPacket {
    /// Device info response (raw payload bytes).
    DeviceInfo(Vec<u8>),
    /// A fragment of a command response. Accumulate until `end_of_command` is true.
    Fragment {
        payload: Vec<u8>,
        start_of_command: bool,
        end_of_command: bool,
        host_cmd_finished: bool,
    },
}

/// Parses a raw Report 6 packet (including the report ID byte).
///
/// Expected format: [report_id=0x06][flags][payload_len][payload...]
/// Returns None if the packet is too short or has wrong report ID.
pub fn parse_report6(raw: &[u8]) -> Option<ParsedPacket> {
    if raw.len() < 3 || raw[0] != HID_REPORT_DATA {
        return None;
    }

    let flags = raw[1];
    let payload_len = raw[2] as usize;

    if raw.len() < 3 + payload_len {
        return None;
    }

    let payload = raw[3..3 + payload_len].to_vec();

    if flags & PACKET_FLAG_DEVICE_INFO != 0 {
        Some(ParsedPacket::DeviceInfo(payload))
    } else {
        Some(ParsedPacket::Fragment {
            payload,
            start_of_command: flags & PACKET_FLAG_START_OF_COMMAND != 0,
            end_of_command: flags & PACKET_FLAG_END_OF_COMMAND != 0,
            host_cmd_finished: flags & PACKET_FLAG_HOST_CMD_FINISHED != 0,
        })
    }
}

/// Reassembles fragmented Report 6 packets into complete command responses.
///
/// Feed it parsed fragments via `push()`. Complete packets are returned by `take_completed()`.
#[derive(Debug, Default)]
pub struct PacketReassembler {
    current: Vec<u8>,
    completed: Vec<Vec<u8>>,
}

impl PacketReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a parsed fragment into the reassembler.
    /// Returns true if `host_cmd_finished` was set on this fragment.
    pub fn push(&mut self, fragment: &ParsedPacket) -> bool {
        match fragment {
            ParsedPacket::Fragment {
                payload,
                start_of_command,
                end_of_command,
                host_cmd_finished,
            } => {
                if *start_of_command && !self.current.is_empty() {
                    // New command started before previous one ended; discard partial.
                    self.current.clear();
                }

                self.current.extend_from_slice(payload);

                if *end_of_command {
                    let complete = std::mem::take(&mut self.current);
                    if !complete.is_empty() {
                        self.completed.push(complete);
                    }
                }

                *host_cmd_finished
            }
            ParsedPacket::DeviceInfo(_) => false,
        }
    }

    /// Takes all completed packets out of the reassembler.
    pub fn take_completed(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_packet_command() {
        let cmd = b"hello";
        let packets = build_command_packets(cmd);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][0], HID_REPORT_COMMAND);
        assert_eq!(
            packets[0][1],
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND
        );
        assert_eq!(packets[0][2], 5);
        assert_eq!(&packets[0][3..8], b"hello");
    }

    #[test]
    fn multi_packet_command() {
        // 62 bytes requires 2 packets (61 + 1).
        let cmd = vec![0xAB; 62];
        let packets = build_command_packets(&cmd);
        assert_eq!(packets.len(), 2);

        // First packet: START flag, 61 bytes payload.
        assert_eq!(packets[0][1], PACKET_FLAG_START_OF_COMMAND);
        assert_eq!(packets[0][2], 61);

        // Second packet: END flag, 1 byte payload.
        assert_eq!(packets[1][1], PACKET_FLAG_END_OF_COMMAND);
        assert_eq!(packets[1][2], 1);
    }

    #[test]
    fn empty_command() {
        let packets = build_command_packets(&[]);
        assert_eq!(packets.len(), 1);
        assert_eq!(
            packets[0][1],
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND
        );
        assert_eq!(packets[0][2], 0);
    }

    #[test]
    fn device_info_request_format() {
        let packet = build_device_info_request();
        assert_eq!(packet[0], HID_REPORT_COMMAND);
        assert_eq!(packet[1], PACKET_FLAG_DEVICE_INFO);
        assert_eq!(packet[2], 0);
    }

    #[test]
    fn parse_report6_device_info() {
        let mut raw = vec![HID_REPORT_DATA, PACKET_FLAG_DEVICE_INFO, 3, 0xAA, 0xBB, 0xCC];
        let parsed = parse_report6(&raw).unwrap();
        match parsed {
            ParsedPacket::DeviceInfo(payload) => assert_eq!(payload, vec![0xAA, 0xBB, 0xCC]),
            _ => panic!("expected DeviceInfo"),
        }

        // Too short.
        raw.truncate(2);
        assert!(parse_report6(&raw).is_none());
    }

    #[test]
    fn reassemble_single_fragment() {
        let mut reassembler = PacketReassembler::new();
        let fragment = ParsedPacket::Fragment {
            payload: vec![1, 2, 3],
            start_of_command: true,
            end_of_command: true,
            host_cmd_finished: false,
        };
        reassembler.push(&fragment);
        let completed = reassembler.take_completed();
        assert_eq!(completed, vec![vec![1, 2, 3]]);
    }

    #[test]
    fn reassemble_multi_fragment() {
        let mut reassembler = PacketReassembler::new();

        reassembler.push(&ParsedPacket::Fragment {
            payload: vec![1, 2],
            start_of_command: true,
            end_of_command: false,
            host_cmd_finished: false,
        });
        assert!(reassembler.take_completed().is_empty());

        reassembler.push(&ParsedPacket::Fragment {
            payload: vec![3, 4],
            start_of_command: false,
            end_of_command: true,
            host_cmd_finished: false,
        });
        let completed = reassembler.take_completed();
        assert_eq!(completed, vec![vec![1, 2, 3, 4]]);
    }

    #[test]
    fn reassemble_discards_partial_on_new_start() {
        let mut reassembler = PacketReassembler::new();

        // Start a command but don't finish it.
        reassembler.push(&ParsedPacket::Fragment {
            payload: vec![0xFF],
            start_of_command: true,
            end_of_command: false,
            host_cmd_finished: false,
        });

        // New start discards the partial.
        reassembler.push(&ParsedPacket::Fragment {
            payload: vec![0xAA],
            start_of_command: true,
            end_of_command: true,
            host_cmd_finished: false,
        });

        let completed = reassembler.take_completed();
        assert_eq!(completed, vec![vec![0xAA]]);
    }

    #[test]
    fn exactly_61_bytes_fits_single_packet() {
        let cmd = vec![0x42; HID_MAX_PAYLOAD_SIZE];
        let packets = build_command_packets(&cmd);
        assert_eq!(packets.len(), 1);
        assert_eq!(
            packets[0][1],
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_END_OF_COMMAND
        );
        assert_eq!(packets[0][2], 61);
    }

    #[test]
    fn large_command_fragments_correctly() {
        // 183 bytes = 3 packets (61 + 61 + 61)
        let cmd = vec![0xCC; 183];
        let packets = build_command_packets(&cmd);
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0][1], PACKET_FLAG_START_OF_COMMAND);
        assert_eq!(packets[1][1], 0); // middle packet: no flags
        assert_eq!(packets[2][1], PACKET_FLAG_END_OF_COMMAND);
        // All payloads are 61 bytes.
        assert_eq!(packets[0][2], 61);
        assert_eq!(packets[1][2], 61);
        assert_eq!(packets[2][2], 61);
    }

    #[test]
    fn parse_report6_wrong_report_id() {
        let raw = vec![0x03, 0x00, 0x02, 0xAA, 0xBB];
        assert!(parse_report6(&raw).is_none());
    }

    #[test]
    fn parse_report6_truncated_payload() {
        // Claims 5 bytes payload but only has 2.
        let raw = vec![HID_REPORT_DATA, 0x05, 5, 0xAA, 0xBB];
        assert!(parse_report6(&raw).is_none());
    }

    #[test]
    fn parse_report6_fragment_flags() {
        let raw = vec![
            HID_REPORT_DATA,
            PACKET_FLAG_START_OF_COMMAND | PACKET_FLAG_HOST_CMD_FINISHED,
            3,
            0x01, 0x02, 0x03,
        ];
        let parsed = parse_report6(&raw).unwrap();
        match parsed {
            ParsedPacket::Fragment {
                payload,
                start_of_command,
                end_of_command,
                host_cmd_finished,
            } => {
                assert_eq!(payload, vec![1, 2, 3]);
                assert!(start_of_command);
                assert!(!end_of_command);
                assert!(host_cmd_finished);
            }
            _ => panic!("expected Fragment"),
        }
    }

    #[test]
    fn reassembler_host_cmd_finished_returns_true() {
        let mut reassembler = PacketReassembler::new();
        let fragment = ParsedPacket::Fragment {
            payload: vec![1],
            start_of_command: true,
            end_of_command: true,
            host_cmd_finished: true,
        };
        let result = reassembler.push(&fragment);
        assert!(result); // host_cmd_finished
    }

    #[test]
    fn reassembler_ignores_device_info() {
        let mut reassembler = PacketReassembler::new();
        let result = reassembler.push(&ParsedPacket::DeviceInfo(vec![1, 2, 3]));
        assert!(!result);
        assert!(reassembler.take_completed().is_empty());
    }

    #[test]
    fn command_packet_zero_padding() {
        // Verify unused bytes in packet are zero.
        let cmd = b"hi";
        let packets = build_command_packets(cmd);
        assert_eq!(packets[0][3], b'h');
        assert_eq!(packets[0][4], b'i');
        // Rest should be zero.
        for &byte in &packets[0][5..] {
            assert_eq!(byte, 0);
        }
    }
}
