//! The system as the C library offers it.
//!
//! Every call here is a function `libc.so` exports, never a system call made
//! in place: a program outside the base resolves them to `libc.so` when it is
//! loaded, and a static base program to the same functions in slibc's rlib.
//! The toolkit reaches the system only through this crate, so an app built on
//! it holds no `syscall` instruction of its own.

#![no_std]

use core::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use core::ptr::NonNull;

use slopos_abi::errno::Errno;
use slopos_abi::fs::UserStatfs;
use slopos_abi::fs::inode_flags_ioctl::FS_IOC_GETFLAGS;
use slopos_abi::net::AF_UNIX;
use slopos_abi::syscall::posix::{CLOCK_MONOTONIC, MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_abi::syscall::types::{Timespec, UserPollFd};
use slopos_abi::syscall::{MFD_CLOEXEC, MsgHdr};
use slopos_abi::unix::{SockAddrUn, UNIX_PATH_MAX};

mod c {
    use super::*;

    unsafe extern "C" {
        pub fn __errno_location() -> *mut c_int;
        pub safe fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
        pub fn connect(fd: c_int, addr: *const c_void, len: c_uint) -> c_int;
        pub fn bind(fd: c_int, addr: *const c_void, len: c_uint) -> c_int;
        pub safe fn listen(fd: c_int, backlog: c_int) -> c_int;
        pub fn accept(fd: c_int, addr: *mut c_void, len: *mut c_uint) -> c_int;
        pub safe fn close(fd: c_int) -> c_int;
        pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        pub fn statfs(path: *const c_char, buf: *mut UserStatfs) -> c_int;
        pub fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
        pub fn sendmsg(fd: c_int, msg: *const MsgHdr, flags: c_int) -> isize;
        pub fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
        pub fn poll(fds: *mut UserPollFd, nfds: c_ulong, timeout: c_int) -> c_int;
        pub fn read(fd: c_int, buf: *mut c_void, len: usize) -> isize;
        pub fn write(fd: c_int, buf: *const c_void, len: usize) -> isize;
        pub fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
        pub safe fn geteuid() -> c_uint;
        pub fn getenv(name: *const c_char) -> *mut c_char;
        pub fn clock_gettime(clock: c_int, ts: *mut Timespec) -> c_int;
        pub fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> c_int;
        pub fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
        pub safe fn ftruncate(fd: c_int, len: c_long) -> c_int;
        pub fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: c_long,
        ) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    }
}

fn errno() -> Errno {
    // SAFETY: the C library hands out the calling thread's own errno slot.
    let raw = unsafe { *c::__errno_location() };
    Errno::from_raw(-raw).unwrap_or(Errno::EIO)
}

fn check(ret: c_int) -> Result<c_int, Errno> {
    if ret < 0 { Err(errno()) } else { Ok(ret) }
}

fn check_len(ret: isize) -> Result<usize, Errno> {
    if ret < 0 {
        Err(errno())
    } else {
        Ok(ret as usize)
    }
}

pub fn socket(domain: i32, ty: i32, protocol: i32) -> Result<i32, Errno> {
    check(c::socket(domain, ty, protocol))
}

fn unix_address(path: &[u8]) -> Result<SockAddrUn, Errno> {
    if path.len() >= UNIX_PATH_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    let mut addr = SockAddrUn {
        family: AF_UNIX,
        ..SockAddrUn::default()
    };
    addr.path[..path.len()].copy_from_slice(path);
    Ok(addr)
}

const UNIX_ADDRESS_LEN: c_uint = core::mem::size_of::<SockAddrUn>() as c_uint;

pub fn connect_unix(fd: i32, path: &[u8]) -> Result<(), Errno> {
    let addr = unix_address(path)?;
    // SAFETY: `addr` is a whole `sockaddr_un` the call only reads.
    check(unsafe { c::connect(fd, (&raw const addr).cast(), UNIX_ADDRESS_LEN) }).map(drop)
}

pub fn bind_unix(fd: i32, path: &[u8]) -> Result<(), Errno> {
    let addr = unix_address(path)?;
    // SAFETY: as for `connect_unix`.
    check(unsafe { c::bind(fd, (&raw const addr).cast(), UNIX_ADDRESS_LEN) }).map(drop)
}

pub fn listen(fd: i32, backlog: i32) -> Result<(), Errno> {
    check(c::listen(fd, backlog)).map(drop)
}

pub fn accept(fd: i32) -> Result<i32, Errno> {
    // SAFETY: a null address asks for no peer address.
    check(unsafe { c::accept(fd, core::ptr::null_mut(), core::ptr::null_mut()) })
}

pub fn close(fd: i32) -> Result<(), Errno> {
    check(c::close(fd)).map(drop)
}

/// `fcntl` for a command whose argument, if it takes one, is an integer.
pub fn fcntl(fd: i32, cmd: i32, arg: i32) -> Result<i32, Errno> {
    // SAFETY: an integer argument is what these commands read.
    check(unsafe { c::fcntl(fd, cmd, arg) })
}

/// What `statfs` says of the filesystem `path` is on.
pub fn statfs(path: &CStr) -> Result<UserStatfs, Errno> {
    let mut stats = UserStatfs::default();
    // SAFETY: `path` is NUL-terminated and `stats` a whole `struct statfs`,
    // which the call writes.
    check(unsafe { c::statfs(path.as_ptr(), &mut stats) })?;
    Ok(stats)
}

