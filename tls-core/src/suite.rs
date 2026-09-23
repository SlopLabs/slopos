//! Cipher suites and the TLS 1.3 key schedule (RFC 8446 §7).

use alloc::vec::Vec;

use crate::chacha::ChaCha20Poly1305;
use crate::gcm::AesGcm;
use crate::hash::{Digest, Hash, Hmac, Sha256, Sha384, hkdf_expand, hkdf_extract};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CipherSuite {
    Aes128GcmSha256,
    Aes256GcmSha384,
    Chacha20Poly1305Sha256,
}

impl CipherSuite {
    pub const ALL: [CipherSuite; 3] = [
        CipherSuite::Aes128GcmSha256,
        CipherSuite::Chacha20Poly1305Sha256,
        CipherSuite::Aes256GcmSha384,
    ];

    pub fn id(self) -> u16 {
        match self {
            CipherSuite::Aes128GcmSha256 => 0x1301,
            CipherSuite::Aes256GcmSha384 => 0x1302,
            CipherSuite::Chacha20Poly1305Sha256 => 0x1303,
        }
    }

    pub fn from_id(id: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn hash(self) -> HashKind {
        match self {
            CipherSuite::Aes256GcmSha384 => HashKind::Sha384,
            _ => HashKind::Sha256,
        }
    }

    fn key_len(self) -> usize {
        match self {
            CipherSuite::Aes128GcmSha256 => 16,
            _ => 32,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CipherSuite::Aes128GcmSha256 => "TLS_AES_128_GCM_SHA256",
            CipherSuite::Aes256GcmSha384 => "TLS_AES_256_GCM_SHA384",
            CipherSuite::Chacha20Poly1305Sha256 => "TLS_CHACHA20_POLY1305_SHA256",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HashKind {
    Sha256,
    Sha384,
}

#[derive(Clone)]
pub enum Transcript {
    Sha256(Sha256),
    Sha384(Sha384),
}

impl Transcript {
    pub fn new(kind: HashKind) -> Self {
        match kind {
            HashKind::Sha256 => Transcript::Sha256(Sha256::new()),
            HashKind::Sha384 => Transcript::Sha384(Sha384::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            Transcript::Sha256(h) => h.update(data),
            Transcript::Sha384(h) => h.update(data),
        }
    }

    pub fn current(&self) -> Digest {
        match self {
            Transcript::Sha256(h) => h.clone().finish(),
            Transcript::Sha384(h) => h.clone().finish(),
        }
    }
}

impl HashKind {
    pub fn len(self) -> usize {
        match self {
            HashKind::Sha256 => 32,
            HashKind::Sha384 => 48,
        }
    }

    pub fn digest(self, data: &[u8]) -> Digest {
        match self {
            HashKind::Sha256 => Sha256::digest(data),
            HashKind::Sha384 => Sha384::digest(data),
        }
    }

    pub fn hmac(self, key: &[u8], data: &[u8]) -> Digest {
        match self {
            HashKind::Sha256 => Hmac::<Sha256>::mac(key, data),
            HashKind::Sha384 => Hmac::<Sha384>::mac(key, data),
        }
    }

    pub fn extract(self, salt: &[u8], ikm: &[u8]) -> Digest {
        match self {
            HashKind::Sha256 => hkdf_extract::<Sha256>(salt, ikm),
            HashKind::Sha384 => hkdf_extract::<Sha384>(salt, ikm),
        }
    }

    pub fn expand_label(self, secret: &[u8], label: &[u8], context: &[u8], out: &mut [u8]) {
        let len = (out.len() as u16).to_be_bytes();
        let label_len = [6 + label.len() as u8];
        let context_len = [context.len() as u8];
        let info: [&[u8]; 6] = [&len, &label_len, b"tls13 ", label, &context_len, context];
        match self {
            HashKind::Sha256 => hkdf_expand::<Sha256>(secret, &info, out),
            HashKind::Sha384 => hkdf_expand::<Sha384>(secret, &info, out),
        }
    }

    pub fn derive_secret(self, secret: &[u8], label: &[u8], transcript: &[u8]) -> Digest {
        let mut out = Digest::empty(self.len());
        self.expand_label(secret, label, transcript, out.as_mut());
        out
    }

    pub fn finished_mac(self, traffic_secret: &[u8], transcript: &[u8]) -> Digest {
        let key = self.derive_secret(traffic_secret, b"finished", &[]);
        self.hmac(key.as_ref(), transcript)
    }

    /// The next traffic secret after a KeyUpdate.
    pub fn next_secret(self, secret: &[u8]) -> Digest {
        self.derive_secret(secret, b"traffic upd", &[])
    }
}

/// The key schedule (RFC 8446 §7.1) between the handshake secret and the
/// application traffic secrets.
pub struct Schedule {
    hash: HashKind,
    handshake: Digest,
}

pub struct HandshakeSecrets {
    pub client: Digest,
    pub server: Digest,
}

pub struct TrafficSecrets {
    pub client: Digest,
    pub server: Digest,
}

impl Schedule {
    /// From the (EC)DHE shared secret and the hash of ClientHello..ServerHello.
    pub fn handshake(hash: HashKind, shared: &[u8], hello_hash: &[u8]) -> (Self, HandshakeSecrets) {
        let zeros = [0u8; 48];
        let zeros = &zeros[..hash.len()];
        let early = hash.extract(&[], zeros);
        let empty = hash.digest(&[]);
        let derived = hash.derive_secret(early.as_ref(), b"derived", empty.as_ref());
        let handshake = hash.extract(derived.as_ref(), shared);
        let secrets = HandshakeSecrets {
            client: hash.derive_secret(handshake.as_ref(), b"c hs traffic", hello_hash),
            server: hash.derive_secret(handshake.as_ref(), b"s hs traffic", hello_hash),
        };
        (Self { hash, handshake }, secrets)
    }

    /// From the hash of ClientHello..server Finished.
    pub fn application(&self, finished_hash: &[u8]) -> TrafficSecrets {
        let hash = self.hash;
        let zeros = [0u8; 48];
        let empty = hash.digest(&[]);
        let derived = hash.derive_secret(self.handshake.as_ref(), b"derived", empty.as_ref());
        let master = hash.extract(derived.as_ref(), &zeros[..hash.len()]);
        TrafficSecrets {
            client: hash.derive_secret(master.as_ref(), b"c ap traffic", finished_hash),
            server: hash.derive_secret(master.as_ref(), b"s ap traffic", finished_hash),
        }
    }
}

pub enum Aead {
    Gcm(AesGcm),
    ChaCha(ChaCha20Poly1305),
}

pub const TAG_LEN: usize = 16;

impl Aead {
    fn seal(&self, nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        match self {
            Aead::Gcm(a) => a.seal(nonce, aad, data),
            Aead::ChaCha(a) => a.seal(nonce, aad, data),
        }
    }

    fn open(&self, nonce: &[u8; 12], aad: &[u8], data: &mut [u8], tag: &[u8]) -> bool {
        match self {
            Aead::Gcm(a) => a.open(nonce, aad, data, tag),
            Aead::ChaCha(a) => a.open(nonce, aad, data, tag),
        }
    }
}

/// One direction's record protection: key, static IV and sequence number.
pub struct Protection {
    suite: CipherSuite,
    secret: Digest,
    aead: Aead,
    iv: [u8; 12],
    seq: u64,
}

impl Drop for Protection {
    fn drop(&mut self) {
        crate::ct::wipe(&mut self.iv);
    }
}

impl Protection {
    pub fn new(suite: CipherSuite, secret: Digest) -> Self {
        let hash = suite.hash();
        let mut key = [0u8; 32];
        let key = &mut key[..suite.key_len()];
        hash.expand_label(secret.as_ref(), b"key", &[], key);
        let mut iv = [0u8; 12];
        hash.expand_label(secret.as_ref(), b"iv", &[], &mut iv);
        let aead = match suite {
            CipherSuite::Chacha20Poly1305Sha256 => {
                Aead::ChaCha(ChaCha20Poly1305::new(key).expect("32-byte key"))
            }
            _ => Aead::Gcm(AesGcm::new(key).expect("AES key length")),
        };
        crate::ct::wipe(key);
        Self {
            suite,
            secret,
            aead,
            iv,
            seq: 0,
        }
    }

    /// The protection after a KeyUpdate.
    pub fn updated(&self) -> Self {
        Self::new(
            self.suite,
            self.suite.hash().next_secret(self.secret.as_ref()),
        )
    }

    fn nonce(&self) -> [u8; 12] {
        let mut nonce = self.iv;
        for (n, s) in nonce[4..].iter_mut().zip(self.seq.to_be_bytes()) {
            *n ^= s;
        }
        nonce
    }

    /// Append one protected record carrying `content_type` and `payload`.
    pub fn seal(&mut self, content_type: u8, payload: &[u8], out: &mut Vec<u8>) {
        let len = payload.len() + 1 + TAG_LEN;
        let header = [23, 3, 3, (len >> 8) as u8, len as u8];
        out.extend_from_slice(&header);
        let start = out.len();
        out.extend_from_slice(payload);
        out.push(content_type);
        let nonce = self.nonce();
        let tag = self.aead.seal(&nonce, &header, &mut out[start..]);
        out.extend_from_slice(&tag);
        self.seq += 1;
    }

    /// Decrypt one record's body in place: the inner content type (0 if all
    /// padding) and content length, or `None` if it does not authenticate. The
    /// padding is found in time that depends only on the length (RFC 8446 §5.4).
    pub fn open(&mut self, header: &[u8; 5], body: &mut [u8]) -> Option<(u8, usize)> {
        let split = body.len().checked_sub(TAG_LEN)?;
        let (data, tag) = body.split_at_mut(split);
        let nonce = self.nonce();
        if !self.aead.open(&nonce, header, data, tag) {
            return None;
        }
        self.seq += 1;
        let (mut kind, mut end) = (0u64, 0u64);
        for (i, &b) in data.iter().enumerate() {
            let last = crate::ct::mask64(u64::from(b != 0));
            kind = (u64::from(b) & last) | (kind & !last);
            end = (i as u64 & last) | (end & !last);
        }
        Some((kind as u8, end as usize))
    }
}
