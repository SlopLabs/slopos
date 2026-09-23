//! What both ends of a connection share: the record layer, handshake
//! message framing, alerts, and the wire codec.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use crate::suite::{Protection, TAG_LEN};
use crate::x509::CertError;

pub const CONTENT_CHANGE_CIPHER_SPEC: u8 = 20;
pub const CONTENT_ALERT: u8 = 21;
pub const CONTENT_HANDSHAKE: u8 = 22;
pub const CONTENT_APPLICATION_DATA: u8 = 23;

pub const HS_CLIENT_HELLO: u8 = 1;
pub const HS_SERVER_HELLO: u8 = 2;
pub const HS_NEW_SESSION_TICKET: u8 = 4;
pub const HS_ENCRYPTED_EXTENSIONS: u8 = 8;
pub const HS_CERTIFICATE: u8 = 11;
pub const HS_CERTIFICATE_REQUEST: u8 = 13;
pub const HS_CERTIFICATE_VERIFY: u8 = 15;
pub const HS_FINISHED: u8 = 20;
pub const HS_KEY_UPDATE: u8 = 24;

pub const EXT_SERVER_NAME: u16 = 0;
pub const EXT_SUPPORTED_GROUPS: u16 = 10;
pub const EXT_SIGNATURE_ALGORITHMS: u16 = 13;
pub const EXT_ALPN: u16 = 16;
pub const EXT_SUPPORTED_VERSIONS: u16 = 43;
pub const EXT_KEY_SHARE: u16 = 51;

pub const GROUP_X25519: u16 = 0x001d;
pub const TLS13: u16 = 0x0304;

pub const SIG_ECDSA_P256_SHA256: u16 = 0x0403;
pub const SIG_ECDSA_P384_SHA384: u16 = 0x0503;
pub const SIG_ECDSA_P521_SHA512: u16 = 0x0603;
pub const SIG_RSA_PSS_SHA256: u16 = 0x0804;
pub const SIG_RSA_PSS_SHA384: u16 = 0x0805;
pub const SIG_RSA_PSS_SHA512: u16 = 0x0806;
pub const SIG_RSA_PKCS1_SHA256: u16 = 0x0401;
pub const SIG_RSA_PKCS1_SHA384: u16 = 0x0501;
pub const SIG_RSA_PKCS1_SHA512: u16 = 0x0601;

/// A record's plaintext may be this long (RFC 8446 §5.1).
pub const MAX_FRAGMENT: usize = 1 << 14;
const MAX_CIPHERTEXT: usize = MAX_FRAGMENT + 256;
/// Sized for a certificate chain, the largest message either side accepts.
const MAX_HANDSHAKE_MESSAGE: usize = 1 << 17;
const MAX_EXTENSIONS: usize = 64;

/// Alert descriptions (RFC 8446 §6).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Alert {
    CloseNotify = 0,
    UnexpectedMessage = 10,
    BadRecordMac = 20,
    RecordOverflow = 22,
    HandshakeFailure = 40,
    BadCertificate = 42,
    UnsupportedCertificate = 43,
    CertificateExpired = 45,
    IllegalParameter = 47,
    UnknownCa = 48,
    DecodeError = 50,
    DecryptError = 51,
    ProtocolVersion = 70,
    InternalError = 80,
    MissingExtension = 109,
    UnsupportedExtension = 110,
    NoApplicationProtocol = 120,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The peer ended the connection with this fatal alert.
    PeerAlert(u8),
    /// This end found the peer at fault and sent it the alert.
    Protocol(Alert, &'static str),
    /// The server's certificate did not verify.
    Certificate(CertError),
    /// The connection was closed, or failed earlier.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::PeerAlert(code) => {
                write!(f, "the peer sent TLS alert {code} ({})", alert_name(*code))
            }
            Error::Protocol(_, why) => f.write_str(why),
            Error::Certificate(e) => write!(f, "certificate rejected: {}", cert_error_text(*e)),
            Error::Closed => f.write_str("the TLS connection is closed"),
        }
    }
}

