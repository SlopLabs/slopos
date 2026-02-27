use super::RawFd;
use super::error::{SyscallResult, demux};
use super::numbers::{
    SYSCALL_ACCEPT, SYSCALL_BIND, SYSCALL_CONNECT, SYSCALL_LISTEN, SYSCALL_NET_INFO,
    SYSCALL_NET_SCAN, SYSCALL_RECV, SYSCALL_SEND, SYSCALL_SOCKET,
};
use super::raw::{syscall1, syscall2, syscall3, syscall4};
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
