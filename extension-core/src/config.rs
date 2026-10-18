//! An extension's configuration: files in `/etc/<name>` that steer what it
//! was granted and never widen it. A person installs one they edited; a key
//! signs one with `<file>.meta`, naming the extension, the file, a serial and
//! the file's SHA-256, under [`NAMESPACE`] into `<file>.meta.sig`. The store's
//! record keeps the last serial a key's configuration went in under, so a
//! signed file once replaced never goes in again.
//!
//! ```text
//! name hello
//! file hello.conf
//! serial 4
//! sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
//! ```

use alloc::format;
use alloc::string::{String, ToString};

use slopos_tls_core::sshsig::{self, PublicKey};

use crate::words::parse_number;
use crate::{hex, parse_digest};

/// What a configuration's signature is made under, so neither a sidecar's
/// nor any other signature by the same key passes for one.
pub const NAMESPACE: &str = "extension-config@slopos";
/// The largest configuration file.
pub const FILE_MAX: u64 = 256 * 1024;
const NAME_MAX: usize = 64;

/// `/etc/<name>`.
pub fn dir(name: &str) -> String {
    format!("/etc/{name}")
}

/// A file's name in `/etc/<name>`: letters, digits, `.`, `_` and `-`, not
/// starting with `.`, at most 64 bytes.
pub fn valid_file(file: &str) -> bool {
    file.len() <= NAME_MAX
        && file
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Signed {
    pub name: String,
    pub file: String,
    pub serial: u64,
    pub sha256: [u8; 32],
}

impl Signed {
    pub fn render(&self) -> String {
        format!(
            "name {}\nfile {}\nserial {}\nsha256 {}\n",
            self.name,
            self.file,
            self.serial,
            hex(&self.sha256)
        )
    }

    /// Only the text [`render`](Self::render) writes.
    pub fn parse(text: &[u8]) -> Option<Signed> {
        let text = core::str::from_utf8(text).ok()?;
        let mut lines = text.strip_suffix('\n')?.split('\n');
        let mut field = |key: &str| lines.next()?.strip_prefix(key)?.strip_prefix(' ');
        let signed = Signed {
            name: field("name")?.to_string(),
            file: field("file")?.to_string(),
            serial: parse_number(field("serial")?)?,
            sha256: parse_digest(field("sha256")?)?,
        };
        (crate::valid_name(&signed.name)
            && valid_file(&signed.file)
            && signed.serial > 0
            && signed.render() == text)
            .then_some(signed)
    }

    /// The key that signed `text` with `signature`.
    pub fn signer(text: &[u8], signature: &[u8]) -> Result<PublicKey, sshsig::Error> {
        sshsig::verify(signature, NAMESPACE, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_tls_core::ed25519::SigningKey;

    fn signed() -> Signed {
        Signed {
            name: "hello".into(),
            file: "hello.conf".into(),
            serial: 4,
            sha256: [0xcd; 32],
        }
    }

    #[test]
    fn a_signed_configuration_has_one_spelling() {
        let text = signed().render();
        assert_eq!(Signed::parse(text.as_bytes()), Some(signed()));
        for other in [
            text.replace("serial 4", "serial 04"),
            text.replace("serial 4", "serial 0"),
            text.replace("file hello.conf", "file ../passwd"),
            text.replace("file hello.conf", "file .hidden"),
            text.replace("name hello", "name etc"),
            text.replace("\n", "\r\n"),
            text.trim_end().into(),
            text.replace("cd", "CD"),
        ] {
            assert_eq!(Signed::parse(other.as_bytes()), None, "{other:?}");
        }
    }

    #[test]
    fn its_signer_signed_under_the_configuration_namespace() {
        let key = SigningKey::from_seed(&[9; 32]);
        let text = signed().render();
        let ok = sshsig::sign(&key, NAMESPACE, text.as_bytes());
        assert_eq!(
            Signed::signer(text.as_bytes(), &ok),
            Ok(PublicKey(key.public()))
        );
        let as_image = sshsig::sign(&key, crate::NAMESPACE, text.as_bytes());
        assert!(Signed::signer(text.as_bytes(), &as_image).is_err());
    }

    #[test]
    fn a_file_name_stays_in_its_directory() {
        for good in ["hello.conf", "a", "x_y-z.1", "Upper.ini"] {
            assert!(valid_file(good), "{good}");
        }
        let long = "a".repeat(NAME_MAX + 1);
        for bad in ["", ".", "..", ".conf", "a/b", "a b", "a\0", long.as_str()] {
            assert!(!valid_file(bad), "{bad:?}");
        }
    }
}
