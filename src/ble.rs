use crate::config::{
    AIR_PRESSURE_CHAR_UUID, COMPANY_ID, ENVIRONMENTAL_SENSING_SERVICE_UUID, HUM_CHAR_UUID,
    MAGIC_MARKER, PROBE_UUID_CHAR_UUID, SOIL_HUM_CHAR_UUID, SOIL_TEMP_CHAR_UUID, TEMP_CHAR_UUID,
};
use crate::time;
use anyhow::{bail, Context};
use esp32_nimble::{uuid128, BLEAdvertisedDevice, BLEDevice, BLEScan};
use log::info;
use std::cell::RefCell;
use std::rc::Rc;

const BLE_SCAN_MS: u64 = 8_000;

#[derive(Debug, Clone)]
pub(crate) struct ProbeReading {
    pub(crate) probe_uuid: String,
    pub(crate) air_temperature_c: f64,
    pub(crate) air_pressure_pa: f64,
    pub(crate) air_humidity_pct: f64,
    pub(crate) soil_temperature_c: f64,
    pub(crate) soil_humidity_pct: f64,
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
    let seen = Rc::new(RefCell::new(Vec::<ProbeCandidate>::new()));
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
                let mfg = data.manufacture_data()?;

                let meta = parse_probe_meta(mfg.company_identifier, mfg.payload)?;

                let mut seen = seen_cb.borrow_mut();

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

    let devices = seen.borrow().clone();
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

    let pressure_raw = match service
        .get_characteristic(uuid128!(AIR_PRESSURE_CHAR_UUID))
        .await
    {
        Ok(pressure_chr) => pressure_chr
            .read_value()
            .await
            .context("air pressure read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let soil_temp_raw = match service
        .get_characteristic(uuid128!(SOIL_TEMP_CHAR_UUID))
        .await
    {
        Ok(soil_temp_chr) => soil_temp_chr
            .read_value()
            .await
            .context("soil temperature read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let soil_hum_raw = match service
        .get_characteristic(uuid128!(SOIL_HUM_CHAR_UUID))
        .await
    {
        Ok(soil_hum_chr) => soil_hum_chr
            .read_value()
            .await
            .context("soil humidity read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let probe_uuid = match service
        .get_characteristic(uuid128!(PROBE_UUID_CHAR_UUID))
        .await
    {
        Ok(uuid_chr) => {
            let raw = uuid_chr
                .read_value()
                .await
                .context("probe uuid read failed")?;
            parse_probe_uuid_ascii(&raw).unwrap_or_else(|| device.addr().to_string())
        }
        Err(_) => device.addr().to_string(),
    };

    info!("raw temp bytes={:02X?}", temp_raw);
    info!("raw pressure bytes={:02X?}", pressure_raw);
    info!("raw hum bytes={:02X?}", hum_raw);
    info!("raw soil temp bytes={:02X?}", soil_temp_raw);
    info!("raw soil hum bytes={:02X?}", soil_hum_raw);

    let air_temperature_c = parse_temperature_c(&temp_raw)?;
    let air_pressure_pa = parse_pressure_pa(&pressure_raw)?;
    let air_humidity_pct = parse_humidity_pct(&hum_raw)?;
    let soil_temperature_c = parse_soil_temperature_c(&soil_temp_raw)?;
    let soil_humidity_pct = parse_soil_humidity_pct(&soil_hum_raw)?;

    let _ = client.disconnect();

    Ok(Some(ProbeReading {
        probe_uuid,
        air_temperature_c,
        air_pressure_pa,
        air_humidity_pct,
        soil_temperature_c,
        soil_humidity_pct,
        timestamp: time::get_unix_now_ms(),
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

fn parse_pressure_pa(raw: &[u8]) -> anyhow::Result<f64> {
    if raw.len() < 4 {
        bail!("pressure payload too short: {:?}", raw);
    }

    let value = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    Ok(value as f64)
}

fn parse_soil_temperature_c(raw: &[u8]) -> anyhow::Result<f64> {
    if raw.len() < 2 {
        bail!("soil temperature payload too short: {:?}", raw);
    }

    let value = i16::from_be_bytes([raw[0], raw[1]]);
    Ok(value as f64 / 100.0)
}

fn parse_soil_humidity_pct(raw: &[u8]) -> anyhow::Result<f64> {
    if raw.len() < 2 {
        bail!("soil humidity payload too short: {:?}", raw);
    }

    let value = u16::from_be_bytes([raw[0], raw[1]]);
    Ok(value as f64 / 100.0)
}

fn parse_probe_uuid_ascii(raw: &[u8]) -> Option<String> {
    if raw.len() != 36 {
        return None;
    }

    let value = core::str::from_utf8(raw).ok()?;
    if value.len() != 36 {
        return None;
    }

    Some(value.to_string())
}
