#![no_std]

use embassy_time::{Duration, Instant, Timer};

use bt_hci::param::BdAddr;
use embedded_io_async::{Read, Write};
use trouble_host::prelude::AddrKind;

/// Info we can collect from advertising before connecting.
#[derive(Clone, Copy)]
pub struct Picked {
    pub kind: AddrKind,
    pub addr: BdAddr,
    pub rssi: i8,

    /// True if advertiser says it is connectable (from event_type bit0).
    pub connectable: bool,
    /// True if advertiser is scannable (event_type bit1).
    pub scannable: bool,
    /// True if this report is a scan response (event_type bit3).
    pub scan_response: bool,
    /// True if legacy PDU (event_type bit4).
    pub legacy: bool,

    pub flags: Option<u8>,

    // local name (if present)
    pub name: Option<[u8; 32]>,
    pub name_len: u8,

    // manufacturer data (if present)
    pub mfg_company_id: Option<u16>,
    pub mfg_data: [u8; 24],
    pub mfg_len: u8,

    // raw adv payload (truncated)
    pub raw_adv: [u8; 64],
    pub raw_adv_len: u8,
}

impl Picked {
    pub fn name_str(&self) -> Option<&str> {
        let name = self.name.as_ref()?; // Option<&[u8;32]>
        let n = self.name_len as usize;
        if n == 0 {
            return None;
        }
        core::str::from_utf8(&name[..n]).ok()
    }

    pub fn raw_adv_bytes(&self) -> &[u8] {
        let n = self.raw_adv_len as usize;
        &self.raw_adv[..n]
    }

    pub fn mfg_bytes(&self) -> &[u8] {
        let n = self.mfg_len as usize;
        &self.mfg_data[..n]
    }
}

// ---------------------
// ADV parsing helpers
// ---------------------
fn parse_adv_fields(adv: &[u8], out: &mut Picked) {
    // Save raw adv (truncated)
    let copy_len = core::cmp::min(out.raw_adv.len(), adv.len());
    out.raw_adv[..copy_len].copy_from_slice(&adv[..copy_len]);
    out.raw_adv_len = copy_len as u8;

    // AD structure: [len][type][data...]
    let mut i = 0;
    while i + 1 < adv.len() {
        let len = adv[i] as usize;
        if len == 0 || i + 1 + len > adv.len() {
            break;
        }
        let ad_type = adv[i + 1];
        let data = &adv[i + 2..i + 1 + len];

        match ad_type {
            0x01 => {
                // Flags
                if let Some(&b) = data.first() {
                    out.flags = Some(b);
                }
            }
            0x08 | 0x09 => {
                // Shortened/Complete Local Name
                let n = core::cmp::min(32, data.len());
                let mut buf = [0u8; 32];
                buf[..n].copy_from_slice(&data[..n]);
                out.name = Some(buf);
                out.name_len = n as u8;
            }
            0xFF => {
                // Manufacturer Specific Data: [company_id LE][payload...]
                if data.len() >= 2 {
                    out.mfg_company_id = Some(u16::from_le_bytes([data[0], data[1]]));
                    let payload = &data[2..];
                    let n = core::cmp::min(out.mfg_data.len(), payload.len());
                    out.mfg_data[..n].copy_from_slice(&payload[..n]);
                    out.mfg_len = n as u8;
                }
            }
            _ => {}
        }

        i += 1 + len;
    }
}

// ---------------------
// Raw HCI helpers
// ---------------------
async fn hci_send<W: Write>(w: &mut W, ogf: u16, ocf: u16, params: &[u8]) {
    // HCI Command packet:
    // [0x01][opcode L][opcode H][param_len][params...]
    let opcode: u16 = (ogf << 10) | ocf;
    let mut hdr = [0u8; 4];
    hdr[0] = 0x01;
    hdr[1] = (opcode & 0xFF) as u8;
    hdr[2] = (opcode >> 8) as u8;
    hdr[3] = params.len() as u8;

    let _ = w.write_all(&hdr).await;
    if !params.is_empty() {
        let _ = w.write_all(params).await;
    }
}

async fn hci_read_packet<R: Read>(r: &mut R, buf: &mut [u8]) -> usize {
    match r.read(buf).await {
        Ok(n) => n,
        Err(_) => 0,
    }
}

fn addr_kind_from_hci(addr_type: u8) -> AddrKind {
    // HCI extended adv report "address type":
    // 0 = public, 1 = random, others exist but these two are the common ones.
    if addr_type == 0 {
        AddrKind::PUBLIC
    } else {
        AddrKind::RANDOM
    }
}

