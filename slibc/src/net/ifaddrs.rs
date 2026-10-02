//! `getifaddrs`: the interfaces and their IPv4 addresses, read from the
//! kernel's `net_query`. That is whole-machine enumeration, so a caller
//! without `SysInspect` gets `EPERM`.

use core::ffi::{c_char, c_int, c_uint, c_void};

use slopos_abi::net::{
    NET_IFNAMSIZ, NET_Q_ADDRS, NET_Q_IFACES, UserAddr, UserIface, UserNetQueryHdr,
};

use crate::errno::{ENOMEM, errno_set};
use crate::mem::malloc;
use crate::net::addr::{AF_INET, SockAddr, SockAddrIn};
use crate::pal::{Pal, Sys};

/// `ifa_ifu` is the broadcast address of a broadcast interface;
/// `<ifaddrs.h>` names it `ifa_broadaddr` and `ifa_dstaddr`.
#[repr(C)]
pub struct ifaddrs {
    pub ifa_next: *mut ifaddrs,
    pub ifa_name: *mut c_char,
    pub ifa_flags: c_uint,
    pub ifa_addr: *mut SockAddr,
    pub ifa_netmask: *mut SockAddr,
    pub ifa_ifu: *mut SockAddr,
    pub ifa_data: *mut c_void,
}

const HDR: usize = size_of::<UserNetQueryHdr>();

/// One query's records, in a buffer `malloc` owns. The kernel's stride is
/// honoured and each record copied out, since neither side promised the
/// other an alignment.
pub(crate) struct Records {
    buf: *mut u8,
    stride: usize,
    pub(crate) count: usize,
}

impl Records {
    pub(crate) fn fetch(what: u32) -> Result<Self, c_int> {
        let mut sizing = [0u8; HDR];
        Sys::net_query(what, 0, sizing.as_mut_ptr(), HDR).map_err(|e| e.raw())?;
        let want = header(sizing.as_ptr()).total_count as usize;
        let stride = (header(sizing.as_ptr()).record_size as usize).max(1);
        let len = HDR + want * stride;
        let buf = malloc::alloc(len).cast::<u8>();
        if buf.is_null() {
            return Err(ENOMEM.raw());
        }
        if let Err(e) = Sys::net_query(what, 0, buf, len) {
            malloc::dealloc(buf.cast());
            return Err(e.raw());
        }
        let hdr = header(buf);
        let stride = (hdr.record_size as usize).max(1);
        let count = (hdr.record_count as usize).min((len - HDR) / stride);
        Ok(Self { buf, stride, count })
    }

    pub(crate) fn get<T: Copy + Default>(&self, i: usize) -> T {
        let mut out = T::default();
        let n = self.stride.min(size_of::<T>());
        // SAFETY: record `i` lies inside the buffer the kernel filled, and
        // every `T` read here is a plain `#[repr(C)]` ABI record.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.buf.add(HDR + i * self.stride),
                (&raw mut out).cast::<u8>(),
                n,
            );
        }
        out
    }
}

impl Drop for Records {
    fn drop(&mut self) {
        malloc::dealloc(self.buf.cast());
    }
}

fn header(buf: *const u8) -> UserNetQueryHdr {
    let mut hdr = UserNetQueryHdr::default();
    // SAFETY: `buf` holds at least a header.
    unsafe { core::ptr::copy_nonoverlapping(buf, (&raw mut hdr).cast::<u8>(), HDR) };
    hdr
}

fn sockaddr_in(addr: [u8; 4]) -> SockAddrIn {
    SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: 0,
        sin_addr: u32::from_ne_bytes(addr),
        sin_zero: [0; 8],
    }
}

fn netmask(prefix_len: u8) -> [u8; 4] {
    let bits = u32::from(prefix_len.min(32));
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    mask.to_be_bytes()
}

