//! Reading a file by a name someone else may have put anything at: a link,
//! a FIFO that never opens, a device that never ends.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use slopos_abi::fs::O_NOFOLLOW;
use slopos_abi::syscall::posix::{O_CLOEXEC, O_NOCTTY, O_NONBLOCK};

/// The most a picture file may hold, past what any picture the decoder
/// takes compresses to.
pub const PICTURE_MAX: usize = 64 << 20;

/// The first `most` bytes of the regular file at `path`, opened without
/// following a link at its last step or waiting on whatever is there:
/// `None` for anything else, and for a file `owner` does not own.
pub fn read(path: &str, most: usize, owner: Option<u32>) -> Option<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((O_NOFOLLOW as u64 | O_NONBLOCK | O_NOCTTY | O_CLOEXEC) as i32)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || owner.is_some_and(|uid| meta.uid() != uid) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(most as u64).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// The picture at `path`, decoded, when it is a regular file the decoder
/// takes.
pub fn picture(path: &str) -> Option<slopos_image::Image> {
    let bytes = read(path, PICTURE_MAX + 1, None).filter(|b| b.len() <= PICTURE_MAX)?;
    slopos_image::decode(&bytes, slopos_image::DecodeOptions::default()).ok()
}
