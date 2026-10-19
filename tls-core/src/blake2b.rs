//! BLAKE2b (RFC 7693), unkeyed, with a digest of 1 to 64 bytes.

const BLOCK: usize = 128;
pub const MAX_OUTPUT: usize = 64;

const IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

#[derive(Clone)]
pub struct Blake2b {
    h: [u64; 8],
    bytes: u128,
    buf: [u8; BLOCK],
    filled: usize,
    len: usize,
}

impl Blake2b {
    /// A hash whose digest is `len` bytes, 1 to [`MAX_OUTPUT`].
    pub fn new(len: usize) -> Self {
        assert!(
            (1..=MAX_OUTPUT).contains(&len),
            "a BLAKE2b digest is 1 to 64 bytes"
        );
        let mut h = IV;
        h[0] ^= 0x0101_0000 ^ len as u64;
        Self {
            h,
            bytes: 0,
            buf: [0; BLOCK],
            filled: 0,
            len,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            // The last block is compressed only by `finish`, flagged final.
            if self.filled == BLOCK {
                self.bytes += BLOCK as u128;
                self.compress(false);
                self.filled = 0;
            }
            let n = (BLOCK - self.filled).min(data.len());
            self.buf[self.filled..self.filled + n].copy_from_slice(&data[..n]);
            self.filled += n;
            data = &data[n..];
        }
    }

    /// The digest, into `out`, which is as long as [`new`](Self::new) said.
    pub fn finish(mut self, out: &mut [u8]) {
        assert_eq!(out.len(), self.len, "the digest's length");
        self.bytes += self.filled as u128;
        self.buf[self.filled..].fill(0);
        self.compress(true);
        for (chunk, word) in out.chunks_mut(8).zip(self.h) {
            chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
        }
        crate::ct::wipe(&mut self.buf);
        crate::ct::wipe(&mut self.h);
    }

    fn compress(&mut self, last: bool) {
        let mut m = [0u64; 16];
        for (word, bytes) in m.iter_mut().zip(self.buf.chunks_exact(8)) {
            *word = u64::from_le_bytes(bytes.try_into().expect("8 bytes"));
        }
        let mut v = [0u64; 16];
        v[..8].copy_from_slice(&self.h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= self.bytes as u64;
        v[13] ^= (self.bytes >> 64) as u64;
        if last {
            v[14] = !v[14];
        }
        for round in 0..12 {
            let s = &SIGMA[round % 10];
            g(&mut v, [0, 4, 8, 12], m[s[0]], m[s[1]]);
            g(&mut v, [1, 5, 9, 13], m[s[2]], m[s[3]]);
            g(&mut v, [2, 6, 10, 14], m[s[4]], m[s[5]]);
            g(&mut v, [3, 7, 11, 15], m[s[6]], m[s[7]]);
            g(&mut v, [0, 5, 10, 15], m[s[8]], m[s[9]]);
            g(&mut v, [1, 6, 11, 12], m[s[10]], m[s[11]]);
            g(&mut v, [2, 7, 8, 13], m[s[12]], m[s[13]]);
            g(&mut v, [3, 4, 9, 14], m[s[14]], m[s[15]]);
        }
        for (i, h) in self.h.iter_mut().enumerate() {
            *h ^= v[i] ^ v[i + 8];
        }
        crate::ct::wipe(&mut m);
    }
}

fn g(v: &mut [u64; 16], [a, b, c, d]: [usize; 4], x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

/// The digest of `parts` in turn, as long as `out`, into it.
pub fn digest(parts: &[&[u8]], out: &mut [u8]) {
    let mut h = Blake2b::new(out.len());
    for part in parts {
        h.update(part);
    }
    h.finish(out);
}
