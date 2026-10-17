//! Ed25519 (RFC 8032), verified strictly: a public key must be a canonical
//! encoding of a point outside the small-order subgroup, `S` must be below
//! the group order, and `R` must be the canonical encoding of `[S]B - [k]A`,
//! so a signature has one form. Not constant-time: verification handles only
//! public values.

use crate::field25519::Fe;
use crate::hash::{Hash, Sha512};

pub const PUBLIC_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

/// The group order L, little-endian.
const L: [u64; 4] = [
    0x5812_631a_5cf5_d3ed,
    0x14de_f9de_a2f7_9cd6,
    0,
    0x1000_0000_0000_0000,
];

/// The base point, `y = 4/5` with `x` even.
const BASE: [u8; 32] = {
    let mut b = [0x66; 32];
    b[0] = 0x58;
    b
};

type Scalar = [u64; 4];

fn small(n: u64) -> Fe {
    Fe([n, 0, 0, 0, 0])
}

/// The curve's `d`, `-121665/121666`.
fn d() -> Fe {
    small(121_665).neg().mul(small(121_666).invert())
}

/// A square root of -1, `2^((p-1)/4)`: 2 is no square modulo p.
fn sqrt_m1() -> Fe {
    let two = small(2);
    two.pow_p58().square().mul(two)
}

/// A point in extended coordinates: `x = X/Z`, `y = Y/Z`, `XY = ZT`.
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

struct Curve {
    d: Fe,
    d2: Fe,
    base: Point,
}

impl Curve {
    fn new() -> Curve {
        let d = d();
        let mut curve = Curve {
            d,
            d2: d.add(d),
            base: Point::IDENTITY,
        };
        curve.base = curve.decode(&BASE).expect("the base point decodes");
        curve
    }

    /// RFC 8032 §5.1.3, refusing `y >= p` and a negative zero `x`.
    fn decode(&self, bytes: &[u8; 32]) -> Option<Point> {
        let negative = bytes[31] >> 7 == 1;
        let mut canonical = *bytes;
        canonical[31] &= 0x7f;
        let y = Fe::from_bytes(bytes);
        if y.to_bytes() != canonical {
            return None;
        }
        let y2 = y.square();
        let u = y2.sub(Fe::ONE);
        let v = self.d.mul(y2).add(Fe::ONE);
        let v3 = v.square().mul(v);
        let v7 = v3.square().mul(v);
        let mut x = u.mul(v3).mul(u.mul(v7).pow_p58());
        let vx2 = v.mul(x.square());
        if vx2.equals(u.neg()) {
            x = x.mul(sqrt_m1());
        } else if !vx2.equals(u) {
            return None;
        }
        if x.is_zero() && negative {
            return None;
        }
        if x.is_negative() != negative {
            x = x.neg();
        }
        Some(Point {
            x,
            y,
            z: Fe::ONE,
            t: x.mul(y),
        })
    }

    /// The unified addition of Hisil, Wong, Carter and Dawson for `a = -1`,
    /// complete on this curve and so a doubling too.
    fn add(&self, p: &Point, q: &Point) -> Point {
        let a = p.y.sub(p.x).mul(q.y.sub(q.x));
        let b = p.y.add(p.x).mul(q.y.add(q.x));
        let c = p.t.mul(self.d2).mul(q.t);
        let d = p.z.mul(q.z).mul_small(2);
        let (e, f, g, h) = (b.sub(a), d.sub(c), d.add(c), b.add(a));
        Point {
            x: e.mul(f),
            y: g.mul(h),
            z: f.mul(g),
            t: e.mul(h),
        }
    }

    fn is_small_order(&self, p: &Point) -> bool {
        let mut q = *p;
        for _ in 0..3 {
            q = self.add(&q, &q);
        }
        q.encode() == Point::IDENTITY.encode()
    }

    /// `[a]P + [b]Q`, by one pass of doublings.
    fn double_mul(&self, a: &Scalar, p: &Point, b: &Scalar, q: &Point) -> Point {
        let pq = self.add(p, q);
        let mut acc = Point::IDENTITY;
        for i in (0..256).rev() {
            acc = self.add(&acc, &acc);
            match (bit(a, i), bit(b, i)) {
                (true, false) => acc = self.add(&acc, p),
                (false, true) => acc = self.add(&acc, q),
                (true, true) => acc = self.add(&acc, &pq),
                (false, false) => {}
            }
        }
        acc
    }
}

impl Point {
    const IDENTITY: Point = Point {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    fn neg(&self) -> Point {
        Point {
            x: self.x.neg(),
            y: self.y,
            z: self.z,
            t: self.t.neg(),
        }
    }

    fn encode(&self) -> [u8; 32] {
        let zinv = self.z.invert();
        let x = self.x.mul(zinv);
        let mut out = self.y.mul(zinv).to_bytes();
        out[31] |= u8::from(x.is_negative()) << 7;
        out
    }
}

fn bit(s: &Scalar, i: usize) -> bool {
    (s[i / 64] >> (i % 64)) & 1 == 1
}

fn limbs<const N: usize>(bytes: &[u8]) -> [u64; N] {
    let mut out = [0u64; N];
    for (limb, chunk) in out.iter_mut().zip(bytes.chunks(8)) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        *limb = u64::from_le_bytes(word);
    }
    out
}

