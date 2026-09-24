use slopos_abi::Errno;
use slopos_abi::file_ops::FileKind;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::net::{AF_INET, AF_UNIX, IPPROTO_ICMP, SOCK_DGRAM, SOCK_STREAM, SockAddrIn};
use slopos_abi::syscall::{MsgHdr, SCM_MAX_FDS};
use slopos_abi::unix::SockAddrUn;
use slopos_fs::fileio::FdTable;
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_msghdr::{msg_iovec_buf, scm_rights_fds};
use slopos_mm::user_ptr::{UserBytes as MmUserBytes, UserPtr as MmUserPtr};
use slopos_net::types::{Ipv4Addr, Port, SockAddr};
use slopos_net::unix_socket::SocketHandle;
use slopos_net::{dns, socket, unix_socket, unix_socket_file_ops};
use slopos_ostd::KVec;

use crate::syscall::args::{Fd, UserBytes, UserPtr};
use crate::syscall::common::{errno_from_neg, errno_from_neg64};

fn rc_i32_to_unit(rc: i32) -> Result<(), Errno> {
    if rc < 0 {
        Err(errno_from_neg(rc))
    } else {
        Ok(())
    }
}

fn rc_i32_to_u64(rc: i32) -> Result<u64, Errno> {
    if rc < 0 {
        Err(errno_from_neg(rc))
    } else {
        Ok(rc as u64)
    }
}

fn rc_i64_to_u64(rc: i64) -> Result<u64, Errno> {
    if rc < 0 {
        Err(errno_from_neg64(rc))
    } else {
        Ok(rc as u64)
    }
}

/// Socket fd lookup result: an AF_UNIX handle or a raw AF_INET pool index.
enum SocketFd {
    Unix(SocketHandle),
    Inet(u32),
}

fn socket_fd_for(table: FdTable, fd: i32) -> Result<SocketFd, Errno> {
    let Some((handle, ops)) = slopos_fs::fileio::fileio_get_handle_and_ops(table, fd) else {
        return Err(Errno::ENOTSOCK);
    };
    if ops.kind() != FileKind::Socket {
        return Err(Errno::ENOTSOCK);
    }
    if ops.is_unix_socket() {
        Ok(SocketFd::Unix(SocketHandle::from_usize(handle)))
    } else {
        Ok(SocketFd::Inet(handle as u32))
    }
}

