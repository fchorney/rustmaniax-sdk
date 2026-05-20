use bytemuck::{Pod, Zeroable};

/// Packed sensor settings for a single panel.
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct PackedSensorSettings {
    pub load_cell_low_threshold: u8,
    pub load_cell_high_threshold: u8,
    pub fsr_low_threshold: [u8; 4],
    pub fsr_high_threshold: [u8; 4],
    pub combined_low_threshold: u16,
    pub combined_high_threshold: u16,
    pub reserved: u16,
}

/// Configuration state for an SMX device (250 bytes, matches firmware layout).
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C, packed)]
pub struct SmxConfig {
    pub master_version: u8,
    pub config_version: u8,
    pub flags: u8,
    pub debounce_nodelay_ms: u16,
    pub debounce_delay_ms: u16,
    pub panel_debounce_us: u16,
    pub auto_calibration_max_deviation: u8,
    pub bad_sensor_minimum_delay_seconds: u8,
    pub auto_calibration_averages_per_update: u16,
    pub auto_calibration_samples_per_average: u16,
    pub auto_calibration_max_tare: u16,
    pub enabled_sensors: [u8; 5],
    pub auto_lights_timeout: u8,
    pub step_color: [u8; 27],
    pub platform_strip_color: [u8; 3],
    pub auto_light_panel_mask: u16,
    pub panel_rotation: u8,
    pub panel_settings: [PackedSensorSettings; 9],
    pub pre_details_delay_ms: u8,
    pub padding: [u8; 32],
    pub padding2: [u8; 17],
}

bitflags::bitflags! {
    /// Flags from SMXConfig::flags.
    #[derive(Clone, Copy, Debug)]
    pub struct ConfigFlags: u8 {
        const AUTO_LIGHTING_USE_PRESSED_ANIMATIONS = 1 << 0;
        const FSR = 1 << 1;
    }
}

// Compile-time assertion: SmxConfig must be exactly 250 bytes (wire format).
const _: () = assert!(std::mem::size_of::<SmxConfig>() == 250);
