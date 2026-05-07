use crate::config::{
    AIR_HUM_CHAR_UUID, AIR_PRESSURE_CHAR_UUID, AIR_TEMP_CHAR_UUID, COMPANY_ID,
    ENVIRONMENTAL_SENSING_SERVICE_UUID, LEGACY_TEST_MARKER, MAGIC_MARKER,
    MOTOR_COMMAND_ACTION_RUN_FOR_DURATION, MOTOR_COMMAND_ACTION_STOP, MOTOR_COMMAND_CHAR_UUID,
    MOTOR_COMMAND_MAX_DURATION_MS, MOTOR_COMMAND_PAYLOAD_ACTION_OFFSET,
    MOTOR_COMMAND_PAYLOAD_COMMAND_ID_LEN, MOTOR_COMMAND_PAYLOAD_COMMAND_ID_OFFSET,
    MOTOR_COMMAND_PAYLOAD_DURATION_MS_OFFSET, MOTOR_COMMAND_PAYLOAD_EXPIRY_MS_OFFSET,
    MOTOR_COMMAND_PAYLOAD_LEN, MOTOR_COMMAND_PAYLOAD_MAGIC, MOTOR_COMMAND_PAYLOAD_MAGIC_OFFSET,
    MOTOR_COMMAND_PAYLOAD_VERSION, MOTOR_COMMAND_PAYLOAD_VERSION_OFFSET, MOTOR_RESULT_CHAR_UUID,
    MOTOR_RESULT_PAYLOAD_COMMAND_ID_OFFSET, MOTOR_RESULT_PAYLOAD_LEN, MOTOR_RESULT_PAYLOAD_MAGIC,
    MOTOR_RESULT_PAYLOAD_MAGIC_OFFSET, MOTOR_RESULT_PAYLOAD_REASON_OFFSET,
    MOTOR_RESULT_PAYLOAD_STATUS_OFFSET, MOTOR_RESULT_PAYLOAD_VERSION,
    MOTOR_RESULT_PAYLOAD_VERSION_OFFSET, PROBE_SETUP_CONFIRM_CHAR_UUID, PROBE_SETUP_CONFIRM_MAGIC,
    PROBE_UUID_CHAR_UUID, SETUP_MARKER, SETUP_PROBE_NAME, SOIL_HUM_CHAR_UUID, SOIL_TEMP_CHAR_UUID,
};
use crate::time;
use anyhow::{bail, Context};
use esp32_nimble::{uuid128, BLEAdvertisedDevice, BLEDevice, BLERemoteService, BLEScan};
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

pub(crate) struct ProbeSessionResult {
    pub(crate) reading: ProbeReading,
    pub(crate) motor_dispatch_result: Option<Result<(), MotorDispatchFailure>>,
    pub(crate) last_motor_result: Option<ProbeMotorResult>,
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

#[derive(Debug, Clone)]
pub(crate) enum MotorDispatchFailure {
    Expired,
    BleWriteFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeMotorResult {
    pub(crate) status: u8,
    pub(crate) reason_code: u8,
    pub(crate) command_id: [u8; MOTOR_COMMAND_PAYLOAD_COMMAND_ID_LEN],
}

impl ProbeMotorResult {
    pub(crate) fn status_label(&self) -> &'static str {
        match self.status {
            3 => "SENT_TO_PROBE",
            4 => "EXECUTING",
            5 => "SUCCEEDED",
            6 => "FAILED",
            7 => "EXPIRED",
            _ => "UNKNOWN",
        }
    }

    pub(crate) fn reason_label(&self) -> &'static str {
        match self.reason_code {
            1 => "NONE",
            3 => "EXPIRED",
            7 => "UART_TIMEOUT",
            8 => "UART_REJECTED",
            _ => "UNKNOWN",
        }
    }

    pub(crate) fn command_id_label(&self) -> String {
        let mut output = String::with_capacity(36);
        for (index, byte) in self.command_id.iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                output.push('-');
            }
            output.push(nibble_to_hex(byte >> 4));
            output.push(nibble_to_hex(byte & 0x0f));
        }
        output
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self.status, 5..=7)
    }
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => '0',
    }
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

