use crate::config::SmxConfig;
use crate::connection::SmxDeviceInfo;

/// State of a single SMX pad.
#[derive(Debug, Default)]
pub struct SmxDevice {
    pub info: SmxDeviceInfo,
    pub config: Option<SmxConfig>,
    pub input_state: u16,
}
