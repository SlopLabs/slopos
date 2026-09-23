//! Certificates made at run time for tests: a DER writer and an issuer
//! that signs with ECDSA P-256.

use alloc::vec::Vec;

use crate::der;
use crate::ec::{Curve, signing};
use crate::rsa::HashAlg;

pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = alloc::vec![tag];
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = (len as u32).to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (4 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(der::SEQUENCE, &parts.concat())
}

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const OID_SHA1_WITH_RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05];
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x0f];
const OID_EXT_KEY_USAGE: &[u8] = &[0x55, 0x1d, 0x25];
const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];
const OID_NAME_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x1e];
const OID_SERVER_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
const OID_CLIENT_AUTH: &[u8] = &[0x2b, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];
/// An arbitrary private-arc OID no verifier knows.
const OID_UNKNOWN: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x99, 0x01];

pub fn name(common_name: &str) -> Vec<u8> {
    let atv = seq(&[
        &tlv(der::OID, OID_CN),
        &tlv(der::UTF8_STRING, common_name.as_bytes()),
    ]);
    seq(&[&tlv(der::SET, &atv)])
}

fn time(unix: i64) -> Vec<u8> {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let clock = alloc::format!(
        "{month:02}{day:02}{:02}{:02}{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    if (1950..2050).contains(&year) {
        tlv(
            der::UTC_TIME,
            alloc::format!("{:02}{clock}", year % 100).as_bytes(),
        )
    } else {
        tlv(
            der::GENERALIZED_TIME,
            alloc::format!("{year:04}{clock}").as_bytes(),
        )
    }
}

pub enum AltName<'a> {
    Dns(&'a str),
    Ip([u8; 4]),
    Raw(&'a [u8]),
}

fn general_names(names: &[AltName<'_>]) -> Vec<u8> {
    let mut body = Vec::new();
    for n in names {
        body.extend(match n {
            AltName::Dns(d) => tlv(der::context_primitive(2), d.as_bytes()),
            AltName::Ip(ip) => tlv(der::context_primitive(7), ip),
            AltName::Raw(bytes) => bytes.to_vec(),
        });
    }
    body
}

pub struct Profile<'a> {
    pub subject: &'a str,
    pub not_before: i64,
    pub not_after: i64,
    /// `Some(path length limit)` makes it a CA.
    pub ca: Option<Option<u8>>,
    pub alt_names: &'a [AltName<'a>],
    /// `Some(false)` names only client authentication.
    pub server_auth: Option<bool>,
    pub permitted_dns: &'a [&'a str],
    pub excluded_dns: &'a [&'a str],
    /// Raw bytes ahead of the subtrees in each constraint set.
    pub leading_subtree: &'a [u8],
    pub unknown_critical: bool,
    /// Carry a sha1WithRSAEncryption signature this code does not verify.
    pub sha1_signature: bool,
}

impl<'a> Profile<'a> {
    pub fn leaf(subject: &'a str, alt_names: &'a [AltName<'a>]) -> Self {
        Self {
            subject,
            not_before: 1_577_836_800,
            not_after: 4_102_444_800,
            ca: None,
            alt_names,
            server_auth: Some(true),
            permitted_dns: &[],
            excluded_dns: &[],
            leading_subtree: &[],
            unknown_critical: false,
            sha1_signature: false,
        }
    }

    pub fn ca(subject: &'a str) -> Self {
        Self {
            ca: Some(None),
            server_auth: None,
            ..Self::leaf(subject, &[])
        }
    }
}

fn extension(oid: &[u8], critical: bool, value: &[u8]) -> Vec<u8> {
    let crit = if critical {
        tlv(der::BOOLEAN, &[0xff])
    } else {
        Vec::new()
    };
    seq(&[&tlv(der::OID, oid), &crit, &tlv(der::OCTET_STRING, value)])
}

pub struct Key {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

impl Key {
    pub fn from_seed(seed: &[u8]) -> Self {
        for counter in 0u8.. {
            let mut input = Vec::from(seed);
            input.push(counter);
            let private = HashAlg::Sha256.digest(&input).as_ref().to_vec();
            if let Some(public) = signing::public_key(Curve::P256, &private) {
                return Self { private, public };
            }
        }
        unreachable!("a scalar in range turns up")
    }

