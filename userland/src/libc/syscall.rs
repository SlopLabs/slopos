//! C-ABI syscall wrappers for libc compatibility.
//!
//! These functions provide POSIX-style interfaces that delegate to the
//! typed syscall module. Used by ffi.rs for extern "C" exports.

use core::ffi::{c_char, c_int, c_void};

use crate::syscall::{fs, memory};

/// POSIX-style read from file descriptor.
#[inline]
pub fn sys_read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    // Safety: libc layer handles raw pointers per POSIX semantics
    unsafe { fs::read_raw(fd, buf, count) as isize }
}

/// POSIX-style write to file descriptor.
#[inline]
pub fn sys_write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    // Safety: libc layer handles raw pointers per POSIX semantics
    unsafe { fs::write_raw(fd, buf, count) as isize }
}

/// POSIX-style open file.
#[inline]
pub fn sys_open(path: *const c_char, flags: c_int) -> c_int {
    // Safety: libc layer handles raw pointers per POSIX semantics
    unsafe { fs::open_raw(path, flags as u32) as c_int }
}

/// POSIX-style close file descriptor.
#[inline]
pub fn sys_close(fd: c_int) -> c_int {
    fs::close_raw(fd) as c_int
}

/// POSIX-style exit with status code.
#[inline]
pub fn sys_exit(status: c_int) -> ! {
    crate::syscall::core::exit_with_code(status)
}

/// POSIX-style brk syscall.
#[inline]
pub fn sys_brk(addr: *mut c_void) -> *mut c_void {
    memory::brk(addr)
}

/// POSIX-style sbrk syscall.
#[inline]
pub fn sys_sbrk(increment: isize) -> *mut c_void {
    memory::sbrk(increment)
}
