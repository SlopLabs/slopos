//! The interface requests a socket answers through `ioctl` — `SIOCGIFCONF`
//! and the per-interface reads — taken from the kernel's `net_query`, as
//! `getifaddrs` is. Layouts and numbers are Linux's.

use core::ffi::{c_int, c_ulong};
use core::mem::size_of;

use slopos_abi::fs::{S_IFMT, S_IFSOCK, UserFsStat};
use slopos_abi::net::{
    NET_IFKIND_ETHERNET, NET_IFKIND_LOOPBACK, NET_IFNAMSIZ, NET_Q_ADDRS, NET_Q_IFACES, UserAddr,
    UserIface,
};

use super::addr::{AF_INET, SockAddr};
use super::ifaddrs::Records;
use crate::errno::{EFAULT, EINVAL, ENODEV};
use crate::pal::{Pal, Sys};

pub const SIOCGIFCONF: c_ulong = 0x8912;
pub const SIOCGIFFLAGS: c_ulong = 0x8913;
pub const SIOCGIFMTU: c_ulong = 0x8921;
pub const SIOCGIFHWADDR: c_ulong = 0x8927;
pub const SIOCGIFINDEX: c_ulong = 0x8933;

/// `ARPHRD_*`: what kind of hardware address `SIOCGIFHWADDR` answers.
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const ARPHRD_NONE: u16 = 0xFFFE;

/// `struct ifreq`: the name, then a union whose largest arm is the 24-byte
/// `struct ifmap`.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct IfReq {
    name: [u8; NET_IFNAMSIZ],
    ifru: [u8; 24],
}

#[repr(C)]
struct IfConf {
    len: c_int,
    buf: *mut IfReq,
}

const _: () = assert!(size_of::<IfReq>() == 40);
const _: () = assert!(size_of::<IfConf>() == 16);

/// The reply to an interface request on `fd`, or `None` for any other
/// request or a descriptor that is no socket, which the kernel answers.
pub(crate) unsafe fn answer(
    fd: c_int,
    request: c_ulong,
    arg: *mut u8,
) -> Option<Result<c_int, c_int>> {
    if !matches!(
        request,
        SIOCGIFCONF | SIOCGIFFLAGS | SIOCGIFMTU | SIOCGIFHWADDR | SIOCGIFINDEX
    ) {
        return None;
    }
    let mut st = UserFsStat::default();
    if Sys::fstat(fd, (&raw mut st).cast()).is_err() || st.st_mode & S_IFMT != S_IFSOCK {
        return None;
    }
    if arg.is_null() {
        return Some(Err(EFAULT.raw()));
    }
    let ifaces = match Records::fetch(NET_Q_IFACES) {
        Ok(ifaces) => ifaces,
        Err(e) => return Some(Err(e)),
    };
    Some(if request == SIOCGIFCONF {
        configuration(&ifaces, &mut *arg.cast::<IfConf>())
    } else {
        one(&ifaces, request, &mut *arg.cast::<IfReq>())
    })
}

/// Each interface that holds an IPv4 address, with that address, as many as
/// `conf` has room for; a null buffer asks how much room all of them take.
unsafe fn configuration(ifaces: &Records, conf: &mut IfConf) -> Result<c_int, c_int> {
    let addrs = Records::fetch(NET_Q_ADDRS)?;
    let room = if conf.buf.is_null() {
        usize::MAX
    } else {
        usize::try_from(conf.len).map_err(|_| EINVAL.raw())? / size_of::<IfReq>()
    };
    let mut written = 0;
    for i in 0..addrs.count {
        let addr = addrs.get::<UserAddr>(i);
        let Some(iface) = (0..ifaces.count)
            .map(|j| ifaces.get::<UserIface>(j))
            .find(|iface| iface.ifindex == addr.ifindex)
        else {
            continue;
        };
        if !conf.buf.is_null() {
            if written == room {
                break;
            }
            let mut req = IfReq {
                name: iface.name,
                ifru: [0; 24],
            };
            let mut sa = SockAddr {
                sa_family: AF_INET as u16,
                sa_data: [0; 14],
            };
            sa.sa_data[2..6].copy_from_slice(&addr.addr);
            put_sockaddr(&mut req, &sa);
            conf.buf.add(written).write(req);
        }
        written += 1;
    }
    conf.len = (written * size_of::<IfReq>()) as c_int;
    Ok(0)
}

unsafe fn one(ifaces: &Records, request: c_ulong, req: &mut IfReq) -> Result<c_int, c_int> {
    let len = req
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(NET_IFNAMSIZ);
    let iface = (0..ifaces.count)
        .map(|j| ifaces.get::<UserIface>(j))
        .find(|iface| {
            let named = iface
                .name
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(NET_IFNAMSIZ);
            iface.name[..named] == req.name[..len]
        })
        .ok_or(ENODEV.raw())?;
    req.ifru = [0; 24];
    match request {
        SIOCGIFFLAGS => req.ifru[..2].copy_from_slice(&(iface.flags as i16).to_ne_bytes()),
        SIOCGIFMTU => req.ifru[..4].copy_from_slice(&(iface.mtu as c_int).to_ne_bytes()),
        SIOCGIFINDEX => req.ifru[..4].copy_from_slice(&(iface.ifindex as c_int).to_ne_bytes()),
        _ => {
            let family = match iface.kind {
                NET_IFKIND_ETHERNET => ARPHRD_ETHER,
                NET_IFKIND_LOOPBACK => ARPHRD_LOOPBACK,
                _ => ARPHRD_NONE,
            };
            let mut sa = SockAddr {
                sa_family: family,
                sa_data: [0; 14],
            };
            sa.sa_data[..6].copy_from_slice(&iface.mac);
            put_sockaddr(req, &sa);
        }
    }
    Ok(0)
}

fn put_sockaddr(req: &mut IfReq, sa: &SockAddr) {
    req.ifru[..2].copy_from_slice(&sa.sa_family.to_ne_bytes());
    req.ifru[2..16].copy_from_slice(&sa.sa_data);
}
