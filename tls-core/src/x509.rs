//! X.509 certificates (RFC 5280) as the Web PKI uses them: parsing, path
//! building to a trust anchor, and server-name matching (RFC 6125).

use alloc::vec::Vec;

use crate::der::{self, Reader};
use crate::ec::{self, Curve};
use crate::rsa::{self, HashAlg};

mod oid {
    pub const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    pub const SHA256_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b];
    pub const SHA384_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c];
    pub const SHA512_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d];
    pub const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    pub const ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
    pub const ECDSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];
    pub const ECDSA_SHA512: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04];
    pub const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    pub const P384: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x22];
    pub const P521: &[u8] = &[0x2b, 0x81, 0x04, 0x00, 0x23];
    pub const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
    pub const KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
    pub const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
    pub const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1d, 0x11];
    pub const NAME_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x1e];
    pub const CERTIFICATE_POLICIES: &[u8] = &[0x55, 0x1d, 0x20];
    pub const SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
    pub const ANY_EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25, 0x00];
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CertError {
    /// The DER does not parse as a certificate this code understands.
    Malformed,
    /// A signature or key algorithm, or a key size, this code does not do.
    UnsupportedAlgorithm,
    UnknownCriticalExtension,
    Expired,
    NotYetValid,
    /// No chain from the leaf reaches a trust anchor.
    UnknownIssuer,
    BadSignature,
    /// A certificate used as an issuer is not a CA, or not for signing
    /// certificates, or is past its path-length limit.
    NotACa,
    CaUsedAsLeaf,
    /// An extended key usage in the chain excludes TLS server authentication.
    WrongUsage,
    /// The leaf does not name the host being connected to.
    NameMismatch,
    NameConstraintViolation,
    ChainTooLong,
    /// Checking the chain would take more signatures or name comparisons
    /// than any real chain needs.
    TooComplex,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyKind {
    Rsa,
    Ec(Curve),
}

#[derive(Clone, Copy)]
pub struct PublicKey<'a> {
    pub kind: KeyKind,
    /// A DER `RSAPublicKey`, or an uncompressed SEC 1 point.
    pub key: &'a [u8],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignatureAlg {
    RsaPkcs1(HashAlg),
    Ecdsa(HashAlg),
}

pub struct Certificate<'a> {
    pub tbs: &'a [u8],
    /// `None` for an algorithm this code does not verify, which leaves the
    /// certificate usable as an anchor but not as a signed link.
    pub signature_alg: Option<SignatureAlg>,
    pub signature: &'a [u8],
    pub issuer: &'a [u8],
    pub subject: &'a [u8],
    pub spki: &'a [u8],
    pub not_before: i64,
    pub not_after: i64,
    pub public_key: PublicKey<'a>,
    /// `Some(path length limit)` for a CA; `u32::MAX` when unlimited.
    pub ca: Option<u32>,
    /// The keyUsage bits, bit 0 first, when the extension is present.
    pub key_usage: Option<u16>,
    pub server_auth_excluded: bool,
    pub subject_alt_names: Option<&'a [u8]>,
    pub name_constraints: Option<&'a [u8]>,
}

const KEY_CERT_SIGN: u16 = 1 << 5;
const MAX_EXTENSIONS: usize = 32;
const DIGITAL_SIGNATURE: u16 = 1 << 0;

fn algorithm(r: &mut Reader<'_>) -> Option<(Vec<u8>, Option<u8>, Vec<u8>)> {
    let mut alg = r.sequence()?;
    let oid = alg.oid()?.to_vec();
    let params = alg.tlv();
    alg.finish()?;
    Some((
        oid,
        params.map(|p| p.tag),
        params.map(|p| p.value.to_vec()).unwrap_or_default(),
    ))
}

