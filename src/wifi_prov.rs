// src/wifi_prov.rs

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use esp32_nimble::{uuid128, BLEAdvertisementData, BLEDevice, NimbleProperties};
use log::info;

use crate::nvs_store::{self, WifiCredentials};
use crate::wifi;
use crate::{ble, grpc, time};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};

const PROV_SERVICE_UUID: &str = "0000ab00-0000-1000-8000-00805f9b34fb";
const CHAR_SSID_UUID: &str = "0000ab01-0000-1000-8000-00805f9b34fb";
const CHAR_PASSWORD_UUID: &str = "0000ab02-0000-1000-8000-00805f9b34fb";
const CHAR_STATUS_UUID: &str = "0000ab03-0000-1000-8000-00805f9b34fb";
const CHAR_PROBES_UUID: &str = "0000ab04-0000-1000-8000-00805f9b34fb";

const STATUS_WAITING: u8 = 0x00;
const STATUS_WIFI_OK: u8 = 0x01;
const STATUS_WIFI_NOK: u8 = 0x02;
const STATUS_CLAIM_OK: u8 = 0x03;
const STATUS_CLAIM_NOK: u8 = 0x04;
const STATUS_PROBE_SCAN_STARTED: u8 = 0x05;
const STATUS_PROBE_SCAN_DONE: u8 = 0x06;
const MAX_PROBE_RESULT_BYTES: usize = 220;
const DUMMY_AIR_TEMPERATURE_C: f64 = 20.0;
const DUMMY_AIR_PRESSURE_PA: f64 = 101_325.0;
const DUMMY_AIR_HUMIDITY_PCT: f64 = 50.0;
const DUMMY_SOIL_TEMPERATURE_C: f64 = 18.0;
const DUMMY_SOIL_HUMIDITY_PCT: f64 = 35.0;

pub struct ProvisionedHub {
    pub credentials: WifiCredentials,
    pub hub_device_id: String,
    pub jwt: String,
    pub setup_probes: Vec<ble::SetupProbe>,
}

#[derive(Default)]
struct PendingWifiCredentials {
    ssid: Option<String>,
    password: Option<String>,
}

impl PendingWifiCredentials {
    fn set_ssid(&mut self, ssid: String) {
        self.ssid = Some(ssid);
        self.password = None;
    }

    fn set_password(&mut self, password: String) {
        self.password = Some(password);
    }

    fn take_complete(&mut self) -> Option<WifiCredentials> {
        if self.ssid.is_none() || self.password.is_none() {
            return None;
        }

        let ssid = self.ssid.take()?;
        let password = self.password.take()?;
        Some(WifiCredentials { ssid, password })
    }
}

enum PendingCredentialsState {
    Complete(WifiCredentials),
    WaitingForPassword(String),
    WaitingForSsid,
    Empty,
}

fn next_pending_credentials_state(
    pending_credentials: &Mutex<PendingWifiCredentials>,
) -> PendingCredentialsState {
    let mut pending = pending_credentials.lock().unwrap();

    if let Some(credentials) = pending.take_complete() {
        return PendingCredentialsState::Complete(credentials);
    }

    if let Some(ssid) = pending.ssid.clone() {
        return PendingCredentialsState::WaitingForPassword(ssid);
    }

    if pending.password.is_some() {
        return PendingCredentialsState::WaitingForSsid;
    }

    PendingCredentialsState::Empty
}

