//! Safe wrappers over `ffi::syscalls::*` for use from tests.

use super::syscalls::{self, SloposStat};

pub fn stat(path: &[u8], stat_buf: &mut SloposStat) -> i32 {
    // SAFETY: `path` is a NUL-terminated byte slice; `stat_buf` is a
    // live `&mut SloposStat`. Both pointers valid for the call's duration.
    unsafe {
        syscalls::stat(
            path.as_ptr() as *const core::ffi::c_char,
            stat_buf as *mut SloposStat,
        )
    }
}

pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    // SAFETY: extern reads no memory; arguments are plain integers.
    unsafe { syscalls::lseek(fd, offset, whence) }
}

pub fn slopos_futex_wake(addr: &u32, count: u32) -> i32 {
    // SAFETY: `addr` is a live `&u32`; pointer is non-null, aligned,
    // valid for one u32 read.
    unsafe { syscalls::slopos_futex_wake(addr as *const u32, count) }
}

pub fn pipe(fds: &mut [i32; 2]) -> i32 {
    // SAFETY: `fds` is a live `&mut [i32; 2]`; pointer is non-null,
    // aligned, valid for two i32 writes.
    unsafe { crate::io::misc::pipe(fds.as_mut_ptr()) }
}

pub fn clock_gettime(clk_id: i32, ts: &mut crate::time::Timespec) -> i32 {
    // SAFETY: `ts` is a live `&mut Timespec`; pointer is non-null, aligned
    // and valid for one write.
    unsafe { crate::time::clock_gettime(clk_id, ts as *mut crate::time::Timespec) }
}

/// `crate::ffi::close` is already a safe extern; this only keeps test imports tidy.
pub fn close(fd: i32) -> i32 {
    crate::ffi::close(fd)
}
