//! Terminal I/O — termios and raw mode.
//!
//! `termios` is the one shape slibc does *not* translate to the
//! libc-declared layout: `NCCS` is 19 here and 32 on Linux, and no std path
//! reads the struct, so the plan keeps that divergence rather than paying for
//! a conversion nothing consumes. [`crate::types`] pins the kernel's size so
//! it stays the stated divergence.
//!
//! These entry points therefore speak [`UserTermios`], and `ioctl` — which
//! lives in [`crate::io::misc`] — passes the caller's pointer through
//! untouched.

#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};
use slopos_abi::syscall::{
    InputFlags, LocalFlags, OutputFlags, TCGETS, TCSETS, TCSETSF, TCSETSW, UserTermios, VMIN, VTIME,
};

pub const TCSANOW: i32 = 0;
pub const TCSADRAIN: i32 = 1;
pub const TCSAFLUSH: i32 = 2;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcgetattr(fd: i32, termios: *mut UserTermios) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    match Sys::ioctl(fd, TCGETS, termios as u64) {
        Ok(_) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `optional_actions` selects when the change takes effect: `TCSANOW`
/// immediately, `TCSADRAIN` after pending output drains, `TCSAFLUSH` after
/// output drains and pending input is discarded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcsetattr(
    fd: i32,
    optional_actions: i32,
    termios: *const UserTermios,
) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }

    let request = match optional_actions {
        TCSANOW => TCSETS,
        TCSADRAIN => TCSETSW,
        TCSAFLUSH => TCSETSF,
        _ => {
            errno_set(crate::errno::EINVAL.raw());
            return -1;
        }
    };

    match Sys::ioctl(fd, request, termios as u64) {
        Ok(_) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfmakeraw(termios: *mut UserTermios) {
    if termios.is_null() {
        return;
    }

    (*termios).c_iflag &= !(InputFlags::IGNBRK
        | InputFlags::INPCK
        | InputFlags::ISTRIP
        | InputFlags::INPCK
        | InputFlags::ICRNL
        | InputFlags::IXON);
    (*termios).c_oflag &= !OutputFlags::OPOST;
    (*termios).c_lflag &= !(LocalFlags::ECHO
        | LocalFlags::ECHOE
        | LocalFlags::ECHOK
        | LocalFlags::ECHONL
        | LocalFlags::ICANON
        | LocalFlags::ISIG
        | LocalFlags::IEXTEN);
    (*termios).c_cc[VMIN] = 1;
    (*termios).c_cc[VTIME] = 0;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfgetispeed(termios: *const UserTermios) -> u32 {
    if termios.is_null() {
        return 0;
    }
    (*termios).c_ispeed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfsetispeed(termios: *mut UserTermios, speed: u32) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    (*termios).c_ispeed = speed;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfgetospeed(termios: *const UserTermios) -> u32 {
    if termios.is_null() {
        return 0;
    }
    (*termios).c_ospeed
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cfsetospeed(termios: *mut UserTermios, speed: u32) -> i32 {
    if termios.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    (*termios).c_ospeed = speed;
    0
}

/// `tcgetpgrp(3)`: the terminal's foreground process group.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcgetpgrp(fd: i32) -> i32 {
    let mut pgrp = 0i32;
    match Sys::ioctl(fd, slopos_abi::syscall::TIOCGPGRP, (&raw mut pgrp) as u64) {
        Ok(_) => pgrp,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `tcsetpgrp(3)`: make `pgrp` the terminal's foreground process group.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tcsetpgrp(fd: i32, pgrp: i32) -> i32 {
    match Sys::ioctl(fd, slopos_abi::syscall::TIOCSPGRP, (&raw const pgrp) as u64) {
        Ok(_) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

static mut PASSWORD: [u8; 128] = [0; 128];

/// `getpass(3)`: prompt on the controlling terminal (stderr if there is none)
/// and read a line from it (stdin if none) with echo off. The answer, newline
/// dropped, lives in static storage; `NULL` on a read error.
///
/// # Safety
/// `prompt` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpass(prompt: *const u8) -> *mut u8 {
    let flags = crate::ffi::O_RDWR | slopos_abi::syscall::O_NOCTTY as i32 | crate::ffi::O_CLOEXEC;
    let tty = Sys::open(b"/dev/tty\0".as_ptr(), flags, 0).ok();
    let (input, output) = match tty {
        Some(fd) => (fd, fd),
        None => (0, 2),
    };

    let mut saved = core::mem::MaybeUninit::<UserTermios>::uninit();
    let restore = Sys::ioctl(input, TCGETS, saved.as_mut_ptr() as u64).is_ok();
    if restore {
        let mut quiet = saved.assume_init();
        quiet.c_lflag.remove(LocalFlags::ECHO);
        quiet.c_lflag.insert(LocalFlags::ICANON);
        let _ = Sys::ioctl(input, TCSETSF, (&raw const quiet) as u64);
    }

    if !prompt.is_null() {
        let _ = Sys::write(output, prompt, crate::string::u_strlen(prompt));
    }
    let buf = (&raw mut PASSWORD).cast::<u8>();
    let answer = match Sys::read(input, buf, 128) {
        Ok(mut len) => {
            if (len > 0 && *buf.add(len - 1) == b'\n') || len == 128 {
                len -= 1;
            }
            *buf.add(len) = 0;
            buf
        }
        Err(e) => {
            errno_set(e.raw());
            core::ptr::null_mut()
        }
    };

    if restore {
        let _ = Sys::ioctl(input, TCSETSF, saved.as_ptr() as u64);
    }
    let _ = Sys::write(output, b"\n".as_ptr(), 1);
    if let Some(fd) = tty {
        let _ = Sys::close(fd);
    }
    answer
}
