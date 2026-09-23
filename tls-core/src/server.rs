//! A TLS 1.3 server for tests: one P-256 certificate chain, no HelloRetryRequest
//! or PSK. Its signer is variable-time, so only tests and `test-server` build it.

use alloc::vec::Vec;

use crate::client::signed_content;
use crate::conn::*;
use crate::ec::{Curve, signing};
use crate::hash::Digest;
use crate::rsa::HashAlg;
use crate::suite::{CipherSuite, Protection, Schedule, Transcript};
use crate::x25519;

pub struct ServerConfig<'a> {
    /// DER certificates, the server's own first.
    pub chain: &'a [Vec<u8>],
    /// The P-256 private scalar of the first certificate.
    pub key: &'a [u8],
    /// In order of preference.
    pub suites: &'a [CipherSuite],
    pub alpn: &'a [&'a [u8]],
    /// Ask the client for a certificate, and carry on without one.
    pub request_certificate: bool,
    /// The server's random and its X25519 secret.
    pub entropy: [u8; 64],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    ClientHello,
    ClientCertificate,
    ClientFinished,
    Connected,
    Closed,
}

pub struct Server<'a> {
    cfg: ServerConfig<'a>,
    state: State,
    records: RecordLayer,
    suite: Option<CipherSuite>,
    transcript: Option<Transcript>,
    client_hs: Option<Digest>,
    client_app: Option<Digest>,
    failure: Option<Error>,
}

