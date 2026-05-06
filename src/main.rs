mod auth;
mod ble;
mod config;
mod grpc;
mod nvs_store;
mod persist;
mod polling;
mod time;
mod wifi;
mod wifi_prov;

use anyhow::{Context, Result};
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{error, info, warn};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

const API_URL: &str = env!("API_URL");
const HUB_NAME: &str = env!("HUB_NAME");

const SCAN_WAIT_SECONDS: u64 = 30;
const UPLINK_THREAD_STACK: usize = 32 * 1024;
const UPLINK_QUEUE_DEPTH: usize = 16;
const GRPC_CONNECT_MAX_ATTEMPTS: u8 = 3;
const GRPC_SEND_MAX_ATTEMPTS: u8 = 3;
const GRPC_RETRY_BACKOFF_MS: u64 = 500;
const MOTOR_DISPATCH_QUEUE_DEPTH: usize = 8;
const MOTOR_DISPATCH_SLEEP_CHUNK_MS: u64 = 250;

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
    let nvs_partition = EspDefaultNvsPartition::take().context("take default NVS partition")?;
    let mut wifi = wifi::init(nvs_partition.clone())?;

    // BLEDevice::take() is a singleton — take it once so it can be reused
    // for both BLE provisioning (if triggered) and the probe scan loop.
    let ble_device = ble::init_device();

    let mut claimed_hub_identity: Option<(String, String)> = None;

    let wifi_creds = match nvs_store::load(nvs_partition.clone()) {
        Some(creds) => {
            info!(
                "[BOOT] NVS credentials found: ssid='{}' → attempting connection",
                creds.ssid
            );
            match wifi::connect(&creds.ssid, &creds.password, &mut wifi) {
                Ok(()) => {
                    info!("[BOOT] WiFi connected from NVS");
                    creds
                }
                Err(e) => {
                    warn!("[BOOT] NVS connection failed ({e:#}) → BLE provisioning");
                    let provisioned =
                        wifi_prov::run(ble_device, nvs_partition.clone(), &mut wifi, || {
                            claim_hub_after_time_sync(nvs_partition.clone())
                        })?;
                    log_setup_probes(&provisioned.setup_probes);
                    claimed_hub_identity = Some((provisioned.hub_device_id, provisioned.jwt));
                    provisioned.credentials
                }
            }
        }
        None => {
            info!("[BOOT] No NVS credentials → BLE provisioning");
            let provisioned = wifi_prov::run(ble_device, nvs_partition.clone(), &mut wifi, || {
                claim_hub_after_time_sync(nvs_partition.clone())
            })?;
            log_setup_probes(&provisioned.setup_probes);
            claimed_hub_identity = Some((provisioned.hub_device_id, provisioned.jwt));
            provisioned.credentials
        }
    };

    info!("[BOOT] WiFi operational on ssid='{}'", wifi_creds.ssid);

    // Resolve hub identity + JWT. Order matters: `esp_fill_random` (used inside
    // `persist::load_or_generate_creds`) only emits CSPRNG-quality output once
    // Wi-Fi has been started, which is guaranteed by the calls above.
    let (hub_device_id, jwt) = match claimed_hub_identity {
        Some(identity) => identity,
        None => claim_hub_after_time_sync(nvs_partition.clone())?,
    };

    let (motor_dispatch_tx, motor_dispatch_rx) =
        mpsc::sync_channel::<polling::MotorDispatchRequest>(MOTOR_DISPATCH_QUEUE_DEPTH);
    let (tx, rx) = mpsc::sync_channel::<SensorReading>(UPLINK_QUEUE_DEPTH);
    spawn_uplink_worker(rx, jwt.clone())?;
    polling::spawn_command_polling_worker(hub_device_id.clone(), jwt, motor_dispatch_tx)?;

    info!("[BOOT] BLE device ready");

    loop {
        drain_motor_dispatch_requests(ble_device, &motor_dispatch_rx);

        let candidates = match block_on(ble::scan_probe_candidates(ble_device)) {
            Ok(v) => v,
            Err(e) => {
                error!("scan_probe_candidates failed: {e:#}");
                sleep_with_motor_dispatch(
                    ble_device,
                    &motor_dispatch_rx,
                    Duration::from_secs(SCAN_WAIT_SECONDS),
                );
                continue;
            }
        };

        info!("Known probe candidates this cycle: {}", candidates.len());

        #[cfg(feature = "fake-probe")]
        {
            let now_ms = unsafe { esp_idf_svc::sys::time(std::ptr::null_mut()) as i64 } * 1000;
            let fake = SensorReading {
                node_id: hub_device_id.clone(),
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
            sleep_with_motor_dispatch(
                ble_device,
                &motor_dispatch_rx,
                Duration::from_secs(SCAN_WAIT_SECONDS),
            );
            continue;
        }

        for candidate in &candidates {
            drain_motor_dispatch_requests(ble_device, &motor_dispatch_rx);

            if matches!(candidate.meta.mode, ble::ProbeMode::Setup) {
                match block_on(ble::acknowledge_setup_probe(ble_device, &candidate.device)) {
                    Ok(()) => info!(
                        "setup probe acknowledged: addr={:?} name='{}' version={}.{}",
                        candidate.device.addr(),
                        candidate.meta.name,
                        candidate.meta.version_major,
                        candidate.meta.version_minor,
                    ),
                    Err(e) => error!(
                        "setup probe acknowledge failed for addr={:?} name='{}': {e:#}",
                        candidate.device.addr(),
                        candidate.meta.name,
                    ),
                }
                sleep_with_motor_dispatch(
                    ble_device,
                    &motor_dispatch_rx,
                    Duration::from_millis(500),
                );
                continue;
            }

            match block_on(ble::read_probe_from_device(ble_device, &candidate.device)) {
                Ok(Some(reading)) => {
                    info!(
                        "probe reading: addr={:?} probe_uuid={} name='{}' version={}.{} air_temp={:.2}°C air_pressure={:.0}Pa air_hum={:.2}% soil_temp={:.2}°C soil_hum={:.2}% ts={}",
                        candidate.device.addr(),
                        reading.probe_uuid.as_str(),
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
                        node_id: reading.probe_uuid.clone(),
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
            sleep_with_motor_dispatch(ble_device, &motor_dispatch_rx, Duration::from_millis(500));
        }

        info!("Polling cycle done. Waiting {SCAN_WAIT_SECONDS}s...");
        sleep_with_motor_dispatch(
            ble_device,
            &motor_dispatch_rx,
            Duration::from_secs(SCAN_WAIT_SECONDS),
        );
    }
}

fn sleep_with_motor_dispatch(
    ble_device: &esp32_nimble::BLEDevice,
    motor_dispatch_rx: &mpsc::Receiver<polling::MotorDispatchRequest>,
    total_sleep: Duration,
) {
    let mut remaining = total_sleep;
    let chunk = Duration::from_millis(MOTOR_DISPATCH_SLEEP_CHUNK_MS);

    while remaining > Duration::ZERO {
        drain_motor_dispatch_requests(ble_device, motor_dispatch_rx);

        let current_sleep = remaining.min(chunk);
        thread::sleep(current_sleep);
        remaining = remaining.saturating_sub(current_sleep);
    }

    drain_motor_dispatch_requests(ble_device, motor_dispatch_rx);
}

fn log_setup_probes(probes: &[ble::SetupProbe]) {
    if probes.is_empty() {
        info!("[PROV] No setup probes picked up during provisioning");
        return;
    }

    for probe in probes {
        info!(
            "[PROV] Setup probe picked up: uuid={} name='{}' version={}.{}",
            probe.probe_uuid, probe.name, probe.version_major, probe.version_minor,
        );
    }
}

fn drain_motor_dispatch_requests(
    ble_device: &esp32_nimble::BLEDevice,
    motor_dispatch_rx: &mpsc::Receiver<polling::MotorDispatchRequest>,
) {
    loop {
        match motor_dispatch_rx.try_recv() {
            Ok(request) => {
                let result = block_on(ble::dispatch_motor_command_to_probe(
                    ble_device,
                    &request.node_id,
                    &request.command_id,
                    request.action,
                    request.duration_ms,
                    request.expires_at_epoch_ms,
                ));

                if result.is_ok() {
                    info!(
                        "motor dispatch queue result: command_id={} node_id={} dispatch_status=delivered_to_ble reason_code=NONE",
                        request.command_id,
                        request.node_id
                    );
                } else {
                    warn!(
                        "motor dispatch queue result: command_id={} node_id={} dispatch_status=ble_failure reason_code=BLE_WRITE_FAILED err={:?}",
                        request.command_id,
                        request.node_id,
                        result
                    );
                }

                let _ = request.response_tx.send(result);
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => break,
        }
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
        "hub setup credentials ready: device_id={} hub_name={} setup_uri=redacted local_dev_artifact=target/harvesthub-dev-identity.env",
        device_id,
        HUB_NAME,
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

fn claim_hub_after_time_sync(nvs: EspDefaultNvsPartition) -> Result<(String, String)> {
    let _sntp = time::get_sync_sntp()?;
    provision_hub(nvs)
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
            let mut delivered = false;

            for send_attempt in 1..=GRPC_SEND_MAX_ATTEMPTS {
                if client.is_none() {
                    for connect_attempt in 1..=GRPC_CONNECT_MAX_ATTEMPTS {
                        match grpc::HubClient::connect_with_token(&jwt).await {
                            Ok(c) => {
                                info!(
                                    "gRPC client connected (attempt {}/{})",
                                    connect_attempt,
                                    GRPC_CONNECT_MAX_ATTEMPTS
                                );
                                client = Some(c);
                                break;
                            }
                            Err(e) => {
                                error!(
                                    "gRPC connect attempt {}/{} failed: {e:#}",
                                    connect_attempt,
                                    GRPC_CONNECT_MAX_ATTEMPTS
                                );
                                if connect_attempt < GRPC_CONNECT_MAX_ATTEMPTS {
                                    tokio::time::sleep(Duration::from_millis(GRPC_RETRY_BACKOFF_MS))
                                        .await;
                                }
                            }
                        }
                    }

                    if client.is_none() {
                        error!(
                            "gRPC connect failed after {} attempts, dropping reading",
                            GRPC_CONNECT_MAX_ATTEMPTS
                        );
                        break;
                    }
                }

                if let Some(c) = client.as_mut() {
                    match c
                        .send_data(grpc::SensorData {
                            node_id: &reading.node_id,
                            air_temperature: reading.air_temperature_c,
                            air_pressure: reading.air_pressure_pa,
                            air_humidity: reading.air_humidity_pct,
                            soil_temperature: reading.soil_temperature_c,
                            soil_humidity: reading.soil_humidity_pct,
                            timestamp: reading.timestamp_unix,
                        })
                        .await
                    {
                        Ok(()) => {
                            info!(
                                "uplink ok: node={} air_temp={:.2} air_pressure={:.0} air_hum={:.2} soil_temp={:.2} soil_hum={:.2} ts={}",
                                reading.node_id,
                                reading.air_temperature_c,
                                reading.air_pressure_pa,
                                reading.air_humidity_pct,
                                reading.soil_temperature_c,
                                reading.soil_humidity_pct,
                                reading.timestamp_unix
                            );
                            delivered = true;
                            break;
                        }
                        Err(e) => {
                            error!(
                                "uplink attempt {}/{} failed: {e:#}; dropping client to force reconnect",
                                send_attempt,
                                GRPC_SEND_MAX_ATTEMPTS
                            );
                            client = None;
                            if send_attempt < GRPC_SEND_MAX_ATTEMPTS {
                                tokio::time::sleep(Duration::from_millis(GRPC_RETRY_BACKOFF_MS))
                                    .await;
                            }
                        }
                    }
                }
            }

            if !delivered {
                error!(
                    "uplink failed after {} attempts, dropping reading: node={} air_temp={:.2} air_pressure={:.0} air_hum={:.2} soil_temp={:.2} soil_hum={:.2} ts={}",
                    GRPC_SEND_MAX_ATTEMPTS,
                    reading.node_id,
                    reading.air_temperature_c,
                    reading.air_pressure_pa,
                    reading.air_humidity_pct,
                    reading.soil_temperature_c,
                    reading.soil_humidity_pct,
                    reading.timestamp_unix
                );
            }
        }
    });

    Ok(())
}
