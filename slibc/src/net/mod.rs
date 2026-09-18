//! POSIX socket API.

pub mod addr;
pub mod dns;
#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};

pub use addr::*;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn socket(domain: i32, sock_type: i32, protocol: i32) -> i32 {
    match Sys::socket(domain, sock_type, protocol) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn bind(fd: i32, addr: *const SockAddr, addrlen: u32) -> i32 {
    match Sys::bind(fd, addr as *const u8, addrlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn listen(fd: i32, backlog: i32) -> i32 {
    match Sys::listen(fd, backlog) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn accept(fd: i32, addr: *mut SockAddr, addrlen: *mut u32) -> i32 {
    match Sys::accept(fd, addr as *mut u8, addrlen) {
        Ok(new_fd) => new_fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn connect(fd: i32, addr: *const SockAddr, addrlen: u32) -> i32 {
    match Sys::connect(fd, addr as *const u8, addrlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize {
    match Sys::send(fd, buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize {
    match Sys::recv(fd, buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: i32,
    dest_addr: *const SockAddr,
    addrlen: u32,
) -> isize {
    match Sys::sendto(fd, buf, len, flags, dest_addr as *const u8, addrlen) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: i32,
    src_addr: *mut SockAddr,
    addrlen: *mut u32,
) -> isize {
    match Sys::recvfrom(fd, buf, len, flags, src_addr as *mut u8, addrlen) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *const u8,
    optlen: u32,
) -> i32 {
    match Sys::setsockopt(fd, level, optname, optval, optlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *mut u8,
    optlen: *mut u32,
) -> i32 {
    match Sys::getsockopt(fd, level, optname, optval, optlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn shutdown(fd: i32, how: i32) -> i32 {
    match Sys::shutdown(fd, how) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpeername(fd: i32, addr: *mut SockAddr, addrlen: *mut u32) -> i32 {
    match Sys::getpeername(fd, addr as *mut u8, addrlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getsockname(fd: i32, addr: *mut SockAddr, addrlen: *mut u32) -> i32 {
    match Sys::getsockname(fd, addr as *mut u8, addrlen) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `socketpair(2)` needs a kernel call that creates two already-connected
/// endpoints at once; SlopOS has none, and the alternative — bind a unique
/// `AF_UNIX` path, listen, connect, accept, unlink — is racy and leaves a
/// filesystem artifact behind, so it is refused rather than approximated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn socketpair(_domain: i32, _ty: i32, _protocol: i32, _sv: *mut i32) -> i32 {
    errno_set(crate::errno::ENOSYS.raw());
    -1
}

/// `accept4(2)`.
///
/// Linux sets the new descriptor's flags atomically with the accept; without
/// an `accept4` syscall this is `accept` followed by `fcntl`, so a concurrent
/// `fork` in the window between them inherits a descriptor whose `O_CLOEXEC`
/// is not yet set. Stated rather than hidden: the ordering is the best a
/// userland implementation can do.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn accept4(
    sockfd: i32,
    addr: *mut SockAddr,
    addrlen: *mut u32,
    flags: i32,
) -> i32 {
    let fd = accept(sockfd, addr, addrlen);
    if fd < 0 {
        return -1;
    }
    if flags & addr::SOCK_CLOEXEC != 0
        && Sys::fcntl(
            fd,
            slopos_abi::syscall::F_SETFD as i32,
            slopos_abi::syscall::FD_CLOEXEC,
        )
        .is_err()
    {
        let _ = Sys::close(fd);
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    if flags & addr::SOCK_NONBLOCK != 0 {
        let current = match Sys::fcntl(fd, slopos_abi::syscall::F_GETFL as i32, 0) {
            Ok(v) => v as u64,
            Err(e) => {
                let _ = Sys::close(fd);
                errno_set(e.raw());
                return -1;
            }
        };
        if let Err(e) = Sys::fcntl(
            fd,
            slopos_abi::syscall::F_SETFL as i32,
            current | slopos_abi::syscall::O_NONBLOCK,
        ) {
            let _ = Sys::close(fd);
            errno_set(e.raw());
            return -1;
        }
    }
    fd
}

/// `sendmsg(2)`. The kernel's `MsgHdr` *is* the Linux-shaped `struct msghdr`
/// the target's `libc` declares, so there is nothing to translate — the
/// ancillary-data helpers (`CMSG_*`) live in `abi` and are shared with the
/// kernel side for the same reason.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sendmsg(
    sockfd: i32,
    msg: *const crate::types::msghdr,
    flags: i32,
) -> isize {
    if msg.is_null() {
        errno_set(crate::errno::EFAULT.raw());
        return -1;
    }
    match Sys::sendmsg(sockfd, msg, flags) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `recvmsg(2)`. The kernel writes `msg_namelen`, `msg_controllen` and
/// `msg_flags` back, so the caller's header is updated in place.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn recvmsg(sockfd: i32, msg: *mut crate::types::msghdr, flags: i32) -> isize {
    if msg.is_null() {
        errno_set(crate::errno::EFAULT.raw());
        return -1;
    }
    match Sys::recvmsg(sockfd, msg, flags) {
        Ok(n) => n as isize,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Interface records one `net_query` snapshot can carry, plus its header.
/// `u64` rather than `u8` so the buffer is 8-aligned: the kernel writes
/// `UserNetQueryHdr` and `UserIface` into it as structs.
const IFACE_QUERY_WORDS: usize = (size_of::<slopos_abi::net::UserNetQueryHdr>()
    + slopos_abi::net::NET_MAX_IFACES * size_of::<slopos_abi::net::UserIface>())
.div_ceil(8);

/// Run one interface snapshot and hand each record to `f` until it answers
/// `Some`. Records are strided by the header's own `record_size`, not by
/// `size_of`, because that field is the kernel's forward-compatibility lever.
unsafe fn with_ifaces<T>(
    mut f: impl FnMut(&slopos_abi::net::UserIface) -> Option<T>,
) -> Result<Option<T>, crate::errno::Errno> {
    let mut words = [0u64; IFACE_QUERY_WORDS];
    let buf = words.as_mut_ptr() as *mut u8;
    let cap = IFACE_QUERY_WORDS * 8;
    Sys::net_query(
        slopos_abi::net::NET_Q_IFACES,
        slopos_abi::net::NET_IFINDEX_GLOBAL,
        buf,
        cap,
    )?;

    let hdr = core::ptr::read(buf as *const slopos_abi::net::UserNetQueryHdr);
    let stride = hdr.record_size as usize;
    if stride < size_of::<slopos_abi::net::UserIface>() {
        return Err(crate::errno::EIO);
    }
    let base = size_of::<slopos_abi::net::UserNetQueryHdr>();
    for i in 0..hdr.record_count as usize {
        let at = base + i * stride;
        if at + stride > cap {
            break;
        }
        let rec = core::ptr::read(buf.add(at) as *const slopos_abi::net::UserIface);
        if let Some(found) = f(&rec) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// `if_nametoindex(3)`. Answers 0 with `errno` set, which is how this one
/// reports failure: index 0 is `NET_IFINDEX_NONE` and names nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn if_nametoindex(ifname: *const core::ffi::c_char) -> u32 {
    if ifname.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return 0;
    }
    let want = crate::string::u_strnlen(ifname as *const u8, slopos_abi::net::NET_IFNAMSIZ);
    if want == 0 {
        errno_set(crate::errno::EINVAL.raw());
        return 0;
    }
    let wanted = core::slice::from_raw_parts(ifname as *const u8, want);

    let found = with_ifaces(|rec| {
        let have = crate::string::u_strnlen(rec.name.as_ptr(), rec.name.len());
        if have == want && &rec.name[..have] == wanted {
            Some(rec.ifindex)
        } else {
            None
        }
    });
    match found {
        Ok(Some(idx)) => idx,
        Ok(None) => {
            errno_set(crate::errno::ENODEV.raw());
            0
        }
        Err(e) => {
            errno_set(e.raw());
            0
        }
    }
}

/// `if_indextoname(3)`. `ifname` must have room for `IF_NAMESIZE` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn if_indextoname(
    ifindex: u32,
    ifname: *mut core::ffi::c_char,
) -> *mut core::ffi::c_char {
    if ifname.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return core::ptr::null_mut();
    }
    let found = with_ifaces(|rec| {
        if rec.ifindex == ifindex {
            Some(rec.name)
        } else {
            None
        }
    });
    match found {
        Ok(Some(name)) => {
            // The record's name is NUL-padded and may fill the field exactly,
            // in which case the terminator has to be added.
            let len = crate::string::u_strnlen(name.as_ptr(), name.len());
            core::ptr::copy_nonoverlapping(name.as_ptr(), ifname as *mut u8, len);
            *(ifname as *mut u8).add(len) = 0;
            ifname
        }
        Ok(None) => {
            errno_set(crate::errno::ENXIO.raw());
            core::ptr::null_mut()
        }
        Err(e) => {
            errno_set(e.raw());
            core::ptr::null_mut()
        }
    }
}
