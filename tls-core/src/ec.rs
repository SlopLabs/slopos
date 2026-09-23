//! ECDSA verification on P-256, P-384 and P-521 (FIPS 186-5, SEC 1).

use alloc::vec::Vec;

use crate::bignum::{self, Limbs, Modulus};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Curve {
    P256,
    P384,
    P521,
}

struct Params {
    bytes: usize,
    p: &'static str,
    n: &'static str,
    b: &'static str,
    gx: &'static str,
    gy: &'static str,
}

const P256: Params = Params {
    bytes: 32,
    p: "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
    n: "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
    b: "5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604b",
    gx: "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
    gy: "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
};

const P384: Params = Params {
    bytes: 48,
    p: "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff",
    n: "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973",
    b: "b3312fa7e23ee7e4988e056be3f82d19181d9c6efe8141120314088f5013875ac656398d8a2ed19d2a85c8edd3ec2aef",
    gx: "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7",
    gy: "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f",
};

const P521: Params = Params {
    bytes: 66,
    p: "01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    n: "01fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e91386409",
    b: "0051953eb9618e1c9a1f929a21a0b68540eea2da725b99b315f3b8b489918ef109e156193951ec7e937b1652c0bd3bb1bf073573df883d2c34f1ef451fd46b503f00",
    gx: "00c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d3dbaa14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5bd66",
    gy: "011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e662c97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd16650",
};

fn hex_limbs(hex: &str, k: usize) -> Limbs {
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |c: u8| (c as char).to_digit(16).expect("hex constant") as u8;
            digit(pair[0]) << 4 | digit(pair[1])
        })
        .collect();
    bignum::from_be(&bytes, k).expect("constant fits")
}

/// Field and group arithmetic for one curve, everything in Montgomery form.
pub(crate) struct Group {
    pub(crate) bytes: usize,
    pub(crate) p: Modulus,
    pub(crate) n: Modulus,
    b: Limbs,
    pub(crate) g: Point,
}

/// Jacobian coordinates; `z == 0` is the point at infinity.
#[derive(Clone)]
pub(crate) struct Point {
    x: Limbs,
    y: Limbs,
    z: Limbs,
}

impl Curve {
    pub(crate) fn group(self) -> Group {
        let params = match self {
            Curve::P256 => &P256,
            Curve::P384 => &P384,
            Curve::P521 => &P521,
        };
        let k = params.bytes.div_ceil(8);
        let p = Modulus::new(hex_limbs(params.p, k)).expect("curve prime");
        let n = Modulus::new(hex_limbs(params.n, k)).expect("curve order");
        let b = p.to_mont(&hex_limbs(params.b, k));
        let g = Point {
            x: p.to_mont(&hex_limbs(params.gx, k)),
            y: p.to_mont(&hex_limbs(params.gy, k)),
            z: p.one_mont(),
        };
        Group {
            bytes: params.bytes,
            p,
            n,
            b,
            g,
        }
    }

    pub fn scalar_bytes(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
        }
    }
}

impl Group {
    fn infinity(&self) -> Point {
        let zero = alloc::vec![0u64; self.p.limbs()];
        Point {
            x: self.p.one_mont(),
            y: self.p.one_mont(),
            z: zero,
        }
    }

    /// An uncompressed SEC 1 point that lies on the curve.
    pub(crate) fn decode_point(&self, sec1: &[u8]) -> Option<Point> {
        let len = self.bytes;
        if sec1.len() != 1 + 2 * len || sec1[0] != 0x04 {
            return None;
        }
        let k = self.p.limbs();
        let x = bignum::from_be(&sec1[1..1 + len], k)?;
        let y = bignum::from_be(&sec1[1 + len..], k)?;
        if !self.p.is_reduced(&x) || !self.p.is_reduced(&y) {
            return None;
        }
        let (x, y) = (self.p.to_mont(&x), self.p.to_mont(&y));
        let f = &self.p;
        let y2 = f.mul(&y, &y);
        let x3 = f.mul(&f.mul(&x, &x), &x);
        let three_x = f.add(&f.add(&x, &x), &x);
        let rhs = f.add(&f.sub(&x3, &three_x), &self.b);
        (y2 == rhs).then(|| Point {
            x,
            y,
            z: f.one_mont(),
        })
    }

    #[cfg(any(test, feature = "test-server"))]
    pub(crate) fn encode_point(&self, pt: &Point) -> Option<Vec<u8>> {
        let (x, y) = self.to_affine(pt)?;
        let mut out = alloc::vec![0x04];
        out.extend(bignum::to_be(&x, self.bytes));
        out.extend(bignum::to_be(&y, self.bytes));
        Some(out)
    }

