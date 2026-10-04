//! The kernel log as a program reads it back: what a command the kernel ran on
//! its behalf added there.

use std::io;

pub const PATH: &str = "/dev/kmsg";
/// How much of the log's tail locates the old end once the ring has wrapped.
const ANCHOR_BYTES: usize = 4096;

/// The whole log as `/dev/kmsg` serves it now.
pub fn read() -> io::Result<Vec<u8>> {
    std::fs::read(PATH)
}

/// What the log gained between `before` and `after`. The log is a ring read
/// by offset, so once it is full its old end has moved down by however much
/// was added; the tail of `before` finds it again. `None` when it wrapped past
/// that tail too.
pub fn added<'a>(before: &[u8], after: &'a [u8]) -> Option<&'a [u8]> {
    if after.len() > before.len() && after[..before.len()] == *before {
        return Some(&after[before.len()..]);
    }
    let anchor = &before[before.len().saturating_sub(ANCHOR_BYTES)..];
    if anchor.is_empty() {
        return Some(after);
    }
    let at = after
        .windows(anchor.len())
        .rposition(|window| window == anchor)?;
    Some(&after[at + anchor.len()..])
}
