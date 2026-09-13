//! Walker over a packed `getdents64(2)` buffer.

use slopos_abi::fs::UserDirent64;

/// `UserDirent64` has no `d_name` member, so `#[repr(C)]` tail-pads the header
/// out to 24 — deliberately not Linux's 19.
pub const DIRENT_NAME_OFFSET: usize = core::mem::size_of::<UserDirent64>();

const D_RECLEN_OFFSET: usize = 16;
const D_TYPE_OFFSET: usize = 18;

/// `name` borrows the buffer and excludes the NUL and the trailing padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirentRecord<'a> {
    pub d_ino: u64,
    pub d_type: u8,
    pub name: &'a [u8],
}

/// A malformed record ends the walk rather than being skipped: `d_reclen` is
/// what says where the next record starts, so a bad one cannot be stepped over.
pub struct DirentIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> DirentIter<'a> {
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl<'a> Iterator for DirentIter<'a> {
    type Item = DirentRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let rec = self.buf.get(self.pos..)?;
        if rec.len() <= DIRENT_NAME_OFFSET {
            return None;
        }

        let reclen = u16::from_ne_bytes([rec[D_RECLEN_OFFSET], rec[D_RECLEN_OFFSET + 1]]) as usize;
        if reclen <= DIRENT_NAME_OFFSET || reclen > rec.len() {
            return None;
        }

        let d_ino = u64::from_ne_bytes([
            rec[0], rec[1], rec[2], rec[3], rec[4], rec[5], rec[6], rec[7],
        ]);
        let d_type = rec[D_TYPE_OFFSET];
        let tail = &rec[DIRENT_NAME_OFFSET..reclen];
        let name_len = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());

        self.pos += reclen;
        Some(DirentRecord {
            d_ino,
            d_type,
            name: &tail[..name_len],
        })
    }
}
