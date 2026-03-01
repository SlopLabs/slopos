use super::RawFd;
use super::error::{SyscallResult, demux};
use super::numbers::{
    SYSCALL_ACCEPT, SYSCALL_BIND, SYSCALL_CONNECT, SYSCALL_GETSOCKOPT, SYSCALL_LISTEN,
    SYSCALL_NET_INFO, SYSCALL_NET_SCAN, SYSCALL_RECV, SYSCALL_RECVFROM, SYSCALL_RESOLVE,
    SYSCALL_SEND, SYSCALL_SENDTO, SYSCALL_SETSOCKOPT, SYSCALL_SHUTDOWN, SYSCALL_SOCKET,
};
use super::raw::{syscall1, syscall2, syscall3, syscall4, syscall5, syscall6};
use slopos_abi::net::{SockAddrIn, UserNetInfo, UserNetMember};

#[inline(always)]
pub fn net_scan(out: &mut [UserNetMember], active_probe: bool) -> i64 {
    if out.is_empty() {
        return 0;
    }

    unsafe {
        syscall3(
            SYSCALL_NET_SCAN,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            if active_probe { 1 } else { 0 },
        ) as i64
    }
}

#[inline(always)]
pub fn net_info(out: &mut UserNetInfo) -> i64 {
    unsafe { syscall1(SYSCALL_NET_INFO, out as *mut UserNetInfo as u64) as i64 }
}

pub fn socket(domain: u16, sock_type: u16, protocol: u16) -> SyscallResult<RawFd> {
    let result = unsafe {
        syscall3(
            SYSCALL_SOCKET,
            domain as u64,
            sock_type as u64,
            protocol as u64,
        )
    };
    demux(result).map(|v| v as RawFd)
}

pub fn bind(fd: RawFd, addr: &SockAddrIn) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_BIND,
            fd as u64,
            addr as *const _ as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    demux(result).map(|_| ())
}

pub fn listen(fd: RawFd, backlog: u32) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_LISTEN, fd as u64, backlog as u64) };
    demux(result).map(|_| ())
}

pub fn accept(fd: RawFd, peer: Option<&mut SockAddrIn>) -> SyscallResult<RawFd> {
    let peer_ptr = peer.map(|p| p as *mut _ as u64).unwrap_or(0);
    let len: u64 = if peer_ptr != 0 {
        core::mem::size_of::<SockAddrIn>() as u64
    } else {
        0
    };
    let result = unsafe { syscall3(SYSCALL_ACCEPT, fd as u64, peer_ptr, len) };
    demux(result).map(|v| v as RawFd)
}

pub fn connect(fd: RawFd, addr: &SockAddrIn) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_CONNECT,
            fd as u64,
            addr as *const _ as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    demux(result).map(|_| ())
}

pub fn send(fd: RawFd, data: &[u8], flags: u32) -> SyscallResult<usize> {
    let result = unsafe {
        syscall4(
            SYSCALL_SEND,
            fd as u64,
            data.as_ptr() as u64,
            data.len() as u64,
            flags as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

pub fn recv(fd: RawFd, buf: &mut [u8], flags: u32) -> SyscallResult<usize> {
    let result = unsafe {
        syscall4(
            SYSCALL_RECV,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            flags as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

pub fn sendto(fd: RawFd, data: &[u8], flags: u32, addr: &SockAddrIn) -> SyscallResult<usize> {
    let result = unsafe {
        syscall6(
            SYSCALL_SENDTO,
            fd as u64,
            data.as_ptr() as u64,
            data.len() as u64,
            flags as u64,
            addr as *const _ as u64,
            core::mem::size_of::<SockAddrIn>() as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

pub fn recvfrom(
    fd: RawFd,
    buf: &mut [u8],
    flags: u32,
    src_addr: Option<&mut SockAddrIn>,
) -> SyscallResult<usize> {
    let src_addr_ptr = src_addr.map(|a| a as *mut _ as u64).unwrap_or(0);
    let result = unsafe {
        syscall6(
            SYSCALL_RECVFROM,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            flags as u64,
            src_addr_ptr,
            if src_addr_ptr != 0 {
                core::mem::size_of::<SockAddrIn>() as u64
            } else {
                0
            },
        )
    };
    demux(result).map(|v| v as usize)
}

pub fn setsockopt(fd: RawFd, level: i32, optname: i32, val: &[u8]) -> SyscallResult<()> {
    let result = unsafe {
        syscall5(
            SYSCALL_SETSOCKOPT,
            fd as u64,
            level as u64,
            optname as u64,
            val.as_ptr() as u64,
            val.len() as u64,
        )
    };
    demux(result).map(|_| ())
}

pub fn getsockopt(fd: RawFd, level: i32, optname: i32, buf: &mut [u8]) -> SyscallResult<usize> {
    let mut optlen = buf.len() as u32;
    let result = unsafe {
        syscall5(
            SYSCALL_GETSOCKOPT,
            fd as u64,
            level as u64,
            optname as u64,
            buf.as_mut_ptr() as u64,
            &mut optlen as *mut u32 as u64,
        )
    };
    demux(result).map(|_| optlen as usize)
}

pub fn shutdown(fd: RawFd, how: i32) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_SHUTDOWN, fd as u64, how as u64) };
    demux(result).map(|_| ())
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

/// Resolve a hostname to an IPv4 address via the in-kernel DNS client.
///
/// Returns `Some([a, b, c, d])` on success, or `None` if resolution fails.
pub fn resolve(hostname: &[u8]) -> Option<[u8; 4]> {
    let mut result = [0u8; 4];
    let rc = unsafe {
        syscall3(
            SYSCALL_RESOLVE,
            hostname.as_ptr() as u64,
            hostname.len() as u64,
            &mut result as *mut [u8; 4] as u64,
        )
    };
    if (rc as i64) < 0 { None } else { Some(result) }
}
