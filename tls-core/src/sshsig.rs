//! SSH signatures over Ed25519 keys: what `ssh-keygen -Y sign` writes, as
//! OpenSSH's `PROTOCOL.sshsig` lays it out and RFC 8709 encodes the key and
//! the signature, and the `ssh-ed25519` public key and fingerprint
//! `ssh-keygen` prints.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ed25519;
use crate::hash::{Hash, Sha256, Sha512};
use crate::pem::{base64_decode, base64_encode};

pub const KEY_TYPE: &str = "ssh-ed25519";
const MAGIC: &[u8; 6] = b"SSHSIG";
const VERSION: u32 = 1;
#[cfg(any(test, feature = "test-server"))]
const LABEL: &str = "SSH SIGNATURE";
const BEGIN: &[u8] = b"-----BEGIN SSH SIGNATURE-----";
const END: &[u8] = b"-----END SSH SIGNATURE-----";
/// Larger than any Ed25519 signature's armor, so a stray file is not decoded
/// whole.
const ARMOR_MAX: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublicKey(pub [u8; ed25519::PUBLIC_KEY_LEN]);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No `SSH SIGNATURE` block, or one whose base64 does not decode.
    Armor,
    Malformed,
    Version,
    /// A key or signature of a type other than `ssh-ed25519`.
    KeyType,
    /// Signed under another namespace than the one asked for.
    Namespace,
    HashAlgorithm,
    /// The signature does not verify.
    Signature,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Armor => "no SSH signature block",
            Error::Malformed => "a malformed SSH signature",
            Error::Version => "an SSH signature of a version other than 1",
            Error::KeyType => "an SSH signature by a key other than ssh-ed25519",
            Error::Namespace => "an SSH signature made for another purpose",
            Error::HashAlgorithm => "an SSH signature over a hash other than sha256 or sha512",
            Error::Signature => "an SSH signature that does not verify",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HashAlgorithm {
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    fn name(self) -> &'static [u8] {
        match self {
            HashAlgorithm::Sha256 => b"sha256",
            HashAlgorithm::Sha512 => b"sha512",
        }
    }

    fn of(name: &[u8]) -> Option<HashAlgorithm> {
        [HashAlgorithm::Sha256, HashAlgorithm::Sha512]
            .into_iter()
            .find(|alg| alg.name() == name)
    }

    fn digest(self, message: &[u8]) -> Vec<u8> {
        match self {
            HashAlgorithm::Sha256 => Sha256::digest(message).as_ref().to_vec(),
            HashAlgorithm::Sha512 => Sha512::digest(message).as_ref().to_vec(),
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Malformed);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn string(&mut self) -> Result<&'a [u8], Error> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn end(&self) -> Result<(), Error> {
        self.0.is_empty().then_some(()).ok_or(Error::Malformed)
    }
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// `string "ssh-ed25519"` then `string <32 or 64 bytes>`, and nothing more.
fn typed<const N: usize>(blob: &[u8]) -> Result<[u8; N], Error> {
    let mut r = Reader(blob);
    if r.string()? != KEY_TYPE.as_bytes() {
        return Err(Error::KeyType);
    }
    let value = r.string()?.try_into().map_err(|_| Error::Malformed)?;
    r.end()?;
    Ok(value)
}

impl PublicKey {
    /// The key's wire form, which a fingerprint hashes.
    pub fn blob(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(51);
        put_string(&mut out, KEY_TYPE.as_bytes());
        put_string(&mut out, &self.0);
        out
    }

    pub fn from_blob(blob: &[u8]) -> Option<PublicKey> {
        typed(blob).ok().map(PublicKey)
    }

    /// The two fields an OpenSSH public key line starts with: the key type
    /// and the base64 of its blob.
    pub fn from_openssh(key_type: &str, base64: &str) -> Option<PublicKey> {
        if key_type != KEY_TYPE || !base64.bytes().all(|b| b.is_ascii_graphic()) {
            return None;
        }
        PublicKey::from_blob(&base64_decode(base64.as_bytes())?)
    }

    pub fn base64(&self) -> String {
        String::from_utf8(base64_encode(&self.blob())).expect("base64 is ASCII")
    }

    /// `SHA256:` and the unpadded base64 of the blob's SHA-256, as
    /// `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> String {
        let mut out = String::from("SHA256:");
        let digest = base64_encode(Sha256::digest(&self.blob()).as_ref());
        let digits = digest.split(|&b| b == b'=').next().unwrap_or_default();
        out.push_str(core::str::from_utf8(digits).expect("base64 is ASCII"));
        out
    }
}

/// What a signature covers: the namespace and the message's digest.
fn signed_data(namespace: &[u8], hash: HashAlgorithm, message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    put_string(&mut out, namespace);
    put_string(&mut out, b"");
    put_string(&mut out, hash.name());
    put_string(&mut out, &hash.digest(message));
    out
}

fn dearmor(armored: &[u8]) -> Result<Vec<u8>, Error> {
    if armored.len() > ARMOR_MAX {
        return Err(Error::Armor);
    }
    let text = armored.trim_ascii();
    let body = text
        .strip_prefix(BEGIN)
        .and_then(|rest| rest.strip_suffix(END))
        .ok_or(Error::Armor)?;
    base64_decode(body).ok_or(Error::Armor)
}

/// The key that made `armored` over `message` under `namespace`, once its
/// signature verifies; the caller decides whether it trusts that key.
pub fn verify(armored: &[u8], namespace: &str, message: &[u8]) -> Result<PublicKey, Error> {
    let blob = dearmor(armored)?;
    let mut r = Reader(&blob);
    if r.take(MAGIC.len())? != MAGIC {
        return Err(Error::Malformed);
    }
    if r.u32()? != VERSION {
        return Err(Error::Version);
    }
    let key = PublicKey(typed(r.string()?)?);
    let signed_under = r.string()?;
    if !r.string()?.is_empty() {
        return Err(Error::Malformed);
    }
    let hash = HashAlgorithm::of(r.string()?).ok_or(Error::HashAlgorithm)?;
    let signature: [u8; ed25519::SIGNATURE_LEN] = typed(r.string()?)?;
    r.end()?;
    if namespace.is_empty() || signed_under != namespace.as_bytes() {
        return Err(Error::Namespace);
    }
    let data = signed_data(signed_under, hash, message);
    if !ed25519::verify(&key.0, &data, &signature) {
        return Err(Error::Signature);
    }
    Ok(key)
}

/// `message` signed by `key` under `namespace` over its SHA-512, armored as
/// `ssh-keygen -Y sign` writes it. For tests, as the signer is.
#[cfg(any(test, feature = "test-server"))]
pub fn sign(key: &ed25519::SigningKey, namespace: &str, message: &[u8]) -> Vec<u8> {
    let hash = HashAlgorithm::Sha512;
    let signature = key.sign(&signed_data(namespace.as_bytes(), hash, message));
    let mut sig_blob = Vec::new();
    put_string(&mut sig_blob, KEY_TYPE.as_bytes());
    put_string(&mut sig_blob, &signature);
    let mut blob = Vec::new();
    blob.extend_from_slice(MAGIC);
    blob.extend_from_slice(&VERSION.to_be_bytes());
    put_string(&mut blob, &PublicKey(key.public()).blob());
    put_string(&mut blob, namespace.as_bytes());
    put_string(&mut blob, b"");
    put_string(&mut blob, hash.name());
    put_string(&mut blob, &sig_blob);
    crate::pem::armor(LABEL, &blob, 70)
}

#[cfg(test)]
mod tests;