fn ge(a: &[u64; 8], b: &[u64; 8]) -> bool {
    for i in (0..8).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// `v mod L`, by long division: `L·2^260` exceeds every 512-bit `v`.
fn reduce(mut v: [u64; 8]) -> Scalar {
    for shift in (0..=259).rev() {
        let mut m = [0u64; 8];
        let (words, bits) = (shift / 64, shift % 64);
        for (i, &limb) in L.iter().enumerate() {
            let at = i + words;
            if at < 8 {
                m[at] |= limb << bits;
            }
            if bits != 0 && at + 1 < 8 {
                m[at + 1] |= limb >> (64 - bits);
            }
        }
        if ge(&v, &m) {
            let mut borrow = 0u64;
            for (a, b) in v.iter_mut().zip(m) {
                let (d, b1) = a.overflowing_sub(b);
                let (d, b2) = d.overflowing_sub(borrow);
                *a = d;
                borrow = u64::from(b1 | b2);
            }
        }
    }
    [v[0], v[1], v[2], v[3]]
}

fn below_l(s: &Scalar) -> bool {
    for i in (0..4).rev() {
        if s[i] != L[i] {
            return s[i] < L[i];
        }
    }
    false
}

fn challenge(r: &[u8], public: &[u8; 32], message: &[u8]) -> Scalar {
    let mut h = Sha512::new();
    h.update(r);
    h.update(public);
    h.update(message);
    let mut digest = [0u8; 64];
    h.finish_into(&mut digest);
    reduce(limbs(&digest))
}

/// Whether `signature` is `public`'s over `message`.
pub fn verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let curve = Curve::new();
    let Some(a) = curve.decode(public) else {
        return false;
    };
    if curve.is_small_order(&a) {
        return false;
    }
    let (r, s) = signature.split_at(32);
    let s: Scalar = limbs(s);
    if !below_l(&s) {
        return false;
    }
    let k = challenge(r, public, message);
    curve.double_mul(&s, &curve.base, &k, &a.neg()).encode() == r
}

/// A signer for tests: its scalar multiplication is not constant-time, so
/// nothing shipped may enable it.
#[cfg(any(test, feature = "test-server"))]
pub struct SigningKey {
    scalar: Scalar,
    prefix: [u8; 32],
    public: [u8; 32],
}

#[cfg(any(test, feature = "test-server"))]
impl SigningKey {
    /// RFC 8032 §5.1.5's key from its 32-byte secret.
    pub fn from_seed(seed: &[u8; 32]) -> SigningKey {
        let mut h = [0u8; 64];
        let mut hash = Sha512::new();
        hash.update(seed);
        hash.finish_into(&mut h);
        h[0] &= 248;
        h[31] &= 127;
        h[31] |= 64;
        let scalar: Scalar = limbs(&h[..32]);
        let curve = Curve::new();
        let public = curve
            .double_mul(&scalar, &curve.base, &[0; 4], &Point::IDENTITY)
            .encode();
        SigningKey {
            scalar,
            prefix: h[32..].try_into().expect("32 bytes"),
            public,
        }
    }

    pub fn public(&self) -> [u8; 32] {
        self.public
    }

    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let curve = Curve::new();
        let mut h = Sha512::new();
        h.update(&self.prefix);
        h.update(message);
        let mut digest = [0u8; 64];
        h.finish_into(&mut digest);
        let r = reduce(limbs(&digest));
        let big_r = curve
            .double_mul(&r, &curve.base, &[0; 4], &Point::IDENTITY)
            .encode();
        let k = challenge(&big_r, &self.public, message);
        let mut wide = [0u64; 8];
        for (i, &ki) in k.iter().enumerate() {
            let mut carry = 0u128;
            for (j, &aj) in self.scalar.iter().enumerate() {
                let t = u128::from(wide[i + j]) + u128::from(ki) * u128::from(aj) + carry;
                wide[i + j] = t as u64;
                carry = t >> 64;
            }
            wide[i + 4] = carry as u64;
        }
        let mut carry = 0u128;
        for (i, limb) in wide.iter_mut().enumerate() {
            let t = u128::from(*limb) + u128::from(r.get(i).copied().unwrap_or(0)) + carry;
            *limb = t as u64;
            carry = t >> 64;
        }
        let s = reduce(wide);
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&big_r);
        for (chunk, limb) in out[32..].chunks_exact_mut(8).zip(s) {
            chunk.copy_from_slice(&limb.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests;
