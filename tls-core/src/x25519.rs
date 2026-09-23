//! X25519 (RFC 7748) over five 51-bit limbs, with a Montgomery ladder whose
//! swaps are masks rather than branches.

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

const MASK51: u64 = (1 << 51) - 1;

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    fn from_bytes(b: &[u8; 32]) -> Fe {
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

    fn to_bytes(self) -> [u8; 32] {
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

    fn add(self, o: Fe) -> Fe {
        let mut r = self;
        for (a, b) in r.0.iter_mut().zip(o.0) {
            *a += b;
        }
        r
    }

    /// `self - o`, adding 4p first so no limb underflows for carried inputs.
    fn sub(self, o: Fe) -> Fe {
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

    fn mul(self, o: Fe) -> Fe {
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

    fn square(self) -> Fe {
        self.mul(self)
    }

    fn mul_small(self, k: u64) -> Fe {
        Self::reduce_wide(self.0.map(|l| u128::from(l) * u128::from(k)))
    }

    fn pow2k(mut self, k: u32) -> Fe {
        for _ in 0..k {
            self = self.square();
        }
        self
    }

    /// `self^(p-2)`, by the usual addition chain for 2^255 - 21.
    fn invert(self) -> Fe {
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
        let z2_250_0 = z2_200_0.pow2k(50).mul(z2_50_0);
        z2_250_0.pow2k(5).mul(z11)
    }

    fn cswap(a: &mut Fe, b: &mut Fe, swap: u64) {
        let mask = crate::ct::mask64(swap);
        for (x, y) in a.0.iter_mut().zip(b.0.iter_mut()) {
            let t = mask & (*x ^ *y);
            *x ^= t;
            *y ^= t;
        }
    }
}

pub const BASEPOINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

pub fn x25519(scalar: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut k = *scalar;
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;

    let x1 = Fe::from_bytes(u);
    let (mut x2, mut z2) = (Fe::ONE, Fe::ZERO);
    let (mut x3, mut z3) = (x1, Fe::ONE);
    let mut swap = 0u64;
    for t in (0..255).rev() {
        let bit = u64::from((k[t / 8] >> (t % 8)) & 1);
        swap ^= bit;
        Fe::cswap(&mut x2, &mut x3, swap);
        Fe::cswap(&mut z2, &mut z3, swap);
        swap = bit;

        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121_665)));
    }
    Fe::cswap(&mut x2, &mut x3, swap);
    Fe::cswap(&mut z2, &mut z3, swap);
    let out = x2.mul(z2.invert()).to_bytes();
    crate::ct::wipe(&mut k);
    for fe in [&mut x2, &mut z2, &mut x3, &mut z3] {
        crate::ct::wipe(&mut fe.0);
    }
    out
}

/// The shared secret, or `None` when it is all zeros: the peer sent a
/// small-order point, and RFC 7748 §6.1 has the caller abort.
pub fn shared_secret(scalar: &[u8; 32], peer: &[u8; 32]) -> Option<[u8; 32]> {
    let s = x25519(scalar, peer);
    let zero = s.iter().fold(0u8, |acc, b| acc | b);
    (core::hint::black_box(zero) != 0).then_some(s)
}
