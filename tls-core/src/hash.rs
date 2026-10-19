//! SHA-256, SHA-384 and SHA-512 (FIPS 180-4), HMAC (RFC 2104) and HKDF
//! (RFC 5869).

use slopos_verity_core::sha256::{IV as H256, compress as compress256};

/// A Merkle–Damgård hash with a fixed-size digest, as HMAC and HKDF need it.
pub trait Hash: Clone {
    const OUTPUT: usize;
    const BLOCK: usize;
    fn new() -> Self;
    fn update(&mut self, data: &[u8]);
    /// Writes the digest into the first [`OUTPUT`](Self::OUTPUT) bytes of `out`.
    fn finish_into(self, out: &mut [u8]);

    fn digest(data: &[u8]) -> Digest {
        let mut h = Self::new();
        h.update(data);
        h.finish()
    }

    fn finish(self) -> Digest {
        let mut d = Digest::empty(Self::OUTPUT);
        self.finish_into(d.as_mut());
        d
    }
}

pub const MAX_OUTPUT: usize = 64;

/// A digest of up to [`MAX_OUTPUT`] bytes, wiped when dropped: most of them
/// are secrets.
#[derive(Clone)]
pub struct Digest {
    bytes: [u8; MAX_OUTPUT],
    len: usize,
}

impl Digest {
    pub fn empty(len: usize) -> Self {
        Self {
            bytes: [0; MAX_OUTPUT],
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Digest {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.bytes);
    }
}

impl AsRef<[u8]> for Digest {
    fn as_ref(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl AsMut<[u8]> for Digest {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[..self.len]
    }
}

impl core::fmt::Debug for Digest {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for b in self.as_ref() {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl PartialEq for Digest {
    fn eq(&self, other: &Self) -> bool {
        crate::ct::eq(self.as_ref(), other.as_ref())
    }
}

impl Eq for Digest {}

const K512: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

const H512: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const H384: [u64; 8] = [
    0xcbbb9d5dc1059ed8,
    0x629a292a367cd507,
    0x9159015a3070dd17,
    0x152fecd8f70e5939,
    0x67332667ffc00b31,
    0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7,
    0x47b5481dbefa4fa4,
];

#[derive(Clone)]
struct Blocks<const B: usize> {
    buf: [u8; B],
    fill: usize,
    total: u128,
}

impl<const B: usize> Blocks<B> {
    const fn new() -> Self {
        Self {
            buf: [0; B],
            fill: 0,
            total: 0,
        }
    }

    fn update(&mut self, mut data: &[u8], mut compress: impl FnMut(&[u8; B])) {
        self.total += data.len() as u128;
        if self.fill > 0 {
            let take = (B - self.fill).min(data.len());
            self.buf[self.fill..self.fill + take].copy_from_slice(&data[..take]);
            self.fill += take;
            data = &data[take..];
            if self.fill < B {
                return;
            }
            compress(&self.buf);
            self.fill = 0;
        }
        let mut chunks = data.chunks_exact(B);
        for block in &mut chunks {
            compress(block.try_into().expect("exact chunk"));
        }
        let rest = chunks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.fill = rest.len();
    }

    /// Appends the padding and the bit length in `len_bytes` big-endian bytes.
    fn pad(&mut self, len_bytes: usize, mut compress: impl FnMut(&[u8; B])) {
        let bits = self.total * 8;
        self.buf[self.fill] = 0x80;
        self.fill += 1;
        if self.fill > B - len_bytes {
            self.buf[self.fill..].fill(0);
            compress(&self.buf);
            self.fill = 0;
        }
        self.buf[self.fill..B - len_bytes].fill(0);
        let len = bits.to_be_bytes();
        self.buf[B - len_bytes..].copy_from_slice(&len[16 - len_bytes..]);
        compress(&self.buf);
    }
}

#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    blocks: Blocks<64>,
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.state);
        crate::ct::wipe(&mut self.blocks.buf);
    }
}

impl Hash for Sha256 {
    const OUTPUT: usize = 32;
    const BLOCK: usize = 64;

    fn new() -> Self {
        Self {
            state: H256,
            blocks: Blocks::new(),
        }
    }

    fn update(&mut self, data: &[u8]) {
        let state = &mut self.state;
        self.blocks.update(data, |b| compress256(state, b));
    }

