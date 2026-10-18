//! System extensions as data: the image an extension ships as and what it
//! declares, what is granted it, the sidecar its signer signs, what the store
//! keeps of it and of the keys the owner trusts, the ceilings every grant
//! sits under, and the check that its programs bind only what `libc.so` and
//! its own libraries export. `/bin/extctl` and `tools/extension` do the I/O.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(any(test, feature = "std"))]
extern crate std;

pub mod accounts;
pub mod admit;
pub mod ceiling;
pub mod declare;
pub mod elf;
pub mod grant;
pub mod image;
pub mod keys;
pub mod record;
pub mod release;
pub mod sidecar;
#[cfg(any(test, feature = "std"))]
pub mod source;
pub mod symbols;
mod words;

pub use words::{LineError, parse_caps, render_caps};

/// Names the system's own paths take beside
/// [`slopos_abi::extension::RESERVED`]: an extension's `/var/log/<name>`,
/// `/run/<name>` or `/etc/<name>` would be one of theirs.
pub const TAKEN: [&str; 9] = [
    "crash", "init", "privd", "sessions", "passwd", "group", "shadow", "keymap", "motd",
];

/// A name an extension may be installed under.
pub fn valid_name(name: &str) -> bool {
    slopos_abi::extension::valid_name(name.as_bytes()) && !TAKEN.contains(&name)
}

/// The store: a directory per extension and the keys the owner trusts.
pub const STORE: &str = slopos_abi::fs::EXTENSION_STORE;
/// The trusted keys, in the store, as generations: `keys.<n>`.
pub const KEYS: &str = "keys";
/// What the store keeps of an extension, in its directory, as generations:
/// `record.<n>`.
pub const RECORD: &str = "record";
/// What any extension can ever be granted, in the sealed base.
pub const CEILING: &str = "/usr/share/slopos/privd/extensions";
/// What a sidecar's signature is made under, so no other signature by the
/// same key passes for one.
pub const NAMESPACE: &str = "extension@slopos";
/// The C library every program of an extension binds against.
pub const LIBC: &str = "/lib/libc.so";
pub const LIBC_SONAME: &str = "libc.so";
pub const INTERPRETER: &str = "/lib/ld-slopos.so.1";

/// One of the two images the store keeps of an extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    Current,
    Previous,
}

impl Slot {
    pub fn name(self) -> &'static str {
        match self {
            Slot::Current => "current",
            Slot::Previous => "previous",
        }
    }
}

/// `<stem>.<n>`'s generation `n`, spelled as it prints. The store believes
/// the newest sealed one.
pub fn generation(name: &str, stem: &str) -> Option<u64> {
    let n = name.strip_prefix(stem)?.strip_prefix('.')?;
    n.parse::<u64>().ok().filter(|g| alloc::format!("{g}") == n)
}

/// The files the store keeps an image as, named by its digest, so writing a
/// new one replaces nothing and the record alone says which is current.
pub fn image_file(digest: &[u8; 32]) -> alloc::string::String {
    alloc::format!("{}.ext", hex(digest))
}

pub fn sidecar_file(digest: &[u8; 32]) -> alloc::string::String {
    alloc::format!("{}.meta", hex(digest))
}

pub fn signature_file(digest: &[u8; 32]) -> alloc::string::String {
    alloc::format!("{}.meta.sig", hex(digest))
}

/// SHA-256, as the image's digest and its manifest's lines take it.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use slopos_tls_core::hash::{Hash, Sha256};
    let mut out = [0u8; 32];
    let mut hash = Sha256::new();
    hash.update(data);
    hash.finish_into(&mut out);
    out
}

pub fn hex(bytes: &[u8]) -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// 64 lowercase hex digits.
pub fn parse_digest(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let nibble = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        *byte = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}