fn signature_alg(oid: &[u8], params: Option<u8>) -> Result<SignatureAlg, CertError> {
    let rsa_params_ok = matches!(params, None | Some(der::NULL));
    let alg = match oid {
        oid::SHA256_WITH_RSA if rsa_params_ok => SignatureAlg::RsaPkcs1(HashAlg::Sha256),
        oid::SHA384_WITH_RSA if rsa_params_ok => SignatureAlg::RsaPkcs1(HashAlg::Sha384),
        oid::SHA512_WITH_RSA if rsa_params_ok => SignatureAlg::RsaPkcs1(HashAlg::Sha512),
        oid::ECDSA_SHA256 if params.is_none() => SignatureAlg::Ecdsa(HashAlg::Sha256),
        oid::ECDSA_SHA384 if params.is_none() => SignatureAlg::Ecdsa(HashAlg::Sha384),
        oid::ECDSA_SHA512 if params.is_none() => SignatureAlg::Ecdsa(HashAlg::Sha512),
        _ => return Err(CertError::UnsupportedAlgorithm),
    };
    Ok(alg)
}

pub fn parse_spki(der: &[u8]) -> Result<PublicKey<'_>, CertError> {
    let mut outer = Reader::new(der);
    let mut spki = outer.sequence().ok_or(CertError::Malformed)?;
    outer.finish().ok_or(CertError::Malformed)?;
    let mut alg = spki.sequence().ok_or(CertError::Malformed)?;
    let key_oid = alg.oid().ok_or(CertError::Malformed)?;
    let kind = match key_oid {
        oid::RSA_ENCRYPTION => {
            alg.optional(der::NULL);
            KeyKind::Rsa
        }
        oid::EC_PUBLIC_KEY => match alg.oid().ok_or(CertError::Malformed)? {
            oid::P256 => KeyKind::Ec(Curve::P256),
            oid::P384 => KeyKind::Ec(Curve::P384),
            oid::P521 => KeyKind::Ec(Curve::P521),
            _ => return Err(CertError::UnsupportedAlgorithm),
        },
        _ => return Err(CertError::UnsupportedAlgorithm),
    };
    alg.finish().ok_or(CertError::Malformed)?;
    let key = spki.bit_string().ok_or(CertError::Malformed)?;
    spki.finish().ok_or(CertError::Malformed)?;
    Ok(PublicKey { kind, key })
}

/// Seconds since the Unix epoch for a proleptic Gregorian date and time.
pub fn unix_time(year: i64, month: u32, day: u32, h: u32, m: u32, s: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s)
}

fn parse_time(tag: u8, v: &[u8]) -> Option<i64> {
    let digits = |s: &[u8]| -> Option<u32> {
        s.iter().try_fold(0u32, |acc, &c| {
            c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0'))
        })
    };
    let (year, rest) = match (tag, v.len()) {
        (der::UTC_TIME, 13) => {
            let yy = i64::from(digits(&v[..2])?);
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &v[2..])
        }
        (der::GENERALIZED_TIME, 15) => (i64::from(digits(&v[..4])?), &v[4..]),
        _ => return None,
    };
    if rest[10] != b'Z' {
        return None;
    }
    let field = |i: usize| digits(&rest[i..i + 2]);
    let (mo, d, h, mi, s) = (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    Some(unix_time(year, mo, d, h, mi, s))
}