pub fn run<F>(
    ble_device: &'static BLEDevice,
    nvs_partition: EspDefaultNvsPartition,
    wifi: &mut BlockingWifi<EspWifi<'static>>,
    mut claim_hub: F,
) -> Result<ProvisionedHub>
where
    F: FnMut() -> Result<(String, String)>,
{
    info!("[PROV] Démarrage du mode provisioning BLE");

    let server = ble_device.get_server();
    let service = server.create_service(uuid128!(PROV_SERVICE_UUID));

    let char_ssid = service
        .lock()
        .create_characteristic(uuid128!(CHAR_SSID_UUID), NimbleProperties::WRITE);

    let char_password = service
        .lock()
        .create_characteristic(uuid128!(CHAR_PASSWORD_UUID), NimbleProperties::WRITE);

    let char_status = service.lock().create_characteristic(
        uuid128!(CHAR_STATUS_UUID),
        NimbleProperties::READ | NimbleProperties::NOTIFY,
    );
    let char_probes = service.lock().create_characteristic(
        uuid128!(CHAR_PROBES_UUID),
        NimbleProperties::READ | NimbleProperties::NOTIFY,
    );

    char_status.lock().set_value(&[STATUS_WAITING]);
    char_probes.lock().set_value(&[]);

    // Les credentials sont persistés entre les connexions BLE.
    // Chaque write SSID démarre une nouvelle paire : le password doit suivre,
    // ce qui évite de mixer un nouveau SSID avec un ancien mot de passe.
    let pending_credentials: Arc<Mutex<PendingWifiCredentials>> =
        Arc::new(Mutex::new(PendingWifiCredentials::default()));

    {
        let store = pending_credentials.clone();
        char_ssid.lock().on_write(move |args| {
            if let Ok(s) = std::str::from_utf8(args.recv_data()) {
                let s = s.trim_end_matches('\0').to_string();
                info!("[PROV] SSID reçu : '{}' ({} octets)", s, s.len());
                store.lock().unwrap().set_ssid(s);
            }
        });
    }

    {
        let store = pending_credentials.clone();
        char_password.lock().on_write(move |args| {
            if let Ok(p) = std::str::from_utf8(args.recv_data()) {
                let p = p.trim_end_matches('\0').to_string();
                info!("[PROV] Password reçu ({} octets)", p.len());
                store.lock().unwrap().set_password(p);
            }
        });
    }

    let ble_advertising = ble_device.get_advertising();
    {
        let mut adv = ble_advertising.lock();
        adv.set_data(
            BLEAdvertisementData::new()
                .name("HarvestHub-Dev")
                .add_service_uuid(uuid128!(PROV_SERVICE_UUID)),
        )?;
        adv.start()?;
    }

    info!("[PROV] Annonce BLE active : 'HarvestHub-Dev'");
    info!("[PROV] En attente des credentials BLE (SSID + Password)...");

    // Boucle principale : on poll les deux valeurs toutes les 500ms
    // peu importe le nombre de connexions/déconnexions BLE entre les writes
    loop {
        thread::sleep(Duration::from_millis(500));

        match next_pending_credentials_state(&pending_credentials) {
            PendingCredentialsState::Complete(creds) => {
                info!(
                    "[PROV] Les deux credentials reçus — tentative WiFi : ssid='{}'",
                    creds.ssid
                );

                match wifi::connect_for_provisioning(&creds.ssid, &creds.password, wifi) {
                    Ok(()) => {
                        info!("[PROV] WiFi OK !");
                        char_status.lock().set_value(&[STATUS_WIFI_OK]).notify();
                        thread::sleep(Duration::from_secs(2));

                        if let Err(e) = nvs_store::save(nvs_partition.clone(), &creds) {
                            log::warn!("[PROV] Impossible de sauvegarder en NVS : {e:#}");
                        }

                        match claim_hub() {
                            Ok((hub_device_id, jwt)) => {
                                info!("[PROV] ClaimHubToken OK !");
                                char_status.lock().set_value(&[STATUS_CLAIM_OK]).notify();
                                thread::sleep(Duration::from_secs(2));

                                info!("[PROV] Scan des sondes en mode setup...");
                                char_status
                                    .lock()
                                    .set_value(&[STATUS_PROBE_SCAN_STARTED])
                                    .notify();

                                let setup_probes = match esp_idf_svc::hal::task::block_on(
                                    ble::discover_setup_probes(ble_device),
                                ) {
                                    Ok(probes) => probes,
                                    Err(e) => {
                                        log::warn!("[PROV] Scan des sondes impossible : {e:#}");
                                        Vec::new()
                                    }
                                };

                                if let Err(e) =
                                    acknowledge_setup_probes_in_database(&jwt, &setup_probes)
                                {
                                    log::warn!(
                                        "[PROV] Setup probe DB acknowledgement failed: {e:#}"
                                    );
                                }

                                let encoded_probes = encode_setup_probes(&setup_probes);
                                char_probes.lock().set_value(encoded_probes.as_bytes());
                                char_status
                                    .lock()
                                    .set_value(&[STATUS_PROBE_SCAN_DONE])
                                    .notify();
                                thread::sleep(Duration::from_secs(1));

                                return Ok(ProvisionedHub {
                                    credentials: creds,
                                    hub_device_id,
                                    jwt,
                                    setup_probes,
                                });
                            }
                            Err(e) => {
                                log::warn!("[PROV] ClaimHubToken NOK : {e:#}");
                                char_status.lock().set_value(&[STATUS_CLAIM_NOK]).notify();
                                thread::sleep(Duration::from_secs(2));

                                char_status.lock().set_value(&[STATUS_WAITING]);

                                info!("[PROV] En attente de credentials après échec claim...");
                            }
                        }
                    }

                    Err(e) => {
                        log::warn!("[PROV] WiFi NOK : {e:#}");
                        char_status.lock().set_value(&[STATUS_WIFI_NOK]).notify();
                        thread::sleep(Duration::from_secs(2));

                        char_status.lock().set_value(&[STATUS_WAITING]);

                        let mut adv = ble_advertising.lock();
                        adv.set_data(
                            BLEAdvertisementData::new()
                                .name("HarvestHub-Dev")
                                .add_service_uuid(uuid128!(PROV_SERVICE_UUID)),
                        )?;
                        adv.start()?;

                        info!("[PROV] En attente de nouveaux credentials...");
                    }
                }
            }

            PendingCredentialsState::WaitingForPassword(ssid) => {
                info!("[PROV] SSID='{}' reçu, en attente du password...", ssid);
            }

            PendingCredentialsState::WaitingForSsid => {
                info!("[PROV] Password reçu, en attente du SSID...");
            }

            PendingCredentialsState::Empty => {
                // Rien encore, on continue d'attendre silencieusement
            }
        }
    }
}

