//! The client side of a TLS 1.3 handshake (RFC 8446 §4) and the connection
//! after it. Offers X25519 only and refuses a HelloRetryRequest; no PSK or
//! early data, and a CertificateRequest gets an empty Certificate.

use alloc::vec::Vec;

use crate::conn::*;
use crate::ec::{self, Curve};
use crate::hash::Digest;
use crate::rsa::{self, HashAlg};
use crate::suite::{CipherSuite, Protection, Schedule, Transcript};
use crate::x509::{self, KeyKind, ServerName, TrustStore};
use crate::x25519;

const HELLO_RETRY_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

const OFFERED_SIGNATURES: [u16; 9] = [
    SIG_ECDSA_P256_SHA256,
    SIG_ECDSA_P384_SHA384,
    SIG_ECDSA_P521_SHA512,
    SIG_RSA_PSS_SHA256,
    SIG_RSA_PSS_SHA384,
    SIG_RSA_PSS_SHA512,
    SIG_RSA_PKCS1_SHA256,
    SIG_RSA_PKCS1_SHA384,
    SIG_RSA_PKCS1_SHA512,
];

pub struct ClientConfig<'a> {
    /// A DNS name or an IPv4 literal: what the certificate must name.
    pub server_name: &'a str,
    pub trust: &'a TrustStore,
    /// Seconds since the Unix epoch, for certificate validity.
    pub now: i64,
    /// Protocols to offer by ALPN, most preferred first; empty offers none.
    pub alpn: &'a [&'a [u8]],
    pub suites: &'a [CipherSuite],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    ServerHello,
    EncryptedExtensions,
    CertificateOrRequest,
    Certificate,
    CertificateVerify,
    Finished,
    Connected,
    Closed,
}

struct Handshake {
    suite: CipherSuite,
    transcript: Transcript,
    schedule: Schedule,
    client_secret: Digest,
    server_secret: Digest,
    leaf: Option<(KeyKind, Vec<u8>)>,
    cert_request_context: Option<Vec<u8>>,
}

pub struct Client<'a> {
    cfg: ClientConfig<'a>,
    name: ServerName<'a>,
    state: State,
    records: RecordLayer,
    key_share: Option<EphemeralSecret>,
    session_id: [u8; 32],
    client_hello: Vec<u8>,
    suite: Option<CipherSuite>,
    hs: Option<Handshake>,
    alpn: Option<Vec<u8>>,
    failure: Option<Error>,
    sent_close: bool,
}

struct EphemeralSecret([u8; 32]);

impl Drop for EphemeralSecret {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.0);
    }
}