impl<'a> Certificate<'a> {
    pub fn parse(der: &'a [u8]) -> Result<Self, CertError> {
        use CertError::Malformed;
        let mut outer = Reader::new(der);
        let mut cert = outer.sequence().ok_or(Malformed)?;
        outer.finish().ok_or(Malformed)?;
        let tbs_tlv = cert.tlv().ok_or(Malformed)?;
        if tbs_tlv.tag != der::SEQUENCE {
            return Err(Malformed);
        }
        let (outer_oid, outer_params_tag, outer_params) = algorithm(&mut cert).ok_or(Malformed)?;
        let signature = cert.bit_string().ok_or(Malformed)?;
        cert.finish().ok_or(Malformed)?;

        let mut tbs = Reader::new(tbs_tlv.value);
        let version = match tbs.optional(der::context(0)) {
            Some(v) => {
                let mut v = Reader::new(v);
                let n = v.small_unsigned().ok_or(Malformed)?;
                v.finish().ok_or(Malformed)?;
                n
            }
            None => 0,
        };
        tbs.expect(der::INTEGER).ok_or(Malformed)?;
        let (inner_oid, inner_params_tag, inner_params) = algorithm(&mut tbs).ok_or(Malformed)?;
        if (&inner_oid, inner_params_tag, &inner_params)
            != (&outer_oid, outer_params_tag, &outer_params)
        {
            return Err(Malformed);
        }
        let signature_alg = signature_alg(&outer_oid, outer_params_tag).ok();
        let issuer = tbs
            .tlv()
            .filter(|t| t.tag == der::SEQUENCE)
            .ok_or(Malformed)?
            .raw;
        let mut validity = tbs.sequence().ok_or(Malformed)?;
        let nb = validity.tlv().ok_or(Malformed)?;
        let na = validity.tlv().ok_or(Malformed)?;
        validity.finish().ok_or(Malformed)?;
        let not_before = parse_time(nb.tag, nb.value).ok_or(Malformed)?;
        let not_after = parse_time(na.tag, na.value).ok_or(Malformed)?;
        let subject = tbs
            .tlv()
            .filter(|t| t.tag == der::SEQUENCE)
            .ok_or(Malformed)?
            .raw;
        let spki = tbs
            .tlv()
            .filter(|t| t.tag == der::SEQUENCE)
            .ok_or(Malformed)?
            .raw;
        let public_key = parse_spki(spki)?;
        tbs.optional(der::context_primitive(1));
        tbs.optional(der::context_primitive(2));

        let mut cert = Certificate {
            tbs: tbs_tlv.raw,
            signature_alg,
            signature,
            issuer,
            subject,
            spki,
            not_before,
            not_after,
            public_key,
            ca: None,
            key_usage: None,
            server_auth_excluded: false,
            subject_alt_names: None,
            name_constraints: None,
        };
        if let Some(exts) = tbs.optional(der::context(3)) {
            if version != 2 {
                return Err(Malformed);
            }
            let mut wrapper = Reader::new(exts);
            let mut list = wrapper.sequence().ok_or(Malformed)?;
            wrapper.finish().ok_or(Malformed)?;
            let mut seen: Vec<&[u8]> = Vec::new();
            while !list.is_empty() {
                let mut ext = list.sequence().ok_or(Malformed)?;
                let id = ext.oid().ok_or(Malformed)?;
                let critical = if ext.peek_tag() == Some(der::BOOLEAN) {
                    ext.boolean().ok_or(Malformed)?
                } else {
                    false
                };
                let value = ext.expect(der::OCTET_STRING).ok_or(Malformed)?;
                ext.finish().ok_or(Malformed)?;
                if seen.len() == MAX_EXTENSIONS || seen.contains(&id) {
                    return Err(Malformed);
                }
                seen.push(id);
                cert.apply_extension(id, critical, value)?;
            }
        }
        tbs.finish().ok_or(Malformed)?;
        Ok(cert)
    }

    fn apply_extension(
        &mut self,
        id: &[u8],
        critical: bool,
        value: &'a [u8],
    ) -> Result<(), CertError> {
        use CertError::Malformed;
        let mut outer = Reader::new(value);
        match id {
            oid::BASIC_CONSTRAINTS => {
                let mut bc = outer.sequence().ok_or(Malformed)?;
                let is_ca = if bc.peek_tag() == Some(der::BOOLEAN) {
                    bc.boolean().ok_or(Malformed)?
                } else {
                    false
                };
                let limit = if bc.is_empty() {
                    u32::MAX
                } else {
                    u32::try_from(bc.small_unsigned().ok_or(Malformed)?).map_err(|_| Malformed)?
                };
                bc.finish().ok_or(Malformed)?;
                self.ca = is_ca.then_some(limit);
            }
            oid::KEY_USAGE => {
                let bits = outer.expect(der::BIT_STRING).ok_or(Malformed)?;
                let (&unused, bytes) = bits.split_first().ok_or(Malformed)?;
                if unused > 7 || bytes.len() > 2 {
                    return Err(Malformed);
                }
                let mut usage = 0u16;
                for (i, byte) in bytes.iter().enumerate() {
                    for bit in 0..8 {
                        if byte & (0x80 >> bit) != 0 {
                            usage |= 1 << (8 * i + bit);
                        }
                    }
                }
                self.key_usage = Some(usage);
            }
            oid::EXT_KEY_USAGE => {
                let mut list = outer.sequence().ok_or(Malformed)?;
                let mut server_auth = false;
                while !list.is_empty() {
                    let purpose = list.oid().ok_or(Malformed)?;
                    server_auth |=
                        purpose == oid::SERVER_AUTH || purpose == oid::ANY_EXTENDED_KEY_USAGE;
                }
                self.server_auth_excluded = !server_auth;
            }
            oid::SUBJECT_ALT_NAME => {
                let names = outer.expect(der::SEQUENCE).ok_or(Malformed)?;
                check_general_names(names)?;
                self.subject_alt_names = Some(names);
            }
            oid::NAME_CONSTRAINTS => {
                let constraints = outer.expect(der::SEQUENCE).ok_or(Malformed)?;
                subtree_sets(constraints)?;
                self.name_constraints = Some(constraints);
            }
            oid::CERTIFICATE_POLICIES => return Ok(()),
            _ if critical => return Err(CertError::UnknownCriticalExtension),
            _ => return Ok(()),
        }
        outer.finish().ok_or(Malformed)
    }

