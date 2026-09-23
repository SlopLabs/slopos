//! DNS resolution — getaddrinfo and friends.

use crate::errno::{EAGAIN, EHOSTUNREACH, EINTR, EINVAL, EIO, ENETUNREACH, Errno, errno_set};
use crate::mem::malloc;
use crate::pal::{Pal, Sys};
use crate::string::u_strlen;

use super::addr::{
    AF_INET, IPPROTO_TCP, IPPROTO_UDP, SOCK_DGRAM, SOCK_STREAM, SockAddr, SockAddrIn,
};

pub const EAI_BADFLAGS: i32 = -1;
pub const EAI_NONAME: i32 = -2;
pub const EAI_AGAIN: i32 = -3;
pub const EAI_FAIL: i32 = -4;
pub const EAI_FAMILY: i32 = -6;
pub const EAI_SOCKTYPE: i32 = -7;
pub const EAI_SERVICE: i32 = -8;
pub const EAI_MEMORY: i32 = -10;
pub const EAI_SYSTEM: i32 = -11;

pub const AI_PASSIVE: i32 = 0x01;
pub const AI_CANONNAME: i32 = 0x02;
pub const AI_NUMERICHOST: i32 = 0x04;
pub const AI_V4MAPPED: i32 = 0x08;
pub const AI_ALL: i32 = 0x10;
pub const AI_ADDRCONFIG: i32 = 0x20;
pub const AI_NUMERICSERV: i32 = 0x0400;
const AI_KNOWN: i32 = AI_PASSIVE
    | AI_CANONNAME
    | AI_NUMERICHOST
    | AI_V4MAPPED
    | AI_ALL
    | AI_ADDRCONFIG
    | AI_NUMERICSERV;

const AF_UNSPEC: i32 = 0;

/// Result node from getaddrinfo — POSIX `struct addrinfo`.
#[repr(C)]
pub struct AddrInfo {
    pub ai_flags: i32,
    pub ai_family: i32,
    pub ai_socktype: i32,
    pub ai_protocol: i32,
    pub ai_addrlen: u32,
    pub ai_addr: *mut SockAddr,
    pub ai_canonname: *mut u8,
    pub ai_next: *mut AddrInfo,
}

/// Decimal ports only; with no services database a name is `EAI_SERVICE`.
unsafe fn parse_service(service: *const u8) -> Result<u16, i32> {
    if service.is_null() {
        return Ok(0);
    }
    let len = u_strlen(service);
    let digits = core::slice::from_raw_parts(service, len);
    if digits.is_empty() || digits.len() > 5 || !digits.iter().all(u8::is_ascii_digit) {
        return Err(EAI_SERVICE);
    }
    let port = digits
        .iter()
        .fold(0u32, |acc, d| acc * 10 + (d - b'0') as u32);
    u16::try_from(port).map_err(|_| EAI_SERVICE)
}

fn resolve_error(errno: Errno) -> i32 {
    match errno {
        EINVAL | EHOSTUNREACH => EAI_NONAME,
        EAGAIN | ENETUNREACH | EINTR => EAI_AGAIN,
        EIO => EAI_FAIL,
        other => {
            errno_set(other.raw());
            EAI_SYSTEM
        }
    }
}

