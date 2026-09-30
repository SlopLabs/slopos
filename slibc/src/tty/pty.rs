//! Pseudo-terminal masters: `/dev/ptmx` allocates one, and its slave is
//! `/dev/pts/<n>`.

use core::ffi::{c_char, c_int};

use crate::errno::{EINVAL, ERANGE, errno_set};
use crate::pal::{Pal, Sys};
use slopos_abi::syscall::{TIOCGPTN, TIOCSPTLCK};

const PTMX: &[u8] = b"/dev/ptmx\0";
const PTS_DIR: &[u8] = b"/dev/pts/";

/// `posix_openpt(3)`: a new master, opened with `flags` (`O_RDWR`,
/// `O_NOCTTY`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_openpt(flags: c_int) -> c_int {
    match Sys::open(PTMX.as_ptr(), flags, 0) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

fn slave_number(fd: c_int) -> Result<u32, c_int> {
    let mut number = 0u32;
    Sys::ioctl(fd, TIOCGPTN, (&raw mut number) as u64)
        .map(|_| number)
        .map_err(|e| e.raw())
}

/// `grantpt(3)`: the slave is already the caller's, with nothing to change on
/// a system of one user; checks only that `fd` is a master.
#[unsafe(no_mangle)]
pub extern "C" fn grantpt(fd: c_int) -> c_int {
    match slave_number(fd) {
        Ok(_) => 0,
        Err(rc) => {
            errno_set(rc);
            -1
        }
    }
}

/// `unlockpt(3)`: let the slave be opened.
#[unsafe(no_mangle)]
pub extern "C" fn unlockpt(fd: c_int) -> c_int {
    let unlock: c_int = 0;
    match Sys::ioctl(fd, TIOCSPTLCK, (&raw const unlock) as u64) {
        Ok(_) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `ptsname_r(3)`: the slave's path. 0, or an errno.
///
/// # Safety
/// `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ptsname_r(fd: c_int, buf: *mut c_char, buflen: usize) -> c_int {
    if buf.is_null() {
        return EINVAL.raw();
    }
    let number = match slave_number(fd) {
        Ok(n) => n,
        Err(rc) => return rc,
    };
    let mut digits = [0u8; SLAVE_DIGITS_MAX];
    let mut at = digits.len();
    let mut rest = number;
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let digits = &digits[at..];
    let len = PTS_DIR.len() + digits.len();
    if len + 1 > buflen {
        return ERANGE.raw();
    }
    let out = buf as *mut u8;
    core::ptr::copy_nonoverlapping(PTS_DIR.as_ptr(), out, PTS_DIR.len());
    core::ptr::copy_nonoverlapping(digits.as_ptr(), out.add(PTS_DIR.len()), digits.len());
    *out.add(len) = 0;
    0
}

const SLAVE_DIGITS_MAX: usize = u32::MAX.ilog10() as usize + 1;
const PTSNAME_MAX: usize = PTS_DIR.len() + SLAVE_DIGITS_MAX + 1;
static mut PTSNAME: [u8; PTSNAME_MAX] = [0; PTSNAME_MAX];

/// `ptsname(3)`: [`ptsname_r`] into static storage; `NULL` with `errno` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ptsname(fd: c_int) -> *mut c_char {
    let buf = (&raw mut PTSNAME).cast::<c_char>();
    match ptsname_r(fd, buf, PTSNAME_MAX) {
        0 => buf,
        rc => {
            errno_set(rc);
            core::ptr::null_mut()
        }
    }
}