impl<'a> Client<'a> {
    /// Start a handshake; the ClientHello is waiting in
    /// [`take_output`](Self::take_output).
    pub fn new(cfg: ClientConfig<'a>, random: &mut dyn FnMut(&mut [u8])) -> Result<Self, Error> {
        let name = ServerName::parse(cfg.server_name).ok_or(Error::Protocol(
            Alert::InternalError,
            "the server name is not a valid host name",
        ))?;
        if cfg.suites.is_empty() {
            return Err(Error::Protocol(
                Alert::InternalError,
                "no cipher suite enabled",
            ));
        }
        if cfg.alpn.iter().any(|p| p.is_empty() || p.len() > 255) {
            return Err(Error::Protocol(
                Alert::InternalError,
                "an application protocol name is empty or longer than 255 bytes",
            ));
        }
        let mut secret = EphemeralSecret([0; 32]);
        random(&mut secret.0);
        let mut hello_random = [0u8; 32];
        random(&mut hello_random);
        let mut session_id = [0u8; 32];
        random(&mut session_id);
        let public = x25519::x25519(&secret.0, &x25519::BASEPOINT);

        let hello = handshake_message(HS_CLIENT_HELLO, |m| {
            put_u16(m, 0x0303);
            m.extend_from_slice(&hello_random);
            vec8(m, |m| m.extend_from_slice(&session_id));
            vec16(m, |m| {
                for s in cfg.suites {
                    put_u16(m, s.id());
                }
            });
            vec8(m, |m| m.push(0));
            vec16(m, |m| {
                if let ServerName::Dns(host) = name {
                    extension(m, EXT_SERVER_NAME, |m| {
                        vec16(m, |m| {
                            m.push(0);
                            vec16(m, |m| m.extend_from_slice(host.as_bytes()));
                        })
                    });
                }
                extension(m, EXT_SUPPORTED_GROUPS, |m| {
                    vec16(m, |m| put_u16(m, GROUP_X25519))
                });
                extension(m, EXT_SIGNATURE_ALGORITHMS, |m| {
                    vec16(m, |m| {
                        OFFERED_SIGNATURES.iter().for_each(|&s| put_u16(m, s))
                    })
                });
                if !cfg.alpn.is_empty() {
                    extension(m, EXT_ALPN, |m| {
                        vec16(m, |m| {
                            for p in cfg.alpn {
                                vec8(m, |m| m.extend_from_slice(p));
                            }
                        })
                    });
                }
                extension(m, EXT_SUPPORTED_VERSIONS, |m| {
                    vec8(m, |m| put_u16(m, TLS13))
                });
                extension(m, EXT_KEY_SHARE, |m| {
                    vec16(m, |m| {
                        put_u16(m, GROUP_X25519);
                        vec16(m, |m| m.extend_from_slice(&public));
                    })
                });
            });
        });

        let mut records = RecordLayer::default();
        records.send_plain_handshake(&hello);
        records.ccs_allowed = true;
        Ok(Self {
            cfg,
            name,
            state: State::ServerHello,
            records,
            key_share: Some(secret),
            session_id,
            client_hello: hello,
            suite: None,
            hs: None,
            alpn: None,
            failure: None,
            sent_close: false,
        })
    }

    pub fn is_handshaking(&self) -> bool {
        !matches!(self.state, State::Connected | State::Closed)
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// The protocol the server chose by ALPN, if any.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.alpn.as_deref()
    }

    pub fn suite(&self) -> Option<CipherSuite> {
        self.suite
    }

    /// Bytes to send to the server.
    pub fn take_output(&mut self) -> Vec<u8> {
        self.records.take_outgoing()
    }

    pub fn wants_write(&self) -> bool {
        self.records.has_outgoing()
    }