/// The inode flags of the file `fd` names, as `lsattr` reads them.
pub fn inode_flags(fd: i32) -> Result<u32, Errno> {
    let mut flags: c_int = 0;
    // SAFETY: `FS_IOC_GETFLAGS` writes one `int` through its argument.
    check(unsafe { c::ioctl(fd, c_ulong::from(FS_IOC_GETFLAGS), &raw mut flags) })?;
    Ok(flags as u32)
}

pub fn send(fd: i32, buf: &[u8], flags: i32) -> Result<usize, Errno> {
    // SAFETY: the call reads `buf` alone.
    check_len(unsafe { c::send(fd, buf.as_ptr().cast(), buf.len(), flags) })
}

/// # Safety
/// Every buffer `msg` points at is valid for reads for the length it names.
pub unsafe fn sendmsg(fd: i32, msg: &MsgHdr, flags: i32) -> Result<usize, Errno> {
    // SAFETY: the caller's.
    check_len(unsafe { c::sendmsg(fd, msg, flags) })
}

/// # Safety
/// Every buffer `msg` points at is valid for writes for the length it names
/// and is not otherwise borrowed.
pub unsafe fn recvmsg(fd: i32, msg: &mut MsgHdr, flags: i32) -> Result<usize, Errno> {
    // SAFETY: the caller's.
    check_len(unsafe { c::recvmsg(fd, msg, flags) })
}

/// The number of descriptors with events, which `poll` writes into `fds`.
pub fn poll(fds: &mut [UserPollFd], timeout_ms: i32) -> Result<usize, Errno> {
    // SAFETY: `fds` is a whole array of `struct pollfd` the call may write.
    check(unsafe { c::poll(fds.as_mut_ptr(), fds.len() as c_ulong, timeout_ms) })
        .map(|n| n as usize)
}

pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, Errno> {
    // SAFETY: the call writes at most `buf.len()` bytes into `buf`.
    check_len(unsafe { c::read(fd, buf.as_mut_ptr().cast(), buf.len()) })
}

pub fn write(fd: i32, buf: &[u8]) -> Result<usize, Errno> {
    // SAFETY: the call reads `buf` alone.
    check_len(unsafe { c::write(fd, buf.as_ptr().cast(), buf.len()) })
}

/// `(read end, write end)`.
pub fn pipe2(flags: i32) -> Result<(i32, i32), Errno> {
    let mut fds = [-1 as c_int; 2];
    // SAFETY: `fds` holds the two descriptors the call writes.
    check(unsafe { c::pipe2(fds.as_mut_ptr(), flags) })?;
    Ok((fds[0], fds[1]))
}

pub fn geteuid() -> u32 {
    c::geteuid()
}

/// The environment's value for `name`, handed to `f`.
pub fn getenv<R>(name: &CStr, f: impl FnOnce(&[u8]) -> R) -> Option<R> {
    // SAFETY: `name` is NUL-terminated and the call only reads it.
    let value = unsafe { c::getenv(name.as_ptr()) };
    if value.is_null() {
        return None;
    }
    // SAFETY: what `getenv` answers is NUL-terminated and valid until the
    // environment changes, which `std::env::set_var`'s contract forbids while
    // another thread reads it.
    Some(f(unsafe { CStr::from_ptr(value) }.to_bytes()))
}

/// Milliseconds on the monotonic clock, 0 if it cannot be read.
pub fn monotonic_ms() -> u64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a whole `struct timespec` the call writes.
    if unsafe { c::clock_gettime(CLOCK_MONOTONIC as c_int, &mut ts) } < 0 {
        return 0;
    }
    ts.tv_sec as u64 * 1_000 + ts.tv_nsec as u64 / 1_000_000
}

/// Sleeps the whole interval, resuming on the remainder a signal leaves.
pub fn sleep_ms(ms: u64) {
    let mut req = Timespec {
        tv_sec: (ms / 1_000) as i64,
        tv_nsec: ((ms % 1_000) * 1_000_000) as i64,
    };
    loop {
        let mut rem = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: both are whole `struct timespec`s; the call writes `rem`.
        if unsafe { c::nanosleep(&req, &mut rem) } == 0 || errno() != Errno::EINTR {
            return;
        }
        req = rem;
    }
}

/// An anonymous file mapped shared and writable, whose descriptor another
/// process can map too.
pub struct SharedMemory {
    fd: i32,
    ptr: NonNull<u8>,
    len: usize,
}

impl SharedMemory {
    pub fn create(len: usize) -> Result<SharedMemory, Errno> {
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        // SAFETY: the name is a NUL-terminated string the call only reads.
        let fd = check(unsafe { c::memfd_create(c"slopos".as_ptr(), MFD_CLOEXEC) })?;
        let fail = |e: Errno| {
            c::close(fd);
            Err(e)
        };
        if c::ftruncate(fd, len as c_long) < 0 {
            return fail(errno());
        }
        // SAFETY: a fresh mapping the kernel places; nothing aliases it.
        let addr = unsafe {
            c::mmap(
                core::ptr::null_mut(),
                len,
                (PROT_READ | PROT_WRITE) as c_int,
                MAP_SHARED as c_int,
                fd,
                0,
            )
        };
        match NonNull::new(addr.cast::<u8>()) {
            Some(ptr) if addr as isize != -1 => Ok(SharedMemory { fd, ptr, len }),
            _ => fail(errno()),
        }
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the mapping is `len` readable bytes for as long as `self`.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, writable, and borrowed through `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // SAFETY: the mapping is this value's, and no slice of it outlives it.
        unsafe { c::munmap(self.ptr.as_ptr().cast(), self.len) };
        c::close(self.fd);
    }
}
