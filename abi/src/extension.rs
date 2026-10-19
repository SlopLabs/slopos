//! Extensions: sealed images activated beside the base, each read-only and
//! pinned at `/opt/<name>`.

/// Where every activation is mounted, at `/opt/<name>`.
pub const DIR: &str = "/opt";

/// Longest name, in bytes.
pub const NAME_MAX: usize = 32;

/// The root of an image's SHA-256 tree, which the activating caller vouches
/// for.
pub const DIGEST_LEN: usize = 32;

/// Entries an image holds beneath its root at most: what the kernel indexes
/// to name an activation's programs.
pub const MAX_ENTRIES: usize = 1 << 16;

/// Activations resident at once: those mounted and those still held after
/// being replaced or deactivated.
pub const MAX_ACTIVATIONS: usize = 32;

/// Names no extension may take: the system's own and the base's directories.
pub const RESERVED: [&str; 11] = [
    "slopos", "base", "bin", "sbin", "lib", "usr", "share", "etc", "ssl", "root", "user",
];

/// Whether `name` is lowercase letters, digits and `-`, at most [`NAME_MAX`]
/// bytes and not [`RESERVED`].
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !RESERVED.iter().any(|reserved| reserved.as_bytes() == name)
}

/// What `SO_PEEREXTENSION` answers: the activation a peer's program is of, as
/// its activator named it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label {
    /// NUL-padded.
    pub name: [u8; NAME_MAX],
    pub digest: [u8; DIGEST_LEN],
}

impl Label {
    pub const LEN: usize = NAME_MAX + DIGEST_LEN;

    /// `None` for a name [`valid_name`] refuses.
    pub fn new(name: &[u8], digest: [u8; DIGEST_LEN]) -> Option<Self> {
        if !valid_name(name) {
            return None;
        }
        let mut padded = [0; NAME_MAX];
        padded[..name.len()].copy_from_slice(name);
        Some(Self {
            name: padded,
            digest,
        })
    }

    pub fn name(&self) -> &[u8] {
        let len = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_MAX);
        &self.name[..len]
    }

    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut out = [0; Self::LEN];
        out[..NAME_MAX].copy_from_slice(&self.name);
        out[NAME_MAX..].copy_from_slice(&self.digest);
        out
    }

    /// `None` unless `bytes` is a whole label naming a valid name.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::LEN] = bytes.try_into().ok()?;
        let mut name = [0; NAME_MAX];
        name.copy_from_slice(&bytes[..NAME_MAX]);
        let mut digest = [0; DIGEST_LEN];
        digest.copy_from_slice(&bytes[NAME_MAX..]);
        let label = Self { name, digest };
        let padded = label.name[label.name().len()..].iter().all(|&b| b == 0);
        (padded && valid_name(label.name())).then_some(label)
    }
}

const _: () = assert!(core::mem::size_of::<Label>() == Label::LEN);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_lowercase_digits_and_dashes_and_not_reserved() {
        for good in ["a", "my-daemon", "x2", "abcdefghijklmnopqrstuvwxyz012345"] {
            assert!(valid_name(good.as_bytes()), "{good}");
        }
        for bad in [
            "",
            "Upper",
            "dot.ted",
            "sl/ash",
            "spa ce",
            "abcdefghijklmnopqrstuvwxyz0123456",
            "bin",
            "slopos",
            "..",
        ] {
            assert!(!valid_name(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn a_label_round_trips_and_a_malformed_one_is_refused() {
        let label = Label::new(b"tool", [7; DIGEST_LEN]).unwrap();
        assert_eq!(label.name(), b"tool");
        assert_eq!(Label::from_bytes(&label.to_bytes()), Some(label));
        let mut gap = label.to_bytes();
        gap[5] = b'x';
        assert_eq!(Label::from_bytes(&gap), None);
        assert_eq!(Label::from_bytes(&label.to_bytes()[1..]), None);
        assert_eq!(Label::new(b"etc", [0; DIGEST_LEN]), None);
    }
}