/// Reject every `flags` bit: this kernel implements no input `MSG_*` option,
/// so there is nothing a caller can legitimately ask for, and a dropped bit is
/// worse than a refusal — a silently ignored `MSG_PEEK` consumes the datagram.
/// `MSG_CTRUNC` is not an input: it is a `msghdr::msg_flags` out-bit.
fn check_msg_flags(flags: u32) -> Result<(), Errno> {
    if flags == 0 {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

define_syscall!(syscall_socket
    (ctx, domain: u32, sock_type: u32, protocol: u32)
    cap(NoneSelf)
    requires(let process_id: process_id, let task_id: task_id)
    -> Result<u64, Errno>
{
    let domain = domain as u16;
    let sock_type = sock_type as u16;
    let protocol = protocol as u16;

    if domain == AF_UNIX {
        if sock_type != SOCK_STREAM {
            return Err(Errno::EPROTONOSUPPORT);
        }
        let handle = unix_socket::unix_create().ok_or(Errno::ENOMEM)?;
        // The backing owns the endpoint from here: a failed install (or a
        // failed backing allocation) closes it.
        let backing = unix_socket_file_ops::unix_socket_backing(handle, process_id.account())
            .ok_or(Errno::ENFILE)?;
        let fd = slopos_fs::fileio_open_fd_with_ops(
            process_id,
            &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
            handle.as_usize(),
            Some(backing),
            slopos_fs::FdFlags::NONE,
        );
        if fd < 0 {
            return Err(Errno::ENOMEM);
        }
        return Ok(fd as u64);
    }

    if domain != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    if sock_type != SOCK_STREAM && sock_type != SOCK_DGRAM {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let _icmp_datagram = sock_type == SOCK_DGRAM && protocol == IPPROTO_ICMP;

    // Both halves of the owner come from the syscall context, never from
    // userland: `net_query` gates owner disclosure by comparing against it.
    let owner = socket::SocketOwner {
        process: Some(process_id),
        task_id,
    };
    let sock_idx = socket::socket_create(domain, sock_type, protocol, owner);
    if sock_idx < 0 {
        return Err(errno_from_neg(sock_idx));
    }

    let backing =
        slopos_net::socket_file_ops::socket_backing(sock_idx as u32, process_id.account())
            .ok_or(Errno::ENFILE)?;
    let fd = slopos_fs::fileio_open_socket_fd(process_id, sock_idx as u32, Some(backing));
    if fd < 0 {
        return Err(Errno::ENOMEM);
    }

    Ok(fd as u64)
});

define_syscall!(syscall_bind
    (ctx, fd: Fd, addr_ptr: u64, addr_len: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    if addr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let addr_len = addr_len as usize;

    match sock_fd {
        SocketFd::Unix(sh) => {
            if addr_len < 4 {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrUn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let path_len = (addr_len - 2).min(slopos_abi::unix::UNIX_PATH_MAX);
            let actual_len = sock_addr.path[..path_len]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_len);
            if actual_len == 0 {
                return Err(Errno::EINVAL);
            }
            rc_i32_to_unit(unix_socket::unix_bind(sh, &sock_addr.path[..actual_len]))
        }
        SocketFd::Inet(sock_idx) => {
            if addr_len < core::mem::size_of::<SockAddrIn>() {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrIn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let port = u16::from_be(sock_addr.port);
            rc_i32_to_unit(socket::socket_bind(sock_idx, sock_addr.addr, port))
        }
    }
});

define_syscall!(syscall_listen
    (ctx, fd: Fd, backlog: u32)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;
    match sock_fd {
        SocketFd::Unix(sh) => rc_i32_to_unit(unix_socket::unix_listen(sh, backlog)),
        SocketFd::Inet(sock_idx) => rc_i32_to_unit(socket::socket_listen(sock_idx, backlog)),
    }
});

define_syscall!(syscall_accept
    (ctx, fd: Fd, peer_ptr: u64, addrlen_ptr: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;
    // `addrlen` is in/out: the caller's buffer size in, the peer address's real
    // length out. A null address pointer declines the peer; a non-null one with
    // no length to read is the malformed pair Linux faults on.
    if peer_ptr != 0 && addrlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let want_peer = peer_ptr != 0;
    let caller_len = if want_peer { read_socklen(addrlen_ptr)? } else { 0 };

    match sock_fd {
        SocketFd::Unix(sh) => {
            let accepted_handle =
                unix_socket::unix_accept(sh).map_err(errno_from_neg)?;
            // The accepting process pays: a connection is remote-triggered, so
            // charging the listener would let a peer exhaust its whole budget.
            let backing =
                unix_socket_file_ops::unix_socket_backing(accepted_handle, process_id.account())
                    .ok_or(Errno::ENFILE)?;
            let new_fd = slopos_fs::fileio_open_fd_with_ops(
                process_id,
                &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
                accepted_handle.as_usize(),
                Some(backing),
                slopos_fs::FdFlags::NONE,
            );
            if new_fd < 0 {
                return Err(Errno::ENOMEM);
            }
            // The descriptor is already installed, so a faulting copy-out must
            // not leave the caller holding a connection it was never told the
            // number of.
            if want_peer
                && let Err(e) =
                    accept_peer_unix(accepted_handle, caller_len, peer_ptr, addrlen_ptr)
            {
                let _ = slopos_fs::file_close_fd(process_id, new_fd);
                return Err(e);
            }
            Ok(new_fd as u64)
        }
        SocketFd::Inet(sock_idx) => {
            let mut peer_ip = [0u8; 4];
            let mut peer_port = 0u16;

            let accepted_idx = socket::socket_accept(
                sock_idx,
                if want_peer { &mut peer_ip as *mut [u8; 4] } else { core::ptr::null_mut() },
                if want_peer { &mut peer_port as *mut u16 } else { core::ptr::null_mut() },
            );
            if accepted_idx < 0 {
                return Err(errno_from_neg(accepted_idx));
            }

            let backing = slopos_net::socket_file_ops::socket_backing(
                accepted_idx as u32,
                process_id.account(),
            )
            .ok_or(Errno::ENFILE)?;
            let new_fd =
                slopos_fs::fileio_open_socket_fd(process_id, accepted_idx as u32, Some(backing));
            if new_fd < 0 {
                return Err(Errno::ENOMEM);
            }

            if want_peer {
                let peer = SockAddrIn {
                    family: AF_INET,
                    port: peer_port.to_be(),
                    addr: peer_ip,
                    _pad: [0; 8],
                };
                if let Err(e) = write_sockaddr(
                    slopos_ostd::util::byte_view::pod_as_bytes(&peer),
                    caller_len,
                    peer_ptr,
                    addrlen_ptr,
                ) {
                    let _ = slopos_fs::file_close_fd(process_id, new_fd);
                    return Err(e);
                }
            }

            Ok(new_fd as u64)
        }
    }
});

define_syscall!(syscall_connect
    (ctx, fd: Fd, addr_ptr: u64, addr_len: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    if addr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let addr_len = addr_len as usize;

    match sock_fd {
        SocketFd::Unix(sh) => {
            if addr_len < 4 {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrUn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let path_len = (addr_len - 2).min(slopos_abi::unix::UNIX_PATH_MAX);
            let actual_len = sock_addr.path[..path_len]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(path_len);
            if actual_len == 0 {
                return Err(Errno::EINVAL);
            }
            rc_i32_to_unit(unix_socket::unix_connect(sh, &sock_addr.path[..actual_len]))
        }
        SocketFd::Inet(sock_idx) => {
            if addr_len < core::mem::size_of::<SockAddrIn>() {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrIn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            let port = u16::from_be(sock_addr.port);
            rc_i32_to_unit(socket::socket_connect(sock_idx, sock_addr.addr, port))
        }
    }
});

// A null `addr` is `send(2)`: with the `send` slot retired that is the only
// spelling left for it, so AF_UNIX takes it too. At most 4096 payload bytes
// move per call; the short count is the caller's to loop on.
define_syscall!(syscall_sendto
    (ctx, fd: Fd, buf: UserBytes, flags: u32, addr_ptr: u64, addr_len: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    check_msg_flags(flags)?;
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    let len = buf.len().min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;
    let copied = if len > 0 {
        let user_data = MmUserBytes::try_new(buf.base_u64(), len).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_data, &mut scratch[..len]).map_err(|_| Errno::EFAULT)?
    } else {
        0
    };

    match sock_fd {
        SocketFd::Unix(sh) => {
            if addr_ptr != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            rc_i32_to_u64(unix_socket::unix_send(sh, &scratch[..copied]))
        }
        SocketFd::Inet(sock_idx) => {
            if addr_ptr == 0 {
                return rc_i64_to_u64(socket::socket_send(sock_idx, &scratch[..copied]));
            }
            if (addr_len as usize) < core::mem::size_of::<SockAddrIn>() {
                return Err(Errno::EINVAL);
            }
            let user_addr = MmUserPtr::<SockAddrIn>::try_new(addr_ptr).map_err(|_| Errno::EFAULT)?;
            let sock_addr = copy_from_user(user_addr).map_err(|_| Errno::EFAULT)?;
            if sock_addr.family != AF_INET {
                return Err(Errno::EAFNOSUPPORT);
            }
            rc_i64_to_u64(socket::socket_sendto(
                sock_idx,
                &scratch[..copied],
                sock_addr.addr,
                u16::from_be(sock_addr.port),
            ))
        }
    }
});

// A null `src` is `recv(2)`, the only spelling left for it. At most 4096
// payload bytes move per call; the short count is the caller's to loop on.
define_syscall!(syscall_recvfrom
    (ctx, fd: Fd, buf: UserBytes, flags: u32, src_ptr: u64, srclen_ptr: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    check_msg_flags(flags)?;
    let sock_fd = socket_fd_for(process_id, fd.raw())?;

    // `srclen` is in/out, as in `accept`: a null `src` declines the sender's
    // address, a non-null one with no length to read is a fault.
    if src_ptr != 0 && srclen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let want_src = src_ptr != 0;
    let caller_len = if want_src { read_socklen(srclen_ptr)? } else { 0 };

    let len = buf.len().min(4096);
    let mut scratch = slopos_ostd::KVec::<u8>::zeroed(4096).map_err(|_| Errno::ENOMEM)?;

    let (copied, src) = match sock_fd {
        SocketFd::Unix(sh) => {
            let rc = unix_socket::unix_recv(sh, &mut scratch[..len]);
            if rc < 0 {
                return Err(errno_from_neg(rc));
            }
            (rc as usize, None)
        }
        SocketFd::Inet(sock_idx) => {
            // `socket_recvfrom` is the datagram path and refuses a stream
            // socket, so a stream receive takes the connection-oriented call
            // and reports no sender address, exactly as Linux does.
            if !want_src || socket::socket_is_tcp(sock_idx) {
                let rc = socket::socket_recv(sock_idx, &mut scratch[..len]);
                if rc < 0 {
                    return Err(errno_from_neg64(rc));
                }
                (rc as usize, None)
            } else {
                let mut from = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));
                let rc =
                    socket::socket_recvfrom(sock_idx, &mut scratch[..len], Some(&mut from));
                if rc < 0 {
                    return Err(errno_from_neg64(rc));
                }
                (rc as usize, Some(from))
            }
        }
    };

    if copied > 0 {
        let user_out = MmUserBytes::try_new(buf.base_u64(), copied).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_out, &scratch[..copied]).map_err(|_| Errno::EFAULT)?;
    }

    if want_src {
        match src {
            Some(from) => {
                let peer = SockAddrIn {
                    family: AF_INET,
                    port: from.port.0.to_be(),
                    addr: from.ip.0,
                    _pad: [0; 8],
                };
                write_sockaddr(
                    slopos_ostd::util::byte_view::pod_as_bytes(&peer),
                    caller_len,
                    src_ptr,
                    srclen_ptr,
                )?;
            }
            // A stream receive carries no sender address: report length 0.
            None => write_sockaddr(&[], caller_len, src_ptr, srclen_ptr)?,
        }
    }

    Ok(copied as u64)
});

define_syscall!(syscall_setsockopt
    (ctx, fd: Fd, level: u32, optname: u32, optval: UserBytes)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    let optlen = optval.len().min(64);
    let mut scratch = [0u8; 64];
    if optlen > 0 {
        let user_data = MmUserBytes::try_new(optval.base_u64(), optlen).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(user_data, &mut scratch[..optlen]).map_err(|_| Errno::EFAULT)?;
    }

    rc_i32_to_unit(socket::socket_setsockopt(sock_idx, level as i32, optname as i32, &scratch[..optlen]))
});

