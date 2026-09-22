//! Miscellaneous POSIX file operations.

use core::ffi::{c_char, c_int, c_ulong, c_void};

use crate::errno::{EINVAL, ENODEV, ENOTTY, ERANGE, errno_set};
use crate::pal::{Pal, Sys};
use crate::types::stat;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(path: *const u8, mode: i32) -> i32 {
    if path.is_null() {
        errno_set(EINVAL.raw());
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
        errno_set(EINVAL.raw());
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
pub unsafe extern "C" fn pipe2(pipefd: *mut c_int, flags: c_int) -> c_int {
    if pipefd.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let fds_arr = &mut *(pipefd as *mut [i32; 2]);
    match Sys::pipe2(fds_arr, flags as u32) {
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
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    match Sys::dup3(oldfd, newfd, flags) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `fcntl(2)`, variadic as C has it.
///
/// The third argument is read only for the commands that carry one: an `int`
/// for the descriptor-flag and duplication commands, a `struct flock *` for
/// the record locks. A command that takes none — `F_GETFD`, `F_GETFL` — must
/// not pull a `va_arg` that the caller never pushed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, mut args: ...) -> c_int {
    use slopos_abi::syscall::{F_DUPFD, F_GETLK, F_SETFD, F_SETFL, F_SETLK, F_SETLKW};

    let cmd_u = cmd as u64;
    let arg: u64 =
        if cmd_u == F_DUPFD || cmd_u == F_DUPFD_CLOEXEC || cmd_u == F_SETFD || cmd_u == F_SETFL {
            args.next_arg::<c_int>() as u64
        } else if cmd_u == F_GETLK || cmd_u == F_SETLK || cmd_u == F_SETLKW {
            args.next_arg::<*mut c_void>() as u64
        } else {
            0
        };

    match Sys::fcntl(fd, cmd, arg) {
        Ok(ret) => ret,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

pub const F_DUPFD_CLOEXEC: u64 = slopos_abi::syscall::F_DUPFD_CLOEXEC;

/// Returns 1 if `fd` is a terminal, 0 otherwise.
///
/// Goes to `Sys::ioctl` rather than the exported `ioctl` so the probe carries
/// the kernel's own `UserTermios` size: the libc-declared `struct termios` is
/// wider, and this is a probe rather than a read of the settings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn isatty(fd: i32) -> i32 {
    let mut buf = [0u8; core::mem::size_of::<slopos_abi::syscall::UserTermios>()];
    match Sys::ioctl(fd, slopos_abi::syscall::TCGETS, buf.as_mut_ptr() as u64) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

/// `ioctl(2)`, variadic as C has it.
///
/// One argument is always read, which is what every libc does and what every
/// caller passes: the requests that take none are vanishingly rare and pass a
/// harmless unused register slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ioctl(fd: c_int, request: c_ulong, mut args: ...) -> c_int {
    let arg = args.next_arg::<*mut c_void>() as u64;
    match Sys::ioctl(fd, request, arg) {
        Ok(ret) => ret,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Directories scanned for the device node matching a terminal descriptor.
/// There is no `/proc`, so the name has to be found the way BSD finds it: by
/// looking for the character device whose `st_rdev` matches.
const TTY_SEARCH_DIRS: [&[u8]; 2] = [b"/dev\0", b"/dev/pts\0"];

/// `ttyname_r(3)`.
///
/// `ENOTTY` when `fd` is not a terminal, `ERANGE` when the answer does not
/// fit, and `ENODEV` when `fd` *is* a terminal whose device node is not
/// reachable under the directories above — which is the honest answer, since
/// without `/proc/self/fd` there is nothing else to consult.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ttyname_r(fd: c_int, buf: *mut c_char, buflen: usize) -> c_int {
    if buf.is_null() || buflen == 0 {
        return EINVAL.raw();
    }
    if isatty(fd) == 0 {
        return ENOTTY.raw();
    }

    let mut target = stat::default();
    if let Err(e) = Sys::fstat(fd, &raw mut target as *mut u8) {
        return e.raw();
    }

    for dir_path in TTY_SEARCH_DIRS {
        let dirp = crate::io::dir::opendir(dir_path.as_ptr() as *const c_char);
        if dirp.is_null() {
            continue;
        }
        let dir_len = dir_path.len() - 1; // drop the NUL
        loop {
            let entry = crate::io::dir::readdir(dirp);
            if entry.is_null() {
                break;
            }
            let name_ptr = (&raw const (*entry).d_name) as *const u8;
            let name_len = crate::string::u_strnlen(name_ptr, 255);
            if name_len == 0 {
                continue;
            }

            // "<dir>/<name>\0" staged on the stack so the candidate can be
            // stat'ed before anything is written into the caller's buffer.
            let mut candidate = [0u8; 320];
            let total = dir_len + 1 + name_len;
            if total + 1 > candidate.len() {
                continue;
            }
            core::ptr::copy_nonoverlapping(dir_path.as_ptr(), candidate.as_mut_ptr(), dir_len);
            candidate[dir_len] = b'/';
            core::ptr::copy_nonoverlapping(
                name_ptr,
                candidate.as_mut_ptr().add(dir_len + 1),
                name_len,
            );

            let mut probe = stat::default();
            if Sys::stat(candidate.as_ptr(), &raw mut probe as *mut u8).is_err() {
                continue;
            }
            if probe.file_kind() != slopos_abi::fs::S_IFCHR || probe.st_rdev != target.st_rdev {
                continue;
            }

            let _ = crate::io::dir::closedir(dirp);
            if total + 1 > buflen {
                return ERANGE.raw();
            }
            core::ptr::copy_nonoverlapping(candidate.as_ptr(), buf as *mut u8, total);
            *(buf as *mut u8).add(total) = 0;
            return 0;
        }
        let _ = crate::io::dir::closedir(dirp);
    }

    ENODEV.raw()
}
