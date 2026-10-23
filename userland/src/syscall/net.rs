use super::RawFd;
use super::error::{SyscallError, SyscallResult};
use super::numbers::{
    SYSCALL_NET_ADDR_CTL, SYSCALL_NET_IFACE_CTL, SYSCALL_NET_MONITOR, SYSCALL_NET_QUERY,
    SYSCALL_NET_RESOLVER_SET, SYSCALL_NET_ROUTE_CTL, SYSCALL_RESOLVE,
};
use super::raw::{syscall2, syscall3, syscall4};
use slopos_abi::net::{
    NET_ADDROP_ADD, NET_ADDROP_DEL, NET_ROUTEOP_ADD, NET_ROUTEOP_DEL, SockAddrIn, UserAddrReq,
    UserResolverReq, UserRouteReq,
};
use slopos_abi::syscall::{F_GETFL, F_SETFL, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};

/// Turn a raw syscall return into a result; every syscall below reports failure
/// as a negated errno.
#[inline]
fn checked(raw: i64) -> SyscallResult<u64> {
    if raw < 0 {
        Err(SyscallError::from_errno((-raw) as i32))
    } else {
        Ok(raw as u64)
    }
}

/// Enumerate one class of network state into `buf`.
///
/// `buf` receives a `UserNetQueryHdr` followed by `record_count` records of
/// `record_size` bytes; completeness is read from the header's `total_count`,
/// not from the return value, so a header-sized buffer is the sizing query.
pub fn net_query(what: u32, ifindex: u32, buf: &mut [u8]) -> SyscallResult<usize> {
    let raw = unsafe {
        syscall4(
            SYSCALL_NET_QUERY,
            what as u64,
            ifindex as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    };
    checked(raw).map(|n| n as usize)
}

/// Per-interface control, and the two global operations addressed to
/// `NET_IFINDEX_GLOBAL`. Requires `NetAdmin`.
pub fn net_iface_ctl(ifindex: u32, op: u32, arg: u64) -> SyscallResult<()> {
    let raw = unsafe { syscall3(SYSCALL_NET_IFACE_CTL, ifindex as u64, op as u64, arg) as i64 };
    checked(raw).map(|_| ())
}

/// Add (`add == true`) or remove an interface address. Requires `NetAdmin`.
pub fn net_addr_ctl(req: &UserAddrReq, add: bool) -> SyscallResult<()> {
    let raw = unsafe {
        syscall3(
            SYSCALL_NET_ADDR_CTL,
            u64::from(if add { NET_ADDROP_ADD } else { NET_ADDROP_DEL }),
            req as *const UserAddrReq as u64,
            core::mem::size_of::<UserAddrReq>() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}

/// Add (`add == true`) or remove a route. Requires `NetAdmin`.
pub fn net_route_ctl(req: &UserRouteReq, add: bool) -> SyscallResult<()> {
    let raw = unsafe {
        syscall3(
            SYSCALL_NET_ROUTE_CTL,
            u64::from(if add {
                NET_ROUTEOP_ADD
            } else {
                NET_ROUTEOP_DEL
            }),
            req as *const UserRouteReq as u64,
            core::mem::size_of::<UserRouteReq>() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}

/// Replace the resolver configuration. Requires `NetAdmin`.
pub fn net_resolver_set(req: &UserResolverReq) -> SyscallResult<()> {
    let raw = unsafe {
        syscall2(
            SYSCALL_NET_RESOLVER_SET,
            req as *const UserResolverReq as u64,
            core::mem::size_of::<UserResolverReq>() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}

/// Open a network-state monitor. The fd becomes `POLLIN`-ready whenever the
/// stack's configuration changes and reads drain whole `NetEvent` records.
pub fn net_monitor(mask: u32, flags: u32) -> SyscallResult<super::OwnedFd> {
    let raw = unsafe { syscall2(SYSCALL_NET_MONITOR, mask as u64, flags as u64) as i64 };
    // SAFETY: a non-negative return is an fd the kernel just allocated to us.
    checked(raw).map(|fd| unsafe { super::OwnedFd::from_raw(fd as i32) })
}

/// `sock_type` carries `SOCK_NONBLOCK` and `SOCK_CLOEXEC` beside the type.
pub fn socket(
    domain: u16,
    sock_type: impl Into<u32>,
    protocol: u16,
) -> SyscallResult<super::OwnedFd> {
    Sys::socket(domain as i32, sock_type.into() as i32, protocol as i32)
        // SAFETY: v is a valid fd just returned by the kernel.
        .map(|v| unsafe { super::OwnedFd::from_raw(v as i32) })
        .map_err(Into::into)
}

pub fn bind(fd: RawFd, addr: &SockAddrIn) -> SyscallResult<()> {
    Sys::bind(
        fd,
        addr as *const _ as *const u8,
        core::mem::size_of::<SockAddrIn>() as u32,
    )
    .map_err(Into::into)
}

pub fn listen(fd: RawFd, backlog: u32) -> SyscallResult<()> {
    Sys::listen(fd, backlog as i32).map_err(Into::into)
}

/// Bind an AF_UNIX socket to `path` (a `SockAddrUn`, `addrlen = 2 + path.len()`).
pub fn bind_unix(fd: RawFd, path: &[u8]) -> SyscallResult<()> {
    let mut addr = slopos_abi::unix::SockAddrUn::default();
    addr.family = slopos_abi::net::AF_UNIX;
    let n = path.len().min(slopos_abi::unix::UNIX_PATH_MAX);
    addr.path[..n].copy_from_slice(&path[..n]);
    Sys::bind(fd, &addr as *const _ as *const u8, (2 + n) as u32).map_err(Into::into)
}

/// Connect an AF_UNIX socket to the listener bound at `path`.
pub fn connect_unix(fd: RawFd, path: &[u8]) -> SyscallResult<()> {
    let mut addr = slopos_abi::unix::SockAddrUn::default();
    addr.family = slopos_abi::net::AF_UNIX;
    let n = path.len().min(slopos_abi::unix::UNIX_PATH_MAX);
    addr.path[..n].copy_from_slice(&path[..n]);
    Sys::connect(fd, &addr as *const _ as *const u8, (2 + n) as u32).map_err(Into::into)
}

/// Two connected AF_UNIX stream sockets; `flags` takes `SOCK_NONBLOCK` and
/// `SOCK_CLOEXEC`.
pub fn socketpair(flags: u32) -> SyscallResult<(super::OwnedFd, super::OwnedFd)> {
    let mut sv = [-1i32; 2];
    Sys::socketpair(
        slopos_abi::net::AF_UNIX as i32,
        (u32::from(slopos_abi::net::SOCK_STREAM) | flags) as i32,
        0,
        sv.as_mut_ptr(),
    )
    .map_err(SyscallError::from)?;
    // SAFETY: both are fds the kernel just allocated to us.
    Ok(unsafe {
        (
            super::OwnedFd::from_raw(sv[0]),
            super::OwnedFd::from_raw(sv[1]),
        )
    })
}

/// The peer's pid and effective ids as of its `connect`, `listen` or
/// `socketpair`.
pub fn peer_cred(fd: RawFd) -> SyscallResult<slopos_abi::syscall::UCred> {
    let mut raw = [0u8; 12];
    let len = getsockopt(
        fd,
        slopos_abi::syscall::SOL_SOCKET,
        slopos_abi::syscall::SO_PEERCRED,
        &mut raw,
    )?;
    if len != raw.len() {
        return Err(slopos_slibc::errno::EIO.into());
    }
    Ok(slopos_abi::syscall::UCred::from_bytes(&raw))
}

/// A close-on-exec pidfd for the process [`peer_cred`] names.
pub fn peer_pidfd(fd: RawFd) -> SyscallResult<super::OwnedFd> {
    let mut raw = [0u8; 4];
    let len = getsockopt(
        fd,
        slopos_abi::syscall::SOL_SOCKET,
        slopos_abi::syscall::SO_PEERPIDFD,
        &mut raw,
    )?;
    if len != raw.len() {
        return Err(slopos_slibc::errno::EIO.into());
    }
    // SAFETY: the kernel just installed this fd for us.
    Ok(unsafe { super::OwnedFd::from_raw(i32::from_ne_bytes(raw)) })
}

/// The path of the program the peer ran when it connected, as long as it
/// still runs it: a base path, or `/opt/<name>/…` for an extension's; `None`
/// for any other.
pub fn peer_program(fd: RawFd) -> SyscallResult<Option<Vec<u8>>> {
    let mut need = 0u32;
    let sized = Sys::getsockopt(
        fd,
        slopos_abi::syscall::SOL_SLOPOS,
        slopos_abi::syscall::SO_PEERPROGRAM,
        core::ptr::null_mut(),
        &mut need,
    );
    match sized {
        Ok(()) => return Ok(None),
        Err(e) if e == slopos_slibc::errno::ERANGE => {}
        Err(e) => return Err(e.into()),
    }
    let mut path = vec![0u8; need as usize];
    let len = getsockopt(
        fd,
        slopos_abi::syscall::SOL_SLOPOS,
        slopos_abi::syscall::SO_PEERPROGRAM,
        &mut path,
    )?;
    path.truncate(len);
    Ok((!path.is_empty()).then_some(path))
}

/// The activation the peer's program is of, as long as it still runs it;
/// `None` for a base program or any other.
pub fn peer_extension(fd: RawFd) -> SyscallResult<Option<slopos_abi::extension::Label>> {
    let mut raw = [0u8; slopos_abi::extension::Label::LEN];
    let len = getsockopt(
        fd,
        slopos_abi::syscall::SOL_SLOPOS,
        slopos_abi::syscall::SO_PEEREXTENSION,
        &mut raw,
    )?;
    match len {
        0 => Ok(None),
        _ => slopos_abi::extension::Label::from_bytes(&raw[..len])
            .map(Some)
            .ok_or(slopos_slibc::errno::EIO.into()),
    }
}

/// The base path of the program the peer runs, `None` once it runs anything
/// else: code from outside the base, an extension's among it.
pub fn peer_base_program(fd: RawFd) -> SyscallResult<Option<Vec<u8>>> {
    // In this order: a program that stops holding never holds again, so an
    // extension's that lapses between the two reads answers no path.
    if peer_extension(fd)?.is_some() {
        return Ok(None);
    }
    peer_program(fd)
}

pub fn accept(fd: RawFd, peer: Option<&mut SockAddrIn>) -> SyscallResult<super::OwnedFd> {
    let mut addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    let peer_ptr = peer
        .map(|p| p as *mut _ as *mut u8)
        .unwrap_or(core::ptr::null_mut());
    let addrlen_ptr = if peer_ptr.is_null() {
        core::ptr::null_mut()
    } else {
        &mut addrlen as *mut u32
    };
    Sys::accept(fd, peer_ptr, addrlen_ptr)
        // SAFETY: v is a valid fd just returned by the kernel.
        .map(|v| unsafe { super::OwnedFd::from_raw(v as i32) })
        .map_err(Into::into)
}

pub fn connect(fd: RawFd, addr: &SockAddrIn) -> SyscallResult<()> {
    Sys::connect(
        fd,
        addr as *const _ as *const u8,
        core::mem::size_of::<SockAddrIn>() as u32,
    )
    .map_err(Into::into)
}

pub fn send(fd: RawFd, data: &[u8], flags: u32) -> SyscallResult<usize> {
    Sys::send(fd, data.as_ptr(), data.len(), flags as i32).map_err(Into::into)
}

pub fn recv(fd: RawFd, buf: &mut [u8], flags: u32) -> SyscallResult<usize> {
    Sys::recv(fd, buf.as_mut_ptr(), buf.len(), flags as i32).map_err(Into::into)
}

pub fn sendto(fd: RawFd, data: &[u8], flags: u32, addr: &SockAddrIn) -> SyscallResult<usize> {
    Sys::sendto(
        fd,
        data.as_ptr(),
        data.len(),
        flags as i32,
        addr as *const _ as *const u8,
        core::mem::size_of::<SockAddrIn>() as u32,
    )
    .map_err(Into::into)
}

pub fn recvfrom(
    fd: RawFd,
    buf: &mut [u8],
    flags: u32,
    src_addr: Option<&mut SockAddrIn>,
) -> SyscallResult<usize> {
    let mut addrlen = core::mem::size_of::<SockAddrIn>() as u32;
    let src_addr_ptr = src_addr
        .map(|a| a as *mut _ as *mut u8)
        .unwrap_or(core::ptr::null_mut());
    let addrlen_ptr = if src_addr_ptr.is_null() {
        core::ptr::null_mut()
    } else {
        &mut addrlen as *mut u32
    };
    Sys::recvfrom(
        fd,
        buf.as_mut_ptr(),
        buf.len(),
        flags as i32,
        src_addr_ptr,
        addrlen_ptr,
    )
    .map_err(Into::into)
}

pub fn setsockopt(fd: RawFd, level: i32, optname: i32, val: &[u8]) -> SyscallResult<()> {
    Sys::setsockopt(fd, level, optname, val.as_ptr(), val.len() as u32).map_err(Into::into)
}

pub fn getsockopt(fd: RawFd, level: i32, optname: i32, buf: &mut [u8]) -> SyscallResult<usize> {
    let mut optlen = buf.len() as u32;
    Sys::getsockopt(
        fd,
        level,
        optname,
        buf.as_mut_ptr(),
        &mut optlen as *mut u32,
    )
    .map(|_| optlen as usize)
    .map_err(Into::into)
}

pub fn shutdown(fd: RawFd, how: i32) -> SyscallResult<()> {
    Sys::shutdown(fd, how).map_err(Into::into)
}

pub fn set_reuse_addr(fd: RawFd) -> SyscallResult<()> {
    let val: i32 = 1;
    setsockopt(
        fd,
        slopos_abi::syscall::SOL_SOCKET,
        slopos_abi::syscall::SO_REUSEADDR,
        &val.to_ne_bytes(),
    )
}

pub(crate) fn resolve(hostname: &[u8]) -> SyscallResult<[u8; 4]> {
    let mut result = [0u8; 4];
    let rc = unsafe {
        syscall3(
            SYSCALL_RESOLVE,
            hostname.as_ptr() as u64,
            hostname.len() as u64,
            &mut result as *mut [u8; 4] as u64,
        )
    };
    let signed = rc as i64;
    if signed < 0 {
        Err(SyscallError::from_errno((-signed) as i32))
    } else {
        Ok(result)
    }
}

pub fn bind_any(fd: RawFd, port: u16) -> SyscallResult<()> {
    let addr = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: port.to_be(),
        addr: [0; 4],
        _pad: [0; 8],
    };
    bind(fd, &addr)
}

pub fn bind_addr(fd: RawFd, ip: [u8; 4], port: u16) -> SyscallResult<()> {
    let addr = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: port.to_be(),
        addr: ip,
        _pad: [0; 8],
    };
    bind(fd, &addr)
}

pub fn set_nonblocking(fd: RawFd) -> SyscallResult<()> {
    let current = Sys::fcntl(fd, F_GETFL as i32, 0).map_err(SyscallError::from)?;
    let flags = (current as u64) | O_NONBLOCK;
    let _ = Sys::fcntl(fd, F_SETFL as i32, flags).map_err(SyscallError::from)?;
    Ok(())
}

pub fn set_blocking(fd: RawFd) -> SyscallResult<()> {
    let current = Sys::fcntl(fd, F_GETFL as i32, 0).map_err(SyscallError::from)?;
    let flags = (current as u64) & !O_NONBLOCK;
    let _ = Sys::fcntl(fd, F_SETFL as i32, flags).map_err(SyscallError::from)?;
    Ok(())
}

/// Send `data` with `fds` attached as one `SCM_RIGHTS` item; the bytes sent,
/// which may be fewer than `data` holds.
pub fn send_with_fds(fd: RawFd, data: &[u8], fds: &[RawFd]) -> SyscallResult<usize> {
    use slopos_abi::fs::UserIovec;
    use slopos_abi::syscall::{CMSG_DATA_OFFSET, CmsgHdr, MsgHdr, SCM_RIGHTS, SOL_SOCKET};
    let item = core::mem::size_of_val(fds);
    let mut control = vec![0u8; slopos_abi::syscall::cmsg_space(item)];
    let header = CmsgHdr {
        cmsg_len: slopos_abi::syscall::cmsg_len(item) as u64,
        cmsg_level: SOL_SOCKET,
        cmsg_type: SCM_RIGHTS,
    };
    // SAFETY: `control` holds a header and `fds` past CMSG_DATA_OFFSET, as
    // cmsg_space sized it; both writes are unaligned-safe.
    unsafe {
        core::ptr::write_unaligned(control.as_mut_ptr() as *mut CmsgHdr, header);
        for (i, &passed) in fds.iter().enumerate() {
            core::ptr::write_unaligned(
                control.as_mut_ptr().add(CMSG_DATA_OFFSET + i * 4) as *mut i32,
                passed,
            );
        }
    }
    let iov = UserIovec {
        iov_base: data.as_ptr() as u64,
        iov_len: data.len() as u64,
    };
    let msg = MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad0: 0,
        msg_iov: &iov as *const UserIovec as u64,
        msg_iovlen: 1,
        msg_control: if fds.is_empty() {
            0
        } else {
            control.as_ptr() as u64
        },
        msg_controllen: if fds.is_empty() {
            0
        } else {
            control.len() as u64
        },
        msg_flags: 0,
        _pad1: 0,
    };
    Sys::sendmsg(fd, &msg, slopos_abi::syscall::MSG_NOSIGNAL as i32).map_err(Into::into)
}

/// Receive into `buf`, taking any descriptors that arrived with it, each made
/// close-on-exec; ones past [`SCM_MAX_FDS`](slopos_abi::syscall::SCM_MAX_FDS)
/// are closed by the kernel.
pub fn recv_with_fds(fd: RawFd, buf: &mut [u8]) -> SyscallResult<(usize, Vec<super::OwnedFd>)> {
    use slopos_abi::fs::UserIovec;
    use slopos_abi::syscall::{CMSG_DATA_OFFSET, CmsgHdr, MsgHdr, SCM_MAX_FDS, SCM_RIGHTS};
    let mut control = vec![0u8; slopos_abi::syscall::cmsg_space(SCM_MAX_FDS * 4)];
    let iov = UserIovec {
        iov_base: buf.as_mut_ptr() as u64,
        iov_len: buf.len() as u64,
    };
    let mut msg = MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        _pad0: 0,
        msg_iov: &iov as *const UserIovec as u64,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr() as u64,
        msg_controllen: control.len() as u64,
        msg_flags: 0,
        _pad1: 0,
    };
    let n = Sys::recvmsg(fd, &mut msg, 0).map_err(SyscallError::from)?;
    let mut fds = Vec::new();
    let used = (msg.msg_controllen as usize).min(control.len());
    if used >= CMSG_DATA_OFFSET {
        // SAFETY: the kernel wrote a header at the start of `control`.
        let header: CmsgHdr =
            unsafe { core::ptr::read_unaligned(control.as_ptr() as *const CmsgHdr) };
        if header.cmsg_type == SCM_RIGHTS {
            let count = (header.cmsg_len as usize)
                .saturating_sub(CMSG_DATA_OFFSET)
                .min(used - CMSG_DATA_OFFSET)
                / 4;
            for i in 0..count {
                // SAFETY: `count` keeps the read inside the used control bytes.
                let raw = unsafe {
                    core::ptr::read_unaligned(
                        control.as_ptr().add(CMSG_DATA_OFFSET + i * 4) as *const i32
                    )
                };
                let _ = super::fs::set_fd_cloexec(raw);
                // SAFETY: the kernel installed this fd for us.
                fds.push(unsafe { super::OwnedFd::from_raw(raw) });
            }
        }
    }
    Ok((n, fds))
}