pub(crate) fn build_motor_command_payload(
    command_id: &str,
    action: u8,
    duration_ms: i32,
    expires_at_ms: i64,
) -> anyhow::Result<[u8; MOTOR_COMMAND_PAYLOAD_LEN]> {
    if action != MOTOR_COMMAND_ACTION_STOP && action != MOTOR_COMMAND_ACTION_RUN_FOR_DURATION {
        bail!("unsupported motor action byte: {action}");
    }

    let mut command_id_compact = [0u8; MOTOR_COMMAND_PAYLOAD_COMMAND_ID_LEN];
    parse_uuid_compact_bytes(command_id, &mut command_id_compact)
        .context("invalid command_id UUID")?;

    let clamped_duration_ms = duration_ms.max(0) as u32;
    let clamped_duration_ms = clamped_duration_ms.min(MOTOR_COMMAND_MAX_DURATION_MS);
    let now_unix_ms = time::get_unix_now_ms();
    if expires_at_ms <= now_unix_ms {
        bail!("command expired before payload encode");
    }
    let remaining_expiry_ms = (expires_at_ms - now_unix_ms).min(u32::MAX as i64) as u32;

    let mut payload = [0u8; MOTOR_COMMAND_PAYLOAD_LEN];
    payload[MOTOR_COMMAND_PAYLOAD_MAGIC_OFFSET..MOTOR_COMMAND_PAYLOAD_VERSION_OFFSET]
        .copy_from_slice(MOTOR_COMMAND_PAYLOAD_MAGIC);
    payload[MOTOR_COMMAND_PAYLOAD_VERSION_OFFSET] = MOTOR_COMMAND_PAYLOAD_VERSION;
    payload[MOTOR_COMMAND_PAYLOAD_ACTION_OFFSET] = action;
    payload[MOTOR_COMMAND_PAYLOAD_COMMAND_ID_OFFSET..MOTOR_COMMAND_PAYLOAD_DURATION_MS_OFFSET]
        .copy_from_slice(&command_id_compact);
    payload[MOTOR_COMMAND_PAYLOAD_DURATION_MS_OFFSET..MOTOR_COMMAND_PAYLOAD_EXPIRY_MS_OFFSET]
        .copy_from_slice(&clamped_duration_ms.to_le_bytes());
    payload[MOTOR_COMMAND_PAYLOAD_EXPIRY_MS_OFFSET..MOTOR_COMMAND_PAYLOAD_LEN]
        .copy_from_slice(&remaining_expiry_ms.to_le_bytes());

    Ok(payload)
}

pub(crate) async fn read_probe_and_maybe_dispatch_motor(
    ble_device: &BLEDevice,
    device: &BLEAdvertisedDevice,
    resolve_motor_dispatch_request: impl FnOnce(&str) -> Option<(String, u8, i32, i64)>,
) -> anyhow::Result<Option<ProbeSessionResult>> {
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

    let reading = match read_probe_from_service(service, device).await {
        Ok(Some(reading)) => reading,
        Ok(None) => {
            let _ = client.disconnect();
            return Ok(None);
        }
        Err(err) => {
            let _ = client.disconnect();
            return Err(err);
        }
    };

    let motor_dispatch_request = resolve_motor_dispatch_request(reading.probe_uuid.as_str());

    let motor_dispatch_result =
        if let Some((command_id, action, duration_ms, expires_at_ms)) = motor_dispatch_request {
            Some(
                write_motor_payload_to_service(
                    service,
                    command_id.as_str(),
                    action,
                    duration_ms,
                    expires_at_ms,
                )
                .await,
            )
        } else {
            None
        };

    let last_motor_result = read_motor_result_from_service(service).await.ok().flatten();

    let _ = client.disconnect();

    Ok(Some(ProbeSessionResult {
        reading,
        motor_dispatch_result,
        last_motor_result,
    }))
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

async fn write_motor_payload_to_service(
    service: &mut BLERemoteService,
    command_id: &str,
    action: u8,
    duration_ms: i32,
    expires_at_ms: i64,
) -> Result<(), MotorDispatchFailure> {
    let characteristic = match service
        .get_characteristic(uuid128!(MOTOR_COMMAND_CHAR_UUID))
        .await
    {
        Ok(characteristic) => characteristic,
        Err(e) => {
            return Err(MotorDispatchFailure::BleWriteFailed(format!(
                "motor command characteristic missing: {e:#}"
            )));
        }
    };

    if expires_at_ms <= time::get_unix_now_ms() {
        return Err(MotorDispatchFailure::Expired);
    }

    let payload = build_motor_command_payload(command_id, action, duration_ms, expires_at_ms)
        .map_err(|e| {
            if expires_at_ms <= time::get_unix_now_ms() {
                MotorDispatchFailure::Expired
            } else {
                MotorDispatchFailure::BleWriteFailed(format!("motor payload encode failed: {e:#}"))
            }
        })?;

    if let Err(e) = characteristic.write_value(&payload, true).await {
        return Err(MotorDispatchFailure::BleWriteFailed(format!(
            "motor command write failed: {e:#}"
        )));
    }

    Ok(())
}

async fn read_motor_result_from_service(
    service: &mut BLERemoteService,
) -> anyhow::Result<Option<ProbeMotorResult>> {
    let characteristic = match service
        .get_characteristic(uuid128!(MOTOR_RESULT_CHAR_UUID))
        .await
    {
        Ok(characteristic) => characteristic,
        Err(_) => return Ok(None),
    };

    let raw = characteristic
        .read_value()
        .await
        .context("motor result read failed")?;

    parse_motor_result_payload(&raw)
}

fn parse_motor_result_payload(raw: &[u8]) -> anyhow::Result<Option<ProbeMotorResult>> {
    if raw.len() != MOTOR_RESULT_PAYLOAD_LEN {
        return Ok(None);
    }

    if &raw[MOTOR_RESULT_PAYLOAD_MAGIC_OFFSET..MOTOR_RESULT_PAYLOAD_VERSION_OFFSET]
        != MOTOR_RESULT_PAYLOAD_MAGIC
    {
        return Ok(None);
    }

    if raw[MOTOR_RESULT_PAYLOAD_VERSION_OFFSET] != MOTOR_RESULT_PAYLOAD_VERSION {
        return Ok(None);
    }

    let mut command_id = [0u8; MOTOR_COMMAND_PAYLOAD_COMMAND_ID_LEN];
    command_id
        .copy_from_slice(&raw[MOTOR_RESULT_PAYLOAD_COMMAND_ID_OFFSET..MOTOR_RESULT_PAYLOAD_LEN]);

    if command_id.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }

    Ok(Some(ProbeMotorResult {
        status: raw[MOTOR_RESULT_PAYLOAD_STATUS_OFFSET],
        reason_code: raw[MOTOR_RESULT_PAYLOAD_REASON_OFFSET],
        command_id,
    }))
}

