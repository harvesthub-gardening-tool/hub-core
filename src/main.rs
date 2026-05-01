mod ble;
mod config;
mod grpc;
mod nvs_store;
mod time;
mod wifi;
mod wifi_prov;

use anyhow::Result;
use esp32_nimble::BLEDevice;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{error, info, warn};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

const HUB_NODE_ID: &str = env!("HUB_NODE_ID");
const SCAN_WAIT_SECONDS: u64 = 30;
const UPLINK_THREAD_STACK: usize = 32 * 1024;
const UPLINK_QUEUE_DEPTH: usize = 16;

#[derive(Debug, Clone)]
struct SensorReading {
    node_id: String,
    temperature_c: f64,
    humidity_pct: f64,
    soil_moisture_pct: f64,
    timestamp_unix: i64,
}

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    EspLogger::initialize_default();

    let _eventfs = esp_idf_svc::io::vfs::MountedEventfs::mount(5)?;

    let nvs_partition = EspDefaultNvsPartition::take()?;
    let mut wifi = wifi::init(nvs_partition.clone())?;

    // -----------------------------------------------------------------------
    // Boot : credentials NVS → connexion directe, sinon provisioning BLE
    // -----------------------------------------------------------------------
    let wifi_creds = match nvs_store::load(nvs_partition.clone()) {
        Some(creds) => {
            info!(
                "[BOOT] Credentials NVS trouvés : ssid='{}' → tentative connexion",
                creds.ssid
            );
            match wifi::connect(&creds.ssid, &creds.password, &mut wifi) {
                Ok(()) => {
                    info!("[BOOT] WiFi connecté depuis NVS");
                    creds
                }
                Err(e) => {
                    warn!("[BOOT] Connexion NVS échouée ({e:#}) → provisioning BLE");
                    let ble_device = BLEDevice::take();
                    wifi_prov::run(ble_device, nvs_partition.clone(), &mut wifi)?
                }
            }
        }
        None => {
            info!("[BOOT] Aucun credential en NVS → provisioning BLE");
            let ble_device = BLEDevice::take();
            wifi_prov::run(ble_device, nvs_partition.clone(), &mut wifi)?
        }
    };

    info!("[BOOT] WiFi opérationnel sur ssid='{}'", wifi_creds.ssid);

    // -----------------------------------------------------------------------
    // SNTP + uplink gRPC
    // -----------------------------------------------------------------------
    let _sntp = time::get_sync_sntp()?;
    let (tx, rx) = mpsc::sync_channel::<SensorReading>(UPLINK_QUEUE_DEPTH);
    spawn_uplink_worker(rx)?;

    // -----------------------------------------------------------------------
    // Boucle principale BLE scan sondes (inchangée)
    // -----------------------------------------------------------------------
    let ble_device = ble::init_device();
    info!("[BOOT] BLE device ready");

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
                node_id: HUB_NODE_ID.to_string(),
                temperature_c: 21.5,
                humidity_pct: 48.0,
                soil_moisture_pct: 33.3,
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
                        "probe reading: addr={:?} name='{}' version={}.{} temp={:.2}°C hum={:.2}% ts={}",
                        candidate.device.addr(),
                        candidate.meta.name,
                        candidate.meta.version_major,
                        candidate.meta.version_minor,
                        reading.temperature_c,
                        reading.humidity_pct,
                        reading.timestamp
                    );
                    let msg = SensorReading {
                        node_id: HUB_NODE_ID.to_string(),
                        temperature_c: reading.temperature_c,
                        humidity_pct: reading.humidity_pct,
                        soil_moisture_pct: 0.0,
                        timestamp_unix: reading.timestamp,
                    };
                    if let Err(e) = tx.try_send(msg) {
                        warn!("uplink queue full, dropping reading: {e}");
                    }
                }
                Ok(None) => info!(
                    "skip addr={:?} name='{}': probe service/chars not readable",
                    candidate.device.addr(),
                    candidate.meta.name,
                ),
                Err(e) => error!(
                    "read failed for addr={:?} name='{}': {e:#}",
                    candidate.device.addr(),
                    candidate.meta.name,
                ),
            }
            thread::sleep(Duration::from_millis(500));
        }

        info!("Polling cycle done. Waiting {SCAN_WAIT_SECONDS}s...");
        thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
    }
}

fn spawn_uplink_worker(rx: Receiver<SensorReading>) -> Result<()> {
    thread::Builder::new()
        .name("uplink".into())
        .stack_size(UPLINK_THREAD_STACK)
        .spawn(move || {
            if let Err(e) = run_uplink_worker(rx) {
                error!("uplink worker terminated: {e:#}");
            }
        })?;
    Ok(())
}

fn run_uplink_worker(rx: Receiver<SensorReading>) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    rt.block_on(async move {
        let mut client: Option<grpc::HubClient> = None;

        while let Ok(reading) = rx.recv() {
            if client.is_none() {
                match grpc::HubClient::connect().await {
                    Ok(c) => { info!("gRPC client connected"); client = Some(c); }
                    Err(e) => { error!("gRPC connect failed, dropping reading: {e:#}"); continue; }
                }
            }

            let c = client.as_mut().unwrap();
            match c.send_data(
                &reading.node_id,
                reading.temperature_c,
                reading.humidity_pct,
                reading.soil_moisture_pct,
                reading.timestamp_unix,
            ).await {
                Ok(()) => info!(
                    "uplink ok: node={} temp={:.2} hum={:.2} ts={}",
                    reading.node_id, reading.temperature_c,
                    reading.humidity_pct, reading.timestamp_unix
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