pub fn cert_error_text(e: CertError) -> &'static str {
    match e {
        CertError::Malformed => "a certificate could not be parsed",
        CertError::UnsupportedAlgorithm => {
            "a certificate uses an unsupported key or signature algorithm"
        }
        CertError::UnknownCriticalExtension => {
            "a certificate carries an unknown critical extension"
        }
        CertError::Expired => "a certificate has expired",
        CertError::NotYetValid => "a certificate is not yet valid (is the clock right?)",
        CertError::UnknownIssuer => "no trusted root issued it",
        CertError::BadSignature => "a signature in the chain does not verify",
        CertError::NotACa => "an issuer in the chain is not allowed to issue certificates",
        CertError::CaUsedAsLeaf => "the server presented a CA certificate as its own",
        CertError::WrongUsage => "a certificate is not valid for TLS server authentication",
        CertError::NameMismatch => "it does not name the host",
        CertError::NameConstraintViolation => "a name falls outside its issuer's name constraints",
        CertError::ChainTooLong => "the chain is too long",
        CertError::TooComplex => "the chain takes too much work to check",
    }
}

fn alert_name(code: u8) -> &'static str {
    match code {
        0 => "close_notify",
        10 => "unexpected_message",
        20 => "bad_record_mac",
        22 => "record_overflow",
        40 => "handshake_failure",
        42 => "bad_certificate",
        43 => "unsupported_certificate",
        44 => "certificate_revoked",
        45 => "certificate_expired",
        46 => "certificate_unknown",
        47 => "illegal_parameter",
        48 => "unknown_ca",
        49 => "access_denied",
        50 => "decode_error",
        51 => "decrypt_error",
        70 => "protocol_version",
        71 => "insufficient_security",
        80 => "internal_error",
        86 => "inappropriate_fallback",
        90 => "user_canceled",
        109 => "missing_extension",
        110 => "unsupported_extension",
        112 => "unrecognized_name",
        113 => "bad_certificate_status_response",
        115 => "unknown_psk_identity",
        116 => "certificate_required",
        120 => "no_application_protocol",
        _ => "unknown",
    }
}

impl Error {
    pub(crate) fn alert(&self) -> Option<Alert> {
        match self {
            Error::Protocol(a, _) => Some(*a),
            Error::Certificate(e) => Some(match e {
                CertError::Expired | CertError::NotYetValid => Alert::CertificateExpired,
                CertError::UnknownIssuer => Alert::UnknownCa,
                CertError::UnsupportedAlgorithm => Alert::UnsupportedCertificate,
                _ => Alert::BadCertificate,
            }),
            Error::PeerAlert(_) | Error::Closed => None,
        }
    }
}

pub(crate) fn decode_error() -> Error {
    Error::Protocol(Alert::DecodeError, "the peer sent a malformed message")
}

pub(crate) fn unexpected(why: &'static str) -> Error {
    Error::Protocol(Alert::UnexpectedMessage, why)
}

/// Reads the length-prefixed structures of RFC 8446 §3.
#[derive(Clone, Copy)]
pub struct Parser<'a> {
    data: &'a [u8],
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn finish(&self) -> Result<(), Error> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(decode_error())
        }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.data.len() < n {
            return Err(decode_error());
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Error> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u24(&mut self) -> Result<usize, Error> {
        let b = self.bytes(3)?;
        Ok((usize::from(b[0]) << 16) | (usize::from(b[1]) << 8) | usize::from(b[2]))
    }

    pub fn vec8(&mut self) -> Result<Parser<'a>, Error> {
        let n = usize::from(self.u8()?);
        Ok(Parser::new(self.bytes(n)?))
    }

    pub fn vec16(&mut self) -> Result<Parser<'a>, Error> {
        let n = usize::from(self.u16()?);
        Ok(Parser::new(self.bytes(n)?))
    }

    pub fn vec24(&mut self) -> Result<Parser<'a>, Error> {
        let n = self.u24()?;
        Ok(Parser::new(self.bytes(n)?))
    }

    pub fn rest(&mut self) -> &'a [u8] {
        core::mem::take(&mut self.data)
    }

    /// Each `(type, body)` of an extension block, refusing a repeated type.
    pub fn extensions(mut self) -> Result<Vec<(u16, &'a [u8])>, Error> {
        let mut out: Vec<(u16, &[u8])> = Vec::new();
        while !self.is_empty() {
            if out.len() == MAX_EXTENSIONS {
                return Err(Error::Protocol(
                    Alert::DecodeError,
                    "an extension block is too long",
                ));
            }
            let ty = self.u16()?;
            let body = self.vec16()?.rest();
            if out.iter().any(|(t, _)| *t == ty) {
                return Err(Error::Protocol(
                    Alert::IllegalParameter,
                    "an extension was repeated",
                ));
            }
            out.push((ty, body));
        }
        Ok(out)
    }
}

