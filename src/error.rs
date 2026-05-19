use std::fmt;

#[derive(Debug)]
pub enum SmxError {
    Hid(hidapi::HidError),
    NotConnected,
    InvalidPacket,
    Timeout,
}

impl fmt::Display for SmxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(e) => write!(f, "HID error: {e}"),
            Self::NotConnected => write!(f, "device not connected"),
            Self::InvalidPacket => write!(f, "invalid packet"),
            Self::Timeout => write!(f, "command timeout"),
        }
    }
}

impl std::error::Error for SmxError {}

impl From<hidapi::HidError> for SmxError {
    fn from(e: hidapi::HidError) -> Self {
        Self::Hid(e)
    }
}
