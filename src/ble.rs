use crate::config::{
    COMPANY_ID, ENVIRONMENTAL_SENSING_SERVICE_UUID, HUM_CHAR_UUID, MAGIC_MARKER, TEMP_CHAR_UUID,
};
use crate::time;
use anyhow::{bail, Context};
use esp32_nimble::{uuid128, BLEAdvertisedDevice, BLEDevice, BLEScan};
use log::info;
use std::sync::{Arc, Mutex};

const BLE_SCAN_MS: u64 = 8_000;

#[derive(Debug, Clone)]
pub(crate) struct ProbeReading {
    pub(crate) temperature_c: f64,
    pub(crate) humidity_pct: f64,
    pub(crate) timestamp: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ProbeMeta {
    pub(crate) version_major: u8,
    pub(crate) version_minor: u8,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ProbeCandidate {
    pub(crate) device: BLEAdvertisedDevice,
    pub(crate) meta: ProbeMeta,
}

pub fn init_device() -> &'static mut BLEDevice {
    BLEDevice::take()
}

pub async fn scan_probe_candidates(ble_device: &BLEDevice) -> anyhow::Result<Vec<ProbeCandidate>> {
    let seen = Arc::new(Mutex::new(Vec::<ProbeCandidate>::new()));
    let seen_cb = seen.clone();

    let mut ble_scan = BLEScan::new();

    ble_scan
        .active_scan(true)
        .interval(100)
        .window(99)
        .start(
            ble_device,
            BLE_SCAN_MS as i32,
            move |device, data| -> Option<BLEAdvertisedDevice> {
                let Some(mfg) = data.manufacture_data() else {
                    return None;
                };

                let Some(meta) = parse_probe_meta(mfg.company_identifier, mfg.payload) else {
                    return None;
                };

                let mut seen = seen_cb.lock().unwrap();

                let already_seen = seen.iter().any(|d| d.device.addr() == device.addr());
                if !already_seen {
                    if let Some(name) = data.name() {
                        info!(
                            "BLE probe adv: addr={:?} rssi={} adv_name='{}' probe_name='{}' version={}.{} company=0x{:04X} payload={:02X?}",
                            device.addr(),
                            device.rssi(),
                            name,
                            meta.name,
                            meta.version_major,
                            meta.version_minor,
                            mfg.company_identifier,
                            mfg.payload
                        );
                    } else {
                        info!(
                            "BLE probe adv: addr={:?} rssi={} probe_name='{}' version={}.{} company=0x{:04X} payload={:02X?}",
                            device.addr(),
                            device.rssi(),
                            meta.name,
                            meta.version_major,
                            meta.version_minor,
                            mfg.company_identifier,
                            mfg.payload
                        );
                    }

                    seen.push(ProbeCandidate {
                        device: *device,
                        meta,
                    });
                }

                None
            },
        )
        .await?;

    let devices = seen.lock().unwrap().clone();
    Ok(devices)
}

pub(crate) async fn read_probe_from_device(
    ble_device: &BLEDevice,
    device: &BLEAdvertisedDevice,
) -> anyhow::Result<Option<ProbeReading>> {
    let mut client = ble_device.new_client();

    client.on_connect(|client| {
        let _ = client.update_conn_params(120, 120, 0, 60);
    });

    client.connect(&device.addr()).await?;
    info!("BLE connected: {:?}", device.addr());

    let service = match client
        .get_service(uuid128!(ENVIRONMENTAL_SENSING_SERVICE_UUID))
        .await
    {
        Ok(s) => s,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let temp_raw = match service.get_characteristic(uuid128!(TEMP_CHAR_UUID)).await {
        Ok(temp_chr) => temp_chr
            .read_value()
            .await
            .context("temperature read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let hum_raw = match service.get_characteristic(uuid128!(HUM_CHAR_UUID)).await {
        Ok(hum_chr) => hum_chr.read_value().await.context("humidity read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    info!("raw temp bytes={:02X?}", temp_raw);
    info!("raw hum bytes={:02X?}", hum_raw);

    let temperature_c = parse_temperature_c(&temp_raw)?;
    let humidity_pct = parse_humidity_pct(&hum_raw)?;

    let _ = client.disconnect();

    Ok(Some(ProbeReading {
        temperature_c,
        humidity_pct,
        timestamp: time::get_unix_now(),
    }))
}

fn parse_probe_meta(company_id: u16, payload: &[u8]) -> Option<ProbeMeta> {
    if company_id != COMPANY_ID {
        return None;
    }

    if payload.len() < MAGIC_MARKER.len() + 3 {
        return None;
    }

    if !payload.starts_with(MAGIC_MARKER) {
        return None;
    }

    let base = MAGIC_MARKER.len();
    let version_major = payload[base];
    let version_minor = payload[base + 1];
    let name_len = payload[base + 2] as usize;

    let name_start = base + 3;
    let name_end = name_start + name_len;

    if payload.len() < name_end {
        return None;
    }

    let name = String::from_utf8_lossy(&payload[name_start..name_end]).to_string();

    Some(ProbeMeta {
        version_major,
        version_minor,
        name,
    })
}

fn parse_temperature_c(raw: &[u8]) -> anyhow::Result<f64> {
    if raw.len() < 2 {
        bail!("temperature payload too short: {:?}", raw);
    }

    let value = i16::from_be_bytes([raw[0], raw[1]]);
    Ok(value as f64 / 100.0)
}

fn parse_humidity_pct(raw: &[u8]) -> anyhow::Result<f64> {
    if raw.len() < 2 {
        bail!("humidity payload too short: {:?}", raw);
    }

    let value = u16::from_be_bytes([raw[0], raw[1]]);
    Ok(value as f64 / 100.0)
}
