//! ChaCha20-Poly1305 (RFC 8439).

pub const KEY: usize = 32;
pub const NONCE: usize = 12;
pub const TAG: usize = 16;

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn block(key: &[u8; KEY], counter: u32, nonce: &[u8; NONCE]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for (w, chunk) in init[4..12].iter_mut().zip(key.chunks_exact(4)) {
        *w = u32::from_le_bytes(chunk.try_into().expect("4 bytes"));
    }
    init[12] = counter;
    for (w, chunk) in init[13..].iter_mut().zip(nonce.chunks_exact(4)) {
        *w = u32::from_le_bytes(chunk.try_into().expect("4 bytes"));
    }
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for ((chunk, w), i) in out.chunks_exact_mut(4).zip(s).zip(init) {
        chunk.copy_from_slice(&w.wrapping_add(i).to_le_bytes());
    }
    crate::ct::wipe(&mut init);
    crate::ct::wipe(&mut s);
    out
}

pub fn chacha20_xor(key: &[u8; KEY], counter: u32, nonce: &[u8; NONCE], data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(64).enumerate() {
        let ks = block(key, counter.wrapping_add(i as u32), nonce);
        for (d, k) in chunk.iter_mut().zip(ks) {
            *d ^= k;
        }
    }
}

/// Poly1305 over three limbs of 44, 44 and 42 bits.
struct Poly1305 {
    r: [u64; 3],
    s: [u64; 2],
    h: [u64; 3],
    buf: [u8; 16],
    fill: usize,
}

impl Drop for Poly1305 {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.r);
        crate::ct::wipe(&mut self.s);
        crate::ct::wipe(&mut self.h);
        crate::ct::wipe(&mut self.buf);
    }
}

const M44: u64 = (1 << 44) - 1;
const M42: u64 = (1 << 42) - 1;

impl Poly1305 {
    fn new(key: &[u8; 32]) -> Self {
        let t0 = u64::from_le_bytes(key[0..8].try_into().expect("8 bytes"));
        let t1 = u64::from_le_bytes(key[8..16].try_into().expect("8 bytes"));
        Self {
            r: [
                t0 & 0xffc_0fff_ffff,
                ((t0 >> 44) | (t1 << 20)) & 0xfff_ffc0_ffff,
                (t1 >> 24) & 0x0f_ffff_fc0f,
            ],
            s: [
                u64::from_le_bytes(key[16..24].try_into().expect("8 bytes")),
                u64::from_le_bytes(key[24..32].try_into().expect("8 bytes")),
            ],
            h: [0; 3],
            buf: [0; 16],
            fill: 0,
        }
    }

    fn block(&mut self, m: &[u8; 16], hibit: u64) {
        let t0 = u64::from_le_bytes(m[0..8].try_into().expect("8 bytes"));
        let t1 = u64::from_le_bytes(m[8..16].try_into().expect("8 bytes"));
        let [r0, r1, r2] = self.r.map(u128::from);
        let (s1, s2) = (r1 * 20, r2 * 20);
        let h0 = u128::from(self.h[0] + (t0 & M44));
        let h1 = u128::from(self.h[1] + (((t0 >> 44) | (t1 << 20)) & M44));
        let h2 = u128::from(self.h[2] + (((t1 >> 24) & M42) | (hibit << 40)));

        let d0 = h0 * r0 + h1 * s2 + h2 * s1;
        let mut d1 = h0 * r1 + h1 * r0 + h2 * s2;
        let mut d2 = h0 * r2 + h1 * r1 + h2 * r0;
        d1 += d0 >> 44;
        let mut n0 = (d0 as u64) & M44;
        d2 += d1 >> 44;
        let mut n1 = (d1 as u64) & M44;
        let carry = (d2 >> 42) as u64;
        let n2 = (d2 as u64) & M42;
        n0 += carry * 5;
        n1 += n0 >> 44;
        n0 &= M44;
        self.h = [n0, n1, n2];
    }

    fn update(&mut self, mut data: &[u8]) {
        if self.fill > 0 {
            let take = (16 - self.fill).min(data.len());
            self.buf[self.fill..self.fill + take].copy_from_slice(&data[..take]);
            self.fill += take;
            data = &data[take..];
            if self.fill < 16 {
                return;
            }
            let buf = self.buf;
            self.block(&buf, 1);
            self.fill = 0;
        }
        let mut chunks = data.chunks_exact(16);
        for m in &mut chunks {
            self.block(m.try_into().expect("16 bytes"), 1);
        }
        let rest = chunks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.fill = rest.len();
    }