    /// Feed bytes received from the server.
    pub fn read_tls(&mut self, data: &[u8]) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.records.push_incoming(data);
        let result = self.drive();
        if let Err(e) = result {
            self.fail(e);
        }
        result
    }

    /// Whether the server said `close_notify`: after the buffered plaintext,
    /// there is no more.
    pub fn peer_closed(&self) -> bool {
        self.records.peer_closed()
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        self.records.read_plaintext(out)
    }

    pub fn plaintext_available(&self) -> usize {
        self.records.plaintext_available()
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.state != State::Connected || self.sent_close {
            return Err(Error::Closed);
        }
        self.records.send(CONTENT_APPLICATION_DATA, data);
        Ok(())
    }

    /// Queue `close_notify`. Nothing more can be written; what the server
    /// still sends can be read.
    pub fn close(&mut self) {
        if !self.sent_close && self.failure.is_none() {
            self.records.send_alert(Alert::CloseNotify);
            self.sent_close = true;
        }
    }

    fn fail(&mut self, e: Error) {
        if let Some(alert) = e.alert() {
            self.records.send_alert(alert);
        }
        self.failure = Some(e);
        self.state = State::Closed;
        self.key_share = None;
        self.hs = None;
    }

    fn drive(&mut self) -> Result<(), Error> {
        while let Some(msg) = self.records.next_message()? {
            self.handle(msg)?;
        }
        if self.records.peer_closed() && self.state != State::Connected {
            self.state = State::Closed;
            return Err(Error::Protocol(
                Alert::HandshakeFailure,
                "the server closed the connection during the handshake",
            ));
        }
        Ok(())
    }

    fn hs(&mut self) -> &mut Handshake {
        self.hs.as_mut().expect("handshake state after ServerHello")
    }

    fn handle(&mut self, msg: Message) -> Result<(), Error> {
        match (self.state, msg.ty) {
            (State::ServerHello, HS_SERVER_HELLO) => self.server_hello(&msg),
            (State::EncryptedExtensions, HS_ENCRYPTED_EXTENSIONS) => {
                self.encrypted_extensions(&msg)
            }
            (State::CertificateOrRequest, HS_CERTIFICATE_REQUEST) => self.certificate_request(&msg),
            (State::CertificateOrRequest | State::Certificate, HS_CERTIFICATE) => {
                self.certificate(&msg)
            }
            (State::CertificateVerify, HS_CERTIFICATE_VERIFY) => self.certificate_verify(&msg),
            (State::Finished, HS_FINISHED) => self.finished(&msg),
            (State::Connected, HS_NEW_SESSION_TICKET) => Ok(()),
            (State::Connected, HS_KEY_UPDATE) => self.key_update(&msg),
            _ => Err(unexpected(
                "the server sent a handshake message out of order",
            )),
        }
    }

    fn server_hello(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        if p.u16()? != 0x0303 {
            return Err(Error::Protocol(
                Alert::ProtocolVersion,
                "the ServerHello's legacy version is not TLS 1.2's",
            ));
        }
        let random = p.bytes(32)?;
        let session_echo = p.vec8()?.rest();
        let suite_id = p.u16()?;
        let compression = p.u8()?;
        let extensions = p.vec16()?.extensions()?;
        p.finish()?;

        if random == HELLO_RETRY_RANDOM {
            return Err(Error::Protocol(
                Alert::HandshakeFailure,
                "the server asked for a key exchange group other than X25519",
            ));
        }
        if session_echo != self.session_id || compression != 0 {
            return Err(Error::Protocol(
                Alert::IllegalParameter,
                "the ServerHello does not echo the ClientHello",
            ));
        }
        let suite = CipherSuite::from_id(suite_id)
            .filter(|s| self.cfg.suites.contains(s))
            .ok_or(Error::Protocol(
                Alert::IllegalParameter,
                "the server chose a cipher suite not offered",
            ))?;

        let mut version = None;
        let mut share = None;
        for (ty, body) in extensions {
            let mut b = Parser::new(body);
            match ty {
                EXT_SUPPORTED_VERSIONS => version = Some(b.u16()?),
                EXT_KEY_SHARE => {
                    if b.u16()? != GROUP_X25519 {
                        return Err(Error::Protocol(
                            Alert::IllegalParameter,
                            "the server's key share is not X25519",
                        ));
                    }
                    share = Some(b.vec16()?.rest());
                }
                _ => {
                    return Err(Error::Protocol(
                        Alert::UnsupportedExtension,
                        "the ServerHello carries an extension that was not offered",
                    ));
                }
            }
            b.finish()?;
        }
        if version != Some(TLS13) {
            return Err(Error::Protocol(
                Alert::ProtocolVersion,
                "the server does not speak TLS 1.3",
            ));
        }
        let share: [u8; 32] = share
            .ok_or(Error::Protocol(
                Alert::MissingExtension,
                "the ServerHello has no key share",
            ))?
            .try_into()
            .map_err(|_| {
                Error::Protocol(
                    Alert::IllegalParameter,
                    "the server's X25519 share is malformed",
                )
            })?;
        if self.records.mid_message() {
            return Err(unexpected("the ServerHello does not end its record"));
        }

        let secret = self
            .key_share
            .as_ref()
            .expect("key share until ServerHello");
        let mut shared = x25519::shared_secret(&secret.0, &share).ok_or(Error::Protocol(
            Alert::IllegalParameter,
            "the server's X25519 share is degenerate",
        ))?;
        self.key_share = None;
        let mut transcript = Transcript::new(suite.hash());
        transcript.update(&core::mem::take(&mut self.client_hello));
        transcript.update(&msg.raw);
        let (schedule, secrets) =
            Schedule::handshake(suite.hash(), &shared, transcript.current().as_ref());
        crate::ct::wipe(&mut shared);
        self.records.read = Some(Protection::new(suite, secrets.server.clone()));
        self.records.write = Some(Protection::new(suite, secrets.client.clone()));
        self.suite = Some(suite);
        self.hs = Some(Handshake {
            suite,
            transcript,
            schedule,
            client_secret: secrets.client,
            server_secret: secrets.server,
            leaf: None,
            cert_request_context: None,
        });
        self.state = State::EncryptedExtensions;
        Ok(())
    }

    fn encrypted_extensions(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        let extensions = p.vec16()?.extensions()?;
        p.finish()?;
        for (ty, body) in extensions {
            match ty {
                EXT_SERVER_NAME if body.is_empty() && matches!(self.name, ServerName::Dns(_)) => {}
                EXT_SUPPORTED_GROUPS => {}
                EXT_ALPN if !self.cfg.alpn.is_empty() => {
                    let mut b = Parser::new(body);
                    let mut list = b.vec16()?;
                    let chosen = list.vec8()?.rest();
                    list.finish()?;
                    b.finish()?;
                    if !self.cfg.alpn.contains(&chosen) {
                        return Err(Error::Protocol(
                            Alert::IllegalParameter,
                            "the server chose an application protocol not offered",
                        ));
                    }
                    self.alpn = Some(chosen.to_vec());
                }
                _ => {
                    return Err(Error::Protocol(
                        Alert::UnsupportedExtension,
                        "the server sent an extension that was not offered",
                    ));
                }
            }
        }
        self.hs().transcript.update(&msg.raw);
        self.state = State::CertificateOrRequest;
        Ok(())
    }

    fn certificate_request(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        let context = p.vec8()?.rest().to_vec();
        p.vec16()?.extensions()?;
        p.finish()?;
        let hs = self.hs();
        hs.transcript.update(&msg.raw);
        hs.cert_request_context = Some(context);
        self.state = State::Certificate;
        Ok(())
    }

    fn certificate(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        if !p.vec8()?.is_empty() {
            return Err(Error::Protocol(
                Alert::IllegalParameter,
                "the server's Certificate has a request context",
            ));
        }
        let mut list = p.vec24()?;
        p.finish()?;
        let mut chain: Vec<&[u8]> = Vec::new();
        while !list.is_empty() {
            chain.push(list.vec24()?.rest());
            list.vec16()?;
        }
        if chain.is_empty() {
            return Err(Error::Protocol(
                Alert::DecodeError,
                "the server sent no certificate",
            ));
        }
        let leaf = x509::verify_server_chain(&chain, self.cfg.trust, self.name, self.cfg.now)
            .map_err(Error::Certificate)?;
        let leaf = (leaf.key.kind, leaf.key.key.to_vec());
        let hs = self.hs();
        hs.leaf = Some(leaf);
        hs.transcript.update(&msg.raw);
        self.state = State::CertificateVerify;
        Ok(())
    }

    fn certificate_verify(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        let scheme = p.u16()?;
        let signature = p.vec16()?.rest();
        p.finish()?;
        let hs = self.hs();
        let (kind, key) = hs.leaf.as_ref().expect("leaf before CertificateVerify");
        let content = signed_content(
            b"TLS 1.3, server CertificateVerify",
            hs.transcript.current().as_ref(),
        );
        if !verify_scheme(scheme, *kind, key, &content, signature)? {
            return Err(Error::Protocol(
                Alert::DecryptError,
                "the server's CertificateVerify signature is wrong",
            ));
        }
        hs.transcript.update(&msg.raw);
        self.state = State::Finished;
        Ok(())
    }

    fn finished(&mut self, msg: &Message) -> Result<(), Error> {
        let hs = self.hs.as_mut().expect("handshake");
        let hash = hs.suite.hash();
        let expected =
            hash.finished_mac(hs.server_secret.as_ref(), hs.transcript.current().as_ref());
        if !crate::ct::eq(msg.body(), expected.as_ref()) {
            return Err(Error::Protocol(
                Alert::DecryptError,
                "the server's Finished does not verify",
            ));
        }
        if self.records.mid_message() {
            return Err(unexpected("the server's Finished does not end its record"));
        }
        hs.transcript.update(&msg.raw);
        let traffic = hs.schedule.application(hs.transcript.current().as_ref());

        self.records.send_ccs();
        if let Some(context) = hs.cert_request_context.take() {
            let empty = handshake_message(HS_CERTIFICATE, |m| {
                vec8(m, |m| m.extend_from_slice(&context));
                vec24(m, |_| {});
            });
            hs.transcript.update(&empty);
            self.records.send(CONTENT_HANDSHAKE, &empty);
        }
        let verify = hash.finished_mac(hs.client_secret.as_ref(), hs.transcript.current().as_ref());
        let fin = handshake_message(HS_FINISHED, |m| m.extend_from_slice(verify.as_ref()));
        self.records.send(CONTENT_HANDSHAKE, &fin);

        self.records.read = Some(Protection::new(hs.suite, traffic.server));
        self.records.write = Some(Protection::new(hs.suite, traffic.client));
        self.hs = None;
        self.records.ccs_allowed = false;
        self.records.app_data_allowed = true;
        self.state = State::Connected;
        Ok(())
    }

    fn key_update(&mut self, msg: &Message) -> Result<(), Error> {
        let request = match msg.body() {
            [0] => false,
            [1] => true,
            _ => {
                return Err(Error::Protocol(
                    Alert::IllegalParameter,
                    "a KeyUpdate is malformed",
                ));
            }
        };
        if self.records.mid_message() {
            return Err(unexpected("a KeyUpdate does not end its record"));
        }
        let read = self.records.read.as_ref().expect("read keys").updated();
        self.records.read = Some(read);
        if request && !self.sent_close {
            let reply = handshake_message(HS_KEY_UPDATE, |m| m.push(0));
            self.records.send(CONTENT_HANDSHAKE, &reply);
            let write = self.records.write.as_ref().expect("write keys").updated();
            self.records.write = Some(write);
        }
        Ok(())
    }
}

