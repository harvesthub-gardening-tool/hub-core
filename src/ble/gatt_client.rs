#![allow(dead_code)]

use trouble_host::gatt::GattClient;
use trouble_host::types::uuid::Uuid;

/// Collected readings from multiple characteristics.
/// Keep Option<> because any read can fail independently.
#[derive(Debug, Clone, Copy)]
pub struct ProbeReading {
    pub temperature_c_x100: Option<i16>,
    pub humidity_pct_x100: Option<u16>,
}

// Typical SIG Temperature (0x2A6E): sint16, unit 0.01°C
fn parse_temp_i16_x100(p: &[u8]) -> Option<i16> {
    if p.len() < 2 {
        return None;
    }
    Some(i16::from_le_bytes([p[0], p[1]]))
}

// Typical SIG Humidity (0x2A6F): uint16, unit 0.01%
fn parse_hum_u16_x100(p: &[u8]) -> Option<u16> {
    if p.len() < 2 {
        return None;
    }
    Some(u16::from_le_bytes([p[0], p[1]]))
}

/// Read multiple characteristics from the same service.
///
/// - Finds service handle by UUID
/// - For each characteristic UUID: reads its value
/// - Returns a struct with Option fields filled when successful
pub async fn read_probe_data_list<C, P, const MAX_SERVICES: usize>(
    gatt: &GattClient<'_, C, P, MAX_SERVICES>,
    service_uuid: [u8; 16],
    char_uuids: &[[u8; 16]],
) -> Option<ProbeReading>
where
    C: trouble_host::Controller,
    P: trouble_host::PacketPool,
{
    let svc_uuid = Uuid::new_long(service_uuid);

    // 1) Lookup service(s) by UUID (returns handles)
    let services = gatt.services_by_uuid(&svc_uuid).await.ok()?;
    let svc = services.first()?; // take the first match

    let mut out = ProbeReading {
        temperature_c_x100: None,
        humidity_pct_x100: None,
    };

    // 2) Read each characteristic by UUID inside that service
    let mut buf = [0u8; 32];

    for &uuid128 in char_uuids {
        let chr_uuid = Uuid::new_long(uuid128);

        let n = match gatt
            .read_characteristic_by_uuid(svc, &chr_uuid, &mut buf)
            .await
        {
            Ok(n) => n,
            Err(_) => continue,
        };

        let val = &buf[..n];

        // If you use your *own* UUIDs, map them here by equality.
        // Replace TEMP_UUID_128 / HUM_UUID_128 with your config constants.
        //
        // Example:
        // if uuid128 == TEMP_UUID_128 { ... }
        // if uuid128 == HUM_UUID_128 { ... }

        // Generic parsing (first successful parser wins):
        if let Some(t) = parse_temp_i16_x100(val) {
            out.temperature_c_x100 = Some(t);
        }

        if let Some(h) = parse_hum_u16_x100(val) {
            out.humidity_pct_x100 = Some(h);
        }
    }

    Some(out)
}
