//! The keys the owner trusts to install extensions, a line each, shaped as
//! `allowed_signers` writes a key: the key, its ceiling, its `user` ceiling
//! (what the actions every person runs unasked may hold together) and a
//! comment.
//!
//! ```text
//! ssh-ed25519 AAAAC3Nza... ceiling=Power,NetAdmin user=- laptop key
//! ```

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use slopos_tls_core::sshsig::{KEY_TYPE, PublicKey};

use crate::words::{LineError, parse_caps, render_caps, valid_text};

/// More keys than any owner trusts.
pub const MAX_KEYS: usize = 64;
const COMMENT_MAX: usize = 120;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Key {
    pub key: PublicKey,
    pub ceiling: u64,
    pub user: u64,
    pub comment: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Keys {
    keys: Vec<Key>,
}

impl Key {
    pub fn render(&self) -> String {
        let mut line = format!(
            "{KEY_TYPE} {} ceiling={} user={}",
            self.key.base64(),
            render_caps(self.ceiling),
            render_caps(self.user)
        );
        if !self.comment.is_empty() {
            line.push(' ');
            line.push_str(&self.comment);
        }
        line
    }

    fn parse(line: &str) -> Result<Key, &'static str> {
        let mut words = line.splitn(5, ' ');
        let mut word = || words.next().unwrap_or_default();
        let key = PublicKey::from_openssh(word(), word()).ok_or("not an ssh-ed25519 key")?;
        let ceiling = word()
            .strip_prefix("ceiling=")
            .and_then(parse_caps)
            .ok_or("ceiling= names capabilities")?;
        let user = word()
            .strip_prefix("user=")
            .and_then(parse_caps)
            .ok_or("user= names capabilities")?;
        Key::new(key, ceiling, user, word())
    }

    /// The user ceiling sits under the ceiling: an unasked action's
    /// capabilities are a grant too.
    pub fn new(
        key: PublicKey,
        ceiling: u64,
        user: u64,
        comment: &str,
    ) -> Result<Key, &'static str> {
        if user & !ceiling != 0 {
            return Err("the user ceiling names what the ceiling does not");
        }
        if !comment.is_empty() && !valid_comment(comment) {
            return Err("a comment is a line of printable ASCII");
        }
        Ok(Key {
            key,
            ceiling,
            user,
            comment: comment.to_string(),
        })
    }
}

/// Whether `comment` goes in a key's line as it is.
pub fn valid_comment(comment: &str) -> bool {
    valid_text(comment, COMMENT_MAX)
}

impl Keys {
    pub fn parse(text: &str) -> Result<Keys, LineError> {
        let mut keys = Keys::default();
        for (i, line) in text.lines().enumerate() {
            let fail = |msg| LineError { line: i + 1, msg };
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let key = Key::parse(line).map_err(fail)?;
            if keys.find(&key.key).is_some() {
                return Err(fail("the key is listed twice"));
            }
            if keys.keys.len() == MAX_KEYS {
                return Err(fail("more keys than the store keeps"));
            }
            keys.keys.push(key);
        }
        Ok(keys)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for key in &self.keys {
            out.push_str(&key.render());
            out.push('\n');
        }
        out
    }

    pub fn find(&self, key: &PublicKey) -> Option<&Key> {
        self.keys.iter().find(|k| k.key == *key)
    }

    pub fn by_fingerprint(&self, fingerprint: &str) -> Option<&Key> {
        self.keys
            .iter()
            .find(|k| k.key.fingerprint() == fingerprint)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Key> {
        self.keys.iter()
    }

    /// Trust `key`, in place of what it was trusted with before.
    pub fn put(&mut self, key: Key) -> Result<(), &'static str> {
        if let Some(held) = self.keys.iter_mut().find(|k| k.key == key.key) {
            *held = key;
        } else if self.keys.len() == MAX_KEYS {
            return Err("more keys than the store keeps");
        } else {
            self.keys.push(key);
        }
        Ok(())
    }

    pub fn remove(&mut self, fingerprint: &str) -> Option<Key> {
        let at = self
            .keys
            .iter()
            .position(|k| k.key.fingerprint() == fingerprint)?;
        Some(self.keys.remove(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slopos_abi::caps;
    use slopos_tls_core::ed25519::SigningKey;

    fn key(seed: u8) -> PublicKey {
        PublicKey(SigningKey::from_seed(&[seed; 32]).public())
    }

    #[test]
    fn keys_read_back_as_written_and_are_found_by_key_or_fingerprint() {
        let mut keys = Keys::default();
        keys.put(
            Key::new(
                key(1),
                caps::POWER | caps::NET_ADMIN,
                caps::POWER,
                "laptop key",
            )
            .unwrap(),
        )
        .unwrap();
        keys.put(Key::new(key(2), 0, 0, "").unwrap()).unwrap();
        let text = keys.render();
        assert!(text.contains(" ceiling=Power,NetAdmin user=Power laptop key\n"));
        assert!(text.contains(" ceiling=- user=-\n"));
        let read = Keys::parse(&format!("# trusted\n\n{text}")).unwrap();
        assert_eq!(read, keys);
        assert_eq!(read.find(&key(2)).map(|k| k.ceiling), Some(0));
        let fingerprint = key(1).fingerprint();
        assert_eq!(
            read.by_fingerprint(&fingerprint).map(|k| k.user),
            Some(caps::POWER)
        );
        keys.put(Key::new(key(1), caps::POWER, 0, "lowered").unwrap())
            .unwrap();
        assert_eq!(keys.iter().count(), 2);
        assert_eq!(keys.find(&key(1)).map(|k| k.ceiling), Some(caps::POWER));
        assert_eq!(
            keys.remove(&fingerprint).map(|k| k.comment),
            Some("lowered".into())
        );
        assert_eq!(keys.remove(&fingerprint), None);
    }

    #[test]
    fn a_line_out_of_shape_names_its_line() {
        let good = Key::new(key(1), caps::POWER, 0, "c").unwrap().render();
        for (bad, why) in [
            (good.replace("ssh-ed25519", "ssh-rsa"), "type"),
            (good.replace("ceiling=Power", "ceiling=Wizard"), "ceiling"),
            (
                good.replace("ceiling=Power user=-", "user=- ceiling=Power"),
                "order",
            ),
            (
                good.replace("user=-", "user=Power,Mount"),
                "user above ceiling",
            ),
            (good.replace(" c", " \"quoted\""), "comment"),
        ] {
            assert_eq!(
                Keys::parse(&format!("{good}\n{}\n", bad.replace(&good, "x")))
                    .map(|_| ())
                    .is_err(),
                true,
                "{why}"
            );
            assert_eq!(
                Keys::parse(&format!("# x\n{bad}\n")).map_err(|e| e.line),
                Err(2),
                "{why}"
            );
        }
        assert_eq!(
            Keys::parse(&format!("{good}\n{good}\n")).map_err(|e| e.line),
            Err(2)
        );
    }

    #[test]
    fn a_comment_that_would_not_read_back_is_refused() {
        let long = "c".repeat(COMMENT_MAX + 1);
        for comment in ["J\u{fc}rgen", "a\nssh-ed25519 x", "\"q\"", long.as_str()] {
            assert!(Key::new(key(1), 0, 0, comment).is_err(), "{comment:?}");
        }
    }
}
