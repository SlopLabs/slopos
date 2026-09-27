//! The `<netdb.h>` lookups besides `getaddrinfo`: `getnameinfo`, and the
//! older `gethostbyname` and `getservbyname` with `h_errno`.
//!
//! There is no reverse resolver and no services database, so a host is
//! named by its numeric form and a service by its port number, as
//! `getnameinfo` does when a name cannot be found.

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int};

use slopos_slibc_core::inet;

use super::addr::{AF_INET, AF_INET6, SOCK_STREAM};
use super::dns::{
    AddrInfo, EAI_AGAIN, EAI_BADFLAGS, EAI_FAIL, EAI_FAMILY, EAI_MEMORY, EAI_NONAME, freeaddrinfo,
    getaddrinfo,
};
use crate::string::u_strlen;

pub const NI_NUMERICHOST: c_int = 0x01;
pub const NI_NUMERICSERV: c_int = 0x02;
pub const NI_NOFQDN: c_int = 0x04;
pub const NI_NAMEREQD: c_int = 0x08;
pub const NI_DGRAM: c_int = 0x10;
pub const NI_NUMERICSCOPE: c_int = 0x100;
const NI_KNOWN: c_int =
    NI_NUMERICHOST | NI_NUMERICSERV | NI_NOFQDN | NI_NAMEREQD | NI_DGRAM | NI_NUMERICSCOPE;

const EAI_OVERFLOW: c_int = -12;

pub const HOST_NOT_FOUND: c_int = 1;
pub const TRY_AGAIN: c_int = 2;
pub const NO_RECOVERY: c_int = 3;
pub const NO_DATA: c_int = 4;

#[thread_local]
static mut H_ERRNO: c_int = 0;

/// The thread's `h_errno`, which `<netdb.h>` spells as a macro over this.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __h_errno_location() -> *mut c_int {
    &raw mut H_ERRNO
}

/// `struct hostent`, as the target's `libc` declares it.
#[repr(C)]
pub struct Hostent {
    pub h_name: *mut c_char,
    pub h_aliases: *mut *mut c_char,
    pub h_addrtype: c_int,
    pub h_length: c_int,
    pub h_addr_list: *mut *mut c_char,
}

/// `struct servent`, as the target's `libc` declares it.
#[repr(C)]
pub struct Servent {
    pub s_name: *mut c_char,
    pub s_aliases: *mut *mut c_char,
    pub s_port: c_int,
    pub s_proto: *mut c_char,
}

const NAME_MAX: usize = 256;

/// The one static result POSIX lets `gethostbyname` overwrite on each call.
struct HostResult {
    entry: Hostent,
    name: [u8; NAME_MAX],
    addr: [u8; 4],
    addrs: [*mut c_char; 2],
    aliases: [*mut c_char; 1],
}

struct HostSlot(UnsafeCell<HostResult>);

// SAFETY: POSIX has `gethostbyname` return one static result that each call
// overwrites; it is not required to be thread-safe, and callers that share it
// across threads serialise themselves.
unsafe impl Sync for HostSlot {}

static HOST: HostSlot = HostSlot(UnsafeCell::new(HostResult {
    entry: Hostent {
        h_name: core::ptr::null_mut(),
        h_aliases: core::ptr::null_mut(),
        h_addrtype: 0,
        h_length: 0,
        h_addr_list: core::ptr::null_mut(),
    },
    name: [0; NAME_MAX],
    addr: [0; 4],
    addrs: [core::ptr::null_mut(); 2],
    aliases: [core::ptr::null_mut(); 1],
}));