    fn finish_into(mut self, out: &mut [u8]) {
        let state = &mut self.state;
        self.blocks.pad(8, |b| compress256(state, b));
        for (chunk, word) in out[..32].chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
    }
}

#[derive(Clone)]
struct Sha512Core {
    state: [u64; 8],
    blocks: Blocks<128>,
}

impl Drop for Sha512Core {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.state);
        crate::ct::wipe(&mut self.blocks.buf);
    }
}

fn compress512(state: &mut [u64; 8], block: &[u8; 128]) {
    let mut w = [0u64; 80];
    for (i, word) in block.chunks_exact(8).enumerate() {
        w[i] = u64::from_be_bytes(word.try_into().expect("8 bytes"));
    }
    for i in 16..80 {
        let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
        let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K512[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (s, v) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *s = s.wrapping_add(v);
    }
}

impl Sha512Core {
    fn with_iv(state: [u64; 8]) -> Self {
        Self {
            state,
            blocks: Blocks::new(),
        }
    }

    fn update(&mut self, data: &[u8]) {
        let state = &mut self.state;
        self.blocks.update(data, |b| compress512(state, b));
    }

    fn finish_into(mut self, out: &mut [u8]) {
        let state = &mut self.state;
        self.blocks.pad(16, |b| compress512(state, b));
        let mut full = [0u8; 64];
        for (chunk, word) in full.chunks_exact_mut(8).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        let n = out.len().min(64);
        out[..n].copy_from_slice(&full[..n]);
    }
}

#[derive(Clone)]
pub struct Sha384(Sha512Core);

impl Hash for Sha384 {
    const OUTPUT: usize = 48;
    const BLOCK: usize = 128;

    fn new() -> Self {
        Self(Sha512Core::with_iv(H384))
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finish_into(self, out: &mut [u8]) {
        self.0.finish_into(&mut out[..48]);
    }
}

#[derive(Clone)]
pub struct Sha512(Sha512Core);

impl Hash for Sha512 {
    const OUTPUT: usize = 64;
    const BLOCK: usize = 128;

    fn new() -> Self {
        Self(Sha512Core::with_iv(H512))
    }

    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finish_into(self, out: &mut [u8]) {
        self.0.finish_into(&mut out[..64]);
    }
}

#[derive(Clone)]
pub struct Hmac<H: Hash> {
    inner: H,
    outer: H,
}

impl<H: Hash> Hmac<H> {
    pub fn new(key: &[u8]) -> Self {
        let mut block = [0u8; 128];
        if key.len() > H::BLOCK {
            H::digest(key)
                .as_ref()
                .iter()
                .zip(block.iter_mut())
                .for_each(|(k, b)| *b = *k);
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner = H::new();
        let mut outer = H::new();
        let mut pad = [0u8; 128];
        for (p, k) in pad.iter_mut().zip(block.iter()).take(H::BLOCK) {
            *p = k ^ 0x36;
        }
        inner.update(&pad[..H::BLOCK]);
        for (p, k) in pad.iter_mut().zip(block.iter()).take(H::BLOCK) {
            *p = k ^ 0x5c;
        }
        outer.update(&pad[..H::BLOCK]);
        crate::ct::wipe(&mut block);
        crate::ct::wipe(&mut pad);
        Self { inner, outer }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub fn finish(self) -> Digest {
        let mut outer = self.outer;
        outer.update(self.inner.finish().as_ref());
        outer.finish()
    }

    pub fn mac(key: &[u8], data: &[u8]) -> Digest {
        let mut h = Self::new(key);
        h.update(data);
        h.finish()
    }
}

pub fn hkdf_extract<H: Hash>(salt: &[u8], ikm: &[u8]) -> Digest {
    let zeros = [0u8; MAX_OUTPUT];
    let salt = if salt.is_empty() {
        &zeros[..H::OUTPUT]
    } else {
        salt
    };
    Hmac::<H>::mac(salt, ikm)
}

/// HKDF-Expand into `out`, which may be at most 255 hash lengths.
pub fn hkdf_expand<H: Hash>(prk: &[u8], info: &[&[u8]], out: &mut [u8]) {
    assert!(out.len() <= 255 * H::OUTPUT, "HKDF output too long");
    let keyed = Hmac::<H>::new(prk);
    let mut previous = Digest::empty(0);
    for (i, chunk) in out.chunks_mut(H::OUTPUT).enumerate() {
        let mut h = keyed.clone();
        h.update(previous.as_ref());
        for part in info {
            h.update(part);
        }
        h.update(&[i as u8 + 1]);
        previous = h.finish();
        chunk.copy_from_slice(&previous.as_ref()[..chunk.len()]);
    }
}