/// Scan for `scan_time`, print every advertiser, and return the strongest *connectable* device.
/// This prevents the "Connecting… forever" problem when the strongest advertiser is not connectable.
///
/// `connector` must implement embedded_io_async Read/Write (BleConnector does).
pub async fn hci_scan_pick_strongest_connectable<C>(
    connector: &mut C,
    scan_time: Duration,
) -> Option<Picked>
where
    C: Read + Write,
{
    // Reset + enable event masks
    hci_send(connector, 0x03, 0x0003, &[]).await; // Reset
    hci_send(connector, 0x03, 0x0001, &[0xFF; 8]).await; // Set Event Mask
    hci_send(connector, 0x08, 0x0001, &[0xFF; 8]).await; // LE Set Event Mask

    // Extended scan params (active, 1M PHY, accept all)
    let interval: u16 = 160; // 100ms (160 * 0.625ms)
    let window: u16 = 80; // 50ms  (80  * 0.625ms)
    let params = [
        0x00, // own_address_type: public
        0x00, // filter policy: accept all
        0x01, // scanning_phys: 1M
        0x01, // scan_type: active
        (interval & 0xFF) as u8,
        (interval >> 8) as u8,
        (window & 0xFF) as u8,
        (window >> 8) as u8,
    ];
    hci_send(connector, 0x08, 0x0041, &params).await; // LE Set Extended Scan Parameters

    // Enable extended scan
    let params = [0x01, 0x01, 0x00, 0x00, 0x00, 0x00]; // enable=1, dup=1, duration=0
    hci_send(connector, 0x08, 0x0042, &params).await; // LE Set Extended Scan Enable

    esp_println::println!("Scanning {:?}… (printing all devices)", scan_time);

    let t_end = Instant::now() + scan_time;
    let mut best: Option<Picked> = None;

    let mut buf = [0u8; 512];

    while Instant::now() < t_end {
        let n = hci_read_packet(connector, &mut buf).await;
        if n < 3 {
            Timer::after(Duration::from_millis(10)).await;
            continue;
        }

        // HCI Event packet: [0x04][event_code][param_len][params...]
        if buf[0] != 0x04 {
            continue;
        }
        let event_code = buf[1];
        let param_len = buf[2] as usize;
        if 3 + param_len > n {
            continue;
        }
        let params = &buf[3..3 + param_len];

        // LE Meta Event
        if event_code != 0x3E || params.is_empty() {
            continue;
        }
        let subevent = params[0];

        // Extended Advertising Report subevent = 0x0D
        if subevent != 0x0D {
            continue;
        }

        let mut idx = 1;
        if idx >= params.len() {
            continue;
        }
        let reports = params[idx] as usize;
        idx += 1;

        for _ in 0..reports {
            // Need at least the fixed header bytes
            if idx + 2 + 1 + 6 + 1 + 1 + 1 + 1 + 1 + 2 + 1 + 6 + 1 > params.len() {
                break;
            }

            // event_type (u16 LE)
            let event_type = u16::from_le_bytes([params[idx], params[idx + 1]]);
            idx += 2;

            let connectable = (event_type & 0x0001) != 0;
            let scannable = (event_type & 0x0002) != 0;
            let _directed = (event_type & 0x0004) != 0;
            let scan_response = (event_type & 0x0008) != 0;
            let legacy = (event_type & 0x0010) != 0;

            let addr_type = params[idx];
            idx += 1;

            let mut addr = [0u8; 6];
            addr.copy_from_slice(&params[idx..idx + 6]);
            idx += 6;

            idx += 1; // primary_phy
            idx += 1; // secondary_phy
            idx += 1; // adv_sid
            idx += 1; // tx_power

            let rssi = params[idx] as i8;
            idx += 1;

            idx += 2; // periodic_adv_interval
            idx += 1; // direct_address_type
            idx += 6; // direct_address

            let data_len = params[idx] as usize;
            idx += 1;
            if idx + data_len > params.len() {
                break;
            }
            let data = &params[idx..idx + data_len];
            idx += data_len;

            let kind = addr_kind_from_hci(addr_type);
            let bd = BdAddr::new(addr);

            let mut p = Picked {
                kind,
                addr: bd,
                rssi,
                connectable,
                scannable,
                scan_response,
                legacy,
                flags: None,
                name: None,
                name_len: 0,
                mfg_company_id: None,
                mfg_data: [0; 24],
                mfg_len: 0,
                raw_adv: [0; 64],
                raw_adv_len: 0,
            };

            parse_adv_fields(data, &mut p);

            // Print everything we have (name only if present)
            if let Some(name) = p.name_str() {
                esp_println::println!(
                    "ADV: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X} kind={:?} rssi={} conn={} scan_rsp={} legacy={} name={}",
                    addr[0], addr[1], addr[2], addr[3], addr[4], addr[5],
                    p.kind, p.rssi, p.connectable, p.scan_response, p.legacy, name
                );
            } else {
                esp_println::println!(
                    "ADV: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X} kind={:?} rssi={} conn={} scan_rsp={} legacy={}",
                    addr[0], addr[1], addr[2], addr[3], addr[4], addr[5],
                    p.kind, p.rssi, p.connectable, p.scan_response, p.legacy
                );
            }

            // Only pick connectable devices (fixes the infinite "Connecting…")
            if !p.connectable {
                continue;
            }

            match best {
                None => best = Some(p),
                Some(cur) if p.rssi > cur.rssi => best = Some(p),
                _ => {}
            }
        }
    }

    // Disable scan
    let params = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    hci_send(connector, 0x08, 0x0042, &params).await;

    best
}