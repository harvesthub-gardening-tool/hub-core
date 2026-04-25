// mod grpc;
mod ble;
mod config;
mod time;
mod wifi;

use anyhow::Result;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::log::EspLogger;
use log::{error, info};
use std::thread;
use std::time::Duration;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

const SCAN_WAIT_SECONDS: u64 = 30;

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let mut wifi = wifi::init()?;
    wifi::connect(WIFI_SSID, WIFI_PASSWORD, &mut wifi)?;

    let _sntp = time::get_sync_sntp()?;

    let ble_device = ble::init_device();
    info!("BLE device ready");

    loop {
        let candidates = match block_on(ble::scan_probe_candidates(&ble_device)) {
            Ok(v) => v,
            Err(e) => {
                error!("scan_probe_candidates failed: {e:#}");
                thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
                continue;
            }
        };

        info!("Known probe candidates this cycle: {}", candidates.len());

        if candidates.is_empty() {
            info!(
                "No probe found this cycle, waiting {}s...",
                SCAN_WAIT_SECONDS
            );
            thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
            continue;
        }

        for candidate in &candidates {
            match block_on(ble::read_probe_from_device(&ble_device, &candidate.device)) {
                Ok(Some(reading)) => {
                    info!(
                        "probe reading: addr={:?} name='{}' version={}.{} temp={:.2}°C hum={:.2}% ts={}",
                        candidate.device.addr(),
                        candidate.meta.name,
                        candidate.meta.version_major,
                        candidate.meta.version_minor,
                        reading.temperature_c,
                        reading.humidity_pct,
                        reading.timestamp
                    );
                }
                Ok(None) => {
                    info!(
                        "skip addr={:?} name='{}' version={}.{}: probe service/chars not readable",
                        candidate.device.addr(),
                        candidate.meta.name,
                        candidate.meta.version_major,
                        candidate.meta.version_minor
                    );
                }
                Err(e) => {
                    error!(
                        "read failed for addr={:?} name='{}' version={}.{}: {e:#}",
                        candidate.device.addr(),
                        candidate.meta.name,
                        candidate.meta.version_major,
                        candidate.meta.version_minor
                    );
                }
            }

            thread::sleep(Duration::from_millis(500));
        }

        info!("Polling cycle done. Waiting {}s...", SCAN_WAIT_SECONDS);
        thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
    }
}
