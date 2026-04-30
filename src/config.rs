pub const COMPANY_ID: u16 = 0x1234;
pub const MAGIC_MARKER: &[u8] = b"HH-PROBE";
pub const LEGACY_TEST_MARKER: &[u8] = b"TEST";

// Environmental Sensing
pub const ENVIRONMENTAL_SENSING_SERVICE_UUID: &str = "0000181a-0000-1000-8000-00805f9b34fb";
pub const AIR_TEMP_CHAR_UUID: &str = "00002a6e-0000-1000-8000-00805f9b34fb";
pub const AIR_PRESSURE_CHAR_UUID: &str = "00002a6d-0000-1000-8000-00805f9b34fb";
pub const AIR_HUM_CHAR_UUID: &str = "00002a6f-0000-1000-8000-00805f9b34fb";

// Harvest Hub vendor characteristics under the Environmental Sensing service.
// Keep these UUIDs in sync with the probe firmware.
pub const PROBE_UUID_CHAR_UUID: &str = "12340002-0000-1000-8000-00805f9b34fb";
pub const SOIL_TEMP_CHAR_UUID: &str = "12340003-0000-1000-8000-00805f9b34fb";
pub const SOIL_HUM_CHAR_UUID: &str = "12340004-0000-1000-8000-00805f9b34fb";