pub fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn with_len(out: &mut Vec<u8>, width: usize, body: impl FnOnce(&mut Vec<u8>)) {
    let at = out.len();
    out.resize(at + width, 0);
    body(out);
    let len = out.len() - at - width;
    assert!(len >> (8 * width) == 0, "a field outgrew its length prefix");
    let bytes = (len as u32).to_be_bytes();
    out[at..at + width].copy_from_slice(&bytes[4 - width..]);
}

pub fn vec8(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    with_len(out, 1, body);
}

pub fn vec16(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    with_len(out, 2, body);
}

pub fn vec24(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) {
    with_len(out, 3, body);
}

pub fn extension(out: &mut Vec<u8>, ty: u16, body: impl FnOnce(&mut Vec<u8>)) {
    put_u16(out, ty);
    vec16(out, body);
}

pub fn handshake_message(ty: u8, body: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut m = alloc::vec![ty];
    vec24(&mut m, body);
    m
}

pub struct Message {
    pub ty: u8,
    /// The whole message, header included, as the transcript hashes it.
    pub raw: Vec<u8>,
}

impl Message {
    pub fn body(&self) -> &[u8] {
        &self.raw[4..]
    }
}

#[derive(Default)]
pub struct RecordLayer {
    incoming: VecDeque<u8>,
    outgoing: Vec<u8>,
    pub read: Option<Protection>,
    pub write: Option<Protection>,
    handshake: VecDeque<u8>,
    plaintext: Vec<u8>,
    plaintext_pos: usize,
    peer_closed: bool,
    /// A middlebox-compatibility ChangeCipherSpec may still arrive.
    pub ccs_allowed: bool,
    pub app_data_allowed: bool,
}

impl RecordLayer {
    pub fn push_incoming(&mut self, data: &[u8]) {
        self.incoming.extend(data);
    }

