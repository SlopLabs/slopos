//! `<name>.meta`: what an extension's signer signs, the image's name,
//! version and SHA-256, so the signed object keeps its shape whatever form
//! the image takes.
//!
//! ```text
//! name hello
//! version 3
//! sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
//! ```
//!
//! Signed with `ssh-keygen -Y sign -n extension@slopos -O hashalg=sha512`
//! into `<name>.meta.sig`.

use alloc::format;
use alloc::string::{String, ToString};

use slopos_tls_core::sshsig::{self, PublicKey};

use crate::words::parse_number;
use crate::{NAMESPACE, hex, parse_digest};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sidecar {
    pub name: String,
    pub version: u64,
    pub sha256: [u8; 32],
}

impl Sidecar {
    pub fn render(&self) -> String {
        format!(
            "name {}\nversion {}\nsha256 {}\n",
            self.name,
            self.version,
            hex(&self.sha256)
        )
    }

    /// Only the text [`render`](Self::render) writes, so one sidecar has one
    /// spelling and one signature covers it.
    pub fn parse(text: &[u8]) -> Option<Sidecar> {
        let text = core::str::from_utf8(text).ok()?;
        let mut lines = text.strip_suffix('\n')?.split('\n');
        let mut field = |key: &str| lines.next()?.strip_prefix(key)?.strip_prefix(' ');
        let name = field("name")?;
        let version = parse_number(field("version")?)?;
        let sha256 = parse_digest(field("sha256")?)?;
        let sidecar = Sidecar {
            name: name.to_string(),
            version,
            sha256,
        };
        (slopos_abi::extension::valid_name(name.as_bytes()) && sidecar.render() == text)
            .then_some(sidecar)
    }

    /// The key that signed `text`, the sidecar, with `signature`.
    pub fn signer(text: &[u8], signature: &[u8]) -> Result<PublicKey, sshsig::Error> {
        sshsig::verify(signature, NAMESPACE, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_tls_core::ed25519::SigningKey;

    fn sidecar() -> Sidecar {
        Sidecar {
            name: "hello".into(),
            version: 3,
            sha256: [0xab; 32],
        }
    }

    #[test]
    fn a_sidecar_has_one_spelling() {
        let text = sidecar().render();
        assert_eq!(Sidecar::parse(text.as_bytes()), Some(sidecar()));
        for other in [
            text.replace("version 3", "version 03"),
            text.replace("name hello", "name  hello"),
            text.replace("\n", "\r\n"),
            text.trim_end().into(),
            format!("{text}\n"),
            text.replace("ab", "AB"),
            text.replace("name hello", "name etc"),
        ] {
            assert_eq!(Sidecar::parse(other.as_bytes()), None, "{other:?}");
        }
    }

    #[test]
    fn its_signer_is_the_key_that_signed_it_under_the_namespace() {
        let key = SigningKey::from_seed(&[5; 32]);
        let text = sidecar().render();
        let signed = sshsig::sign(&key, NAMESPACE, text.as_bytes());
        assert_eq!(
            Sidecar::signer(text.as_bytes(), &signed),
            Ok(PublicKey(key.public()))
        );
        let elsewhere = sshsig::sign(&key, "file", text.as_bytes());
        assert!(Sidecar::signer(text.as_bytes(), &elsewhere).is_err());
        let other = sidecar().render().replace("version 3", "version 4");
        assert!(Sidecar::signer(other.as_bytes(), &signed).is_err());
    }
}
