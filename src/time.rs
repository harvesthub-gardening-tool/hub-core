use anyhow::Result;
use esp_idf_svc::sntp::{EspSntp, SyncStatus};
use log::info;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{thread, time::Duration};

pub fn get_unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs() as i64
}

pub fn get_sync_sntp() -> Result<EspSntp<'static>> {
    let sntp = EspSntp::new_default()?;
    info!("SNTP started, waiting for time sync...");

    for attempt in 1..=30 {
        match sntp.get_sync_status() {
            SyncStatus::Completed => {
                info!("SNTP sync completed");
                break;
            }
            SyncStatus::InProgress => {
                info!("SNTP sync in progress... ({}/30)", attempt);
            }
            SyncStatus::Reset => {
                info!("SNTP not synced yet... ({}/30)", attempt);
            }
        }

        thread::sleep(Duration::from_secs(1));
    }

    Ok(sntp)
}
