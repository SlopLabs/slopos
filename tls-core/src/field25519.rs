//! Arithmetic modulo 2^255 - 19 over five 51-bit limbs, which X25519 and
//! Ed25519 share.

#[derive(Clone, Copy)]
pub(crate) struct Fe(pub(crate) [u64; 5]);

const MASK51: u64 = (1 << 51) - 1;

impl Fe {
    pub(crate) const ZERO: Fe = Fe([0; 5]);
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// The low 255 bits of `b`, little-endian; the top bit is ignored.
    pub(crate) fn from_bytes(b: &[u8; 32]) -> Fe {
        let load = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
        Fe([
            load(0) & MASK51,
            (load(6) >> 3) & MASK51,
            (load(12) >> 6) & MASK51,
            (load(19) >> 1) & MASK51,
            (load(24) >> 12) & MASK51,
        ])
    }

    fn carry(mut self) -> Fe {
        for _ in 0..2 {
            for i in 0..4 {
                self.0[i + 1] += self.0[i] >> 51;
                self.0[i] &= MASK51;
            }
            self.0[0] += 19 * (self.0[4] >> 51);
            self.0[4] &= MASK51;
        }
        self
    }

    /// The canonical encoding, fully reduced.
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry().0;
        // Subtract p once if h >= p: q is 1 exactly then.
        let mut q = (h[0] + 19) >> 51;
        for limb in &h[1..] {
            q = (limb + q) >> 51;
        }
        h[0] += 19 * q;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK51;
        }
        h[4] &= MASK51;
        let packed = [
            h[0] | (h[1] << 51),
            (h[1] >> 13) | (h[2] << 38),
            (h[2] >> 26) | (h[3] << 25),
            (h[3] >> 39) | (h[4] << 12),
        ];
        let mut out = [0u8; 32];
        for (chunk, w) in out.chunks_exact_mut(8).zip(packed) {
            chunk.copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub(crate) fn add(self, o: Fe) -> Fe {
        let mut r = self;
        for (a, b) in r.0.iter_mut().zip(o.0) {
            *a += b;
        }
        r
    }

    /// `self - o`, adding 4p first so no limb underflows for carried inputs.
    pub(crate) fn sub(self, o: Fe) -> Fe {
        const FOUR_P: [u64; 5] = [
            0x1f_ffff_ffff_ffb4,
            0x1f_ffff_ffff_fffc,
            0x1f_ffff_ffff_fffc,
            0x1f_ffff_ffff_fffc,
            0x1f_ffff_ffff_fffc,
        ];
        let mut r = self;
        for ((a, b), p) in r.0.iter_mut().zip(o.0).zip(FOUR_P) {
            *a = *a + p - b;
        }
        r.carry()
    }

    pub(crate) fn neg(self) -> Fe {
        Fe::ZERO.sub(self)
    }

    pub(crate) fn mul(self, o: Fe) -> Fe {
        let a = self.0.map(u128::from);
        let b = o.0.map(u128::from);
        let b19 = [b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19];
        let t = [
            a[0] * b[0] + a[1] * b19[3] + a[2] * b19[2] + a[3] * b19[1] + a[4] * b19[0],
            a[0] * b[1] + a[1] * b[0] + a[2] * b19[3] + a[3] * b19[2] + a[4] * b19[1],
            a[0] * b[2] + a[1] * b[1] + a[2] * b[0] + a[3] * b19[3] + a[4] * b19[2],
            a[0] * b[3] + a[1] * b[2] + a[2] * b[1] + a[3] * b[0] + a[4] * b19[3],
            a[0] * b[4] + a[1] * b[3] + a[2] * b[2] + a[3] * b[1] + a[4] * b[0],
        ];
        Self::reduce_wide(t)
    }

    fn reduce_wide(mut t: [u128; 5]) -> Fe {
        let m = u128::from(MASK51);
        for i in 0..4 {
            t[i + 1] += t[i] >> 51;
            t[i] &= m;
        }
        let top = t[4] >> 51;
        t[4] &= m;
        t[0] += top * 19;
        t[1] += t[0] >> 51;
        t[0] &= m;
        Fe(t.map(|l| l as u64))
    }

    pub(crate) fn square(self) -> Fe {
        self.mul(self)
    }

    pub(crate) fn mul_small(self, k: u64) -> Fe {
        Self::reduce_wide(self.0.map(|l| u128::from(l) * u128::from(k)))
    }

    fn pow2k(mut self, k: u32) -> Fe {
        for _ in 0..k {
            self = self.square();
        }
        self
    }

    /// `(self^11, self^(2^250 - 1))`, the shared head of both chains below.
    fn pow_2_250_1(self) -> (Fe, Fe) {
        let z2 = self.square();
        let z9 = z2.pow2k(2).mul(self);
        let z11 = z9.mul(z2);
        let z2_5_0 = z11.square().mul(z9);
        let z2_10_0 = z2_5_0.pow2k(5).mul(z2_5_0);
        let z2_20_0 = z2_10_0.pow2k(10).mul(z2_10_0);
        let z2_40_0 = z2_20_0.pow2k(20).mul(z2_20_0);
        let z2_50_0 = z2_40_0.pow2k(10).mul(z2_10_0);
        let z2_100_0 = z2_50_0.pow2k(50).mul(z2_50_0);
        let z2_200_0 = z2_100_0.pow2k(100).mul(z2_100_0);
        (z11, z2_200_0.pow2k(50).mul(z2_50_0))
    }

    /// `self^(p-2)`.
    pub(crate) fn invert(self) -> Fe {
        let (z11, z2_250_0) = self.pow_2_250_1();
        z2_250_0.pow2k(5).mul(z11)
    }

    /// `self^((p-5)/8)`, the exponent a square root modulo p is taken with.
    pub(crate) fn pow_p58(self) -> Fe {
        self.pow_2_250_1().1.pow2k(2).mul(self)
    }

    pub(crate) fn is_zero(self) -> bool {
        self.to_bytes() == [0; 32]
    }

    pub(crate) fn is_negative(self) -> bool {
        self.to_bytes()[0] & 1 == 1
    }

    pub(crate) fn equals(self, o: Fe) -> bool {
        self.to_bytes() == o.to_bytes()
    }

    pub(crate) fn cswap(a: &mut Fe, b: &mut Fe, swap: u64) {
        let mask = crate::ct::mask64(swap);
        for (x, y) in a.0.iter_mut().zip(b.0.iter_mut()) {
            let t = mask & (*x ^ *y);
            *x ^= t;
            *y ^= t;
        }
    }
}
