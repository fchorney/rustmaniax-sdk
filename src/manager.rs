/// Callback reasons for device state changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateReason {
    Updated,
    InputState,
    Connected,
    Disconnected,
    ConfigUpdated,
    SensorTestData,
}