    fn is_infinity(&self, pt: &Point) -> bool {
        bignum::is_zero(&pt.z)
    }

    fn double(&self, pt: &Point) -> Point {
        if self.is_infinity(pt) {
            return pt.clone();
        }
        let f = &self.p;
        let delta = f.mul(&pt.z, &pt.z);
        let gamma = f.mul(&pt.y, &pt.y);
        let beta = f.mul(&pt.x, &gamma);
        let t = f.mul(&f.sub(&pt.x, &delta), &f.add(&pt.x, &delta));
        let alpha = f.add(&f.add(&t, &t), &t);
        let beta4 = f.add(&f.add(&beta, &beta), &f.add(&beta, &beta));
        let x3 = f.sub(&f.mul(&alpha, &alpha), &f.add(&beta4, &beta4));
        let yz = f.add(&pt.y, &pt.z);
        let z3 = f.sub(&f.sub(&f.mul(&yz, &yz), &gamma), &delta);
        let gamma2 = f.mul(&gamma, &gamma);
        let gamma2_8 = {
            let g2 = f.add(&gamma2, &gamma2);
            let g4 = f.add(&g2, &g2);
            f.add(&g4, &g4)
        };
        let y3 = f.sub(&f.mul(&alpha, &f.sub(&beta4, &x3)), &gamma2_8);
        Point {
            x: x3,
            y: y3,
            z: z3,
        }
    }

    fn add(&self, a: &Point, b: &Point) -> Point {
        if self.is_infinity(a) {
            return b.clone();
        }
        if self.is_infinity(b) {
            return a.clone();
        }
        let f = &self.p;
        let z1z1 = f.mul(&a.z, &a.z);
        let z2z2 = f.mul(&b.z, &b.z);
        let u1 = f.mul(&a.x, &z2z2);
        let u2 = f.mul(&b.x, &z1z1);
        let s1 = f.mul(&f.mul(&a.y, &b.z), &z2z2);
        let s2 = f.mul(&f.mul(&b.y, &a.z), &z1z1);
        let h = f.sub(&u2, &u1);
        let r = f.sub(&s2, &s1);
        if bignum::is_zero(&h) {
            return if bignum::is_zero(&r) {
                self.double(a)
            } else {
                self.infinity()
            };
        }
        let hh = f.mul(&h, &h);
        let hhh = f.mul(&h, &hh);
        let v = f.mul(&u1, &hh);
        let x3 = f.sub(&f.sub(&f.mul(&r, &r), &hhh), &f.add(&v, &v));
        let y3 = f.sub(&f.mul(&r, &f.sub(&v, &x3)), &f.mul(&s1, &hhh));
        let z3 = f.mul(&f.mul(&a.z, &b.z), &h);
        Point {
            x: x3,
            y: y3,
            z: z3,
        }
    }

    /// `u1·G + u2·Q` by interleaved double-and-add; scalars are plain limbs.
    fn double_mul(&self, u1: &[u64], q: &Point, u2: &[u64]) -> Point {
        let gq = self.add(&self.g, q);
        let mut acc = self.infinity();
        let top = bignum::bits(u1).max(bignum::bits(u2));
        for i in (0..top).rev() {
            acc = self.double(&acc);
            let b1 = (u1[i / 64] >> (i % 64)) & 1 == 1;
            let b2 = (u2[i / 64] >> (i % 64)) & 1 == 1;
            acc = match (b1, b2) {
                (true, true) => self.add(&acc, &gq),
                (true, false) => self.add(&acc, &self.g),
                (false, true) => self.add(&acc, q),
                (false, false) => acc,
            };
        }
        acc
    }

    #[cfg(any(test, feature = "test-server"))]
    pub(crate) fn mul_g(&self, k: &[u64]) -> Point {
        let zero = alloc::vec![0u64; self.n.limbs()];
        let inf = self.infinity();
        self.double_mul(k, &inf, &zero)
    }

    /// Plain (non-Montgomery) affine coordinates.
    fn to_affine(&self, pt: &Point) -> Option<(Limbs, Limbs)> {
        if self.is_infinity(pt) {
            return None;
        }
        let f = &self.p;
        let zinv = f.invert_prime(&pt.z);
        let zinv2 = f.mul(&zinv, &zinv);
        let x = f.from_mont(&f.mul(&pt.x, &zinv2));
        let y = f.from_mont(&f.mul(&pt.y, &f.mul(&zinv2, &zinv)));
        Some((x, y))
    }

