// src/nvs_store.rs

use anyhow::Result;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::info;

const NVS_NAMESPACE: &str = "harvesthub";
const KEY_SSID: &str = "wifi_ssid";
const KEY_PASS: &str = "wifi_pass";

#[derive(Debug, Clone)]
pub struct WifiCredentials {
    pub ssid: String,
    pub password: String,
}

fn open_nvs(nvs_partition: EspDefaultNvsPartition) -> Result<EspNvs<NvsDefault>> {
    Ok(EspNvs::new(nvs_partition, NVS_NAMESPACE, true)?)
}

pub fn load(nvs_partition: EspDefaultNvsPartition) -> Option<WifiCredentials> {
    let nvs = open_nvs(nvs_partition).ok()?;

    // get_str retourne Option<&str> dans le buffer fourni
    let mut ssid_buf = [0u8; 33];
    let mut pass_buf = [0u8; 65];

    // get_str remplit le buffer et retourne Some(longueur) ou None
    let ssid = nvs.get_str(KEY_SSID, &mut ssid_buf).ok()??;
    let pass = nvs.get_str(KEY_PASS, &mut pass_buf).ok()??;

    let ssid = ssid.trim_end_matches('\0').to_string();
    let pass = pass.trim_end_matches('\0').to_string();

    if ssid.is_empty() {
        return None;
    }

    info!("[NVS] Credentials chargés : ssid='{}'", ssid);
    Some(WifiCredentials { ssid, password: pass })
}

pub fn save(nvs_partition: EspDefaultNvsPartition, creds: &WifiCredentials) -> Result<()> {
    let nvs = open_nvs(nvs_partition)?;
    nvs.set_str(KEY_SSID, &creds.ssid)?;
    nvs.set_str(KEY_PASS, &creds.password)?;
    info!("[NVS] Credentials sauvegardés : ssid='{}'", creds.ssid);
    Ok(())
}

#[allow(dead_code)]
pub fn clear(nvs_partition: EspDefaultNvsPartition) -> Result<()> {
    let nvs = open_nvs(nvs_partition)?;
    let _ = nvs.remove(KEY_SSID);
    let _ = nvs.remove(KEY_PASS);
    info!("[NVS] Credentials effacés");
    Ok(())
}
