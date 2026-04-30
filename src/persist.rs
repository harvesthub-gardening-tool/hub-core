//! NVS-backed persistence for hub identity and credentials.
//!
//! Storage layout (NVS namespace `"hub"`):
//!   - `device_id`  : UUIDv4, lowercase hex with hyphens (36 bytes), generated once on first boot.
//!   - `hub_secret` : 32 random bytes hex-encoded (64 bytes), generated once on first boot.
//!   - `jwt`        : variable-length hub JWT obtained from `auth.v2.ClaimHubToken`.
//!
//! Entropy contract: `esp_fill_random` only emits CSPRNG-quality output once the
//! RF subsystem is enabled (Wi-Fi or BLE). All call sites here are reached AFTER
//! `wifi::connect()` returns, so the requirement is satisfied. Calling these
//! functions before Wi-Fi/BLE init would yield pseudo-random output and is a
//! security bug — do not move them earlier in the boot sequence.
//!
//! See: <https://github.com/espressif/esp-idf/blob/master/docs/en/api-reference/system/random.rst>

use anyhow::{Context, Result};
use core::fmt::Write as _;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use log::info;

const NAMESPACE: &str = "hub";
const KEY_DEVICE_ID: &str = "device_id";
const KEY_HUB_SECRET: &str = "hub_secret";
const KEY_JWT: &str = "jwt";

/// Returns `(device_id, hub_secret)`. Generates and persists both on first boot;
/// re-reads them on subsequent boots.
pub fn load_or_generate_creds(nvs: EspDefaultNvsPartition) -> Result<(String, String)> {
    // Try read first — namespace may not exist yet on a fresh device, which is fine.
    let read_handle = EspNvs::new(nvs.clone(), NAMESPACE, true)
        .context("open NVS namespace 'hub' for read/write")?;

    let existing_id = read_str(&read_handle, KEY_DEVICE_ID)?;
    let existing_secret = read_str(&read_handle, KEY_HUB_SECRET)?;

    if let (Some(id), Some(secret)) = (existing_id, existing_secret) {
        info!("hub creds loaded from NVS (device_id={id})");
        return Ok((id, secret));
    }

    // Generate fresh creds. Either or both keys missing → regenerate the pair atomically
    // so the QR payload printed to serial always matches what's stored.
    let device_id = generate_uuid_v4();
    let hub_secret = generate_hex_secret();

    read_handle
        .set_str(KEY_DEVICE_ID, &device_id)
        .context("write device_id to NVS")?;
    read_handle
        .set_str(KEY_HUB_SECRET, &hub_secret)
        .context("write hub_secret to NVS")?;

    info!("hub creds generated and persisted (device_id={device_id})");
    Ok((device_id, hub_secret))
}

pub fn read_hub_jwt(nvs: EspDefaultNvsPartition) -> Result<Option<String>> {
    let handle =
        EspNvs::new(nvs, NAMESPACE, false).context("open NVS namespace 'hub' read-only")?;
    read_str(&handle, KEY_JWT)
}

pub fn write_hub_jwt(nvs: EspDefaultNvsPartition, jwt: &str) -> Result<()> {
    let handle =
        EspNvs::new(nvs, NAMESPACE, true).context("open NVS namespace 'hub' for jwt write")?;
    handle.set_str(KEY_JWT, jwt).context("write jwt to NVS")?;
    Ok(())
}

/// Dev-only override: seed NVS with a JWT supplied at build time via `HUB_TOKEN=…`.
/// Skipped silently if the token already matches what's stored, so reboots stay quiet.
pub fn seed_jwt_from_env(nvs: EspDefaultNvsPartition, token: &str) -> Result<()> {
    if let Some(existing) = read_hub_jwt(nvs.clone())? {
        if existing == token {
            return Ok(());
        }
    }
    write_hub_jwt(nvs, token)?;
    info!("HUB_TOKEN seeded into NVS (dev override)");
    Ok(())
}

/// Read a UTF-8 string from NVS, returning `None` if the key is absent.
///
/// Pattern from esp-idf-svc 0.52.1: `str_len` returns required byte count
/// (including trailing NUL), then `get_str` fills the exact buffer.
fn read_str<P>(handle: &EspNvs<P>, key: &str) -> Result<Option<String>>
where
    P: esp_idf_svc::nvs::NvsPartitionId,
{
    let Some(len) = handle
        .str_len(key)
        .with_context(|| format!("nvs str_len('{key}')"))?
    else {
        return Ok(None);
    };

    let mut buf = vec![0u8; len];
    let value = handle
        .get_str(key, &mut buf)
        .with_context(|| format!("nvs get_str('{key}')"))?
        .map(ToOwned::to_owned);
    Ok(value)
}

/// Generate a UUIDv4 (RFC 4122 §4.4) using ESP32-S3 hardware RNG.
///
/// 16 random bytes, then patch byte 6 (version=4) and byte 8 (variant=10xx)
/// per spec, then hex-encode with hyphens.
fn generate_uuid_v4() -> String {
    let mut b = [0u8; 16];
    fill_random(&mut b);

    // Version: top nibble of byte 6 = 0b0100
    b[6] = (b[6] & 0x0F) | 0x40;
    // Variant: top two bits of byte 8 = 0b10
    b[8] = (b[8] & 0x3F) | 0x80;

    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        write!(&mut out, "{byte:02x}").unwrap();
    }
    out
}

/// 32 random bytes → 64-char lowercase hex.
fn generate_hex_secret() -> String {
    let mut b = [0u8; 32];
    fill_random(&mut b);

    let mut out = String::with_capacity(64);
    for byte in b {
        write!(&mut out, "{byte:02x}").unwrap();
    }
    out
}

/// Fill `buf` with cryptographically secure random bytes. CALLER must ensure
/// Wi-Fi or BLE has been initialized; otherwise output is pseudo-random only.
fn fill_random(buf: &mut [u8]) {
    // SAFETY: `esp_fill_random` only writes `buf.len()` bytes into `buf`. The pointer
    // is valid for writes for the duration of the call, and the length is exact.
    unsafe {
        esp_idf_svc::sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len());
    }
}
