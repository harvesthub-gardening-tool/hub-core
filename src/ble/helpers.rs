pub fn uuid16_to_uuid128(u: u16) -> [u8; 16] {
    // Standard Bluetooth Base UUID:
    // 0000xxxx-0000-1000-8000-00805F9B34FB
    let mut out = [
        0xFB, 0x34, 0x9B, 0x5F, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];

    // Put 16-bit UUID into bytes 12..14 in *Bluetooth UUID byte order* (big-end in string form).
    out[12] = (u & 0xFF) as u8;
    out[13] = (u >> 8) as u8;

    out
}
