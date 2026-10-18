//! What the store keeps of an extension: whether it is enabled and why not,
//! the key whose updates go in unasked, the version floor and the versions
//! refused since a rollback, and its two images with what each was granted.
//! Removing the extension keeps the record without images.
//!
//! ```text
//! name hello
//! enabled yes
//! signer SHA256:bbXpuKG6zhzdmnxq256TlqzFBzRl2f6OOg722cYNbU8
//! floor 3 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
//! refused 4
//! uids hello=100,hello-worker=101
//! config 2
//! image current
//!   version 3
//!   sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
//!   signer SHA256:bbXpuKG6zhzdmnxq256TlqzFBzRl2f6OOg722cYNbU8
//!   grant NetAdmin unasked - users hello commands hello
//! ```
//!
//! `uids` follows `refused` once any uid is given, `config` once a key's
//! configuration went in, and `why` follows them when a check stopped the
//! extension. A record has one spelling, the one
//! [`Record::render`] writes.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::grant::Grant;
use crate::words::{parse_number, valid_text};
use crate::{Slot, hex, parse_digest};

const WHY_MAX: usize = 200;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Record {
    pub name: String,
    pub enabled: bool,
    /// The fingerprint of the key whose updates go in unasked.
    pub signer: Option<String>,
    /// The highest version installed, and its digest.
    pub floor: Option<(u64, [u8; 32])>,
    /// Versions a rollback left, refused to a signed install.
    pub refused: Vec<u64>,
    /// Every uid ever given it, by name, kept once it is removed:
    /// [`crate::accounts`].
    pub uids: Vec<(String, u32)>,
    /// The serial of the last configuration a key signed for it,
    /// [`crate::config`], kept for good so none older goes in again.
    pub config: u64,
    pub why: Option<String>,
    pub current: Option<Installed>,
    pub previous: Option<Installed>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Installed {
    pub version: u64,
    pub sha256: [u8; 32],
    /// The key that signed it, `None` when a person approved it.
    pub signer: Option<String>,
    pub grant: Grant,
}

/// `SHA256:` and 43 base64 digits, as a key's fingerprint is spelt.
pub fn valid_fingerprint(text: &str) -> bool {
    text.strip_prefix("SHA256:").is_some_and(|digits| {
        digits.len() == 43
            && digits
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    })
}

fn optional(word: Option<&String>) -> &str {
    word.map_or("-", String::as_str)
}

impl Record {
    pub fn new(name: &str) -> Record {
        Record {
            name: name.to_string(),
            enabled: false,
            signer: None,
            floor: None,
            refused: Vec::new(),
            uids: Vec::new(),
            config: 0,
            why: None,
            current: None,
            previous: None,
        }
    }

    pub fn uid(&self, user: &str) -> Option<u32> {
        self.uids
            .iter()
            .find(|(name, _)| name == user)
            .map(|&(_, uid)| uid)
    }

    /// Whether it is to run: enabled, stopped by no check, and installed.
    pub fn runs(&self) -> bool {
        self.enabled && self.why.is_none() && self.current.is_some()
    }

    /// Its number: the uid of its own name.
    pub fn number(&self) -> Option<u32> {
        self.uid(&self.name)
    }

    pub fn image(&self, slot: Slot) -> Option<&Installed> {
        match slot {
            Slot::Current => self.current.as_ref(),
            Slot::Previous => self.previous.as_ref(),
        }
    }

    /// Stopped because `why`, a reason printable whatever it held.
    pub fn stop(&mut self, why: &str) {
        self.enabled = false;
        let shown: String = why
            .chars()
            .map(|c| match c {
                ' '..='~' if c != '"' => c,
                _ => '?',
            })
            .take(WHY_MAX)
            .collect();
        self.why = Some(shown).filter(|why| !why.trim().is_empty());
    }

    /// `image` made current, the current one kept as previous; the floor
    /// rises to it and what it is newer than is no longer refused. A person's
    /// install enables the extension; a key's leaves one the person disabled
    /// as it is.
    pub fn install(&mut self, image: Installed) {
        let disabled = !self.enabled && self.why.is_none() && self.current.is_some();
        self.enabled = image.signer.is_none() || !disabled;
        if self
            .floor
            .is_none_or(|(version, _)| image.version > version)
        {
            self.floor = Some((image.version, image.sha256));
        }
        self.refused.retain(|&v| v > image.version);
        self.signer = image.signer.clone();
        self.why = None;
        if let Some(current) = self.current.replace(image) {
            self.previous = Some(current);
        }
    }

    /// The previous image made current and the current one previous; the
    /// floor falls to the one that runs, which is refused no longer, and the
    /// version left is refused until a newer one installs.
    pub fn roll_back(&mut self) -> Result<(), &'static str> {
        let previous = self
            .previous
            .take()
            .ok_or("there is no previous image to go back to")?;
        let left = self.current.replace(previous);
        let current = self.current.as_ref().expect("just set");
        self.floor = Some((current.version, current.sha256));
        self.signer = current.signer.clone();
        let running = current.version;
        self.refused.retain(|&v| v != running);
        if let Some(left) = left {
            if !self.refused.contains(&left.version) && left.version != current.version {
                self.refused.push(left.version);
                self.refused.sort_unstable();
            }
            self.previous = Some(left);
        }
        Ok(())
    }

    /// Its images taken away; the floor, the refused versions, the signer and
    /// the uids stay, so what was refused is refused still and what its users
    /// left keeps its owner.
    pub fn remove(&mut self) {
        self.current = None;
        self.previous = None;
        self.enabled = false;
        self.why = None;
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "name {}\nenabled {}\nsigner {}\n",
            self.name,
            if self.enabled { "yes" } else { "no" },
            optional(self.signer.as_ref())
        );
        if let Some((version, digest)) = &self.floor {
            out.push_str(&format!("floor {version} {}\n", hex(digest)));
        }
        let refused: Vec<String> = self.refused.iter().map(u64::to_string).collect();
        out.push_str(&format!(
            "refused {}\n",
            crate::words::render_list(refused.iter().map(String::as_str))
        ));
        if !self.uids.is_empty() {
            let uids: Vec<String> = self
                .uids
                .iter()
                .map(|(name, uid)| format!("{name}={uid}"))
                .collect();
            out.push_str(&format!("uids {}\n", uids.join(",")));
        }
        if self.config != 0 {
            out.push_str(&format!("config {}\n", self.config));
        }
        if let Some(why) = &self.why {
            out.push_str(&format!("why {why}\n"));
        }
        for (slot, image) in [
            (Slot::Current, &self.current),
            (Slot::Previous, &self.previous),
        ] {
            if let Some(image) = image {
                out.push_str(&format!(
                    "image {}\n  version {}\n  sha256 {}\n  signer {}\n  {}\n",
                    slot.name(),
                    image.version,
                    hex(&image.sha256),
                    optional(image.signer.as_ref()),
                    image.grant.words().join(" ")
                ));
            }
        }
        out
    }

    /// Only the text [`render`](Self::render) writes.
    pub fn parse(text: &str) -> Option<Record> {
        let mut lines = Lines(text.strip_suffix('\n')?.split('\n').peekable());
        let name = lines.take("name")?;
        let enabled = match lines.take("enabled")? {
            "yes" => true,
            "no" => false,
            _ => return None,
        };
        let mut record = Record {
            enabled,
            signer: signer_of(lines.take("signer")?)?,
            ..Record::new(name)
        };
        if let Some(floor) = lines.take_if("floor") {
            let (version, digest) = floor.split_once(' ')?;
            record.floor = Some((parse_number(version)?, parse_digest(digest)?));
        }
        record.refused = match lines.take("refused")? {
            "-" => Vec::new(),
            list => list.split(',').map(parse_number).collect::<Option<_>>()?,
        };
        if let Some(uids) = lines.take_if("uids") {
            for pair in uids.split(',') {
                let (name, uid) = pair.split_once('=')?;
                let uid = u32::try_from(parse_number(uid)?).ok()?;
                let fresh = !record.uids.iter().any(|(n, u)| n == name || *u == uid);
                (crate::declare::valid_user(name)
                    && (crate::accounts::FIRST_UID..=crate::accounts::LAST_UID).contains(&uid)
                    && fresh)
                    .then_some(())?;
                record.uids.push((name.to_string(), uid));
            }
        }
        if let Some(serial) = lines.take_if("config") {
            record.config = parse_number(serial)?;
        }
        if let Some(why) = lines.take_if("why") {
            record.why = Some(why.to_string()).filter(|why| valid_text(why, WHY_MAX));
            record.why.as_ref()?;
        }
        while let Some(slot) = lines.take_if("image") {
            let image = Installed {
                version: parse_number(lines.take("  version")?)?,
                sha256: parse_digest(lines.take("  sha256")?)?,
                signer: signer_of(lines.take("  signer")?)?,
                grant: Grant::parse(
                    &lines
                        .0
                        .next()?
                        .strip_prefix("  ")?
                        .split(' ')
                        .collect::<Vec<_>>(),
                )?,
            };
            let held = match slot {
                "current" if record.previous.is_none() => &mut record.current,
                "previous" => &mut record.previous,
                _ => return None,
            };
            if held.replace(image).is_some() {
                return None;
            }
        }
        (lines.0.next().is_none()
            && slopos_abi::extension::valid_name(name.as_bytes())
            && record.render() == text)
            .then_some(record)
    }
}

struct Lines<'a>(core::iter::Peekable<core::str::Split<'a, char>>);

impl<'a> Lines<'a> {
    /// What follows `key` and a space on the next line.
    fn take(&mut self, key: &str) -> Option<&'a str> {
        self.0.next()?.strip_prefix(key)?.strip_prefix(' ')
    }

    /// [`take`](Self::take), when the next line has that key.
    fn take_if(&mut self, key: &str) -> Option<&'a str> {
        let value = self.0.peek()?.strip_prefix(key)?.strip_prefix(' ')?;
        self.0.next();
        Some(value)
    }
}

/// A fingerprint, or `-` for none; `None` for neither.
fn signer_of(word: &str) -> Option<Option<String>> {
    match word {
        "-" => Some(None),
        fingerprint if valid_fingerprint(fingerprint) => Some(Some(fingerprint.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
