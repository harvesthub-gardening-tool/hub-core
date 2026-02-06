use crate::hexdump;
use bt_hci::param::{AddrKind, BdAddr};

use crate::config::{COMPANY_ID, COMPANY_NAME, MAGIC_MARKER};

/// Advertising data we collect during scanning (no heap, fixed buffers).
#[derive(Clone, Copy)]
pub struct AdvData {
    pub kind: AddrKind,
    pub addr: BdAddr,
    pub rssi: i8,

    pub connectable: bool,
    pub scannable: bool,
    pub scan_response: bool,
    pub legacy: bool,

    pub flags: Option<u8>,

    // Local name (from AD types 0x08/0x09). Often in scan response.
    pub name: Option<[u8; 32]>,
    pub name_len: u8,

    // Manufacturer data (payload without company_id)
    pub mfg_company_id: Option<u16>,
    pub mfg_data: [u8; 24],
    pub mfg_len: u8,

    // Parsed "probe identity" from mfg marker (optional)
    pub probe_name: Option<[u8; 32]>,
    pub probe_name_len: u8,
    pub probe_ver_major: Option<u8>,
    pub probe_ver_minor: Option<u8>,

    // Service UUIDs (if present)
    pub uuids128: [[u8; 16]; 4],
    pub uuids128_len: u8,

    // Raw payloads, kept separate for clarity
    pub raw_adv: [u8; 64],
    pub raw_adv_len: u8,

    pub raw_scan_rsp: [u8; 64],
    pub raw_scan_rsp_len: u8,
}

impl AdvData {
    pub fn name_str(&self) -> Option<&str> {
        let name = self.name.as_ref()?;
        let n = self.name_len as usize;
        if n == 0 {
            return None;
        }
        core::str::from_utf8(&name[..n]).ok()
    }

    pub fn probe_name_str(&self) -> Option<&str> {
        let name = self.probe_name.as_ref()?;
        let n = self.probe_name_len as usize;
        if n == 0 {
            return None;
        }
        core::str::from_utf8(&name[..n]).ok()
    }

    pub fn mfg_bytes(&self) -> &[u8] {
        let n = self.mfg_len as usize;
        &self.mfg_data[..n]
    }

    pub fn raw_adv_bytes(&self) -> &[u8] {
        let n = self.raw_adv_len as usize;
        &self.raw_adv[..n]
    }

    pub fn raw_scan_rsp_bytes(&self) -> &[u8] {
        let n = self.raw_scan_rsp_len as usize;
        &self.raw_scan_rsp[..n]
    }

    /// Merge fields from another packet for the same device (ADV + SCAN_RSP).
    pub fn merge_from(&mut self, other: &AdvData) {
        // Keep strongest RSSI (closest)
        if other.rssi > self.rssi {
            self.rssi = other.rssi;
        }

        // Any true flags stick
        self.connectable |= other.connectable;
        self.scannable |= other.scannable;
        self.legacy |= other.legacy;

        // scan_response is "did we see a scan response packet"
        self.scan_response |= other.scan_response;

        // Prefer already-set, but fill missing fields
        if self.flags.is_none() {
            self.flags = other.flags;
        }

        if self.name.is_none() && other.name.is_some() {
            self.name = other.name;
            self.name_len = other.name_len;
        }

        if self.mfg_company_id.is_none() && other.mfg_company_id.is_some() {
            self.mfg_company_id = other.mfg_company_id;
            self.mfg_data = other.mfg_data;
            self.mfg_len = other.mfg_len;
        }

        if self.probe_name.is_none() && other.probe_name.is_some() {
            self.probe_name = other.probe_name;
            self.probe_name_len = other.probe_name_len;
        }
        if self.probe_ver_major.is_none() {
            self.probe_ver_major = other.probe_ver_major;
        }
        if self.probe_ver_minor.is_none() {
            self.probe_ver_minor = other.probe_ver_minor;
        }

        // Merge UUIDs (avoid duplicates)
        let mut i = 0usize;
        while i < other.uuids128_len as usize {
            let uuid = other.uuids128[i];
            if !self.has_uuid128_le(&uuid) {
                let n = self.uuids128_len as usize;
                if n < self.uuids128.len() {
                    self.uuids128[n] = uuid;
                    self.uuids128_len = (n + 1) as u8;
                }
            }
            i += 1;
        }

        // Keep both raw buffers if present
        if self.raw_adv_len == 0 && other.raw_adv_len > 0 {
            self.raw_adv = other.raw_adv;
            self.raw_adv_len = other.raw_adv_len;
        }
        if self.raw_scan_rsp_len == 0 && other.raw_scan_rsp_len > 0 {
            self.raw_scan_rsp = other.raw_scan_rsp;
            self.raw_scan_rsp_len = other.raw_scan_rsp_len;
        }
    }

