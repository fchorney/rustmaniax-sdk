/// USB report flags for packet fragmentation.
pub const PACKET_FLAG_START_OF_COMMAND: u8 = 0x04;
pub const PACKET_FLAG_END_OF_COMMAND: u8 = 0x01;
pub const PACKET_FLAG_HOST_CMD_FINISHED: u8 = 0x02;
pub const PACKET_FLAG_DEVICE_INFO: u8 = 0x80;

/// HID report IDs.
pub const HID_REPORT_INPUT_STATE: u8 = 0x03;
pub const HID_REPORT_COMMAND: u8 = 0x05;
pub const HID_REPORT_DATA: u8 = 0x06;

/// HID packet sizing.
pub const HID_PACKET_SIZE: usize = 64;
pub const HID_MAX_PAYLOAD_SIZE: usize = 61;

/// SMX USB vendor/product IDs.
pub const SMX_USB_VENDOR_ID: u16 = 0x2341;
pub const SMX_USB_PRODUCT_ID: u16 = 0x8037;