    /// The leftmost bits of `hash`, as many as the order has (SEC 1 §4.1.3).
    pub(crate) fn hash_scalar(&self, hash: &[u8]) -> Limbs {
        let k = self.n.limbs();
        let mut e = bignum::from_be(hash, hash.len().div_ceil(8).max(k)).expect("sized to fit");
        bignum::shr(&mut e, (8 * hash.len()).saturating_sub(self.n.bits()));
        e.truncate(k);
        self.n.reduce_once(&e)
    }

    pub(crate) fn x_mod_n(&self, pt: &Point) -> Option<Limbs> {
        let (x, _) = self.to_affine(pt)?;
        Some(self.n.reduce_once(&x))
    }
}

/// Verify an ECDSA signature given as the two big-endian integers.
pub fn verify(curve: Curve, public: &[u8], hash: &[u8], r: &[u8], s: &[u8]) -> bool {
    let g = curve.group();
    let Some(q) = g.decode_point(public) else {
        return false;
    };
    let k = g.n.limbs();
    let (Some(r), Some(s)) = (bignum::from_be(r, k), bignum::from_be(s, k)) else {
        return false;
    };
    if bignum::is_zero(&r) || bignum::is_zero(&s) || !g.n.is_reduced(&r) || !g.n.is_reduced(&s) {
        return false;
    }
    let n = &g.n;
    let e = g.hash_scalar(hash);
    let w = n.invert_prime(&n.to_mont(&s));
    let u1 = n.from_mont(&n.mul(&n.to_mont(&e), &w));
    let u2 = n.from_mont(&n.mul(&n.to_mont(&r), &w));
    let point = g.double_mul(&u1, &q, &u2);
    g.x_mod_n(&point).is_some_and(|v| v == r)
}

/// Split a DER `ECDSA-Sig-Value` into its two integers.
pub fn parse_der_signature(der: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut outer = crate::der::Reader::new(der);
    let mut seq = outer.sequence()?;
    outer.finish()?;
    let r = seq.unsigned_integer()?;
    let s = seq.unsigned_integer()?;
    seq.finish()?;
    Some((r, s))
}

/// ECDSA signing for the test server: variable-time, so never for a key
/// that matters.
#[cfg(any(test, feature = "test-server"))]
pub mod signing {
    use alloc::vec::Vec;

    use super::Curve;
    use crate::bignum;
    use crate::rsa::HashAlg;

    pub fn public_key(curve: Curve, private: &[u8]) -> Option<Vec<u8>> {
        let g = curve.group();
        let d = bignum::from_be(private, g.n.limbs())?;
        if bignum::is_zero(&d) || !g.n.is_reduced(&d) {
            return None;
        }
        g.encode_point(&g.mul_g(&d))
    }

    /// A DER `ECDSA-Sig-Value` over `hash`, with a nonce derived from the key
    /// and the hash so no randomness is needed.
    pub fn sign(curve: Curve, private: &[u8], hash: &[u8]) -> Vec<u8> {
        let g = curve.group();
        let n = &g.n;
        let d = bignum::from_be(private, n.limbs()).expect("private scalar");
        let e = g.hash_scalar(hash);
        let nonce_hash = match curve {
            Curve::P256 => HashAlg::Sha256,
            Curve::P384 => HashAlg::Sha384,
            Curve::P521 => HashAlg::Sha512,
        };
        for counter in 0u32.. {
            let mut seed = Vec::from(private);
            seed.extend_from_slice(hash);
            seed.extend_from_slice(&counter.to_be_bytes());
            let k = bignum::from_be(nonce_hash.digest(&seed).as_ref(), n.limbs()).expect("fits");
            if bignum::is_zero(&k) || !n.is_reduced(&k) {
                continue;
            }
            let Some(r) = g.x_mod_n(&g.mul_g(&k)) else {
                continue;
            };
            if bignum::is_zero(&r) {
                continue;
            }
            let kinv = n.invert_prime(&n.to_mont(&k));
            let rd = n.mul(&n.to_mont(&r), &n.to_mont(&d));
            let sum = n.add(&n.to_mont(&e), &rd);
            let s = n.from_mont(&n.mul(&kinv, &sum));
            if bignum::is_zero(&s) {
                continue;
            }
            let int = |v: &[u64]| {
                let mut bytes = bignum::to_be(v, g.bytes);
                let lead = bytes.iter().take_while(|&&b| b == 0).count();
                bytes.drain(..lead.min(bytes.len() - 1));
                if bytes[0] & 0x80 != 0 {
                    bytes.insert(0, 0);
                }
                crate::testpki::tlv(crate::der::INTEGER, &bytes)
            };
            let mut body = int(&r);
            body.extend(int(&s));
            return crate::testpki::tlv(crate::der::SEQUENCE, &body);
        }
        unreachable!("the nonce search ends")
    }
}
