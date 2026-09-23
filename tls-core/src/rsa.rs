//! RSA signature verification (RFC 8017): PKCS #1 v1.5 and PSS.

use alloc::vec::Vec;

use crate::bignum::{self, Modulus};
use crate::hash::{Digest, Hash, Sha256, Sha384, Sha512};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HashAlg {
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlg {
    pub fn digest(self, data: &[u8]) -> Digest {
        match self {
            HashAlg::Sha256 => Sha256::digest(data),
            HashAlg::Sha384 => Sha384::digest(data),
            HashAlg::Sha512 => Sha512::digest(data),
        }
    }

    fn digest_parts(self, parts: &[&[u8]]) -> Digest {
        fn run<H: Hash>(parts: &[&[u8]]) -> Digest {
            let mut h = H::new();
            for p in parts {
                h.update(p);
            }
            h.finish()
        }
        match self {
            HashAlg::Sha256 => run::<Sha256>(parts),
            HashAlg::Sha384 => run::<Sha384>(parts),
            HashAlg::Sha512 => run::<Sha512>(parts),
        }
    }

    pub fn len(self) -> usize {
        match self {
            HashAlg::Sha256 => 32,
            HashAlg::Sha384 => 48,
            HashAlg::Sha512 => 64,
        }
    }

    /// The DER `DigestInfo` that precedes the hash in a v1.5 signature.
    fn digest_info_prefix(self) -> &'static [u8] {
        match self {
            HashAlg::Sha256 => &[
                0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x01, 0x05, 0x00, 0x04, 0x20,
            ],
            HashAlg::Sha384 => &[
                0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x02, 0x05, 0x00, 0x04, 0x30,
            ],
            HashAlg::Sha512 => &[
                0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x03, 0x05, 0x00, 0x04, 0x40,
            ],
        }
    }
}

/// Modulus sizes; any other is refused before any arithmetic.
pub const MODULUS_BITS: core::ops::RangeInclusive<usize> = 2048..=8192;

pub struct PublicKey<'a> {
    pub modulus: &'a [u8],
    pub exponent: &'a [u8],
}

impl<'a> PublicKey<'a> {
    /// A DER `RSAPublicKey`.
    pub fn from_der(der: &'a [u8]) -> Option<Self> {
        let mut outer = crate::der::Reader::new(der);
        let mut seq = outer.sequence()?;
        outer.finish()?;
        let modulus = seq.unsigned_integer()?;
        let exponent = seq.unsigned_integer()?;
        seq.finish()?;
        Some(Self { modulus, exponent })
    }

    /// `signature^e mod n` as a byte string as long as the modulus, and the
    /// modulus's length in bits.
    fn open(&self, signature: &[u8]) -> Option<(Vec<u8>, usize)> {
        let modulus = self.modulus.strip_prefix(&[0]).unwrap_or(self.modulus);
        let top = *modulus.first().filter(|&&b| b != 0)?;
        let bits = modulus.len() * 8 - top.leading_zeros() as usize;
        if !MODULUS_BITS.contains(&bits) {
            return None;
        }
        let n = Modulus::from_be(self.modulus)?;
        let k = bits.div_ceil(8);
        if signature.len() != k {
            return None;
        }
        let e = bignum::from_be(self.exponent, n.limbs())?;
        if bignum::bits(&e) < 2 || e[0] & 1 == 0 || bignum::bits(&e) > 32 {
            return None;
        }
        let s = bignum::from_be(signature, n.limbs())?;
        if !n.is_reduced(&s) {
            return None;
        }
        let m = n.from_mont(&n.pow(&n.to_mont(&s), &e));
        Some((bignum::to_be(&m, k), bits))
    }

    pub fn verify_pkcs1(&self, hash: HashAlg, message: &[u8], signature: &[u8]) -> bool {
        let Some((em, _)) = self.open(signature) else {
            return false;
        };
        let prefix = hash.digest_info_prefix();
        let t_len = prefix.len() + hash.len();
        if em.len() < t_len + 11 {
            return false;
        }
        let pad = em.len() - t_len - 3;
        let mut expected = Vec::with_capacity(em.len());
        expected.extend_from_slice(&[0x00, 0x01]);
        expected.resize(2 + pad, 0xff);
        expected.push(0x00);
        expected.extend_from_slice(prefix);
        expected.extend_from_slice(hash.digest(message).as_ref());
        crate::ct::eq(&em, &expected)
    }

    /// RSASSA-PSS with MGF1 over the same hash and a salt as long as the
    /// hash, the only parameters TLS 1.3 signs with.
    pub fn verify_pss(&self, hash: HashAlg, message: &[u8], signature: &[u8]) -> bool {
        let Some((em, mod_bits)) = self.open(signature) else {
            return false;
        };
        let em_bits = mod_bits - 1;
        let em_len = em_bits.div_ceil(8);
        let (high, em) = em.split_at(em.len() - em_len);
        if high.iter().any(|&b| b != 0) {
            return false;
        }
        let h_len = hash.len();
        let s_len = h_len;
        if em_len < h_len + s_len + 2 || em[em_len - 1] != 0xbc {
            return false;
        }
        let (masked_db, rest) = em.split_at(em_len - h_len - 1);
        let h = &rest[..h_len];
        let top_bits = 8 * em_len - em_bits;
        let top_mask = 0xffu8 >> top_bits;
        if masked_db[0] & !top_mask != 0 {
            return false;
        }
        let mut db = masked_db.to_vec();
        mgf1_xor(hash, h, &mut db);
        db[0] &= top_mask;
        let ps_len = em_len - h_len - s_len - 2;
        if db[..ps_len].iter().any(|&b| b != 0) || db[ps_len] != 0x01 {
            return false;
        }
        let salt = &db[ps_len + 1..];
        let m_hash = hash.digest(message);
        let h2 = hash.digest_parts(&[&[0u8; 8], m_hash.as_ref(), salt]);
        crate::ct::eq(h, h2.as_ref())
    }
}

fn mgf1_xor(hash: HashAlg, seed: &[u8], out: &mut [u8]) {
    for (counter, chunk) in out.chunks_mut(hash.len()).enumerate() {
        let mask = hash.digest_parts(&[seed, &(counter as u32).to_be_bytes()]);
        for (o, m) in chunk.iter_mut().zip(mask.as_ref()) {
            *o ^= m;
        }
    }
}