define_syscall!(syscall_getsockopt
    (ctx, fd: Fd, level: u32, optname: u32, optval_ptr: u64, optlen_ptr_raw: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    if optval_ptr == 0 || optlen_ptr_raw == 0 {
        return Err(Errno::EFAULT);
    }

    let user_optlen = MmUserPtr::<u32>::try_new(optlen_ptr_raw).map_err(|_| Errno::EFAULT)?;
    let optlen = copy_from_user(user_optlen).map_err(|_| Errno::EFAULT)? as usize;
    let optlen = optlen.min(64);

    let mut scratch = [0u8; 64];
    let rc = socket::socket_getsockopt(sock_idx, level as i32, optname as i32, &mut scratch[..optlen]);
    if rc < 0 {
        return Err(errno_from_neg(rc));
    }

    let written = rc as usize;
    if written > 0 {
        let user_data = MmUserBytes::try_new(optval_ptr, written).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_data, &scratch[..written]).map_err(|_| Errno::EFAULT)?;
    }

    let actual_len = written as u32;
    copy_to_user(user_optlen, &actual_len).map_err(|_| Errno::EFAULT)?;

    Ok(())
});

define_syscall!(syscall_shutdown
    (ctx, fd: Fd, how: u32)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    let sock_idx = match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Inet(idx) => idx,
        SocketFd::Unix(_) => return Err(Errno::ENOTSOCK),
    };

    rc_i32_to_unit(socket::socket_shutdown(sock_idx, how as i32))
});