    pub fn print(&self) {
        esp_println::println!(
            "\t- rssi={}\n\t- kind={:?}\n\t- connectable={}\n\t- scan_response={}\n\t- legacy={}",
            self.rssi,
            self.kind,
            self.connectable,
            self.scan_response,
            self.legacy
        );

        if let Some(flags) = self.flags {
            esp_println::println!("\t- flags: {}", Self::fmt_flags(flags));
        }

        if let Some(name) = self.name_str() {
            esp_println::println!("\t- name: {}", name);
        }

        // Company name + mfg marker info
        if let Some(cid) = self.mfg_company_id {
            let cname = Self::company_name(cid);
            match cname {
                Some(n) => esp_println::println!(
                    "\t- mfg: \n\t   - company_id=0x{:04X} ({}) len={}",
                    cid,
                    n,
                    self.mfg_len
                ),
                None => esp_println::println!(
                    "\t- mfg: \n\t   - company_id=0x{:04X} len={}",
                    cid,
                    self.mfg_len
                ),
            }

            if self.mfg_len > 0 {
                hexdump("\t   - data:", self.mfg_bytes());
            }

            if self.is_probe() {
                if let (Some(maj), Some(min)) = (self.probe_ver_major, self.probe_ver_minor) {
                    esp_println::println!("\t- version: {}.{}", maj, min);
                }

                if let Some(pn) = self.probe_name_str() {
                    esp_println::println!("\t- probe_name: {}", pn);
                }
            }
        }

        if self.uuids128_len > 0 {
            esp_println::println!("\t- uuids128 ({}):", self.uuids128_len);
            let n = self.uuids128_len as usize;
            for u in &self.uuids128[..n] {
                esp_println::println!("\t   - {}", Self::uuid128_le_to_str(u));
            }
        }

        // Show both raw payloads separately (clear!)
        if self.raw_adv_len > 0 {
            esp_println::println!("\t- raw_adv: len={}", self.raw_adv_len);
            hexdump("\t   - adv:", self.raw_adv_bytes());
        }
        if self.raw_scan_rsp_len > 0 {
            esp_println::println!("\traw_scan_rsp: len={}", self.raw_scan_rsp_len);
            hexdump("\t   - rsp:", self.raw_scan_rsp_bytes());
        }
    }

    /// Parse AD fields from a payload (either ADV payload or SCAN_RSP payload).
    /// Also saves into raw_adv/raw_scan_rsp depending on `is_scan_rsp`.
    pub fn parse_ad_payload(&mut self, payload: &[u8], is_scan_rsp: bool) {
        // save raw payload (truncated) into the right buffer
        if is_scan_rsp {
            let copy_len = core::cmp::min(self.raw_scan_rsp.len(), payload.len());
            self.raw_scan_rsp[..copy_len].copy_from_slice(&payload[..copy_len]);
            self.raw_scan_rsp_len = copy_len as u8;
        } else {
            let copy_len = core::cmp::min(self.raw_adv.len(), payload.len());
            self.raw_adv[..copy_len].copy_from_slice(&payload[..copy_len]);
            self.raw_adv_len = copy_len as u8;
        }

        // Parse AD structures: [len][type][data...]
        let mut i = 0usize;
        while i + 1 < payload.len() {
            let len = payload[i] as usize;
            if len == 0 || i + 1 + len > payload.len() {
                break;
            }

            let ad_type = payload[i + 1];
            let data = &payload[i + 2..i + 1 + len];

            match ad_type {
                0x01 => {
                    if let Some(&b) = data.first() {
                        self.flags = Some(b);
                    }
                }

                0x08 | 0x09 => {
                    // Local name
                    let n = core::cmp::min(32, data.len());
                    let mut buf = [0u8; 32];
                    buf[..n].copy_from_slice(&data[..n]);
                    self.name = Some(buf);
                    self.name_len = n as u8;
                }

                0xFF => {
                    // Manufacturer: [company_id LE][payload...]
                    if data.len() >= 2 {
                        self.mfg_company_id = Some(u16::from_le_bytes([data[0], data[1]]));
                        let payload2 = &data[2..];
                        let n = core::cmp::min(self.mfg_data.len(), payload2.len());
                        self.mfg_data[..n].copy_from_slice(&payload2[..n]);
                        self.mfg_len = n as u8;

                        // Try parse our marker format
                        self.try_parse_probe_marker();
                    }
                }

                0x06 | 0x07 => {
                    // 128-bit UUID list
                    let mut j = 0usize;
                    while j + 15 < data.len() {
                        let mut uuid = [0u8; 16];
                        uuid.copy_from_slice(&data[j..j + 16]);
                        j += 16;

                        if self.has_uuid128_le(&uuid) {
                            continue;
                        }
                        let n = self.uuids128_len as usize;
                        if n < self.uuids128.len() {
                            self.uuids128[n] = uuid;
                            self.uuids128_len = (n + 1) as u8;
                        } else {
                            break;
                        }
                    }
                }

                _ => {}
            }

            i += 1 + len;
        }
    }