    pub fn signed_by(&self, issuer: &PublicKey<'_>) -> Result<(), CertError> {
        let alg = self.signature_alg.ok_or(CertError::UnsupportedAlgorithm)?;
        verify_signature(issuer, alg, self.tbs, self.signature)
    }
}

pub fn verify_signature(
    key: &PublicKey<'_>,
    alg: SignatureAlg,
    message: &[u8],
    signature: &[u8],
) -> Result<(), CertError> {
    let ok = match (alg, key.kind) {
        (SignatureAlg::RsaPkcs1(hash), KeyKind::Rsa) => {
            let rsa = rsa::PublicKey::from_der(key.key).ok_or(CertError::Malformed)?;
            rsa.verify_pkcs1(hash, message, signature)
        }
        (SignatureAlg::Ecdsa(hash), KeyKind::Ec(curve)) => {
            let (r, s) = ec::parse_der_signature(signature).ok_or(CertError::BadSignature)?;
            ec::verify(curve, key.key, hash.digest(message).as_ref(), r, s)
        }
        _ => return Err(CertError::UnsupportedAlgorithm),
    };
    if ok {
        Ok(())
    } else {
        Err(CertError::BadSignature)
    }
}

#[derive(Clone)]
pub struct Anchor {
    pub subject: Vec<u8>,
    pub spki: Vec<u8>,
    pub name_constraints: Option<Vec<u8>>,
}

#[derive(Clone, Default)]
pub struct TrustStore {
    anchors: Vec<Anchor>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.anchors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.anchors.is_empty()
    }

    /// Trust a DER certificate's subject, key and name constraints; its dates
    /// and signature are not checked.
    pub fn add_der(&mut self, der: &[u8]) -> Result<(), CertError> {
        let cert = Certificate::parse(der)?;
        self.anchors.push(Anchor {
            subject: cert.subject.to_vec(),
            spki: cert.spki.to_vec(),
            name_constraints: cert.name_constraints.map(<[u8]>::to_vec),
        });
        Ok(())
    }

    /// Every `CERTIFICATE` block of a PEM bundle; returns how many were
    /// skipped as unusable.
    pub fn add_pem_bundle(&mut self, pem: &[u8]) -> usize {
        let mut skipped = 0;
        for block in crate::pem::certificates(pem) {
            if !block.is_some_and(|der| self.add_der(&der).is_ok()) {
                skipped += 1;
            }
        }
        skipped
    }
}

/// The key of a leaf proven to chain to an anchor, for CertificateVerify.
pub struct VerifiedLeaf<'a> {
    pub key: PublicKey<'a>,
}

#[derive(Clone, Copy)]
pub enum ServerName<'a> {
    Dns(&'a str),
    Ipv4([u8; 4]),
}

impl<'a> ServerName<'a> {
    /// An IPv4 literal, or a DNS name whose last label is not all digits
    /// (RFC 3696 §2), so no spelling is both.
    pub fn parse(host: &'a str) -> Option<Self> {
        if let Some(ip) = parse_ipv4(host) {
            return Some(ServerName::Ipv4(ip));
        }
        let host = host.strip_suffix('.').unwrap_or(host);
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
            && !host
                .rsplit('.')
                .next()
                .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()));
        valid.then_some(ServerName::Dns(host))
    }
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for slot in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = p.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