define_syscall!(syscall_resolve
    (ctx, hostname_ptr: u64, hostname_len: u64, result_ptr: u64)
    cap(NoneSelf)
    requires(let _process_id: process_id)
    -> Result<(), Errno>
{
    if hostname_ptr == 0 || result_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let hostname_len = hostname_len as usize;
    if hostname_len == 0 || hostname_len > 253 {
        return Err(Errno::EINVAL);
    }

    let mut hostname_buf = [0u8; 253];
    let user_hostname = MmUserBytes::try_new(hostname_ptr, hostname_len).map_err(|_| Errno::EFAULT)?;
    let copied = copy_bytes_from_user(user_hostname, &mut hostname_buf[..hostname_len])
        .map_err(|_| Errno::EFAULT)?;
    if copied != hostname_len {
        return Err(Errno::EFAULT);
    }

    let result_addr = match dns::dns_resolve(&hostname_buf[..hostname_len]) {
        Ok(addr) => addr,
        Err(dns::DnsResolveError::InvalidHostname) => return Err(Errno::EINVAL),
        Err(dns::DnsResolveError::NoDnsServer) => return Err(Errno::ENETUNREACH),
        Err(
            dns::DnsResolveError::Timeout
            | dns::DnsResolveError::TransmitFailed
            | dns::DnsResolveError::ServerFailure
            | dns::DnsResolveError::Busy,
        ) => return Err(Errno::EAGAIN),
        Err(dns::DnsResolveError::NameNotFound) => return Err(Errno::EHOSTUNREACH),
        Err(dns::DnsResolveError::ParseFailed) => return Err(Errno::EIO),
        Err(dns::DnsResolveError::Interrupted) => return Err(Errno::EINTR),
    };

    let user_result = MmUserBytes::try_new(result_ptr, 4).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(user_result, &result_addr).map_err(|_| Errno::EFAULT)?;

    Ok(())
});

