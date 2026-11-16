//! SHA-256 (FIPS 180-4).

/// Fold whole message blocks, `blocks.len()` a multiple of [`BLOCK`], into a
/// state.
pub type Compress = fn(&mut [u32; 8], &[u8]);

pub const OUTPUT: usize = 32;
pub const BLOCK: usize = 64;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The state a hash starts from.
pub const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Fold one message block into `state`.
pub fn compress(state: &mut [u32; 8], block: &[u8; BLOCK]) {
    let mut w = [0u32; 64];
    for (i, word) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
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

/// [`Compress`] on any CPU.
pub fn compress_blocks(state: &mut [u32; 8], blocks: &[u8]) {
    for block in blocks.chunks_exact(BLOCK) {
        if let Ok(block) = block.try_into() {
            compress(state, block);
        }
    }
}

#[derive(Clone)]
pub struct Sha256 {
    compress: Compress,
    state: [u32; 8],
    buf: [u8; BLOCK],
    fill: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub const fn new() -> Self {
        Self::using(compress_blocks)
    }

    pub const fn using(compress: Compress) -> Self {
        Self {
            compress,
            state: IV,
            buf: [0; BLOCK],
            fill: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.fill > 0 {
            let take = (BLOCK - self.fill).min(data.len());
            self.buf[self.fill..self.fill + take].copy_from_slice(&data[..take]);
            self.fill += take;
            data = &data[take..];
            if self.fill < BLOCK {
                return;
            }
            (self.compress)(&mut self.state, &self.buf);
            self.fill = 0;
        }
        let whole = data.len() - data.len() % BLOCK;
        if whole > 0 {
            (self.compress)(&mut self.state, &data[..whole]);
        }
        let rest = &data[whole..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.fill = rest.len();
    }

    pub fn finish(mut self) -> [u8; OUTPUT] {
        let bits = self.total.wrapping_mul(8);
        self.buf[self.fill] = 0x80;
        self.fill += 1;
        if self.fill > BLOCK - 8 {
            self.buf[self.fill..].fill(0);
            (self.compress)(&mut self.state, &self.buf);
            self.fill = 0;
        }
        self.buf[self.fill..BLOCK - 8].fill(0);
        self.buf[BLOCK - 8..].copy_from_slice(&bits.to_be_bytes());
        (self.compress)(&mut self.state, &self.buf);
        let mut out = [0; OUTPUT];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

pub fn digest(data: &[u8]) -> [u8; OUTPUT] {
    digest_using(compress_blocks, data)
}

pub fn digest_using(compress: Compress, data: &[u8]) -> [u8; OUTPUT] {
    let mut hash = Sha256::using(compress);
    hash.update(data);
    hash.finish()
}

/// The SHA instructions, for userland: the kernel is soft-float, so it is
/// built without them. Calling [`ni::compress_blocks`] needs [`ni::present`]
/// first, which only a caller allowed `unsafe` can act on.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
pub mod ni {
    use core::arch::x86_64::{
        __cpuid, __cpuid_count, __m128i, _mm_add_epi32, _mm_alignr_epi8, _mm_blend_epi16,
        _mm_extract_epi32, _mm_set_epi32, _mm_sha256msg1_epu32, _mm_sha256msg2_epu32,
        _mm_sha256rnds2_epu32, _mm_shuffle_epi32,
    };

    use super::{BLOCK, K};

    /// Whether this CPU has the SHA, SSSE3 and SSE4.1 instructions.
    pub fn present() -> bool {
        let leaf1 = __cpuid(1);
        let ssse3_sse41 = leaf1.ecx & (1 << 9) != 0 && leaf1.ecx & (1 << 19) != 0;
        ssse3_sse41 && __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 29) != 0
    }

    #[inline]
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    fn words(bytes: &[u8], at: usize) -> __m128i {
        let w = |k: usize| {
            let i = at + 4 * k;
            i32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
        };
        _mm_set_epi32(w(3), w(2), w(1), w(0))
    }

    #[inline]
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    fn schedule(w0: __m128i, w1: __m128i, w2: __m128i, w3: __m128i) -> __m128i {
        let t = _mm_add_epi32(_mm_sha256msg1_epu32(w0, w1), _mm_alignr_epi8::<4>(w3, w2));
        _mm_sha256msg2_epu32(t, w3)
    }

    /// [`super::Compress`] in the SHA instructions, as Intel's reference
    /// lays out the state: `abef` and `cdgh`.
    #[target_feature(enable = "sha,sse2,ssse3,sse4.1")]
    pub fn compress_blocks(state: &mut [u32; 8], blocks: &[u8]) {
        let s = |i: usize| state[i] as i32;
        let dcba = _mm_set_epi32(s(3), s(2), s(1), s(0));
        let efgh = _mm_set_epi32(s(7), s(6), s(5), s(4));
        let cdab = _mm_shuffle_epi32::<0xB1>(dcba);
        let efgh = _mm_shuffle_epi32::<0x1B>(efgh);
        let mut abef = _mm_alignr_epi8::<8>(cdab, efgh);
        let mut cdgh = _mm_blend_epi16::<0xF0>(efgh, cdab);
        for block in blocks.chunks_exact(BLOCK) {
            let (abef_was, cdgh_was) = (abef, cdgh);
            let mut w = [
                words(block, 0),
                words(block, 16),
                words(block, 32),
                words(block, 48),
            ];
            for i in 0..16 {
                if i >= 4 {
                    w[i % 4] = schedule(w[i % 4], w[(i + 1) % 4], w[(i + 2) % 4], w[(i + 3) % 4]);
                }
                let k = |j: usize| K[4 * i + j] as i32;
                let t = _mm_add_epi32(w[i % 4], _mm_set_epi32(k(3), k(2), k(1), k(0)));
                cdgh = _mm_sha256rnds2_epu32(cdgh, abef, t);
                abef = _mm_sha256rnds2_epu32(abef, cdgh, _mm_shuffle_epi32::<0x0E>(t));
            }
            abef = _mm_add_epi32(abef, abef_was);
            cdgh = _mm_add_epi32(cdgh, cdgh_was);
        }
        let feba = _mm_shuffle_epi32::<0x1B>(abef);
        let dchg = _mm_shuffle_epi32::<0xB1>(cdgh);
        let dcba = _mm_blend_epi16::<0xF0>(feba, dchg);
        let hgef = _mm_alignr_epi8::<8>(dchg, feba);
        *state = [
            _mm_extract_epi32::<0>(dcba) as u32,
            _mm_extract_epi32::<1>(dcba) as u32,
            _mm_extract_epi32::<2>(dcba) as u32,
            _mm_extract_epi32::<3>(dcba) as u32,
            _mm_extract_epi32::<0>(hgef) as u32,
            _mm_extract_epi32::<1>(hgef) as u32,
            _mm_extract_epi32::<2>(hgef) as u32,
            _mm_extract_epi32::<3>(hgef) as u32,
        ];
    }
}