/// `getifaddrs(3)`: one entry per interface, with no address, then one per
/// IPv4 address. The list is one allocation, released by [`freeifaddrs`].
///
/// # Safety
/// `out` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getifaddrs(out: *mut *mut ifaddrs) -> c_int {
    let fetched = Records::fetch(NET_Q_IFACES)
        .and_then(|ifaces| Records::fetch(NET_Q_ADDRS).map(|addrs| (ifaces, addrs)));
    let (ifaces, addrs) = match fetched {
        Ok(pair) => pair,
        Err(rc) => {
            errno_set(rc);
            return -1;
        }
    };
    let entries = ifaces.count + addrs.count;
    let name_len = NET_IFNAMSIZ + 1;
    let size = entries * (size_of::<ifaddrs>() + name_len + 3 * size_of::<SockAddrIn>());
    let block = malloc::alloc(size.max(1)).cast::<u8>();
    if block.is_null() {
        errno_set(ENOMEM.raw());
        return -1;
    }
    core::ptr::write_bytes(block, 0, size.max(1));
    // Nodes, then addresses, then names, so each part starts aligned for what
    // it holds.
    let nodes = block.cast::<ifaddrs>();
    let socks = block
        .add(entries * size_of::<ifaddrs>())
        .cast::<SockAddrIn>();
    let names = socks.add(3 * entries).cast::<u8>();

    let mut at = 0usize;
    let mut push = |iface: &UserIface, addr: Option<&UserAddr>| {
        let node = nodes.add(at);
        let name = names.add(at * name_len);
        let len = iface
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(NET_IFNAMSIZ);
        core::ptr::copy_nonoverlapping(iface.name.as_ptr(), name, len);
        (*node).ifa_name = name.cast();
        (*node).ifa_flags = iface.flags & 0xffff;
        if let Some(addr) = addr {
            let [local, mask, broadcast] = [
                socks.add(3 * at),
                socks.add(3 * at + 1),
                socks.add(3 * at + 2),
            ];
            let netmask = netmask(addr.prefix_len);
            *local = sockaddr_in(addr.addr);
            *mask = sockaddr_in(netmask);
            (*node).ifa_addr = local.cast();
            (*node).ifa_netmask = mask.cast();
            if iface.flags & slopos_abi::net::IFF_BROADCAST != 0 {
                let host = !u32::from_be_bytes(netmask);
                *broadcast = sockaddr_in((u32::from_be_bytes(addr.addr) | host).to_be_bytes());
                (*node).ifa_ifu = broadcast.cast();
            }
        }
        if at > 0 {
            (*nodes.add(at - 1)).ifa_next = node;
        }
        at += 1;
    };
    for i in 0..ifaces.count {
        push(&ifaces.get::<UserIface>(i), None);
    }
    for i in 0..addrs.count {
        let addr = addrs.get::<UserAddr>(i);
        if let Some(iface) = (0..ifaces.count)
            .map(|j| ifaces.get::<UserIface>(j))
            .find(|iface| iface.ifindex == addr.ifindex)
        {
            push(&iface, Some(&addr));
        }
    }
    *out = if at == 0 {
        malloc::dealloc(block.cast());
        core::ptr::null_mut()
    } else {
        nodes
    };
    0
}

/// `freeifaddrs(3)`.
///
/// # Safety
/// `ifa` is what `getifaddrs` produced, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn freeifaddrs(ifa: *mut ifaddrs) {
    malloc::dealloc(ifa.cast());
}

pub const IF_NAMESIZE: usize = 16;
pub const IFNAMSIZ: usize = 16;

pub const IFF_UP: c_uint = 0x1;
pub const IFF_BROADCAST: c_uint = 0x2;
pub const IFF_DEBUG: c_uint = 0x4;
pub const IFF_LOOPBACK: c_uint = 0x8;
pub const IFF_POINTOPOINT: c_uint = 0x10;
pub const IFF_NOTRAILERS: c_uint = 0x20;
pub const IFF_RUNNING: c_uint = 0x40;
pub const IFF_NOARP: c_uint = 0x80;
pub const IFF_PROMISC: c_uint = 0x100;
pub const IFF_ALLMULTI: c_uint = 0x200;
pub const IFF_MASTER: c_uint = 0x400;
pub const IFF_SLAVE: c_uint = 0x800;
pub const IFF_MULTICAST: c_uint = 0x1000;
pub const IFF_PORTSEL: c_uint = 0x2000;
pub const IFF_AUTOMEDIA: c_uint = 0x4000;
pub const IFF_DYNAMIC: c_uint = 0x8000;