/// What a CertificateVerify signs (RFC 8446 §4.4.3).
pub(crate) fn signed_content(context: &[u8], transcript: &[u8]) -> Vec<u8> {
    let mut content = alloc::vec![0x20u8; 64];
    content.extend_from_slice(context);
    content.push(0);
    content.extend_from_slice(transcript);
    content
}

/// `Err` for a scheme unfit for the key or for TLS 1.3 handshakes, or a key
/// that does not parse; `Ok(false)` for a bad signature.
fn verify_scheme(
    scheme: u16,
    kind: KeyKind,
    key: &[u8],
    content: &[u8],
    sig: &[u8],
) -> Result<bool, Error> {
    let mismatch = Error::Protocol(
        Alert::IllegalParameter,
        "the server signed its handshake with a scheme unfit for its key",
    );
    let ecdsa = |curve: Curve, hash: HashAlg| match ec::parse_der_signature(sig) {
        Some((r, s)) => ec::verify(curve, key, hash.digest(content).as_ref(), r, s),
        None => false,
    };
    Ok(match (scheme, kind) {
        (SIG_ECDSA_P256_SHA256, KeyKind::Ec(Curve::P256)) => ecdsa(Curve::P256, HashAlg::Sha256),
        (SIG_ECDSA_P384_SHA384, KeyKind::Ec(Curve::P384)) => ecdsa(Curve::P384, HashAlg::Sha384),
        (SIG_ECDSA_P521_SHA512, KeyKind::Ec(Curve::P521)) => ecdsa(Curve::P521, HashAlg::Sha512),
        (SIG_RSA_PSS_SHA256 | SIG_RSA_PSS_SHA384 | SIG_RSA_PSS_SHA512, KeyKind::Rsa) => {
            let hash = match scheme {
                SIG_RSA_PSS_SHA256 => HashAlg::Sha256,
                SIG_RSA_PSS_SHA384 => HashAlg::Sha384,
                _ => HashAlg::Sha512,
            };
            let rsa = rsa::PublicKey::from_der(key).ok_or(Error::Protocol(
                Alert::BadCertificate,
                "the server's RSA key does not parse",
            ))?;
            rsa.verify_pss(hash, content, sig)
        }
        _ => return Err(mismatch),
    })
}