/// IPv4-only getaddrinfo: a dotted-decimal literal, or one A record from the
/// kernel's resolver.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getaddrinfo(
    node: *const u8,
    service: *const u8,
    hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
) -> i32 {
    if res.is_null() {
        errno_set(EINVAL.raw());
        return EAI_SYSTEM;
    }
    *res = core::ptr::null_mut();

    if node.is_null() && service.is_null() {
        return EAI_NONAME;
    }

    let (flags, family, sock_type, protocol) = if !hints.is_null() {
        let h = &*hints;
        (h.ai_flags, h.ai_family, h.ai_socktype, h.ai_protocol)
    } else {
        (0, AF_UNSPEC, 0, 0)
    };
    if family != AF_UNSPEC && family != AF_INET {
        return EAI_FAMILY;
    }
    if flags & !AI_KNOWN != 0 || (flags & AI_CANONNAME != 0 && node.is_null()) {
        return EAI_BADFLAGS;
    }
    let kinds: [(i32, i32); 2] = [(SOCK_STREAM, IPPROTO_TCP), (SOCK_DGRAM, IPPROTO_UDP)];
    let wanted = |&&(ty, proto): &&(i32, i32)| {
        (sock_type == 0 || sock_type == ty) && (protocol == 0 || protocol == proto)
    };
    if !kinds.iter().any(|k| wanted(&k)) {
        return EAI_SOCKTYPE;
    }
    let port = match parse_service(service) {
        Ok(port) => port,
        Err(_) if flags & AI_NUMERICSERV != 0 => return EAI_NONAME,
        Err(code) => return code,
    };

    let resolved_addr = if node.is_null() {
        let host = if flags & AI_PASSIVE != 0 {
            [0, 0, 0, 0]
        } else {
            [127, 0, 0, 1]
        };
        u32::from_ne_bytes(host)
    } else if let Some(addr) = super::addr::parse_ipv4(node) {
        addr
    } else if flags & AI_NUMERICHOST != 0 {
        return EAI_NONAME;
    } else {
        let hostname_len = u_strlen(node);
        let mut result_buf = [0u8; 4];
        match Sys::resolve(node, hostname_len, result_buf.as_mut_ptr()) {
            Ok(()) => u32::from_ne_bytes(result_buf),
            Err(errno) => return resolve_error(errno),
        }
    };

    let mut head: *mut AddrInfo = core::ptr::null_mut();
    let mut tail: *mut *mut AddrInfo = &mut head;
    for &(sock_type, protocol) in kinds.iter().filter(wanted) {
        let ai = new_node(resolved_addr, port, sock_type, protocol);
        if ai.is_null() {
            freeaddrinfo(head);
            return EAI_MEMORY;
        }
        *tail = ai;
        tail = &mut (*ai).ai_next;
    }
    if flags & AI_CANONNAME != 0 && !node.is_null() {
        let len = u_strlen(node);
        let name = malloc::alloc(len + 1) as *mut u8;
        if name.is_null() {
            freeaddrinfo(head);
            return EAI_MEMORY;
        }
        core::ptr::copy_nonoverlapping(node, name, len);
        *name.add(len) = 0;
        (*head).ai_canonname = name;
    }

    *res = head;
    0
}

unsafe fn new_node(addr: u32, port: u16, sock_type: i32, protocol: i32) -> *mut AddrInfo {
    let alloc_size = core::mem::size_of::<AddrInfo>() + core::mem::size_of::<SockAddrIn>();
    let ptr = malloc::alloc(alloc_size);
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    let ai = ptr as *mut AddrInfo;
    let sa = (ptr as *mut u8).add(core::mem::size_of::<AddrInfo>()) as *mut SockAddrIn;
    core::ptr::write_bytes(sa, 0, 1);
    (*sa).sin_family = AF_INET as u16;
    (*sa).sin_port = port.to_be();
    (*sa).sin_addr = addr;
    (*ai).ai_flags = 0;
    (*ai).ai_family = AF_INET;
    (*ai).ai_socktype = sock_type;
    (*ai).ai_protocol = protocol;
    (*ai).ai_addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    (*ai).ai_addr = sa as *mut SockAddr;
    (*ai).ai_canonname = core::ptr::null_mut();
    (*ai).ai_next = core::ptr::null_mut();
    ai
}

/// Free the linked list allocated by `getaddrinfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn freeaddrinfo(res: *mut AddrInfo) {
    let mut cur = res;
    while !cur.is_null() {
        let next = (*cur).ai_next;
        if !(*cur).ai_canonname.is_null() {
            malloc::dealloc((*cur).ai_canonname as *mut core::ffi::c_void);
        }
        // AddrInfo and SockAddrIn share one allocation; `ai_addr` points inside
        // it, so only the AddrInfo pointer is freed.
        malloc::dealloc(cur as *mut core::ffi::c_void);
        cur = next;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gai_strerror(errcode: i32) -> *const u8 {
    match errcode {
        0 => b"Success\0".as_ptr(),
        EAI_BADFLAGS => b"Bad value for ai_flags\0".as_ptr(),
        EAI_NONAME => b"Name or service not known\0".as_ptr(),
        EAI_AGAIN => b"Temporary failure in name resolution\0".as_ptr(),
        EAI_FAIL => b"Non-recoverable failure in name resolution\0".as_ptr(),
        EAI_FAMILY => b"Address family not supported\0".as_ptr(),
        EAI_SOCKTYPE => b"ai_socktype not supported\0".as_ptr(),
        EAI_SERVICE => b"Service not supported for socket type\0".as_ptr(),
        EAI_MEMORY => b"Memory allocation failure\0".as_ptr(),
        EAI_SYSTEM => b"System error\0".as_ptr(),
        _ => b"Unknown error\0".as_ptr(),
    }
}