const MAX_INTERMEDIATES: usize = 6;
const MAX_SIGNATURE_CHECKS: usize = 64;
const MAX_NAME_COMPARISONS: usize = 250_000;

/// Build and check a path from `chain[0]` to an anchor in `store`.
pub fn verify_server_chain<'c>(
    chain: &[&'c [u8]],
    store: &TrustStore,
    name: ServerName<'_>,
    now: i64,
) -> Result<VerifiedLeaf<'c>, CertError> {
    let leaf_der = chain.first().ok_or(CertError::Malformed)?;
    let leaf = Certificate::parse(leaf_der)?;
    check_time(&leaf, now)?;
    if leaf.ca.is_some() {
        return Err(CertError::CaUsedAsLeaf);
    }
    if leaf.server_auth_excluded {
        return Err(CertError::WrongUsage);
    }
    if leaf.key_usage.is_some_and(|u| u & DIGITAL_SIGNATURE == 0) {
        return Err(CertError::WrongUsage);
    }
    match_name(&leaf, name)?;

    let intermediates: Vec<Certificate<'c>> = chain[1..]
        .iter()
        .filter_map(|der| Certificate::parse(der).ok())
        .collect();
    let mut search = Search {
        leaf: &leaf,
        intermediates: &intermediates,
        store,
        now,
        path: Vec::new(),
        signatures: MAX_SIGNATURE_CHECKS,
        comparisons: MAX_NAME_COMPARISONS,
    };
    search.from(&leaf)?;
    Ok(VerifiedLeaf {
        key: leaf.public_key,
    })
}

fn check_time(cert: &Certificate<'_>, now: i64) -> Result<(), CertError> {
    if now < cert.not_before {
        return Err(CertError::NotYetValid);
    }
    if now > cert.not_after {
        return Err(CertError::Expired);
    }
    Ok(())
}

/// Depth-first search upwards from the leaf; `path` holds the intermediates
/// chosen so far, leaf end first, so none is revisited. The depth limit ends a
/// branch; running out of budget ends the whole search.
struct Search<'s, 'c> {
    leaf: &'s Certificate<'c>,
    intermediates: &'s [Certificate<'c>],
    store: &'s TrustStore,
    now: i64,
    path: Vec<usize>,
    signatures: usize,
    comparisons: usize,
}

impl<'s, 'c> Search<'s, 'c> {
    fn spend(&mut self) -> Result<(), CertError> {
        self.signatures = self
            .signatures
            .checked_sub(1)
            .ok_or(CertError::TooComplex)?;
        Ok(())
    }

    /// Hold everything the issuer at the top of the current path vouches for
    /// to that issuer's name constraints.
    fn constrain(&mut self, constraints: &[u8]) -> Result<(), CertError> {
        let below =
            core::iter::once(self.leaf).chain(self.path.iter().map(|&i| &self.intermediates[i]));
        check_name_constraints(constraints, below, &mut self.comparisons)
    }

    fn from(&mut self, child: &Certificate<'c>) -> Result<(), CertError> {
        let mut best = CertError::UnknownIssuer;
        let store = self.store;
        for anchor in store.anchors.iter().filter(|a| a.subject == child.issuer) {
            self.spend()?;
            let checked = parse_spki(&anchor.spki)
                .and_then(|key| child.signed_by(&key))
                .and_then(|()| match &anchor.name_constraints {
                    Some(nc) => self.constrain(nc),
                    None => Ok(()),
                });
            match checked {
                Ok(()) => return Ok(()),
                Err(CertError::TooComplex) => return Err(CertError::TooComplex),
                Err(e) => best = e,
            }
        }
        if self.path.len() >= MAX_INTERMEDIATES {
            return Err(CertError::ChainTooLong);
        }
        let intermediates = self.intermediates;
        for (i, issuer) in intermediates.iter().enumerate() {
            if issuer.subject != child.issuer || self.path.contains(&i) {
                continue;
            }
            self.spend()?;
            let checked = check_issuer(issuer, self.path.len(), self.now)
                .and_then(|()| child.signed_by(&issuer.public_key))
                .and_then(|()| match issuer.name_constraints {
                    Some(nc) => self.constrain(nc),
                    None => Ok(()),
                })
                .and_then(|()| {
                    self.path.push(i);
                    let above = self.from(issuer);
                    self.path.pop();
                    above
                });
            match checked {
                Ok(()) => return Ok(()),
                Err(CertError::TooComplex) => return Err(CertError::TooComplex),
                Err(e) => best = e,
            }
        }
        Err(best)
    }
}