async fn read_probe_from_service(
    service: &mut BLERemoteService,
    device: &BLEAdvertisedDevice,
) -> anyhow::Result<Option<ProbeReading>> {
    let air_temp_raw = match service
        .get_characteristic(uuid128!(AIR_TEMP_CHAR_UUID))
        .await
    {
        Ok(chr) => chr
            .read_value()
            .await
            .context("air temperature read failed")?,
        Err(_) => return Ok(None),
    };

    let air_pressure_raw = match service
        .get_characteristic(uuid128!(AIR_PRESSURE_CHAR_UUID))
        .await
    {
        Ok(chr) => chr.read_value().await.context("air pressure read failed")?,
        Err(_) => return Ok(None),
    };

    let air_hum_raw = match service
        .get_characteristic(uuid128!(AIR_HUM_CHAR_UUID))
        .await
    {
        Ok(chr) => chr.read_value().await.context("air humidity read failed")?,
        Err(_) => return Ok(None),
    };

    let soil_temp_raw = match service
        .get_characteristic(uuid128!(SOIL_TEMP_CHAR_UUID))
        .await
    {
        Ok(chr) => chr
            .read_value()
            .await
            .context("soil temperature read failed")?,
        Err(_) => return Ok(None),
    };

    let soil_hum_raw = match service
        .get_characteristic(uuid128!(SOIL_HUM_CHAR_UUID))
        .await
    {
        Ok(chr) => chr
            .read_value()
            .await
            .context("soil humidity read failed")?,
        Err(_) => return Ok(None),
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

fn parse_uuid_compact_bytes(
    raw_uuid: &str,
    output: &mut [u8; MOTOR_COMMAND_PAYLOAD_COMMAND_ID_LEN],
) -> anyhow::Result<()> {
    let mut nibble_index = 0usize;
    let mut high_nibble = 0u8;

    for byte in raw_uuid.bytes() {
        if byte == b'-' {
            continue;
        }

        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => bail!("invalid UUID character '{}'", byte as char),
        };

        if nibble_index % 2 == 0 {
            high_nibble = nibble;
        } else {
            let out_index = nibble_index / 2;
            if out_index >= output.len() {
                bail!("UUID too long");
            }
            output[out_index] = (high_nibble << 4) | nibble;
        }

        nibble_index += 1;
    }

    if nibble_index != 32 {
        bail!("UUID must contain exactly 32 hexadecimal digits");
    }

    Ok(())
}
