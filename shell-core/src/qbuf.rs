//! A byte buffer that remembers, per byte, how expansion produced it.
//!
//! Field splitting and pathname expansion act on unquoted expansion output
//! only, which is a per-byte question a plain `Vec<u8>` cannot answer. An
//! in-band sentinel — what several C shells use — collides with the arbitrary
//! bytes a filename may hold; a parallel flag vector cannot.

use alloc::vec::Vec;

/// Quoted: neither splits into fields nor acts as a pattern character.
pub const Q_QUOTED: u8 = 1 << 0;
/// Came from an unquoted expansion, so eligible for field splitting.
pub const Q_SPLIT: u8 = 1 << 1;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QBuf {
    pub bytes: Vec<u8>,
    pub flags: Vec<u8>,
}

impl QBuf {
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            flags: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn push(&mut self, byte: u8, flags: u8) {
        self.bytes.push(byte);
        self.flags.push(flags);
    }

    pub fn extend(&mut self, bytes: &[u8], flags: u8) {
        self.bytes.extend_from_slice(bytes);
        self.flags.resize(self.bytes.len(), flags);
    }

    pub fn append(&mut self, other: &QBuf) {
        self.bytes.extend_from_slice(&other.bytes);
        self.flags.extend_from_slice(&other.flags);
    }

    pub fn slice(&self, start: usize, end: usize) -> QBuf {
        QBuf {
            bytes: self.bytes[start..end].to_vec(),
            flags: self.flags[start..end].to_vec(),
        }
    }

    #[inline]
    pub fn quoted(&self, i: usize) -> bool {
        self.flags[i] & Q_QUOTED != 0
    }

    #[inline]
    pub fn splittable(&self, i: usize) -> bool {
        self.flags[i] & Q_SPLIT != 0
    }

    /// The finished bytes, dropping the provenance.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}
