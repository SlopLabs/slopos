//! Passwords as the user database keeps them: Argon2id at RFC 9106's second
//! recommended cost, spelt in the PHC string format, whose base64 is the
//! standard alphabet unpadded:
//!
//! ```text
//! $argon2id$v=19$m=65536,t=3,p=4$<salt>$<tag>
//! ```

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use slopos_tls_core::argon2::{self, Params};
use slopos_tls_core::ct;

pub const COST: Params = Params {
    memory_kib: 1 << 16,
    passes: 3,
    lanes: 4,
};
pub const SALT_LEN: usize = 16;
/// The longest password a hash is made of, which `/sbin/authcheck` reads
/// whole.
pub const SECRET_MAX: usize = 1024;
const TAG_LEN: usize = 32;

/// The most a hash read back may cost to check: four times a new one's
/// memory, and passes and lanes a stronger cost could take.
const MEMORY_MAX_KIB: u32 = 1 << 18;
const PASSES_MAX: u32 = 16;
const LANES_MAX: u32 = 16;
const SALT: core::ops::RangeInclusive<usize> = 8..=48;
const TAG: core::ops::RangeInclusive<usize> = 16..=64;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hash {
    params: Params,
    salt: Vec<u8>,
    tag: Vec<u8>,
}

impl Hash {
    /// `secret`'s hash under `salt`, which is random and the hash's alone.
    pub fn new(secret: &[u8], salt: &[u8; SALT_LEN]) -> Result<Hash, argon2::Error> {
        Hash::costing(COST, secret, salt)
    }

    fn costing(params: Params, secret: &[u8], salt: &[u8]) -> Result<Hash, argon2::Error> {
        let mut tag = alloc::vec![0u8; TAG_LEN];
        argon2::argon2id(params, secret, salt, &[], &[], &mut tag)?;
        Ok(Hash {
            params,
            salt: salt.to_vec(),
            tag,
        })
    }

    /// Whether `secret` is the password this is the hash of.
    pub fn matches(&self, secret: &[u8]) -> Result<bool, argon2::Error> {
        let mut tag = alloc::vec![0u8; self.tag.len()];
        argon2::argon2id(self.params, secret, &self.salt, &[], &[], &mut tag)?;
        let same = ct::eq(&tag, &self.tag);
        ct::wipe(&mut tag);
        Ok(same)
    }

    pub fn render(&self) -> String {
        let Params {
            memory_kib,
            passes,
            lanes,
        } = self.params;
        format!(
            "$argon2id$v=19$m={memory_kib},t={passes},p={lanes}${}${}",
            encode(&self.salt),
            encode(&self.tag)
        )
    }

    pub fn parse(text: &str) -> Option<Hash> {
        let mut fields = text.split('$');
        let ["", "argon2id", "v=19", cost, salt, tag] =
            core::array::from_fn(|_| fields.next()).map(|field| field.unwrap_or("\0"))
        else {
            return None;
        };
        if fields.next().is_some() {
            return None;
        }
        let mut costs = cost.split(',');
        let mut take = |key: &str| number(costs.next()?.strip_prefix(key)?);
        let params = Params {
            memory_kib: take("m=")?,
            passes: take("t=")?,
            lanes: take("p=")?,
        };
        let hash = Hash {
            params,
            salt: decode(salt)?,
            tag: decode(tag)?,
        };
        let sound = costs.next().is_none()
            && (1..=LANES_MAX).contains(&params.lanes)
            && (1..=PASSES_MAX).contains(&params.passes)
            && (8 * params.lanes..=MEMORY_MAX_KIB).contains(&params.memory_kib)
            && SALT.contains(&hash.salt.len())
            && TAG.contains(&hash.tag.len());
        (sound && hash.render() == text).then_some(hash)
    }
}

fn number(text: &str) -> Option<u32> {
    (!text.starts_with('0') && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let word = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(word >> (18 - 6 * i)) as usize & 63]));
        }
    }
    out
}

/// Unpadded base64 in its one spelling: the bits past the last byte zero.
fn decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let value = ALPHABET.iter().position(|&a| a == c)?;
        acc = (acc << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    (bits < 6 && acc == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference implementation's own: "password" under "somesalt".
    const REFERENCE: &str =
        "$argon2id$v=19$m=256,t=2,p=1$c29tZXNhbHQ$nf65EOgLrQMR/uIPnA4rEsF5h7TKyQwu9U1bMCHGi/4";

    #[test]
    fn a_reference_hash_reads_and_checks() {
        let hash = Hash::parse(REFERENCE).expect("a hash");
        assert_eq!(hash.salt, b"somesalt");
        assert_eq!(hash.render(), REFERENCE);
        assert_eq!(hash.matches(b"password"), Ok(true));
        assert_eq!(hash.matches(b"passwore"), Ok(false));
        assert_eq!(hash.matches(b""), Ok(false));
    }

    #[test]
    fn a_new_hash_checks_what_made_it() {
        let cheap = Params {
            memory_kib: 64,
            passes: 1,
            lanes: 2,
        };
        let hash = Hash::costing(cheap, b"hunter2", &[7; SALT_LEN]).expect("a hash");
        let back = Hash::parse(&hash.render()).expect("its spelling reads back");
        assert_eq!(back, hash);
        assert_eq!(back.matches(b"hunter2"), Ok(true));
        assert_eq!(back.matches(b"hunter3"), Ok(false));
        let salted = Hash::costing(cheap, b"hunter2", &[8; SALT_LEN]).expect("a hash");
        assert_ne!(salted.tag, hash.tag);
    }

    #[test]
    fn the_cost_written_is_rfc9106s_second() {
        let spelt = Hash {
            params: COST,
            salt: alloc::vec![0; SALT_LEN],
            tag: alloc::vec![0; TAG_LEN],
        }
        .render();
        assert!(
            spelt.starts_with("$argon2id$v=19$m=65536,t=3,p=4$"),
            "{spelt}"
        );
        assert!(Hash::parse(&spelt).is_some());
    }

    #[test]
    fn anything_else_is_no_hash() {
        let swap = |from: &str, to: &str| REFERENCE.replacen(from, to, 1);
        for bad in [
            String::new(),
            swap("argon2id", "argon2i"),
            swap("v=19", "v=16"),
            swap("m=256,t=2,p=1", "t=2,m=256,p=1"),
            swap("m=256", "m=0256"),
            swap("m=256", "m=7"),
            swap("m=256", "m=262145"),
            swap("t=2", "t=0"),
            swap("t=2", "t=17"),
            swap("p=1", "p=0"),
            swap("p=1", "p=1,keyid=x"),
            swap("c29tZXNhbHQ", "c29tZXNhbHQ="),
            swap("c29tZXNhbHQ", "c29tZXNhbHR"),
            swap("c29tZXNhbHQ", "c29tZQ"),
            swap("/4", "/4$"),
            swap("nf65", "nf6"),
            format!("{REFERENCE}\n"),
        ] {
            assert_eq!(Hash::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn base64_round_trips_every_length() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        for len in 0..40 {
            let text = encode(&bytes[..len]);
            assert_eq!(text.len(), (4 * len).div_ceil(3));
            assert_eq!(decode(&text).as_deref(), Some(&bytes[..len]));
        }
        assert_eq!(encode(b"somesalt"), "c29tZXNhbHQ");
    }
}