fn check_issuer(issuer: &Certificate<'_>, below: usize, now: i64) -> Result<(), CertError> {
    check_time(issuer, now)?;
    let Some(limit) = issuer.ca else {
        return Err(CertError::NotACa);
    };
    if (below as u64) > u64::from(limit) {
        return Err(CertError::NotACa);
    }
    if issuer.key_usage.is_some_and(|u| u & KEY_CERT_SIGN == 0) {
        return Err(CertError::NotACa);
    }
    if issuer.server_auth_excluded {
        return Err(CertError::WrongUsage);
    }
    Ok(())
}

const GN_DNS: u8 = der::context_primitive(2);
const GN_IP: u8 = der::context_primitive(7);
const IPV4_OR_IPV6_ADDRESS: [usize; 2] = [4, 16];
const IPV4_OR_IPV6_ADDRESS_AND_MASK: [usize; 2] = [8, 32];

/// Refuse a `GeneralNames` with a name that does not parse: the iterators below
/// stop at the first such name, so the names after it would escape every check.
fn check_general_names(seq: &[u8]) -> Result<(), CertError> {
    let mut r = Reader::new(seq);
    if r.is_empty() {
        return Err(CertError::Malformed);
    }
    while !r.is_empty() {
        check_general_name(r.tlv(), IPV4_OR_IPV6_ADDRESS)?;
    }
    Ok(())
}

fn check_general_name(name: Option<der::Tlv<'_>>, ip_lens: [usize; 2]) -> Result<(), CertError> {
    match name {
        Some(n) if n.tag != GN_IP || ip_lens.contains(&n.value.len()) => Ok(()),
        _ => Err(CertError::Malformed),
    }
}

fn general_names(seq: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut r = Reader::new(seq);
    core::iter::from_fn(move || {
        let t = r.tlv()?;
        Some((t.tag, t.value))
    })
}

/// The permitted and excluded `GeneralSubtrees` of a `NameConstraints`, each a
/// bare base: RFC 5280 §4.2.1.10 profiles `minimum` and `maximum` away.
fn subtree_sets(constraints: &[u8]) -> Result<[&[u8]; 2], CertError> {
    use CertError::Malformed;
    let mut r = Reader::new(constraints);
    let permitted = r.optional(der::context(0));
    let excluded = r.optional(der::context(1));
    r.finish().ok_or(Malformed)?;
    if permitted.is_none() && excluded.is_none() {
        return Err(Malformed);
    }
    for set in [permitted, excluded].into_iter().flatten() {
        let mut subtrees = Reader::new(set);
        if subtrees.is_empty() {
            return Err(Malformed);
        }
        while !subtrees.is_empty() {
            let mut subtree = subtrees.sequence().ok_or(Malformed)?;
            check_general_name(subtree.tlv(), IPV4_OR_IPV6_ADDRESS_AND_MASK)?;
            subtree.finish().ok_or(Malformed)?;
        }
    }
    Ok([permitted.unwrap_or(&[]), excluded.unwrap_or(&[])])
}

fn subtrees(set: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut r = Reader::new(set);
    core::iter::from_fn(move || {
        let base = r.sequence()?.tlv()?;
        Some((base.tag, base.value))
    })
}

fn match_name(leaf: &Certificate<'_>, name: ServerName<'_>) -> Result<(), CertError> {
    let sans = leaf.subject_alt_names.ok_or(CertError::NameMismatch)?;
    let found = general_names(sans).any(|(tag, value)| match (name, tag) {
        (ServerName::Dns(host), GN_DNS) => dns_matches(value, host.as_bytes()),
        (ServerName::Ipv4(ip), GN_IP) => value == ip,
        _ => false,
    });
    if found {
        Ok(())
    } else {
        Err(CertError::NameMismatch)
    }
}

