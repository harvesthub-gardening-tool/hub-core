use anyhow::{anyhow, Result};
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use log::info;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{thread, time::Duration};

/// Returns the current time as Unix milliseconds.
///
/// The backend (`InsertSensorData`) decodes the timestamp with `time.UnixMilli`,
/// so all timestamps emitted by the firmware MUST be in milliseconds.
pub fn get_unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as i64
}

pub fn get_sync_sntp() -> Result<EspSntp<'static>> {
    let sntp = EspSntp::new_default()?;
    info!("SNTP started, waiting for time sync...");

    for attempt in 1..=30 {
        if matches!(sntp.get_sync_status(), SyncStatus::Completed) {
            info!("SNTP sync completed after {attempt} attempt(s)");
            return Ok(sntp);
        }
        info!("SNTP not synced yet... ({attempt}/30)");
        thread::sleep(Duration::from_secs(1));
    }

    Err(anyhow!(
        "SNTP failed to sync after 30s; aborting (JWT validation requires correct wall clock)"
    ))
}
