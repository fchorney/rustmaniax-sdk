/// Device info retrieved on connection.
#[derive(Clone, Debug, Default)]
pub struct SmxDeviceInfo {
    pub is_player2: bool,
    pub serial: String,
    pub firmware_version: u16,
}