    /// Pad the pending partial block with zeros, as the AEAD construction
    /// does between its sections.
    fn pad16(&mut self) {
        if self.fill > 0 {
            self.buf[self.fill..].fill(0);
            let buf = self.buf;
            self.block(&buf, 1);
            self.fill = 0;
        }
    }

    fn finish(mut self) -> [u8; TAG] {
        if self.fill > 0 {
            self.buf[self.fill] = 1;
            self.buf[self.fill + 1..].fill(0);
            let buf = self.buf;
            self.block(&buf, 0);
        }
        let [mut h0, mut h1, mut h2] = self.h;
        h1 += h0 >> 44;
        h0 &= M44;
        h2 += h1 >> 44;
        h1 &= M44;
        h0 += (h2 >> 42) * 5;
        h2 &= M42;
        h1 += h0 >> 44;
        h0 &= M44;
        h2 += h1 >> 44;
        h1 &= M44;
        h0 += (h2 >> 42) * 5;
        h2 &= M42;
        h1 += h0 >> 44;
        h0 &= M44;

        let mut g0 = h0 + 5;
        let mut g1 = h1 + (g0 >> 44);
        g0 &= M44;
        let g2 = (h2 + (g1 >> 44)).wrapping_sub(1 << 42);
        g1 &= M44;
        // `g2` borrowed exactly when h < p: keep h then, else h - p.
        let keep_h = 0u64.wrapping_sub(g2 >> 63);
        let h0 = (h0 & keep_h) | (g0 & !keep_h);
        let h1 = (h1 & keep_h) | (g1 & !keep_h);
        let h2 = (h2 & keep_h) | (g2 & !keep_h);

        let h = u128::from(h0) | (u128::from(h1) << 44) | (u128::from(h2) << 88);
        let s = u128::from(self.s[0]) | (u128::from(self.s[1]) << 64);
        h.wrapping_add(s).to_le_bytes()
    }
}

pub struct ChaCha20Poly1305 {
    key: [u8; KEY],
}

/// Counter blocks 1 through 2^32 - 1 carry data (RFC 8439 §2.8).
pub const MAX_DATA: u64 = ((1 << 32) - 1) * 64;

impl Drop for ChaCha20Poly1305 {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.key);
    }
}

impl ChaCha20Poly1305 {
    pub fn new(key: &[u8]) -> Option<Self> {
        Some(Self {
            key: key.try_into().ok()?,
        })
    }

    fn tag(&self, nonce: &[u8; NONCE], aad: &[u8], ciphertext: &[u8]) -> [u8; TAG] {
        let mut otk = [0u8; 32];
        chacha20_xor(&self.key, 0, nonce, &mut otk);
        let mut mac = Poly1305::new(&otk);
        crate::ct::wipe(&mut otk);
        mac.update(aad);
        mac.pad16();
        mac.update(ciphertext);
        mac.pad16();
        mac.update(&(aad.len() as u64).to_le_bytes());
        mac.update(&(ciphertext.len() as u64).to_le_bytes());
        mac.finish()
    }

    /// # Panics
    ///
    /// Past [`MAX_DATA`], where the block counter would wrap.
    pub fn seal(&self, nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8]) -> [u8; TAG] {
        assert!(
            data.len() as u64 <= MAX_DATA,
            "ChaCha20-Poly1305 input too long"
        );
        chacha20_xor(&self.key, 1, nonce, data);
        self.tag(nonce, aad, data)
    }

    pub fn open(&self, nonce: &[u8; NONCE], aad: &[u8], data: &mut [u8], tag: &[u8]) -> bool {
        if data.len() as u64 > MAX_DATA || !crate::ct::eq(&self.tag(nonce, aad, data), tag) {
            return false;
        }
        chacha20_xor(&self.key, 1, nonce, data);
        true
    }
}

#[cfg(test)]
pub(crate) fn poly1305_for_test(key: &[u8; 32], msg: &[u8]) -> [u8; TAG] {
    let mut p = Poly1305::new(key);
    p.update(msg);
    p.finish()
}