/// Staging-buffer cap for AF_UNIX user↔kernel marshalling, the same 4 KiB
/// bound `sendto`/`recvfrom` above stage through. The short count is the
/// caller's to loop on.
const MSG_STAGING_CAP: usize = 4096;

/// Turn the descriptor numbers a control buffer names into owned aliases of
/// the sender's open-file descriptions.
fn collect_scm_rights(
    table: FdTable,
    msg: &MsgHdr,
    files: &mut KVec<slopos_fs::LeafFileRef>,
) -> Result<(), Errno> {
    let mut fd_buf = [0i32; SCM_MAX_FDS];
    let n_fds = scm_rights_fds(msg, &mut fd_buf)?;
    for &send_fd in fd_buf.iter().take(n_fds) {
        let file = slopos_fs::fileio_clone_file_ref(table, send_fd).ok_or(Errno::EBADF)?;
        // A description that owns descriptions cannot travel this way: passing
        // one into a queue it can reach closes a reference cycle nothing
        // collects.
        let leaf = slopos_fs::LeafFileRef::try_new(file).map_err(|refused| {
            drop(refused);
            Errno::EOPNOTSUPP
        })?;
        files.push(leaf).map_err(|_| Errno::ENOMEM)?;
    }
    Ok(())
}

define_syscall!(syscall_sendmsg
    (ctx, fd: Fd, msg_ptr: UserPtr<MsgHdr>, flags: u32)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    check_msg_flags(flags)?;

    let sock_fd = socket_fd_for(process_id, fd.raw())?;
    let sh = match sock_fd {
        SocketFd::Unix(sh) => sh,
        SocketFd::Inet(_) => return Err(Errno::ENOTSOCK),
    };

    let msg: MsgHdr = copy_from_user(msg_ptr.inner()).map_err(|_| Errno::EFAULT)?;

    let io_buf = msg_iovec_buf(&msg)?;
    let data_len = IoBufRead::len(&io_buf).min(MSG_STAGING_CAP);
    let mut scratch = KVec::<u8>::zeroed(MSG_STAGING_CAP).map_err(|_| Errno::ENOMEM)?;
    let staged = if data_len > 0 {
        io_buf.copy_out(0, &mut scratch[..data_len])?
    } else {
        0
    };

    // Owned aliases of the fds being passed, each sharing the sender's open-file
    // description per POSIX fd-passing semantics; on error the vec drops them.
    let mut files: KVec<slopos_fs::LeafFileRef> =
        KVec::with_capacity(SCM_MAX_FDS).map_err(|_| Errno::ENOMEM)?;
    collect_scm_rights(process_id, &msg, &mut files)?;

    let rc = unix_socket::unix_sendmsg(
        sh,
        &scratch[..staged],
        &mut files,
        process_id.account(),
    );
    if rc < 0 {
        // Uncommitted aliases drop with `files`.
        return Err(errno_from_neg(rc));
    }
    Ok(rc as u64)
});

