use crate::ble::adv::AdvData;
use alloc::vec::Vec;
use bt_hci::param::{AddrKind, BdAddr};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read, Write};

async fn hci_send<W: Write>(w: &mut W, ogf: u16, ocf: u16, params: &[u8]) -> Result<(), ()> {
    let opcode: u16 = (ogf << 10) | ocf;

    let mut hdr = [0u8; 4];
    hdr[0] = 0x01;
    hdr[1] = (opcode & 0xFF) as u8;
    hdr[2] = (opcode >> 8) as u8;
    hdr[3] = params.len() as u8;

    w.write_all(&hdr).await.map_err(|_| ())?;
    if !params.is_empty() {
        w.write_all(params).await.map_err(|_| ())?;
    }
    Ok(())
}

async fn hci_read<R: Read>(r: &mut R, buf: &mut [u8]) -> usize {
    r.read(buf).await.unwrap_or(0)
}

fn addr_kind_from_hci(addr_type: u8) -> AddrKind {
    if addr_type == 0 {
        AddrKind::PUBLIC
    } else {
        AddrKind::RANDOM
    }
}

async fn hci_scan_start<C: Read + Write>(connector: &mut C) -> Result<(), ()> {
    hci_send(connector, 0x03, 0x0003, &[]).await?;
    hci_send(connector, 0x03, 0x0001, &[0xFF; 8]).await?;
    hci_send(connector, 0x08, 0x0001, &[0xFF; 8]).await?;

    let interval: u16 = 160; // 100 ms
    let window: u16 = 80; // 50 ms

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

    hci_send(connector, 0x08, 0x0041, &params).await?;

    // enable scan, filter duplicates
    let enable = [0x01, 0x01, 0x00, 0x00, 0x00, 0x00];
    hci_send(connector, 0x08, 0x0042, &enable).await?;

    Ok(())
}

async fn hci_scan_stop<C: Read + Write>(connector: &mut C) -> Result<(), ()> {
    let disable = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    hci_send(connector, 0x08, 0x0042, &disable).await
}

fn parse_next_ext_adv_report(buf: &[u8]) -> Option<AdvData> {
    if buf.len() < 3 || buf[0] != 0x04 {
        return None;
    }

    let event_code = buf[1];
    let param_len = buf[2] as usize;
    if buf.len() < 3 + param_len {
        return None;
    }

    let params = &buf[3..3 + param_len];

    if event_code != 0x3E || params.is_empty() {
        return None;
    }

    // Extended Advertising Report subevent
    if params[0] != 0x0D {
        return None;
    }

    let mut idx = 1;
    let reports = params[idx] as usize;
    idx += 1;
    if reports == 0 {
        return None;
    }

    // fixed fields for one report
    if idx + 2 + 1 + 6 + 4 + 1 + 2 + 1 + 6 + 1 > params.len() {
        return None;
    }

    let event_type = u16::from_le_bytes([params[idx], params[idx + 1]]);
    idx += 2;

    let connectable = (event_type & 0x0001) != 0;
    let scannable = (event_type & 0x0002) != 0;
    let scan_rsp_bit = (event_type & 0x0008) != 0;
    let legacy = (event_type & 0x0010) != 0;

    let addr_type = params[idx];
    idx += 1;

    let mut addr = [0u8; 6];
    addr.copy_from_slice(&params[idx..idx + 6]);
    idx += 6;

    // primary_phy, secondary_phy, adv_sid, tx_power
    idx += 4;

    let rssi = params[idx] as i8;
    idx += 1;

    // periodic_adv_interval
    idx += 2;
    // direct_address_type + direct_address
    idx += 1 + 6;

    let data_len = params[idx] as usize;
    idx += 1;
    if idx + data_len > params.len() {
        return None;
    }

    let data = &params[idx..idx + data_len];

    let kind = addr_kind_from_hci(addr_type);
    let bd = BdAddr::new(addr);

    let mut adv = AdvData {
        kind,
        addr: bd,
        rssi,
        connectable,
        scannable,
        scan_response: scan_rsp_bit,
        legacy,

        flags: None,

        name: None,
        name_len: 0,

        mfg_company_id: None,
        mfg_data: [0; 24],
        mfg_len: 0,

        probe_name: None,
        probe_name_len: 0,
        probe_ver_major: None,
        probe_ver_minor: None,

        uuids128: [[0; 16]; 4],
        uuids128_len: 0,

        raw_adv: [0; 64],
        raw_adv_len: 0,

        raw_scan_rsp: [0; 64],
        raw_scan_rsp_len: 0,
    };

    // IMPORTANT: parse into the proper raw buffer
    adv.parse_ad_payload(data, scan_rsp_bit);

    Some(adv)
}

async fn hci_scan_next<C: Read + Write>(connector: &mut C, buf: &mut [u8]) -> Option<AdvData> {
    loop {
        let n = hci_read(connector, buf).await;
        if n < 3 {
            Timer::after(Duration::from_millis(10)).await;
            continue;
        }
        if let Some(p) = parse_next_ext_adv_report(&buf[..n]) {
            return Some(p);
        }
    }
}

pub async fn hci_scan_probes<C: Read + Write>(
    connector: &mut C,
    scan_time: Duration,
) -> Vec<AdvData> {
    let _ = hci_scan_start(connector).await;

    let end = Instant::now() + scan_time;
    let mut buf = [0u8; 512];

    let mut probes: Vec<AdvData> = Vec::new();

    while Instant::now() < end {
        if let Some(p) = hci_scan_next(connector, &mut buf).await {
            // Merge everything by address first (ADV + SCAN_RSP become one entry)
            if let Some(existing) = probes.iter_mut().find(|e| e.addr == p.addr) {
                existing.merge_from(&p);
                continue;
            }

            // only store if it looks like one of our probes
            if !p.is_probe() {
                continue;
            }

            probes.push(p);
        }
    }

    let _ = hci_scan_stop(connector).await;
    probes
}
