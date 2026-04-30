mod auth;
mod ble;
mod config;
mod grpc;
mod persist;
mod time;
mod wifi;

use anyhow::{Context, Result};
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{error, info, warn};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
const API_URL: &str = env!("API_URL");

const SCAN_WAIT_SECONDS: u64 = 30;
const UPLINK_THREAD_STACK: usize = 32 * 1024;
const UPLINK_QUEUE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
struct SensorReading {
    node_id: String,
    air_temperature_c: f64,
    air_pressure_pa: f64,
    air_humidity_pct: f64,
    soil_temperature_c: f64,
    soil_humidity_pct: f64,
    timestamp_unix: i64,
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let _eventfs = esp_idf_svc::io::vfs::MountedEventfs::mount(5)?;

    // NVS partition is one-shot; take it here and clone for every consumer.
    let nvs = EspDefaultNvsPartition::take().context("take default NVS partition")?;

    let mut wifi = wifi::init(nvs.clone())?;
    wifi::connect(WIFI_SSID, WIFI_PASSWORD, &mut wifi)?;

    let _sntp = time::get_sync_sntp()?;

    // Resolve hub identity + JWT. Order matters: `esp_fill_random` (used inside
    // `persist::load_or_generate_creds`) only emits CSPRNG-quality output once
    // Wi-Fi has been started, which is guaranteed by the calls above.
    let (device_id, jwt) = provision_hub(nvs.clone())?;

    let (tx, rx) = mpsc::sync_channel::<SensorReading>(UPLINK_QUEUE_DEPTH);
    spawn_uplink_worker(rx, jwt)?;

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

        #[cfg(feature = "fake-probe")]
        {
            let now_ms = unsafe { esp_idf_svc::sys::time(std::ptr::null_mut()) as i64 } * 1000;
            let fake = SensorReading {
                node_id: device_id.clone(),
                air_temperature_c: 21.5,
                air_pressure_pa: 101_325.0,
                air_humidity_pct: 48.0,
                soil_temperature_c: 18.7,
                soil_humidity_pct: 33.3,
                timestamp_unix: now_ms,
            };
            info!("[fake-probe] injecting synthetic reading ts_ms={now_ms}");
            if let Err(e) = tx.try_send(fake) {
                warn!("[fake-probe] uplink queue full, dropping reading: {e}");
            }
        }

        if candidates.is_empty() {
            info!("No probe found this cycle, waiting {SCAN_WAIT_SECONDS}s...");
            thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
            continue;
        }

        for candidate in &candidates {
            match block_on(ble::read_probe_from_device(&ble_device, &candidate.device)) {
                Ok(Some(reading)) => {
                    info!(
                        "probe reading: addr={:?} name='{}' version={}.{} air_temp={:.2}°C air_pressure={:.0}Pa air_hum={:.2}% soil_temp={:.2}°C soil_hum={:.2}% ts={}",
                        candidate.device.addr(),
                        candidate.meta.name,
                        candidate.meta.version_major,
                        candidate.meta.version_minor,
                        reading.air_temperature_c,
                        reading.air_pressure_pa,
                        reading.air_humidity_pct,
                        reading.soil_temperature_c,
                        reading.soil_humidity_pct,
                        reading.timestamp
                    );

                    let msg = SensorReading {
                        node_id: device_id.clone(),
                        air_temperature_c: reading.air_temperature_c,
                        air_pressure_pa: reading.air_pressure_pa,
                        air_humidity_pct: reading.air_humidity_pct,
                        soil_temperature_c: reading.soil_temperature_c,
                        soil_humidity_pct: reading.soil_humidity_pct,
                        timestamp_unix: reading.timestamp,
                    };
                    if let Err(e) = tx.try_send(msg) {
                        warn!("uplink queue full, dropping reading: {e}");
                    }
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

        info!("Polling cycle done. Waiting {SCAN_WAIT_SECONDS}s...");
        thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
    }
}

fn provision_hub(nvs: EspDefaultNvsPartition) -> Result<(String, String)> {
    let (device_id, hub_secret) = persist::load_or_generate_creds(nvs.clone())?;

    if let Some(dev_token) = option_env!("HUB_TOKEN") {
        persist::seed_jwt_from_env(nvs.clone(), dev_token)?;
        info!("using HUB_TOKEN dev override (skipping ClaimHubToken)");
        return Ok((device_id, dev_token.to_string()));
    }

    info!(
        "\n=== HUB QR PAYLOAD ===\ndevice_id={device_id}\nhub_secret={hub_secret}\n======================\n"
    );

    if let Some(jwt) = persist::read_hub_jwt(nvs.clone())? {
        info!("hub JWT loaded from NVS");
        return Ok((device_id, jwt));
    }

    info!("no JWT persisted; calling auth.v2.ClaimHubToken");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for ClaimHubToken")?;

    let jwt = rt
        .block_on(auth::claim_hub_token(API_URL, &device_id, &hub_secret))
        .context("ClaimHubToken failed")?;

    persist::write_hub_jwt(nvs, &jwt)?;
    info!("hub JWT obtained and persisted");
    Ok((device_id, jwt))
}

fn spawn_uplink_worker(rx: Receiver<SensorReading>, jwt: String) -> Result<()> {
    thread::Builder::new()
        .name("uplink".into())
        .stack_size(UPLINK_THREAD_STACK)
        .spawn(move || {
            if let Err(e) = run_uplink_worker(rx, jwt) {
                error!("uplink worker terminated: {e:#}");
            }
        })?;
    Ok(())
}

fn run_uplink_worker(rx: Receiver<SensorReading>, jwt: String) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    rt.block_on(async move {
        let mut client: Option<grpc::HubClient> = None;

        while let Ok(reading) = rx.recv() {
            if client.is_none() {
                match grpc::HubClient::connect_with_token(&jwt).await {
                    Ok(c) => {
                        info!("gRPC client connected");
                        client = Some(c);
                    }
                    Err(e) => {
                        error!("gRPC connect failed, dropping reading: {e:#}");
                        continue;
                    }
                }
            }

            let c = client.as_mut().unwrap();
            match c
                .send_data(
                    &reading.node_id,
                    reading.air_temperature_c,
                    reading.air_pressure_pa,
                    reading.air_humidity_pct,
                    reading.soil_temperature_c,
                    reading.soil_humidity_pct,
                    reading.timestamp_unix,
                )
                .await
            {
                Ok(()) => info!(
                    "uplink ok: node={} air_temp={:.2} air_pressure={:.0} air_hum={:.2} soil_temp={:.2} soil_hum={:.2} ts={}",
                    reading.node_id,
                    reading.air_temperature_c,
                    reading.air_pressure_pa,
                    reading.air_humidity_pct,
                    reading.soil_temperature_c,
                    reading.soil_humidity_pct,
                    reading.timestamp_unix
                ),
                Err(e) => {
                    error!("uplink failed: {e:#}; dropping client to force reconnect");
                    client = None;
                }
            }
        }
    });

    Ok(())
}
