//! `inet_pton` and `inet_ntop`, over `slopos_slibc_core::inet`.

use core::ffi::{c_char, c_int, c_void};

use slopos_slibc_core::inet;

use super::addr::{AF_INET, AF_INET6};
use crate::errno::{EAFNOSUPPORT, ENOSPC, errno_set};
use crate::string::u_strlen;

pub const INET_ADDRSTRLEN: c_int = 16;
pub const INET6_ADDRSTRLEN: c_int = 46;

/// 1 with the address in `dst`, 0 for text that is not one, -1 with
/// `EAFNOSUPPORT` for a family other than `AF_INET` or `AF_INET6`.
///
/// # Safety
/// `src` is NUL-terminated; `dst` holds an `in_addr` or an `in6_addr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inet_pton(af: c_int, src: *const c_char, dst: *mut c_void) -> c_int {
    let text = core::slice::from_raw_parts(src.cast::<u8>(), u_strlen(src.cast()));
    match af {
        AF_INET => match inet::parse_ipv4(text) {
            Some(addr) => {
                core::ptr::copy_nonoverlapping(addr.as_ptr(), dst.cast::<u8>(), addr.len());
                1
            }
            None => 0,
        },
        AF_INET6 => match inet::parse_ipv6(text) {
            Some(addr) => {
                core::ptr::copy_nonoverlapping(addr.as_ptr(), dst.cast::<u8>(), addr.len());
                1
            }
            None => 0,
        },
        _ => {
            errno_set(EAFNOSUPPORT.raw());
            -1
        }
    }
}

/// `dst`, or null with `EAFNOSUPPORT` or `ENOSPC`.
///
/// # Safety
/// `src` holds an `in_addr` or an `in6_addr`; `dst` has `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inet_ntop(
    af: c_int,
    src: *const c_void,
    dst: *mut c_char,
    size: u32,
) -> *const c_char {
    let mut text = [0u8; inet::IPV6_TEXT_MAX];
    let written = match af {
        AF_INET => {
            let mut addr = [0u8; 4];
            core::ptr::copy_nonoverlapping(src.cast::<u8>(), addr.as_mut_ptr(), 4);
            inet::format_ipv4(addr, &mut text)
        }
        AF_INET6 => {
            let mut addr = [0u8; 16];
            core::ptr::copy_nonoverlapping(src.cast::<u8>(), addr.as_mut_ptr(), 16);
            inet::format_ipv6(addr, &mut text)
        }
        _ => {
            errno_set(EAFNOSUPPORT.raw());
            return core::ptr::null();
        }
    };
    let Some(len) = written else {
        errno_set(ENOSPC.raw());
        return core::ptr::null();
    };
    if dst.is_null() || len >= size as usize {
        errno_set(ENOSPC.raw());
        return core::ptr::null();
    }
    core::ptr::copy_nonoverlapping(text.as_ptr(), dst.cast::<u8>(), len);
    *dst.cast::<u8>().add(len) = 0;
    dst
}