/// An IPv4 address for `name` through `getaddrinfo`, or null with
/// `h_errno` set.
///
/// # Safety
/// `name` is NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gethostbyname(name: *const c_char) -> *mut Hostent {
    if name.is_null() {
        H_ERRNO = HOST_NOT_FOUND;
        return core::ptr::null_mut();
    }
    let mut hints: AddrInfo = core::mem::zeroed();
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    let mut found: *mut AddrInfo = core::ptr::null_mut();
    let code = getaddrinfo(name.cast(), core::ptr::null(), &hints, &mut found);
    if code != 0 {
        H_ERRNO = match code {
            EAI_NONAME => HOST_NOT_FOUND,
            EAI_AGAIN => TRY_AGAIN,
            EAI_FAIL | EAI_MEMORY => NO_RECOVERY,
            _ => NO_RECOVERY,
        };
        return core::ptr::null_mut();
    }
    let result = &mut *HOST.0.get();
    // `sin_addr` sits four bytes into a `sockaddr_in`.
    core::ptr::copy_nonoverlapping(
        (*found).ai_addr.cast::<u8>().add(4),
        result.addr.as_mut_ptr(),
        4,
    );
    freeaddrinfo(found);

    let len = u_strlen(name.cast()).min(NAME_MAX - 1);
    core::ptr::copy_nonoverlapping(name.cast::<u8>(), result.name.as_mut_ptr(), len);
    result.name[len] = 0;
    result.addrs = [result.addr.as_mut_ptr().cast(), core::ptr::null_mut()];
    result.aliases = [core::ptr::null_mut()];
    result.entry = Hostent {
        h_name: result.name.as_mut_ptr().cast(),
        h_aliases: result.aliases.as_mut_ptr(),
        h_addrtype: AF_INET,
        h_length: 4,
        h_addr_list: result.addrs.as_mut_ptr(),
    };
    &mut result.entry
}

/// No services database: every name is not found.
///
/// # Safety
/// Any arguments; neither is read.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getservbyname(
    _name: *const c_char,
    _proto: *const c_char,
) -> *mut Servent {
    core::ptr::null_mut()
}

/// Copies `text` and a terminator into `dst`, or answers `EAI_OVERFLOW`.
unsafe fn put(text: &[u8], dst: *mut c_char, len: u32) -> Result<(), c_int> {
    if text.len() >= len as usize {
        return Err(EAI_OVERFLOW);
    }
    core::ptr::copy_nonoverlapping(text.as_ptr(), dst.cast::<u8>(), text.len());
    *dst.cast::<u8>().add(text.len()) = 0;
    Ok(())
}

/// # Safety
/// `sa` holds `salen` bytes; `host` and `serv` are null or hold `hostlen`
/// and `servlen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getnameinfo(
    sa: *const super::SockAddr,
    salen: u32,
    host: *mut c_char,
    hostlen: u32,
    serv: *mut c_char,
    servlen: u32,
    flags: c_int,
) -> c_int {
    if flags & !NI_KNOWN != 0 {
        return EAI_BADFLAGS;
    }
    if sa.is_null() {
        return EAI_FAMILY;
    }
    let raw = sa.cast::<u8>();
    let family = c_int::from(u16::from_ne_bytes([*raw, *raw.add(1)]));
    let port = u16::from_be_bytes([*raw.add(2), *raw.add(3)]);
    let mut text = [0u8; inet::IPV6_TEXT_MAX];
    let written = match family {
        AF_INET if salen >= 16 => {
            let mut addr = [0u8; 4];
            core::ptr::copy_nonoverlapping(raw.add(4), addr.as_mut_ptr(), 4);
            inet::format_ipv4(addr, &mut text)
        }
        AF_INET6 if salen >= 28 => {
            let mut addr = [0u8; 16];
            core::ptr::copy_nonoverlapping(raw.add(8), addr.as_mut_ptr(), 16);
            inet::format_ipv6(addr, &mut text)
        }
        _ => return EAI_FAMILY,
    };
    let Some(len) = written else {
        return EAI_OVERFLOW;
    };

    if !host.is_null() && hostlen > 0 {
        if flags & NI_NAMEREQD != 0 {
            return EAI_NONAME;
        }
        if let Err(code) = put(&text[..len], host, hostlen) {
            return code;
        }
    }
    if !serv.is_null() && servlen > 0 {
        let mut digits = [0u8; 5];
        let mut at = digits.len();
        let mut value = port;
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        if let Err(code) = put(&digits[at..], serv, servlen) {
            return code;
        }
    }
    0
}
