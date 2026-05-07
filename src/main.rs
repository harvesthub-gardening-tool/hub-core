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
use std::thread;
use std::time::Duration;

const API_URL: &str = env!("API_URL");
const HUB_NAME: &str = env!("HUB_NAME");

const SCAN_WAIT_SECONDS: u64 = 30;
const GRPC_SEND_MAX_ATTEMPTS: u8 = 3;
const GRPC_RETRY_BACKOFF_MS: u64 = 500;

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
                    restart_after_successful_provisioning(provisioned);
                }
            }
        }
        None => {
            info!("[BOOT] No NVS credentials → BLE provisioning");
            let provisioned = wifi_prov::run(ble_device, nvs_partition.clone(), &mut wifi, || {
                claim_hub_after_time_sync(nvs_partition.clone())
            })?;
            restart_after_successful_provisioning(provisioned);
        }
    };

    info!("[BOOT] WiFi operational on ssid='{}'", wifi_creds.ssid);
    log_heap("after-wifi");

    // Resolve hub identity + JWT. Order matters: `esp_fill_random` (used inside
    // `persist::load_or_generate_creds`) only emits CSPRNG-quality output once
    // Wi-Fi has been started, which is guaranteed by the calls above.
    let (hub_device_id, jwt) = claim_hub_after_time_sync(nvs_partition.clone())?;
    log_heap("after-jwt");

    let radio_memory_gate = polling::RadioMemoryGate::default();
    let mut command_poller = polling::CommandPoller::default();

    log_heap("before-first-command-poll-window");
    poll_commands_in_quiet_window(
        &mut command_poller,
        &hub_device_id,
        &jwt,
        &radio_memory_gate,
    );
    info!("[BOOT] first command poll window finished before BLE scan startup");
    log_heap("after-first-command-poll-window");

    info!("[BOOT] BLE device ready");

    loop {
        let candidates = match run_with_radio_memory_gate(&radio_memory_gate, || {
            log_heap("before-ble-scan");
            block_on(ble::scan_probe_candidates(ble_device))
        }) {
            Ok(v) => v,
            Err(e) => {
                error!("scan_probe_candidates failed: {e:#}");
                poll_commands_in_quiet_window(
                    &mut command_poller,
                    &hub_device_id,
                    &jwt,
                    &radio_memory_gate,
                );
                thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
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
            upload_reading(&jwt, &radio_memory_gate, &fake);
        }

        if candidates.is_empty() {
            info!("No probe found this cycle, waiting {SCAN_WAIT_SECONDS}s...");
            poll_commands_in_quiet_window(
                &mut command_poller,
                &hub_device_id,
                &jwt,
                &radio_memory_gate,
            );
            thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
            continue;
        }

        for candidate in &candidates {
            if matches!(candidate.meta.mode, ble::ProbeMode::Setup) {
                match run_with_radio_memory_gate(&radio_memory_gate, || {
                    block_on(ble::acknowledge_setup_probe(ble_device, &candidate.device))
                }) {
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
                thread::sleep(Duration::from_millis(500));
                continue;
            }

            match run_with_radio_memory_gate(&radio_memory_gate, || {
                block_on(ble::read_probe_and_maybe_dispatch_motor(
                    ble_device,
                    &candidate.device,
                    |probe_uuid| {
                        command_poller
                            .peek_pending_for_probe_uuid(probe_uuid)
                            .map(|request| {
                                (
                                    request.command_id,
                                    request.action,
                                    request.duration_ms,
                                    request.expires_at_epoch_ms,
                                )
                            })
                    },
                ))
            }) {
                Ok(Some(session)) => {
                    let probe_result_log = session
                        .last_motor_result
                        .as_ref()
                        .map(|result| {
                            format!(
                                " motor_status={} motor_reason={} motor_command_id={}",
                                result.status_label(),
                                result.reason_label(),
                                result.command_id_label(),
                            )
                        })
                        .unwrap_or_default();
                    let reading = session.reading;
                    info!(
                        "probe reading: addr={:?} probe_uuid={} name='{}' version={}.{} air_temp={:.2}°C air_pressure={:.0}Pa air_hum={:.2}% soil_temp={:.2}°C soil_hum={:.2}% ts={}{}",
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
                        reading.timestamp,
                        probe_result_log,
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
                    upload_reading(&jwt, &radio_memory_gate, &msg);
                    if let Some(probe_result) = session.last_motor_result {
                        command_poller.apply_probe_motor_result(
                            &hub_device_id,
                            &jwt,
                            &radio_memory_gate,
                            reading.probe_uuid.as_str(),
                            probe_result,
                        );
                    }
                    if let Some(result) = session.motor_dispatch_result {
                        command_poller.complete_dispatched_for_probe(
                            &hub_device_id,
                            &jwt,
                            &radio_memory_gate,
                            reading.probe_uuid.as_str(),
                            result,
                        );
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
        poll_commands_in_quiet_window(
            &mut command_poller,
            &hub_device_id,
            &jwt,
            &radio_memory_gate,
        );
        thread::sleep(Duration::from_secs(SCAN_WAIT_SECONDS));
    }
}

fn poll_commands_in_quiet_window(
    command_poller: &mut polling::CommandPoller,
    hub_device_id: &str,
    jwt: &str,
    radio_memory_gate: &polling::RadioMemoryGate,
) {
    info!("command poll quiet window starting");
    command_poller.poll_once(hub_device_id, jwt, radio_memory_gate);
    info!("command poll quiet window finished");
}

fn upload_reading(
    jwt: &str,
    radio_memory_gate: &polling::RadioMemoryGate,
    reading: &SensorReading,
) {
    for send_attempt in 1..=GRPC_SEND_MAX_ATTEMPTS {
        let send_result = upload_reading_once(jwt, radio_memory_gate, reading);
        match send_result {
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
                return;
            }
            Err(e) => {
                error!(
                    "uplink attempt {}/{} failed: {e:#}",
                    send_attempt, GRPC_SEND_MAX_ATTEMPTS
                );
                if send_attempt < GRPC_SEND_MAX_ATTEMPTS {
                    thread::sleep(Duration::from_millis(GRPC_RETRY_BACKOFF_MS));
                }
            }
        }
    }

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

fn upload_reading_once(
    jwt: &str,
    radio_memory_gate: &polling::RadioMemoryGate,
    reading: &SensorReading,
) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(1)
        .build()?;

    rt.block_on(async {
        let _guard = radio_memory_gate.lock();
        log_heap("before-uplink-connect");
        let mut client = grpc::HubClient::connect_with_token(jwt).await?;

        log_heap("before-uplink-send");
        client
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
    })
}

fn run_with_radio_memory_gate<T>(
    radio_memory_gate: &polling::RadioMemoryGate,
    f: impl FnOnce() -> T,
) -> T {
    let _guard = radio_memory_gate.lock();
    f()
}

fn log_heap(label: &str) {
    let free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
    let min_free_heap = unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() };
    let free_8bit =
        unsafe { esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_8BIT) };
    let largest_8bit = unsafe {
        esp_idf_svc::sys::heap_caps_get_largest_free_block(esp_idf_svc::sys::MALLOC_CAP_8BIT)
    };
    let internal_caps = esp_idf_svc::sys::MALLOC_CAP_INTERNAL | esp_idf_svc::sys::MALLOC_CAP_8BIT;
    let free_internal = unsafe { esp_idf_svc::sys::heap_caps_get_free_size(internal_caps) };
    let largest_internal =
        unsafe { esp_idf_svc::sys::heap_caps_get_largest_free_block(internal_caps) };
    info!(
        "[HEAP] {label}: free={free_heap} min_free={min_free_heap} free_8bit={free_8bit} largest_8bit={largest_8bit} free_internal={free_internal} largest_internal={largest_internal}"
    );
}

fn restart_after_successful_provisioning(provisioned: wifi_prov::ProvisionedHub) -> ! {
    log_setup_probes(&provisioned.setup_probes);
    let hub_device_id = provisioned.hub_device_id;
    let jwt_len = provisioned.jwt.len();
    info!(
        "[BOOT] Provisioning complete for ssid='{}' hub_device_id={} jwt_bytes={}; restarting to boot from saved credentials",
        provisioned.credentials.ssid,
        hub_device_id,
        jwt_len
    );
    thread::sleep(Duration::from_millis(500));

    // A provisioning session leaves BLE/Wi-Fi/gRPC setup state resident. Restarting
    // after credentials and the hub JWT are persisted gives the normal boot path a
    // clean heap before it spawns the long-lived uplink and command-polling workers.
    unsafe { esp_idf_svc::sys::esp_restart() }
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
        .max_blocking_threads(1)
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
