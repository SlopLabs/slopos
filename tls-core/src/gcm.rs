//! AES-GCM (NIST SP 800-38D) with a 96-bit nonce and a 128-bit tag. GHASH
//! multiplies integers whose operands keep one bit in five, so no carry reaches
//! a kept bit of the product and no table is indexed by the hash key.

use crate::aes::{Aes, BLOCK, LANES};

pub const TAG: usize = 16;
/// The most one nonce may protect: 2^32 - 2 blocks (SP 800-38D §5.2.1.1).
pub const MAX_DATA: u64 = ((1 << 32) - 2) * BLOCK as u64;
pub const NONCE: usize = 12;

fn clmul64(x: u64, y: u64) -> u128 {
    const M64: [u64; 5] = [
        0x1084_2108_4210_8421,
        0x2108_4210_8421_0842,
        0x4210_8421_0842_1084,
        0x8421_0842_1084_2108,
        0x0842_1084_2108_4210,
    ];
    const M128: [u128; 5] = [
        0x2108_4210_8421_0842_1084_2108_4210_8421,
        0x4210_8421_0842_1084_2108_4210_8421_0842,
        0x8421_0842_1084_2108_4210_8421_0842_1084,
        0x0842_1084_2108_4210_8421_0842_1084_2108,
        0x1084_2108_4210_8421_0842_1084_2108_4210,
    ];
    let xs = M64.map(|m| (x & m) as u128);
    let ys = M64.map(|m| (y & m) as u128);
    let mut out = 0u128;
    for (r, mask) in M128.iter().enumerate() {
        let mut z = 0u128;
        for (i, xi) in xs.iter().enumerate() {
            z ^= xi * ys[(r + 5 - i) % 5];
        }
        out |= z & mask;
    }
    out
}

/// Multiplication in GF(2^128) with GCM's bit order, both operands and the
/// result loaded big-endian.
fn gf_mul(a: u128, b: u128) -> u128 {
    let (a1, a0) = ((a >> 64) as u64, a as u64);
    let (b1, b0) = ((b >> 64) as u64, b as u64);
    let lo = clmul64(a0, b0);
    let hi = clmul64(a1, b1);
    let mid = clmul64(a0 ^ a1, b0 ^ b1) ^ lo ^ hi;
    let high = hi ^ (mid >> 64);
    let low = lo ^ (mid << 64);
    let zh = (high << 1) | (low >> 127);
    let zl = low << 1;
    let spill = (zl << 127) ^ (zl << 126) ^ (zl << 121);
    zh ^ zl ^ (zl >> 1) ^ (zl >> 2) ^ (zl >> 7) ^ spill ^ (spill >> 1) ^ (spill >> 2) ^ (spill >> 7)
}

struct Ghash {
    h: u128,
    acc: u128,
}

impl Drop for Ghash {
    fn drop(&mut self) {
        crate::ct::wipe(core::slice::from_mut(&mut self.h));
    }
}

impl Ghash {
    fn update(&mut self, data: &[u8]) {
        for chunk in data.chunks(BLOCK) {
            let mut block = [0u8; BLOCK];
            block[..chunk.len()].copy_from_slice(chunk);
            self.acc = gf_mul(self.acc ^ u128::from_be_bytes(block), self.h);
        }
    }
}

#[derive(Clone)]
pub struct AesGcm {
    aes: Aes,
    h: u128,
}

impl Drop for AesGcm {
    fn drop(&mut self) {
        crate::ct::wipe(core::slice::from_mut(&mut self.h));
    }
}

impl AesGcm {
    pub fn new(key: &[u8]) -> Option<Self> {
        let aes = Aes::new(key)?;
        let mut h = [0u8; BLOCK];
        aes.encrypt_block(&mut h);
        let gcm = Self {
            aes,
            h: u128::from_be_bytes(h),
        };
        crate::ct::wipe(&mut h);
        Some(gcm)
    }

    /// XOR `data` with the keystream starting at counter block `first`.
    fn ctr(&self, nonce: &[u8; NONCE], first: u32, data: &mut [u8]) {
        let mut counter = first;
        for group in data.chunks_mut(BLOCK * LANES) {
            let mut ks = [0u8; BLOCK * LANES];
            for lane in ks.chunks_exact_mut(BLOCK) {
                lane[..NONCE].copy_from_slice(nonce);
                lane[NONCE..].copy_from_slice(&counter.to_be_bytes());
                counter = counter.wrapping_add(1);
            }
            self.aes.encrypt_lanes(&mut ks);
            for (d, k) in group.iter_mut().zip(ks) {
                *d ^= k;
            }
        }
    }

    fn tag(&self, nonce: &[u8; NONCE], aad: &[u8], ciphertext: &[u8]) -> [u8; TAG] {
        let mut g = Ghash { h: self.h, acc: 0 };
        g.update(aad);
        g.update(ciphertext);
        let lengths = ((aad.len() as u128 * 8) << 64) | (ciphertext.len() as u128 * 8);
        g.update(&lengths.to_be_bytes());
        let mut tag = g.acc.to_be_bytes();
        self.ctr(nonce, 1, &mut tag);
        tag
    }

    /// Encrypt `data` in place and return the tag.
    ///
    /// # Panics
    ///
    /// Past [`MAX_DATA`], where the block counter would wrap into the tag's.
    pub fn seal(&self, nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8]) -> [u8; TAG] {
        assert!(data.len() as u64 <= MAX_DATA, "AES-GCM input too long");
        self.ctr(nonce, 2, data);
        self.tag(nonce, aad, data)
    }

    /// Check the tag, then decrypt `data` in place. On a bad tag `data` is
    /// left as it was.
    pub fn open(&self, nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8], tag: &[u8]) -> bool {
        if data.len() as u64 > MAX_DATA || !crate::ct::eq(&self.tag(nonce, aad, data), tag) {
            return false;
        }
        self.ctr(nonce, 2, data);
        true
    }
}

#[cfg(test)]
pub(crate) fn gf_mul_for_test(a: u128, b: u128) -> u128 {
    gf_mul(a, b)
}

#[cfg(test)]
pub(crate) fn clmul64_for_test(x: u64, y: u64) -> u128 {
    clmul64(x, y)
}
