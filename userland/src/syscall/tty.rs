//! Console I/O with no descriptor: the kernel log and the controlling
//! terminal, neither of which is a `write(2)`/`read(2)` on an fd.

use super::numbers::{SYSCALL_CTTY_READ, SYSCALL_KLOG_WRITE};
use super::raw::{syscall2, syscall5};
use slopos_abi::syscall::SYSCALL_FONT_SET;

/// Write to the serialized kernel console. Linux's nearest analogue is a
/// write to `/dev/kmsg`.
#[inline(always)]
pub fn write(buf: &[u8]) -> i64 {
    unsafe { syscall2(SYSCALL_KLOG_WRITE, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

/// Cooked read of the caller's controlling terminal; `-ENXIO` without one.
#[inline(always)]
pub fn read(buf: &mut [u8]) -> i64 {
    unsafe { syscall2(SYSCALL_CTTY_READ, buf.as_mut_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
pub fn font_set_coverage(data: &[u8], width: u16, height: u16) -> i64 {
    unsafe {
        syscall5(
            SYSCALL_FONT_SET,
            data.as_ptr() as u64,
            width as u64,
            height as u64,
            slopos_font::GLYPH_COUNT as u64,
            1,
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
