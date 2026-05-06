pub const COMPANY_ID: u16 = 0x1234;
pub const MAGIC_MARKER: &[u8] = b"HH-PROBE";
pub const SETUP_MARKER: &[u8] = b"HH-SETUP";
pub const SETUP_PROBE_NAME: &str = "HH-PROBE-SETUP";
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
pub const PROBE_SETUP_CONFIRM_CHAR_UUID: &str = "12340005-0000-1000-8000-00805f9b34fb";
pub const MOTOR_COMMAND_CHAR_UUID: &str = "12340006-0000-1000-8000-00805f9b34fb";
pub const PROBE_SETUP_CONFIRM_MAGIC: &[u8] = b"HHSETUP1";

// Motor command write payload layout (little-endian fixed-width fields):
// [0..4]   magic      = MOTOR_COMMAND_PAYLOAD_MAGIC
// [4]      version    = MOTOR_COMMAND_PAYLOAD_VERSION
// [5]      action     = MOTOR_COMMAND_ACTION_*
// [6..22]  command_id = 16-byte command identifier (compact UUID bytes)
// [22..26] duration   = requested motor run duration in ms (u32 LE)
// [26..30] expires_at = remaining TTL in ms at hub write-time (u32 LE)
// Safety defaults for later handlers:
// - Probe accepts only one active motor command at a time.
// - Duplicate command_id values are ignored for MOTOR_COMMAND_DUPLICATE_RETENTION_MS.
// - Duration is always clamped to MOTOR_COMMAND_MAX_DURATION_MS.
// - Expired commands must be ignored.
pub const MOTOR_COMMAND_PAYLOAD_MAGIC: &[u8; 4] = b"HHMC";
pub const MOTOR_COMMAND_PAYLOAD_VERSION: u8 = 1;
pub const MOTOR_COMMAND_ACTION_STOP: u8 = 0;
pub const MOTOR_COMMAND_ACTION_RUN_FOR_DURATION: u8 = 1;
pub const MOTOR_COMMAND_PAYLOAD_MAGIC_OFFSET: usize = 0;
pub const MOTOR_COMMAND_PAYLOAD_VERSION_OFFSET: usize = 4;
pub const MOTOR_COMMAND_PAYLOAD_ACTION_OFFSET: usize = 5;
pub const MOTOR_COMMAND_PAYLOAD_COMMAND_ID_OFFSET: usize = 6;
pub const MOTOR_COMMAND_PAYLOAD_COMMAND_ID_LEN: usize = 16;
pub const MOTOR_COMMAND_PAYLOAD_DURATION_MS_OFFSET: usize = 22;
pub const MOTOR_COMMAND_PAYLOAD_EXPIRY_MS_OFFSET: usize = 26;
pub const MOTOR_COMMAND_PAYLOAD_LEN: usize = 30;
pub const MOTOR_COMMAND_MAX_DURATION_MS: u32 = 5_000;
pub const MOTOR_COMMAND_DEFAULT_EXPIRY_MS: u32 = 30_000;
pub const MOTOR_COMMAND_DUPLICATE_RETENTION_MS: u32 = MOTOR_COMMAND_DEFAULT_EXPIRY_MS;

// Backend motor command polling defaults.
pub const MOTOR_COMMAND_POLL_INTERVAL_MS: u64 = 2_000;
pub const MOTOR_COMMAND_POLL_JITTER_MS: u64 = 500;
pub const MOTOR_COMMAND_POLL_BACKOFF_INITIAL_MS: u64 = 500;
pub const MOTOR_COMMAND_POLL_BACKOFF_MAX_MS: u64 = 15_000;
pub const MOTOR_COMMAND_POLL_LEASE_DURATION_MS: i32 = 15_000;
pub const MOTOR_COMMAND_POLL_BATCH_SIZE: i32 = 1;