#[inline(never)]
fn recvmsg_impl(table: FdTable, fd: Fd, msg_ptr: UserPtr<MsgHdr>) -> Result<u64, Errno> {
    let sh = match socket_fd_for(table, fd.raw())? {
        SocketFd::Unix(sh) => sh,
        SocketFd::Inet(_) => return Err(Errno::ENOTSOCK),
    };

    let msg: MsgHdr = copy_from_user(msg_ptr.inner()).map_err(|_| Errno::EFAULT)?;

    let mut io_buf = msg_iovec_buf(&msg)?;
    let data_len = IoBufWrite::len(&io_buf).min(MSG_STAGING_CAP);
    let mut scratch = KVec::<u8>::zeroed(MSG_STAGING_CAP).map_err(|_| Errno::ENOMEM)?;

    let mut received: KVec<slopos_fs::FileRef> =
        KVec::with_capacity(SCM_MAX_FDS).map_err(|_| Errno::ENOMEM)?;
    let (bytes_read, n_fds) =
        unix_socket::unix_recvmsg(sh, &mut scratch[..data_len], &mut received, SCM_MAX_FDS);

    if bytes_read < 0 {
        // `received` drops, closing any drained aliases.
        return Err(errno_from_neg(bytes_read));
    }
    debug_assert_eq!(n_fds, received.len());

    let copied = bytes_read as usize;
    if copied > 0 {
        io_buf.copy_in(0, &scratch[..copied])?;
    }

    slopos_fs::fileio::fileio_deliver_scm_rights(table, &msg, received, msg_ptr.inner())?;

    Ok(copied as u64)
}

define_syscall!(syscall_recvmsg
    (ctx, fd: Fd, msg_ptr: UserPtr<MsgHdr>, flags: u32)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    check_msg_flags(flags)?;
    recvmsg_impl(process_id, fd, msg_ptr)
});

// `accept` and `getsockname` / `getpeername` each stage a 110-byte
// `SockAddrUn` on the unix branch, built from the 108-byte path copy
// `unix_get_peer_path` returns by value. Neither branch is near the 2 KiB
// stack gate; the `#[inline(never)]` split is frame hygiene, keeping the
// dispatch frame off the union of both branches' staging rather than being
// what makes the gate pass.

/// Read an in/out `socklen_t*`. A negative length is `EINVAL`, as in Linux's
/// `move_addr_to_user`.
fn read_socklen(addrlen_ptr: u64) -> Result<usize, Errno> {
    let user_len_ptr = MmUserPtr::<u32>::try_new(addrlen_ptr).map_err(|_| Errno::EFAULT)?;
    let caller_len = copy_from_user(user_len_ptr).map_err(|_| Errno::EFAULT)?;
    if (caller_len as i32) < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(caller_len as usize)
}

/// Copy `addr` out truncated to the caller's buffer, then report the address's
/// real length through `addrlen_ptr`.
fn write_sockaddr(
    addr: &[u8],
    caller_len: usize,
    addr_buf: u64,
    addrlen_ptr: u64,
) -> Result<(), Errno> {
    let copy_len = caller_len.min(addr.len());
    if copy_len > 0 {
        let user_buf = MmUserBytes::try_new(addr_buf, copy_len).map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_buf, &addr[..copy_len]).map_err(|_| Errno::EFAULT)?;
    }
    let user_len_ptr = MmUserPtr::<u32>::try_new(addrlen_ptr).map_err(|_| Errno::EFAULT)?;
    let actual = addr.len() as u32;
    copy_to_user(user_len_ptr, &actual).map_err(|_| Errno::EFAULT)?;
    Ok(())
}

