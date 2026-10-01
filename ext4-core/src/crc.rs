//! The two checksums the format uses: CRC-32C for `metadata_csum` and jbd2,
//! CRC-16 for the older `gdt_csum` group descriptors.
//!
//! Both are the raw reflected update with no inversion on either side: ext4
//! seeds with `!0` where it wants an initial inversion and stores the running
//! value as the checksum, so the caller owns both ends.

const fn crc32c_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0x82F6_3B78 ^ (c >> 1)
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

static CRC32C: [[u32; 256]; 8] = crc32c_tables();

/// Fold `data` into the running CRC-32C `state` (Castagnoli, reflected).
pub fn crc32c(mut state: u32, data: &[u8]) -> u32 {
    let t = &CRC32C;
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
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
    for &b in rest {
        state = (state >> 8) ^ t[0][((state ^ u32::from(b)) & 0xFF) as usize];
    }
    state
}

const CRC32C_POLY: u32 = 0x82F6_3B78;

/// `x^0` in the reflected representation.
const ONE: u32 = 1 << 31;

/// `a * b` modulo the polynomial, both reflected.
const fn mul_mod_poly(a: u32, mut b: u32) -> u32 {
    let mut product = 0;
    let mut m = ONE;
    while m != 0 {
        if a & m != 0 {
            product ^= b;
        }
        m >>= 1;
        b = if b & 1 != 0 {
            (b >> 1) ^ CRC32C_POLY
        } else {
            b >> 1
        };
    }
    product
}

/// `x^(8 * 2^k)` for each `k`: appending `2^k` zero bytes.
const ZERO_BYTE_POWERS: [u32; 64] = {
    let mut powers = [0u32; 64];
    let mut x8 = ONE >> 8;
    let mut k = 0;
    while k < 64 {
        powers[k] = x8;
        x8 = mul_mod_poly(x8, x8);
        k += 1;
    }
    powers
};

/// Fold `count` zero bytes into `state`, in time logarithmic in `count`.
pub fn crc32c_zeros(state: u32, mut count: usize) -> u32 {
    let mut shift = ONE;
    let mut k = 0;
    while count != 0 {
        if count & 1 != 0 {
            shift = mul_mod_poly(ZERO_BYTE_POWERS[k], shift);
        }
        count >>= 1;
        k += 1;
    }
    mul_mod_poly(shift, state)
}

/// The change in the CRC-32C of `len` bytes when byte `at` is XORed with
/// `delta`, which the CRC's linearity makes independent of the other bytes.
/// Zero for a byte past `len`.
pub fn crc32c_byte_delta(at: usize, delta: u8, len: usize) -> u32 {
    match len.checked_sub(at + 1) {
        Some(after) => crc32c_zeros(CRC32C[0][usize::from(delta)], after),
        None => 0,
    }
}

const fn crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xA001 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC16: [u16; 256] = crc16_table();

/// Fold `data` into the running CRC-16 `state` (polynomial 0x8005, reflected).
pub fn crc16(mut state: u16, data: &[u8]) -> u16 {
    for &b in data {
        state = (state >> 8) ^ CRC16[((state ^ u16::from(b)) & 0xFF) as usize];
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_the_catalogue_check_value() {
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn crc32c_is_incremental() {
        let data = b"the quick brown fox jumps over the lazy dog, twice over";
        let whole = crc32c(!0, data);
        for split in 0..data.len() {
            let (a, b) = data.split_at(split);
            assert_eq!(crc32c(crc32c(!0, a), b), whole);
        }
    }

    #[test]
    fn zeros_match_a_zero_buffer() {
        let state = crc32c(!0, b"prefix");
        for count in [0, 1, 7, 8, 9, 255, 4095, 4096] {
            assert_eq!(
                crc32c_zeros(state, count),
                crc32c(state, &std::vec![0u8; count])
            );
        }
    }

    #[test]
    fn a_byte_delta_is_the_checksum_change() {
        let mut data = [0u8; 300];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i * 37 % 251) as u8;
        }
        let seed = 0x1234_5678;
        for (at, delta) in [(0, 1u8), (17, 0x80), (299, 0x10), (150, 0xFF)] {
            let before = crc32c(seed, &data);
            data[at] ^= delta;
            assert_eq!(
                before ^ crc32c_byte_delta(at, delta, data.len()),
                crc32c(seed, &data)
            );
        }
    }

    #[test]
    fn crc16_is_crc16_arc() {
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }
}