impl<'a> Server<'a> {
    pub fn new(cfg: ServerConfig<'a>) -> Self {
        Self {
            cfg,
            state: State::ClientHello,
            records: RecordLayer::default(),
            suite: None,
            transcript: None,
            client_hs: None,
            client_app: None,
            failure: None,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    pub fn suite(&self) -> Option<CipherSuite> {
        self.suite
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        self.records.take_outgoing()
    }

    pub fn read_tls(&mut self, data: &[u8]) -> Result<(), Error> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        self.records.push_incoming(data);
        let result = self.drive();
        if let Err(e) = result {
            if let Some(alert) = e.alert() {
                self.records.send_alert(alert);
            }
            self.failure = Some(e);
            self.state = State::Closed;
        }
        result
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        self.records.read_plaintext(out)
    }

    pub fn peer_closed(&self) -> bool {
        self.records.peer_closed()
    }

    /// Application data may go out once the server's flight has: RFC 8446
    /// lets a server send before the client's Finished.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        match self.state {
            State::ClientHello | State::Closed => Err(Error::Closed),
            _ => {
                self.records.send(CONTENT_APPLICATION_DATA, data);
                Ok(())
            }
        }
    }

    pub fn close(&mut self) {
        if self.state != State::Closed {
            self.records.send_alert(Alert::CloseNotify);
            self.state = State::Closed;
        }
    }

    /// Rotate the sending key, optionally asking the peer to do the same.
    pub fn key_update(&mut self, request: bool) {
        let msg = handshake_message(HS_KEY_UPDATE, |m| m.push(u8::from(request)));
        self.records.send(CONTENT_HANDSHAKE, &msg);
        let next = self.records.write.as_ref().expect("keys").updated();
        self.records.write = Some(next);
    }

    fn drive(&mut self) -> Result<(), Error> {
        while let Some(msg) = self.records.next_message()? {
            match (self.state, msg.ty) {
                (State::ClientHello, HS_CLIENT_HELLO) => self.client_hello(&msg)?,
                (State::ClientCertificate, HS_CERTIFICATE) => self.client_certificate(&msg)?,
                (State::ClientFinished, HS_FINISHED) => self.client_finished(&msg)?,
                (State::Connected, HS_KEY_UPDATE) => self.peer_key_update(&msg)?,
                _ => {
                    return Err(unexpected(
                        "the client sent a handshake message out of order",
                    ));
                }
            }
        }
        Ok(())
    }

    fn client_hello(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        p.u16()?;
        p.bytes(32)?;
        let session_id = p.vec8()?.rest().to_vec();
        let mut offered = p.vec16()?;
        let mut client_suites = Vec::new();
        while !offered.is_empty() {
            client_suites.push(offered.u16()?);
        }
        p.vec8()?;
        let extensions = p.vec16()?.extensions()?;
        p.finish()?;

        let mut tls13 = false;
        let mut share = None;
        let mut ecdsa = false;
        let mut client_alpn: Vec<&[u8]> = Vec::new();
        for (ty, body) in &extensions {
            let mut b = Parser::new(body);
            match *ty {
                EXT_SUPPORTED_VERSIONS => {
                    let mut v = b.vec8()?;
                    while !v.is_empty() {
                        tls13 |= v.u16()? == TLS13;
                    }
                }
                EXT_KEY_SHARE => {
                    let mut list = b.vec16()?;
                    while !list.is_empty() {
                        let group = list.u16()?;
                        let key = list.vec16()?.rest();
                        if group == GROUP_X25519 {
                            share = Some(key);
                        }
                    }
                }
                EXT_SIGNATURE_ALGORITHMS => {
                    let mut list = b.vec16()?;
                    while !list.is_empty() {
                        ecdsa |= list.u16()? == SIG_ECDSA_P256_SHA256;
                    }
                }
                EXT_ALPN => {
                    let mut list = b.vec16()?;
                    while !list.is_empty() {
                        client_alpn.push(list.vec8()?.rest());
                    }
                }
                _ => {}
            }
        }
        if !tls13 {
            return Err(Error::Protocol(
                Alert::ProtocolVersion,
                "the client does not offer TLS 1.3",
            ));
        }
        let share: [u8; 32] = share
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::Protocol(
                Alert::HandshakeFailure,
                "the client offers no X25519 share",
            ))?;
        if !ecdsa {
            return Err(Error::Protocol(
                Alert::HandshakeFailure,
                "the client does not accept ECDSA P-256",
            ));
        }
        let suite = self
            .cfg
            .suites
            .iter()
            .copied()
            .find(|s| client_suites.contains(&s.id()))
            .ok_or(Error::Protocol(
                Alert::HandshakeFailure,
                "no cipher suite in common",
            ))?;
        let alpn = self
            .cfg
            .alpn
            .iter()
            .find(|p| client_alpn.contains(p))
            .copied();
        if !client_alpn.is_empty() && !self.cfg.alpn.is_empty() && alpn.is_none() {
            return Err(Error::Protocol(
                Alert::NoApplicationProtocol,
                "no application protocol in common",
            ));
        }

        let secret: [u8; 32] = self.cfg.entropy[32..].try_into().expect("32 bytes");
        let public = x25519::x25519(&secret, &x25519::BASEPOINT);
        let shared = x25519::shared_secret(&secret, &share).ok_or(Error::Protocol(
            Alert::IllegalParameter,
            "the client's X25519 share is degenerate",
        ))?;

        let hello = handshake_message(HS_SERVER_HELLO, |m| {
            put_u16(m, 0x0303);
            m.extend_from_slice(&self.cfg.entropy[..32]);
            vec8(m, |m| m.extend_from_slice(&session_id));
            put_u16(m, suite.id());
            m.push(0);
            vec16(m, |m| {
                extension(m, EXT_SUPPORTED_VERSIONS, |m| put_u16(m, TLS13));
                extension(m, EXT_KEY_SHARE, |m| {
                    put_u16(m, GROUP_X25519);
                    vec16(m, |m| m.extend_from_slice(&public));
                });
            });
        });
        let hash = suite.hash();
        let mut transcript = Transcript::new(hash);
        transcript.update(&msg.raw);
        transcript.update(&hello);
        let (schedule, secrets) = Schedule::handshake(hash, &shared, transcript.current().as_ref());

        self.records.send_plain_handshake(&hello);
        self.records.send_ccs();
        self.records.write = Some(Protection::new(suite, secrets.server.clone()));
        self.records.read = Some(Protection::new(suite, secrets.client.clone()));
        self.records.ccs_allowed = true;

        let mut flight = Vec::new();
        let mut add = |m: Vec<u8>, t: &mut Transcript| {
            t.update(&m);
            flight.extend(m);
        };
        add(
            handshake_message(HS_ENCRYPTED_EXTENSIONS, |m| {
                vec16(m, |m| {
                    if let Some(p) = alpn {
                        extension(m, EXT_ALPN, |m| {
                            vec16(m, |m| vec8(m, |m| m.extend_from_slice(p)))
                        });
                    }
                })
            }),
            &mut transcript,
        );
        if self.cfg.request_certificate {
            add(
                handshake_message(HS_CERTIFICATE_REQUEST, |m| {
                    vec8(m, |_| {});
                    vec16(m, |m| {
                        extension(m, EXT_SIGNATURE_ALGORITHMS, |m| {
                            vec16(m, |m| put_u16(m, SIG_ECDSA_P256_SHA256))
                        })
                    });
                }),
                &mut transcript,
            );
        }
        add(
            handshake_message(HS_CERTIFICATE, |m| {
                vec8(m, |_| {});
                vec24(m, |m| {
                    for cert in self.cfg.chain {
                        vec24(m, |m| m.extend_from_slice(cert));
                        vec16(m, |_| {});
                    }
                });
            }),
            &mut transcript,
        );
        let content = signed_content(
            b"TLS 1.3, server CertificateVerify",
            transcript.current().as_ref(),
        );
        let signature = signing::sign(
            Curve::P256,
            self.cfg.key,
            HashAlg::Sha256.digest(&content).as_ref(),
        );
        add(
            handshake_message(HS_CERTIFICATE_VERIFY, |m| {
                put_u16(m, SIG_ECDSA_P256_SHA256);
                vec16(m, |m| m.extend_from_slice(&signature));
            }),
            &mut transcript,
        );
        let verify = hash.finished_mac(secrets.server.as_ref(), transcript.current().as_ref());
        add(
            handshake_message(HS_FINISHED, |m| m.extend_from_slice(verify.as_ref())),
            &mut transcript,
        );
        self.records.send(CONTENT_HANDSHAKE, &flight);

        let traffic = schedule.application(transcript.current().as_ref());
        self.records.write = Some(Protection::new(suite, traffic.server));
        self.client_hs = Some(secrets.client);
        self.client_app = Some(traffic.client);
        self.suite = Some(suite);
        self.transcript = Some(transcript);
        self.state = if self.cfg.request_certificate {
            State::ClientCertificate
        } else {
            State::ClientFinished
        };
        Ok(())
    }

    fn client_certificate(&mut self, msg: &Message) -> Result<(), Error> {
        let mut p = Parser::new(msg.body());
        p.vec8()?;
        if !p.vec24()?.is_empty() {
            return Err(Error::Protocol(
                Alert::UnsupportedCertificate,
                "client certificates are not verified",
            ));
        }
        p.finish()?;
        self.transcript
            .as_mut()
            .expect("transcript")
            .update(&msg.raw);
        self.state = State::ClientFinished;
        Ok(())
    }

    fn client_finished(&mut self, msg: &Message) -> Result<(), Error> {
        let suite = self.suite.expect("suite");
        let transcript = self.transcript.as_mut().expect("transcript");
        let expected = suite.hash().finished_mac(
            self.client_hs.as_ref().expect("secret").as_ref(),
            transcript.current().as_ref(),
        );
        if !crate::ct::eq(msg.body(), expected.as_ref()) {
            return Err(Error::Protocol(
                Alert::DecryptError,
                "the client's Finished does not verify",
            ));
        }
        if self.records.mid_message() {
            return Err(unexpected("the client's Finished does not end its record"));
        }
        let secret = self.client_app.take().expect("secret");
        self.records.read = Some(Protection::new(suite, secret));
        self.records.ccs_allowed = false;
        self.records.app_data_allowed = true;
        self.client_hs = None;
        self.state = State::Connected;
        Ok(())
    }

    fn peer_key_update(&mut self, msg: &Message) -> Result<(), Error> {
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
        let next = self.records.read.as_ref().expect("keys").updated();
        self.records.read = Some(next);
        if request {
            self.key_update(false);
        }
        Ok(())
    }
}