    /// Probe recognition (manufacturer-only).
    pub fn is_probe(&self) -> bool {
        self.mfg_company_id == Some(COMPANY_ID)
            && (self.mfg_len as usize) >= MAGIC_MARKER.len()
            && &self.mfg_data[..MAGIC_MARKER.len()] == MAGIC_MARKER
    }

    /// Parse: magic_marker ver_major ver_minor name_len name_bytes...
    fn try_parse_probe_marker(&mut self) {
        self.probe_name = None;
        self.probe_name_len = 0;
        self.probe_ver_major = None;
        self.probe_ver_minor = None;

        let m = &MAGIC_MARKER;
        let total = self.mfg_len as usize;

        // Need at least MAGIC + 3 bytes (maj,min,name_len)
        if total < m.len() + 3 {
            return;
        }

        // Check marker
        if &self.mfg_data[..m.len()] != m {
            return;
        }

        let off = m.len();
        let maj = self.mfg_data[off];
        let min = self.mfg_data[off + 1];
        let name_len = self.mfg_data[off + 2] as usize;

        self.probe_ver_major = Some(maj);
        self.probe_ver_minor = Some(min);

        if name_len == 0 {
            return;
        }

        let avail = total.saturating_sub(off + 3);
        let n = core::cmp::min(32, core::cmp::min(name_len, avail));
        if n == 0 {
            return;
        }

        let mut buf = [0u8; 32];
        buf[..n].copy_from_slice(&self.mfg_data[off + 3..off + 3 + n]);
        self.probe_name = Some(buf);
        self.probe_name_len = n as u8;
    }

    fn has_uuid128_le(&self, uuid_le: &[u8; 16]) -> bool {
        let n = self.uuids128_len as usize;
        self.uuids128[..n].iter().any(|u| u == uuid_le)
    }

    fn uuid128_le_to_str(uuid_le: &[u8; 16]) -> heapless::String<36> {
        let b = uuid_le;
        let mut s: heapless::String<36> = heapless::String::new();
        use core::fmt::Write;

        let _ = write!(
            s,
            "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
            b[3], b[2], b[1], b[0],
            b[5], b[4],
            b[7], b[6],
            b[8], b[9],
            b[10], b[11], b[12], b[13], b[14], b[15],
        );
        s
    }

    fn fmt_flags(flags: u8) -> heapless::String<64> {
        use core::fmt::Write;
        let mut s: heapless::String<64> = heapless::String::new();
        let _ = write!(s, "0x{:02X}", flags);

        if flags & 0x01 != 0 {
            let _ = write!(s, " LE_LimitedDisc");
        }
        if flags & 0x02 != 0 {
            let _ = write!(s, " LE_GeneralDisc");
        }
        if flags & 0x04 != 0 {
            let _ = write!(s, " BR_EDR_NotSupported");
        }
        if flags & 0x08 != 0 {
            let _ = write!(s, " Simul_LE_BR_EDR_Ctrl");
        }
        if flags & 0x10 != 0 {
            let _ = write!(s, " Simul_LE_BR_EDR_Host");
        }

        s
    }

    fn company_name(cid: u16) -> Option<&'static str> {
        if cid == COMPANY_ID {
            Some(COMPANY_NAME)
        } else {
            None
        }
    }
}
