//! CRC-32 as IEEE 802.3, zlib and the UEFI specification define it: reflected,
//! polynomial `0xEDB88320`, seeded and finished with all ones.

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        tables[0][i] = c;
        i += 1;
    }
    let mut t = 1usize;
    while t < 8 {
        let mut i = 0usize;
        while i < 256 {
            let prev = tables[t - 1][i];
            tables[t][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        t += 1;
    }
    tables
}

/// Slicing-by-8: `TABLES[k][b]` is byte `b`'s contribution followed by `k`
/// zero bytes, so eight bytes fold in with eight independent lookups.
static TABLES: [[u32; 256]; 8] = build_tables();

/// A running state before any byte is fed.
pub const INIT: u32 = 0xFFFF_FFFF;

pub fn feed(mut state: u32, data: &[u8]) -> u32 {
    let t = &TABLES;
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let lo = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ state;
        let hi = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
        state = t[7][(lo & 0xFF) as usize]
            ^ t[6][((lo >> 8) & 0xFF) as usize]
            ^ t[5][((lo >> 16) & 0xFF) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xFF) as usize]
            ^ t[2][((hi >> 8) & 0xFF) as usize]
            ^ t[1][((hi >> 16) & 0xFF) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &b in words.remainder() {
        state = (state >> 8) ^ t[0][((state ^ b as u32) & 0xFF) as usize];
    }
    state
}

pub const fn finish(state: u32) -> u32 {
    state ^ 0xFFFF_FFFF
}

/// `crc32(&[]) == 0`, as zlib's.
pub fn crc32(data: &[u8]) -> u32 {
    finish(feed(INIT, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_value_matches_the_catalogue() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn feeding_in_pieces_is_feeding_whole() {
        let data: std::vec::Vec<u8> = (0..1027u32).map(|i| (i * 7 + 3) as u8).collect();
        assert_eq!(crc32(&data), 0x02AD_D968);
        let mut state = INIT;
        for piece in data.chunks(13) {
            state = feed(state, piece);
        }
        assert_eq!(finish(state), crc32(&data));
    }
}
