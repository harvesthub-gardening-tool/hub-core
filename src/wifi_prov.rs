// src/wifi_prov.rs

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use esp32_nimble::{uuid128, BLEAdvertisementData, BLEDevice, NimbleProperties};
use log::info;

use crate::nvs_store::{self, WifiCredentials};
use crate::wifi;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};

const PROV_SERVICE_UUID:  &str = "0000ab00-0000-1000-8000-00805f9b34fb";
const CHAR_SSID_UUID:     &str = "0000ab01-0000-1000-8000-00805f9b34fb";
const CHAR_PASSWORD_UUID: &str = "0000ab02-0000-1000-8000-00805f9b34fb";
const CHAR_STATUS_UUID:   &str = "0000ab03-0000-1000-8000-00805f9b34fb";

const STATUS_WAITING: u8 = 0x00;
const STATUS_OK:      u8 = 0x01;
const STATUS_NOK:     u8 = 0x02;

pub fn run(
    ble_device: &'static BLEDevice,
    nvs_partition: EspDefaultNvsPartition,
    wifi: &mut BlockingWifi<EspWifi<'static>>,
) -> Result<WifiCredentials> {
    info!("[PROV] Démarrage du mode provisioning BLE");

    let server = ble_device.get_server();
    let service = server.create_service(uuid128!(PROV_SERVICE_UUID));

    let char_ssid = service
        .lock()
        .create_characteristic(uuid128!(CHAR_SSID_UUID), NimbleProperties::WRITE);

    let char_password = service
        .lock()
        .create_characteristic(uuid128!(CHAR_PASSWORD_UUID), NimbleProperties::WRITE);

    let char_status = service
        .lock()
        .create_characteristic(
            uuid128!(CHAR_STATUS_UUID),
            NimbleProperties::READ | NimbleProperties::NOTIFY,
        );

    char_status.lock().set_value(&[STATUS_WAITING]);

    // Les credentials sont persistés entre les connexions BLE
    // nRF Connect peut se déconnecter entre le write SSID et le write password
    let received_ssid: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let received_pass: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    {
        let store = received_ssid.clone();
        char_ssid.lock().on_write(move |args| {
            if let Ok(s) = std::str::from_utf8(args.recv_data()) {
                let s = s.trim_end_matches('\0').to_string();
                info!("[PROV] SSID reçu : '{}' ({} octets)", s, s.len());
                *store.lock().unwrap() = Some(s);
            }
        });
    }

    {
        let store = received_pass.clone();
        char_password.lock().on_write(move |args| {
            if let Ok(p) = std::str::from_utf8(args.recv_data()) {
                let p = p.trim_end_matches('\0').to_string();
                info!("[PROV] Password reçu ({} octets)", p.len());
                *store.lock().unwrap() = Some(p);
            }
        });
    }

    let ble_advertising = ble_device.get_advertising();
    {
        let mut adv = ble_advertising.lock();
        adv.set_data(
            BLEAdvertisementData::new()
                .name("HarvestHub-Setup")
                .add_service_uuid(uuid128!(PROV_SERVICE_UUID)),
        )?;
        adv.start()?;
    }

    info!("[PROV] Annonce BLE active : 'HarvestHub-Setup'");
    info!("[PROV] En attente des credentials BLE (SSID + Password)...");

    // Boucle principale : on poll les deux valeurs toutes les 500ms
    // peu importe le nombre de connexions/déconnexions BLE entre les writes
    loop {
        thread::sleep(Duration::from_millis(500));

        let ssid = received_ssid.lock().unwrap().clone();
        let pass = received_pass.lock().unwrap().clone();

        match (ssid, pass) {
            (Some(ssid), Some(pass)) => {
                info!("[PROV] Les deux credentials reçus — tentative WiFi : ssid='{}'", ssid);

                // Stopper l'annonce pendant la tentative
                let _ = ble_advertising.lock().stop();

                let creds = WifiCredentials { ssid, password: pass };

                match wifi::connect(&creds.ssid, &creds.password, wifi) {
                    Ok(()) => {
                        info!("[PROV] WiFi OK !");
                        char_status.lock().set_value(&[STATUS_OK]).notify();
                        thread::sleep(Duration::from_secs(2));

                        if let Err(e) = nvs_store::save(nvs_partition, &creds) {
                            log::warn!("[PROV] Impossible de sauvegarder en NVS : {e:#}");
                        }
                        return Ok(creds);
                    }

                    Err(e) => {
                        log::warn!("[PROV] WiFi NOK : {e:#}");
                        char_status.lock().set_value(&[STATUS_NOK]).notify();
                        thread::sleep(Duration::from_secs(2));

                        // Reset pour permettre un nouvel essai
                        char_status.lock().set_value(&[STATUS_WAITING]);
                        *received_ssid.lock().unwrap() = None;
                        *received_pass.lock().unwrap() = None;

                        // Relancer l'annonce
                        let mut adv = ble_advertising.lock();
                        adv.set_data(
                            BLEAdvertisementData::new()
                                .name("HarvestHub-Setup")
                                .add_service_uuid(uuid128!(PROV_SERVICE_UUID)),
                        )?;
                        adv.start()?;

                        info!("[PROV] En attente de nouveaux credentials...");
                    }
                }
            }

            (Some(ssid), None) => {
                info!("[PROV] SSID='{}' reçu, en attente du password...", ssid);
            }

            (None, Some(_)) => {
                info!("[PROV] Password reçu, en attente du SSID...");
            }

            (None, None) => {
                // Rien encore, on continue d'attendre silencieusement
            }
        }
    }
}