    pub fn spki(&self) -> Vec<u8> {
        let alg = seq(&[&tlv(der::OID, OID_EC_PUBLIC_KEY), &tlv(der::OID, OID_P256)]);
        let mut bits = alloc::vec![0u8];
        bits.extend_from_slice(&self.public);
        seq(&[&alg, &tlv(der::BIT_STRING, &bits)])
    }
}

pub fn issue(
    profile: &Profile<'_>,
    subject_key: &Key,
    issuer: &str,
    issuer_key: &Key,
    serial: u8,
) -> Vec<u8> {
    let mut exts = Vec::new();
    match profile.ca {
        Some(limit) => {
            let limit = limit.map(|l| tlv(der::INTEGER, &[l])).unwrap_or_default();
            exts.extend(extension(
                OID_BASIC_CONSTRAINTS,
                true,
                &seq(&[&tlv(der::BOOLEAN, &[0xff]), &limit]),
            ));
            exts.extend(extension(
                OID_KEY_USAGE,
                true,
                &tlv(der::BIT_STRING, &[1, 0x06]),
            ));
        }
        None => {
            exts.extend(extension(
                OID_KEY_USAGE,
                true,
                &tlv(der::BIT_STRING, &[7, 0x80]),
            ));
        }
    }
    if let Some(server) = profile.server_auth {
        let oid = if server {
            OID_SERVER_AUTH
        } else {
            OID_CLIENT_AUTH
        };
        exts.extend(extension(
            OID_EXT_KEY_USAGE,
            false,
            &seq(&[&tlv(der::OID, oid)]),
        ));
    }
    if !profile.alt_names.is_empty() {
        exts.extend(extension(
            OID_SAN,
            false,
            &tlv(der::SEQUENCE, &general_names(profile.alt_names)),
        ));
    }
    if !profile.permitted_dns.is_empty() || !profile.excluded_dns.is_empty() {
        let subtrees = |names: &[&str]| {
            let mut list = profile.leading_subtree.to_vec();
            for dns in names {
                list.extend(seq(&[&tlv(der::context_primitive(2), dns.as_bytes())]));
            }
            list
        };
        let mut nc = Vec::new();
        if !profile.permitted_dns.is_empty() {
            nc.extend(tlv(der::context(0), &subtrees(profile.permitted_dns)));
        }
        if !profile.excluded_dns.is_empty() {
            nc.extend(tlv(der::context(1), &subtrees(profile.excluded_dns)));
        }
        exts.extend(extension(
            OID_NAME_CONSTRAINTS,
            true,
            &tlv(der::SEQUENCE, &nc),
        ));
    }
    if profile.unknown_critical {
        exts.extend(extension(OID_UNKNOWN, true, &tlv(der::NULL, &[])));
    }

    let sig_alg = if profile.sha1_signature {
        seq(&[&tlv(der::OID, OID_SHA1_WITH_RSA), &tlv(der::NULL, &[])])
    } else {
        seq(&[&tlv(der::OID, OID_ECDSA_SHA256)])
    };
    let tbs = seq(&[
        &tlv(der::context(0), &tlv(der::INTEGER, &[2])),
        &tlv(der::INTEGER, &[serial & 0x7f]),
        &sig_alg,
        &name(issuer),
        &seq(&[&time(profile.not_before), &time(profile.not_after)]),
        &name(profile.subject),
        &subject_key.spki(),
        &tlv(der::context(3), &tlv(der::SEQUENCE, &exts)),
    ]);
    let hash = HashAlg::Sha256.digest(&tbs);
    let signature = signing::sign(Curve::P256, &issuer_key.private, hash.as_ref());
    let mut bits = alloc::vec![0u8];
    bits.extend(signature);
    seq(&[&tbs, &sig_alg, &tlv(der::BIT_STRING, &bits)])
}