fn acknowledge_setup_probes_in_database(jwt: &str, probes: &[ble::SetupProbe]) -> Result<()> {
    if probes.is_empty() {
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(1)
        .build()
        .context("build tokio runtime for setup probe DB acknowledgement")?;

    rt.block_on(async move {
        let mut client = grpc::HubClient::connect_with_token(jwt)
            .await
            .context("connect to GardenService for setup probe DB acknowledgement")?;
        let mut failed_acknowledgements = 0usize;

        for probe in probes {
            let timestamp = time::get_unix_now_ms();
            match client
                .send_data(grpc::SensorData {
                    node_id: &probe.probe_uuid,
                    air_temperature: DUMMY_AIR_TEMPERATURE_C,
                    air_pressure: DUMMY_AIR_PRESSURE_PA,
                    air_humidity: DUMMY_AIR_HUMIDITY_PCT,
                    soil_temperature: DUMMY_SOIL_TEMPERATURE_C,
                    soil_humidity: DUMMY_SOIL_HUMIDITY_PCT,
                    timestamp,
                })
                .await
            {
                Ok(()) => info!(
                    "[PROV] Setup probe DB acknowledgement sent: uuid={} name='{}' ts={}",
                    probe.probe_uuid, probe.name, timestamp,
                ),
                Err(e) => {
                    failed_acknowledgements += 1;
                    log::warn!(
                        "[PROV] Setup probe DB acknowledgement failed: uuid={} name='{}': {e:#}",
                        probe.probe_uuid,
                        probe.name,
                    );
                }
            }
        }

        if failed_acknowledgements > 0 {
            bail!(
                "{} of {} setup probe DB acknowledgement(s) failed",
                failed_acknowledgements,
                probes.len()
            );
        }

        Ok(())
    })
}

fn encode_setup_probes(probes: &[ble::SetupProbe]) -> String {
    let mut encoded = String::new();

    for probe in probes {
        let line = format!(
            "{}|{}|{}.{}",
            probe.probe_uuid, probe.name, probe.version_major, probe.version_minor,
        );
        let separator_len = usize::from(!encoded.is_empty());
        if encoded.len() + separator_len + line.len() > MAX_PROBE_RESULT_BYTES {
            break;
        }

        if !encoded.is_empty() {
            encoded.push('\n');
        }
        encoded.push_str(&line);
    }

    encoded
}