/// RFC 6125 §6.4: case-insensitive, a wildcard only as the whole leftmost label
/// standing for exactly one label and never directly under a single-label
/// suffix. A pattern with a trailing dot matches nothing; the host never has one.
pub fn dns_matches(pattern: &[u8], host: &[u8]) -> bool {
    match pattern.strip_prefix(b"*.") {
        Some(rest) => {
            if !rest.contains(&b'.') || rest.contains(&b'*') {
                return false;
            }
            match host.iter().position(|&b| b == b'.') {
                Some(dot) if dot > 0 => host[dot + 1..].eq_ignore_ascii_case(rest),
                _ => false,
            }
        }
        None => !pattern.contains(&b'*') && pattern.eq_ignore_ascii_case(host),
    }
}

/// Whether `name` lies in the DNS subtree `base` (RFC 5280 §4.2.1.10). A
/// base with a leading dot holds only names below it.
fn in_subtree(name: &[u8], base: &[u8]) -> bool {
    let (base, below_only) = match base.strip_prefix(b".") {
        Some(b) => (b, true),
        None => (base, false),
    };
    if base.is_empty() {
        return true;
    }
    if name.len() == base.len() {
        return !below_only && name.eq_ignore_ascii_case(base);
    }
    name.len() > base.len()
        && name[name.len() - base.len()..].eq_ignore_ascii_case(base)
        && name[name.len() - base.len() - 1] == b'.'
}

/// Whether every name the dNSName `san` stands for lies in `base`. `*.X`
/// stands for every one-label child of `X`.
fn dns_within(san: &[u8], base: &[u8]) -> bool {
    match san.strip_prefix(b"*.") {
        Some(parent) => in_subtree(parent, base.strip_prefix(b".").unwrap_or(base)),
        None => in_subtree(san, base),
    }
}

/// Whether any name the dNSName `san` stands for lies in `base`: `*.X`
/// also reaches a base that is one label below `X`.
fn dns_meets(san: &[u8], base: &[u8]) -> bool {
    let Some(parent) = san.strip_prefix(b"*.") else {
        return in_subtree(san, base);
    };
    dns_within(san, base)
        || base
            .iter()
            .position(|&b| b == b'.')
            .is_some_and(|dot| dot > 0 && base[dot + 1..].eq_ignore_ascii_case(parent))
}

fn ip_in_subtree(ip: &[u8], base: &[u8]) -> bool {
    base.len() == 2 * ip.len()
        && ip
            .iter()
            .zip(&base[..ip.len()])
            .zip(&base[ip.len()..])
            .all(|((a, net), mask)| a & mask == net & mask)
}

/// Hold every dNSName and iPAddress below an issuer to its `NameConstraints`,
/// charging names × bases to `budget` first. Only these two forms can match a
/// server name, so leaving the others unconstrained widens nothing.
fn check_name_constraints<'x, 'c: 'x>(
    constraints: &'x [u8],
    certs: impl Iterator<Item = &'x Certificate<'c>>,
    budget: &mut usize,
) -> Result<(), CertError> {
    let [permitted, excluded] = subtree_sets(constraints)?;
    let bases = subtrees(permitted).count() + subtrees(excluded).count();
    for cert in certs {
        let Some(sans) = cert.subject_alt_names else {
            continue;
        };
        let names = general_names(sans)
            .filter(|&(tag, _)| tag == GN_DNS || tag == GN_IP)
            .count();
        *budget = budget
            .checked_sub(names.saturating_mul(bases))
            .ok_or(CertError::TooComplex)?;
        for (tag, value) in general_names(sans) {
            let (within, meets): (fn(&[u8], &[u8]) -> bool, fn(&[u8], &[u8]) -> bool) = match tag {
                GN_DNS => (dns_within, dns_meets),
                GN_IP => (ip_in_subtree, ip_in_subtree),
                _ => continue,
            };
            if subtrees(excluded).any(|(btag, base)| btag == tag && meets(value, base)) {
                return Err(CertError::NameConstraintViolation);
            }
            let mut of_this_form = subtrees(permitted)
                .filter(|&(btag, _)| btag == tag)
                .peekable();
            if of_this_form.peek().is_some() && !of_this_form.any(|(_, base)| within(value, base)) {
                return Err(CertError::NameConstraintViolation);
            }
        }
    }
    Ok(())
}
