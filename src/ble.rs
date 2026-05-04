use crate::config::{
    AIR_HUM_CHAR_UUID, AIR_PRESSURE_CHAR_UUID, AIR_TEMP_CHAR_UUID, COMPANY_ID,
    ENVIRONMENTAL_SENSING_SERVICE_UUID, LEGACY_TEST_MARKER, MAGIC_MARKER,
    PROBE_SETUP_CONFIRM_CHAR_UUID, PROBE_SETUP_CONFIRM_MAGIC, PROBE_UUID_CHAR_UUID, SETUP_MARKER,
    SETUP_PROBE_NAME, SOIL_HUM_CHAR_UUID, SOIL_TEMP_CHAR_UUID,
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
    pub(crate) mode: ProbeMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeMode {
    Normal,
    Setup,
}

#[derive(Debug, Clone)]
pub(crate) struct ProbeCandidate {
    pub(crate) device: BLEAdvertisedDevice,
    pub(crate) meta: ProbeMeta,
}

#[derive(Debug, Clone)]
pub(crate) struct SetupProbe {
    pub(crate) probe_uuid: String,
    pub(crate) name: String,
    pub(crate) version_major: u8,
    pub(crate) version_minor: u8,
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
                let advertised_name = data.name().map(|name| name.to_string());
                let meta = data
                    .manufacture_data()
                    .and_then(|mfg| {
                        parse_probe_meta(
                        mfg.company_identifier,
                        mfg.payload,
                        advertised_name.clone(),
                        )
                    })
                    .or_else(|| parse_probe_meta_from_name(advertised_name.as_deref()))?;

                let mut seen = seen_cb.borrow_mut();

                if let Some(existing) = seen
                    .iter_mut()
                    .find(|candidate| candidate.device.addr() == device.addr())
                {
                    if matches!(meta.mode, ProbeMode::Setup)
                        && !matches!(existing.meta.mode, ProbeMode::Setup)
                    {
                        info!(
                            "BLE probe adv upgrade: addr={:?} name='{}' mode={:?}",
                            device.addr(),
                            meta.name,
                            meta.mode,
                        );
                        existing.meta = meta;
                    }
                } else {
                    match data.manufacture_data() {
                        Some(mfg) => {
                            if let Some(name) = advertised_name.as_deref() {
                                info!(
                                    "BLE probe adv: addr={:?} rssi={} adv_name='{}' probe_name='{}' mode={:?} version={}.{} company=0x{:04X} payload={:02X?}",
                                    device.addr(),
                                    device.rssi(),
                                    name,
                                    meta.name,
                                    meta.mode,
                                    meta.version_major,
                                    meta.version_minor,
                                    mfg.company_identifier,
                                    mfg.payload
                                );
                            } else {
                                info!(
                                    "BLE probe adv: addr={:?} rssi={} probe_name='{}' mode={:?} version={}.{} company=0x{:04X} payload={:02X?}",
                                    device.addr(),
                                    device.rssi(),
                                    meta.name,
                                    meta.mode,
                                    meta.version_major,
                                    meta.version_minor,
                                    mfg.company_identifier,
                                    mfg.payload
                                );
                            }
                        }
                        None => info!(
                            "BLE probe adv: addr={:?} rssi={} adv_name='{}' probe_name='{}' mode={:?} version={}.{} company=<none>",
                            device.addr(),
                            device.rssi(),
                            advertised_name.as_deref().unwrap_or(""),
                            meta.name,
                            meta.mode,
                            meta.version_major,
                            meta.version_minor,
                        ),
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

pub(crate) async fn acknowledge_setup_probe(
    ble_device: &BLEDevice,
    device: &BLEAdvertisedDevice,
) -> anyhow::Result<()> {
    let mut client = ble_device.new_client();

    client.connect(&device.addr()).await?;
    info!("BLE setup probe picked up: {:?}", device.addr());

    let service = match client
        .get_service(uuid128!(ENVIRONMENTAL_SENSING_SERVICE_UUID))
        .await
    {
        Ok(service) => service,
        Err(e) => {
            let _ = client.disconnect();
            return Err(e).context("setup probe environmental service missing");
        }
    };

    match service
        .get_characteristic(uuid128!(PROBE_SETUP_CONFIRM_CHAR_UUID))
        .await
    {
        Ok(characteristic) => characteristic
            .write_value(PROBE_SETUP_CONFIRM_MAGIC, true)
            .await
            .context("setup probe confirmation write failed")?,
        Err(e) => {
            let _ = client.disconnect();
            return Err(e).context("setup probe confirmation characteristic missing");
        }
    }

    let _ = client.disconnect();
    Ok(())
}

pub(crate) async fn discover_setup_probes(
    ble_device: &BLEDevice,
) -> anyhow::Result<Vec<SetupProbe>> {
    let candidates = scan_probe_candidates(ble_device).await?;
    let mut probes = Vec::new();

    for candidate in candidates
        .iter()
        .filter(|candidate| matches!(candidate.meta.mode, ProbeMode::Setup))
    {
        match read_setup_probe_identity(ble_device, candidate).await {
            Ok(probe) => probes.push(probe),
            Err(e) => info!(
                "setup probe pickup failed: addr={:?} name='{}': {e:#}",
                candidate.device.addr(),
                candidate.meta.name,
            ),
        }
    }

    Ok(probes)
}

async fn read_setup_probe_identity(
    ble_device: &BLEDevice,
    candidate: &ProbeCandidate,
) -> anyhow::Result<SetupProbe> {
    let mut client = ble_device.new_client();

    client.connect(&candidate.device.addr()).await?;
    info!(
        "BLE setup probe picked up: addr={:?} name='{}' version={}.{}",
        candidate.device.addr(),
        candidate.meta.name,
        candidate.meta.version_major,
        candidate.meta.version_minor,
    );

    let service = match client
        .get_service(uuid128!(ENVIRONMENTAL_SENSING_SERVICE_UUID))
        .await
    {
        Ok(service) => service,
        Err(e) => {
            let _ = client.disconnect();
            return Err(e).context("setup probe environmental service missing");
        }
    };

    let probe_uuid = match service
        .get_characteristic(uuid128!(PROBE_UUID_CHAR_UUID))
        .await
    {
        Ok(characteristic) => match characteristic.read_value().await {
            Ok(raw) => {
                parse_probe_uuid_ascii(&raw).unwrap_or_else(|| candidate.device.addr().to_string())
            }
            Err(e) => {
                let _ = client.disconnect();
                return Err(e).context("setup probe uuid read failed");
            }
        },
        Err(_) => candidate.device.addr().to_string(),
    };

    match service
        .get_characteristic(uuid128!(PROBE_SETUP_CONFIRM_CHAR_UUID))
        .await
    {
        Ok(characteristic) => characteristic
            .write_value(PROBE_SETUP_CONFIRM_MAGIC, true)
            .await
            .context("setup probe confirmation write failed")?,
        Err(e) => {
            let _ = client.disconnect();
            return Err(e).context("setup probe confirmation characteristic missing");
        }
    }

    let _ = client.disconnect();
    Ok(SetupProbe {
        probe_uuid,
        name: candidate.meta.name.clone(),
        version_major: candidate.meta.version_major,
        version_minor: candidate.meta.version_minor,
    })
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

    let air_temp_raw = match service
        .get_characteristic(uuid128!(AIR_TEMP_CHAR_UUID))
        .await
    {
        Ok(chr) => chr
            .read_value()
            .await
            .context("air temperature read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let air_pressure_raw = match service
        .get_characteristic(uuid128!(AIR_PRESSURE_CHAR_UUID))
        .await
    {
        Ok(chr) => chr.read_value().await.context("air pressure read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let air_hum_raw = match service
        .get_characteristic(uuid128!(AIR_HUM_CHAR_UUID))
        .await
    {
        Ok(chr) => chr.read_value().await.context("air humidity read failed")?,
        Err(_) => {
            let _ = client.disconnect();
            return Ok(None);
        }
    };

    let soil_temp_raw = match service
        .get_characteristic(uuid128!(SOIL_TEMP_CHAR_UUID))
        .await
    {
        Ok(chr) => chr
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
        Ok(chr) => chr
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
        Ok(chr) => {
            let raw = chr.read_value().await.context("probe uuid read failed")?;
            parse_probe_uuid_ascii(&raw).unwrap_or_else(|| device.addr().to_string())
        }
        Err(_) => device.addr().to_string(),
    };

    info!("raw air temp bytes={:02X?}", air_temp_raw);
    info!("raw air pressure bytes={:02X?}", air_pressure_raw);
    info!("raw air hum bytes={:02X?}", air_hum_raw);
    info!("raw soil temp bytes={:02X?}", soil_temp_raw);
    info!("raw soil hum bytes={:02X?}", soil_hum_raw);

    let air_temperature_c = parse_centi_celsius_i16(&air_temp_raw, "air temperature")?;
    let air_pressure_pa = parse_pressure_pa(&air_pressure_raw)?;
    let air_humidity_pct = parse_centi_percent_u16(&air_hum_raw, "air humidity")?;
    let soil_temperature_c = parse_centi_celsius_i16(&soil_temp_raw, "soil temperature")?;
    let soil_humidity_pct = parse_centi_percent_u16(&soil_hum_raw, "soil humidity")?;

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

fn parse_probe_meta(
    company_id: u16,
    payload: &[u8],
    advertised_name: Option<String>,
) -> Option<ProbeMeta> {
    if company_id != COMPANY_ID {
        return None;
    }

    let (marker_len, marker_mode) = matching_marker(payload)?;

    let base = marker_len;
    if payload.len() == base + 2 {
        let version_major = payload[base];
        let version_minor = payload[base + 1];
        let name =
            advertised_name.unwrap_or_else(|| default_probe_name_for_mode(marker_mode).to_string());
        let mode = if name == SETUP_PROBE_NAME {
            ProbeMode::Setup
        } else {
            marker_mode
        };

        return Some(ProbeMeta {
            version_major,
            version_minor,
            name,
            mode,
        });
    }

    let version_major = payload[base];
    let version_minor = payload[base + 1];
    let name_len = payload[base + 2] as usize;

    let name_start = base + 3;
    let name_end = name_start + name_len;

    if payload.len() < name_end {
        return None;
    }

    let name = String::from_utf8_lossy(&payload[name_start..name_end]).to_string();

    let mode = if marker_mode == ProbeMode::Setup || name == SETUP_PROBE_NAME {
        ProbeMode::Setup
    } else {
        ProbeMode::Normal
    };

    Some(ProbeMeta {
        version_major,
        version_minor,
        name,
        mode,
    })
}

fn parse_probe_meta_from_name(advertised_name: Option<&str>) -> Option<ProbeMeta> {
    let name = advertised_name?;
    let mode = match name {
        SETUP_PROBE_NAME => ProbeMode::Setup,
        "HH-PROBE-A" => ProbeMode::Normal,
        _ => return None,
    };

    Some(ProbeMeta {
        version_major: 0,
        version_minor: 0,
        name: name.to_string(),
        mode,
    })
}

fn matching_marker(payload: &[u8]) -> Option<(usize, ProbeMode)> {
    if payload.starts_with(MAGIC_MARKER) && payload.len() >= MAGIC_MARKER.len() + 2 {
        return Some((MAGIC_MARKER.len(), ProbeMode::Normal));
    }

    if payload.starts_with(SETUP_MARKER) && payload.len() >= SETUP_MARKER.len() + 2 {
        return Some((SETUP_MARKER.len(), ProbeMode::Setup));
    }

    if payload.starts_with(LEGACY_TEST_MARKER) && payload.len() >= LEGACY_TEST_MARKER.len() + 2 {
        return Some((LEGACY_TEST_MARKER.len(), ProbeMode::Normal));
    }

    None
}

fn default_probe_name_for_mode(mode: ProbeMode) -> &'static str {
    match mode {
        ProbeMode::Normal => "HH-PROBE-A",
        ProbeMode::Setup => SETUP_PROBE_NAME,
    }
}

fn parse_centi_celsius_i16(raw: &[u8], label: &str) -> anyhow::Result<f64> {
    if raw.len() < 2 {
        bail!("{label} payload too short: {:?}", raw);
    }

    let value = i16::from_be_bytes([raw[0], raw[1]]);
    Ok(value as f64 / 100.0)
}

fn parse_pressure_pa(raw: &[u8]) -> anyhow::Result<f64> {
    if raw.len() < 4 {
        bail!("air pressure payload too short: {:?}", raw);
    }

    let value = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    Ok(value as f64)
}

fn parse_centi_percent_u16(raw: &[u8], label: &str) -> anyhow::Result<f64> {
    if raw.len() < 2 {
        bail!("{label} payload too short: {:?}", raw);
    }

    let value = u16::from_be_bytes([raw[0], raw[1]]);
    if value == u16::MAX {
        bail!("humidity payload is invalid sentinel: {:02X?}", raw);
    }

    if value > 10_000 {
        bail!(
            "humidity payload out of range (>100.00%): raw={:02X?} value={}",
            raw,
            value
        );
    }

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
