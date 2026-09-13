//! Miscellaneous POSIX file operations.

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const u8, mode: i32) -> i32 {
    if path.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    match Sys::access(path, mode as u32) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Stub: the mask is ignored; always reports 0o022.
#[unsafe(no_mangle)]
pub extern "C" fn umask(_mask: u32) -> u32 {
    0o022
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn chmod(path: *const u8, mode: u32) -> i32 {
    match Sys::chmod(path, mode) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pipe(pipefd: *mut i32) -> i32 {
    if pipefd.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    let fds_arr = &mut *(pipefd as *mut [i32; 2]);
    match Sys::pipe(fds_arr) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup(oldfd: i32) -> i32 {
    match Sys::dup(oldfd) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dup2(oldfd: i32, newfd: i32) -> i32 {
    match Sys::dup2(oldfd, newfd) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fcntl(fd: i32, cmd: i32, arg: i64) -> i32 {
    match Sys::fcntl(fd, cmd, arg as u64) {
        Ok(ret) => ret,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Returns 1 if `fd` is a terminal, 0 otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isatty(fd: i32) -> i32 {
    let mut buf = [0u8; core::mem::size_of::<slopos_abi::syscall::UserTermios>()];
    match Sys::ioctl(fd, slopos_abi::syscall::TCGETS, buf.as_mut_ptr() as u64) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}
