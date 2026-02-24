pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const IPV4_BROADCAST: [u8; 4] = [255, 255, 255, 255];
pub const IPPROTO_UDP: u8 = 17;

pub fn header_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < header.len() {
        let word = u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        sum = sum.wrapping_add(word);
        i += 2;
    }

    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }

    !(sum as u16)
}
