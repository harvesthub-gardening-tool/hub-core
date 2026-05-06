// src/nvs_store.rs

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::{info, warn};

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
    let nvs = match open_nvs(nvs_partition) {
        Ok(handle) => handle,
        Err(e) => {
            warn!("[NVS] Impossible d'ouvrir les credentials Wi-Fi : {e:#}");
            return None;
        }
    };

    let ssid = match read_str(&nvs, KEY_SSID) {
        Ok(Some(value)) => value,
        Ok(None) => return None,
        Err(e) => {
            warn!("[NVS] Lecture SSID Wi-Fi impossible : {e:#}");
            return None;
        }
    };
    let pass = match read_str(&nvs, KEY_PASS) {
        Ok(Some(value)) => value,
        Ok(None) => return None,
        Err(e) => {
            warn!("[NVS] Lecture password Wi-Fi impossible : {e:#}");
            return None;
        }
    };

    if ssid.is_empty() {
        return None;
    }

    info!("[NVS] Credentials chargés : ssid='{}'", ssid);
    Some(WifiCredentials {
        ssid,
        password: pass,
    })
}

fn read_str(handle: &EspNvs<NvsDefault>, key: &str) -> Result<Option<String>> {
    let Some(len) = handle
        .str_len(key)
        .with_context(|| format!("nvs str_len('{key}')"))?
    else {
        return Ok(None);
    };

    let mut buf = vec![0u8; len];
    let value = handle
        .get_str(key, &mut buf)
        .with_context(|| format!("nvs get_str('{key}')"))?
        .map(ToOwned::to_owned);
    Ok(value)
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
