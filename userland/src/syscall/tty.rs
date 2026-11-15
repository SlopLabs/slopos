//! Console I/O: the kernel log, through `/dev/kmsg`, the console font, and
//! terminal names.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicI32, Ordering};

use super::RawFd;
use super::error::SyscallError;
use super::raw::syscall4;
use slopos_abi::fs::{KMSG_PATH, KMSG_RECORD_MAX};
use slopos_abi::syscall::{F_DUPFD_CLOEXEC, O_CLOEXEC, SYSCALL_FONT_SET};
use slopos_slibc::pal::{Pal, Sys};

/// The lowest number the log's descriptor takes: below it a shell's
/// redirections reuse numbers, and a kept one would be clobbered.
const LOG_FD_MIN: u64 = 10;

/// The kernel log's descriptor, `-1` until it is opened; kept for the
/// process's life.
static LOG_FD: AtomicI32 = AtomicI32::new(-1);

/// Open the kernel log now, so a panic hook that writes it later never opens
/// a file. Later calls answer the descriptor already open.
pub fn open_log() -> io::Result<RawFd> {
    let fd = LOG_FD.load(Ordering::Acquire);
    if fd >= 0 {
        return Ok(fd);
    }
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(O_CLOEXEC as i32)
        .open(KMSG_PATH)?;
    let opened = Sys::fcntl(file.as_raw_fd(), F_DUPFD_CLOEXEC as i32, LOG_FD_MIN)
        .map_err(|e| io::Error::from_raw_os_error(e.0))?;
    match LOG_FD.compare_exchange(-1, opened, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(opened),
        Err(winner) => {
            let _ = super::fs::close_fd_raw(opened);
            Ok(winner)
        }
    }
}

/// Write `buf` to the kernel log, one record per line, a line longer than a
/// record split across several. Answers `buf.len()`, or a negative errno.
pub fn write(buf: &[u8]) -> i64 {
    let fd = match open_log() {
        Ok(fd) => fd,
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(SyscallError::EIO.errno());
            return -i64::from(errno);
        }
    };
    for line in buf.split(|&b| b == b'\n').filter(|line| !line.is_empty()) {
        for record in line.chunks(KMSG_RECORD_MAX) {
            if let Err(e) = super::fs::write_slice(fd, record) {
                return -i64::from(e.errno());
            }
        }
    }
    buf.len() as i64
}

#[inline(always)]
pub fn font_set(data: &[u8], width: u16, height: u16) -> i64 {
    unsafe {
        syscall4(
            SYSCALL_FONT_SET,
            data.as_ptr() as u64,
            width as u64,
            height as u64,
            slopos_font::GLYPH_COUNT as u64,
        ) as i64
    }
}

/// The path of the terminal `fd` reads, as `ttyname` finds it.
pub fn name_of(fd: super::RawFd) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is as long as the length handed with it.
    let rc = unsafe { slopos_slibc::io::misc::ttyname_r(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    String::from_utf8(buf[..len].to_vec()).ok()
}
