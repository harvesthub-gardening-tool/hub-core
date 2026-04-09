use esp_println::{print, println};
use trouble_host::gatt::GattClient;
use trouble_host::types::uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub struct ProbeReading {
    pub temperature_c_x100: Option<i16>,
    pub humidity_pct_x100: Option<u16>,
}

fn parse_temp_i16_x100(p: &[u8]) -> Option<i16> {
    if p.len() < 2 {
        return None;
    }
    Some(i16::from_le_bytes([p[0], p[1]]))
}

fn parse_hum_u16_x100(p: &[u8]) -> Option<u16> {
    if p.len() < 2 {
        return None;
    }
    Some(u16::from_le_bytes([p[0], p[1]]))
}

fn dump_bytes(prefix: &str, data: &[u8]) {
    print!("{prefix}");
    for b in data {
        print!(" {:02X}", b);
    }
    println!();
}

pub async fn read_probe_data<C, P, const MAX_SERVICES: usize>(
    gatt: &GattClient<'_, C, P, MAX_SERVICES>,
    service_uuid16: u16,
    temp_uuid16: u16,
    hum_uuid16: u16,
) -> Option<ProbeReading>
where
    C: trouble_host::Controller,
    P: trouble_host::PacketPool,
{
    println!("[GATT] Discover service...");
    let services = match gatt
        .services_by_uuid(&Uuid::new_short(service_uuid16))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            println!("[GATT] services_by_uuid failed: {:?}", e);
            return None;
        }
    };

    let svc = match services.first() {
        Some(s) => s,
        None => {
            println!("[GATT] service not found");
            return None;
        }
    };

    let mut out = ProbeReading {
        temperature_c_x100: None,
        humidity_pct_x100: None,
    };

    let mut buf = [0u8; 32];

    println!("[GATT] Resolve temperature characteristic...");
    match gatt
        .characteristic_by_uuid::<i16>(svc, &Uuid::new_short(temp_uuid16))
        .await
    {
        Ok(temp_chr) => {
            println!("[GATT] Read temperature...");
            match gatt.read_characteristic(&temp_chr, &mut buf).await {
                Ok(n) => {
                    let val = &buf[..n];
                    dump_bytes("[GATT] temp raw:", val);
                    out.temperature_c_x100 = parse_temp_i16_x100(val);
                }
                Err(e) => {
                    println!("[GATT] temperature read failed: {:?}", e);
                }
            }
        }
        Err(e) => {
            println!("[GATT] temperature characteristic resolve failed: {:?}", e);
        }
    }

    println!("[GATT] Resolve humidity characteristic...");
    match gatt
        .characteristic_by_uuid::<u16>(svc, &Uuid::new_short(hum_uuid16))
        .await
    {
        Ok(hum_chr) => {
            println!("[GATT] Read humidity...");
            match gatt.read_characteristic(&hum_chr, &mut buf).await {
                Ok(n) => {
                    let val = &buf[..n];
                    dump_bytes("[GATT] hum raw :", val);
                    out.humidity_pct_x100 = parse_hum_u16_x100(val);
                }
                Err(e) => {
                    println!("[GATT] humidity read failed: {:?}", e);
                }
            }
        }
        Err(e) => {
            println!("[GATT] humidity characteristic resolve failed: {:?}", e);
        }
    }

    Some(out)
}