#[inline(never)]
fn write_unix_sockaddr(
    addr_un: &SockAddrUn,
    path_len: usize,
    addr_buf: u64,
    addrlen_ptr: u64,
) -> Result<(), Errno> {
    let caller_len = read_socklen(addrlen_ptr)?;
    let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(addr_un);
    write_sockaddr(
        &addr_bytes[..2 + path_len],
        caller_len,
        addr_buf,
        addrlen_ptr,
    )
}

#[inline(never)]
fn write_inet_sockaddr(
    sock_addr_in: &SockAddrIn,
    addr_buf: u64,
    addrlen_ptr: u64,
) -> Result<(), Errno> {
    let caller_len = read_socklen(addrlen_ptr)?;
    let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(sock_addr_in);
    write_sockaddr(addr_bytes, caller_len, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn accept_peer_unix(
    handle: SocketHandle,
    caller_len: usize,
    addr_buf: u64,
    addrlen_ptr: u64,
) -> Result<(), Errno> {
    let mut addr_un = SockAddrUn::default();
    addr_un.family = AF_UNIX;
    let path_len = match unix_socket::unix_get_peer_path(handle) {
        Some((path, len)) => {
            addr_un.path[..len].copy_from_slice(&path[..len]);
            len
        }
        None => 0,
    };
    let addr_bytes = slopos_ostd::util::byte_view::pod_as_bytes(&addr_un);
    write_sockaddr(
        &addr_bytes[..2 + path_len],
        caller_len,
        addr_buf,
        addrlen_ptr,
    )
}

#[inline(never)]
fn getpeername_unix(sh: SocketHandle, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let (path, path_len) = unix_socket::unix_get_peer_path(sh).ok_or(Errno::ENOTCONN)?;
    let mut addr_un = SockAddrUn::default();
    addr_un.family = AF_UNIX;
    if path_len > 0 {
        addr_un.path[..path_len].copy_from_slice(&path[..path_len]);
    }
    write_unix_sockaddr(&addr_un, path_len, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn getpeername_inet(sock_idx: u32, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let peer = socket::socket_get_peer_addr(sock_idx).ok_or(Errno::ENOTCONN)?;
    let sock_addr_in = peer.to_user();
    write_inet_sockaddr(&sock_addr_in, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn getsockname_unix(sh: SocketHandle, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let path_len = unix_socket::unix_get_local_path_len(sh);
    let mut addr_un = SockAddrUn::default();
    addr_un.family = AF_UNIX;
    if path_len > 0 {
        if let Some(path) = unix_socket::unix_get_local_path(sh) {
            addr_un.path[..path_len].copy_from_slice(&path[..path_len]);
        }
    }
    write_unix_sockaddr(&addr_un, path_len, addr_buf, addrlen_ptr)
}

#[inline(never)]
fn getsockname_inet(sock_idx: u32, addr_buf: u64, addrlen_ptr: u64) -> Result<(), Errno> {
    let local = match socket::socket_get_local_addr(sock_idx) {
        Some(l) => l,
        None => {
            // POSIX: getsockname on an unbound socket returns
            // AF_INET + zero/zero rather than EINVAL.
            let zeroed = SockAddrIn {
                family: AF_INET,
                port: 0,
                addr: [0; 4],
                _pad: [0; 8],
            };
            return write_inet_sockaddr(&zeroed, addr_buf, addrlen_ptr);
        }
    };
    let sock_addr_in = local.to_user();
    write_inet_sockaddr(&sock_addr_in, addr_buf, addrlen_ptr)
}

define_syscall!(syscall_getpeername
    (ctx, fd: Fd, addr_buf: u64, addrlen_ptr: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    if addr_buf == 0 || addrlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Unix(sh) => getpeername_unix(sh, addr_buf, addrlen_ptr),
        SocketFd::Inet(sock_idx) => getpeername_inet(sock_idx, addr_buf, addrlen_ptr),
    }
});

define_syscall!(syscall_getsockname
    (ctx, fd: Fd, addr_buf: u64, addrlen_ptr: u64)
    cap(NoneFd)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    if addr_buf == 0 || addrlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    match socket_fd_for(process_id, fd.raw())? {
        SocketFd::Unix(sh) => getsockname_unix(sh, addr_buf, addrlen_ptr),
        SocketFd::Inet(sock_idx) => getsockname_inet(sock_idx, addr_buf, addrlen_ptr),
    }
});