    pub fn take_outgoing(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outgoing)
    }

    pub fn has_outgoing(&self) -> bool {
        !self.outgoing.is_empty()
    }

    pub fn peer_closed(&self) -> bool {
        self.peer_closed
    }

    /// A handshake message has been read in part: a key change now would
    /// split it across keys, which RFC 8446 §5.1 forbids.
    pub fn mid_message(&self) -> bool {
        !self.handshake.is_empty()
    }

    pub fn send_plain_handshake(&mut self, msg: &[u8]) {
        for chunk in msg.chunks(MAX_FRAGMENT) {
            self.outgoing.extend_from_slice(&[
                CONTENT_HANDSHAKE,
                3,
                1,
                (chunk.len() >> 8) as u8,
                chunk.len() as u8,
            ]);
            self.outgoing.extend_from_slice(chunk);
        }
    }

    pub fn send_ccs(&mut self) {
        self.outgoing
            .extend_from_slice(&[CONTENT_CHANGE_CIPHER_SPEC, 3, 3, 0, 1, 1]);
    }

    pub fn send(&mut self, content_type: u8, data: &[u8]) {
        let protection = self.write.as_mut().expect("write keys installed");
        for chunk in data.chunks(MAX_FRAGMENT) {
            protection.seal(content_type, chunk, &mut self.outgoing);
        }
    }

    pub fn send_alert(&mut self, alert: Alert) {
        let body = [if alert == Alert::CloseNotify { 1 } else { 2 }, alert as u8];
        if self.write.is_some() {
            self.send(CONTENT_ALERT, &body);
        } else {
            self.outgoing
                .extend_from_slice(&[CONTENT_ALERT, 3, 3, 0, 2, body[0], body[1]]);
        }
    }

    pub fn read_plaintext(&mut self, out: &mut [u8]) -> usize {
        let available = &self.plaintext[self.plaintext_pos..];
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.plaintext_pos += n;
        if self.plaintext_pos == self.plaintext.len() {
            self.plaintext.clear();
            self.plaintext_pos = 0;
        }
        n
    }

    pub fn plaintext_available(&self) -> usize {
        self.plaintext.len() - self.plaintext_pos
    }

    /// The next complete handshake message, consuming records until one is
    /// whole. Application data and alerts met on the way are taken in.
    pub fn next_message(&mut self) -> Result<Option<Message>, Error> {
        loop {
            if let Some(msg) = self.split_message()? {
                return Ok(Some(msg));
            }
            if self.peer_closed || !self.next_record()? {
                return Ok(None);
            }
        }
    }

    fn split_message(&mut self) -> Result<Option<Message>, Error> {
        if self.handshake.len() < 4 {
            return Ok(None);
        }
        let len = (usize::from(self.handshake[1]) << 16)
            | (usize::from(self.handshake[2]) << 8)
            | usize::from(self.handshake[3]);
        if len > MAX_HANDSHAKE_MESSAGE {
            return Err(Error::Protocol(
                Alert::DecodeError,
                "a handshake message is too large",
            ));
        }
        if self.handshake.len() < 4 + len {
            return Ok(None);
        }
        let raw: Vec<u8> = self.handshake.drain(..4 + len).collect();
        Ok(Some(Message { ty: raw[0], raw }))
    }

    fn next_record(&mut self) -> Result<bool, Error> {
        if self.incoming.len() < 5 {
            return Ok(false);
        }
        let len = usize::from(u16::from_be_bytes([self.incoming[3], self.incoming[4]]));
        let limit = match self.read {
            Some(_) => MAX_CIPHERTEXT,
            None => MAX_FRAGMENT,
        };
        if len > limit {
            return Err(Error::Protocol(
                Alert::RecordOverflow,
                "the peer sent an oversized record",
            ));
        }
        if self.incoming.len() < 5 + len {
            return Ok(false);
        }
        let mut record = self.incoming.drain(..5 + len);
        let header: [u8; 5] = core::array::from_fn(|_| record.next().expect("5 bytes"));
        let mut body: Vec<u8> = record.collect();

        let outer = header[0];
        if outer == CONTENT_CHANGE_CIPHER_SPEC {
            if !self.ccs_allowed || body != [1] {
                return Err(unexpected("the peer sent a stray ChangeCipherSpec"));
            }
            return Ok(true);
        }
        let (content_type, content) = match self.read.as_mut() {
            None => {
                if outer != CONTENT_HANDSHAKE && outer != CONTENT_ALERT {
                    return Err(unexpected(
                        "the peer sent an unprotected record out of turn",
                    ));
                }
                (outer, &body[..])
            }
            Some(protection) => {
                if outer != CONTENT_APPLICATION_DATA {
                    return Err(unexpected(
                        "the peer sent an unprotected record after keys changed",
                    ));
                }
                if body.len() > MAX_FRAGMENT + 1 + TAG_LEN {
                    return Err(Error::Protocol(
                        Alert::RecordOverflow,
                        "the peer sent an oversized record",
                    ));
                }
                let (ty, n) = protection.open(&header, &mut body).ok_or(Error::Protocol(
                    Alert::BadRecordMac,
                    "a record failed to decrypt",
                ))?;
                (ty, &body[..n])
            }
        };
        match content_type {
            CONTENT_HANDSHAKE if !content.is_empty() => self.handshake.extend(content),
            CONTENT_ALERT => {
                if content.len() != 2 {
                    return Err(decode_error());
                }
                if content[1] == Alert::CloseNotify as u8 {
                    self.peer_closed = true;
                } else {
                    return Err(Error::PeerAlert(content[1]));
                }
            }
            CONTENT_APPLICATION_DATA if self.app_data_allowed => {
                if !self.handshake.is_empty() {
                    return Err(unexpected(
                        "application data interleaved with a handshake message",
                    ));
                }
                self.plaintext.extend_from_slice(content);
            }
            _ => return Err(unexpected("the peer sent a record of an unexpected type")),
        }
        Ok(true)
    }
}
