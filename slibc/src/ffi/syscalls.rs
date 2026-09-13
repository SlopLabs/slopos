use crate::errno::EFAULT;
use crate::pal::Pal;
use crate::pal::Sys;
use slopos_abi::fs::{AT_FDCWD, AT_SYMLINK_NOFOLLOW, UserIovec};
use slopos_abi::spawn::SpawnAttrs;
use slopos_abi::syscall::{Timespec, UserUtsname};

/// Pins the originals of the layout asserts in `slibc/std_pal/**`, which is
/// compiled into `std` and so cannot depend on `slopos-abi`.
mod std_pal_layout_pins {
    use core::mem::{offset_of, size_of};
    use slopos_abi::fs::{UserDirent64, UserFsStat, UserIovec};
    use slopos_abi::signal::UserSiginfo;
    use slopos_abi::spawn::{SpawnAttrs, SpawnFdAction};
    use slopos_abi::syscall::{Timespec, UserUtsname};

    // slibc/std_pal/fs/slopos.rs: `SloposStat`, `Timespec`
    const _: () = assert!(size_of::<Timespec>() == 16);
    const _: () = assert!(size_of::<UserFsStat>() == 144);
    const _: () = assert!(offset_of!(UserFsStat, st_mode) == 24);
    const _: () = assert!(offset_of!(UserFsStat, st_size) == 48);
    const _: () = assert!(offset_of!(UserFsStat, st_atim) == 72);
    const _: () = assert!(offset_of!(UserFsStat, st_mtim) == 88);
    const _: () = assert!(offset_of!(UserFsStat, st_ctim) == 104);

    // slibc/std_pal/fs/slopos.rs: `DIRENT_NAME_OFFSET`
    const _: () = assert!(size_of::<UserDirent64>() == 24);

    // slibc/std_pal/fd/slopos.rs: `Iovec`
    const _: () = assert!(size_of::<UserIovec>() == 16);

    // slibc/std_pal/process/slopos.rs: `SpawnFdAction`, `SpawnAttrs`
    const _: () = assert!(size_of::<SpawnFdAction>() == 40);
    const _: () = assert!(size_of::<SpawnAttrs>() == 64);
    const _: () = assert!(offset_of!(SpawnAttrs, flags) == 4);
    const _: () = assert!(offset_of!(SpawnAttrs, envp_ptr) == 32);
    const _: () = assert!(offset_of!(SpawnAttrs, cwd_ptr) == 48);

    // slibc/std_pal/net/hostname_slopos.rs: `UTS_FIELD_LEN * UTS_FIELDS`
    const _: () = assert!(size_of::<UserUtsname>() == 325);
    const _: () = assert!(offset_of!(UserUtsname, nodename) == 65);

    // slibc/std_pal/pal/slopos/stack_overflow.rs: `SI_ADDR_OFFSET`
    const _: () = assert!(offset_of!(UserSiginfo, si_addr) == 24);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    match Sys::lseek(fd, offset, whence) {
        Ok(pos) => pos,
        Err(e) => -(e.raw() as i64),
    }
}

pub use slopos_abi::fs::UserFsStat as SloposStat;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fstat(fd: i32, stat_buf: *mut SloposStat) -> i32 {
    if stat_buf.is_null() {
        return -EFAULT.raw();
    }
    match Sys::fstat(fd, stat_buf as *mut u8) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_stat(path: *const u8, stat_buf: *mut SloposStat) -> i32 {
    if stat_buf.is_null() {
        return -EFAULT.raw();
    }
    match Sys::stat(path, stat_buf as *mut u8) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_lstat(path: *const u8, stat_buf: *mut SloposStat) -> i32 {
    unsafe { slopos_fstatat(AT_FDCWD, path, stat_buf, AT_SYMLINK_NOFOLLOW) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fstatat(
    dirfd: i32,
    path: *const u8,
    stat_buf: *mut SloposStat,
    flags: u32,
) -> i32 {
    if stat_buf.is_null() {
        return -EFAULT.raw();
    }
    match Sys::fstatat(dirfd, path, stat_buf, flags) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fsync(fd: i32) -> i32 {
    match Sys::fsync(fd) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fdatasync(fd: i32) -> i32 {
    match Sys::fdatasync(fd) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_sync() -> i32 {
    match Sys::sync() {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_mkdir(path: *const u8, mode: u32) -> i32 {
    match Sys::mkdir(path, mode) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_unlink(path: *const u8) -> i32 {
    match Sys::unlink(path) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_rename(old: *const u8, new: *const u8) -> i32 {
    match Sys::rename(old, new) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_dup(fd: i32) -> i32 {
    match Sys::dup(fd) {
        Ok(new_fd) => new_fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_dup2(old: i32, new: i32) -> i32 {
    match Sys::dup2(old, new) {
        Ok(fd) => fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_list(path: *const u8, buf: *mut u8, buf_len: usize) -> isize {
    match Sys::list(path, buf, buf_len) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_pipe(fds: *mut i32) -> i32 {
    let fds_arr = unsafe { &mut *(fds as *mut [i32; 2]) };
    match Sys::pipe(fds_arr) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_kill(pid: i32, sig: i32) -> i32 {
    match Sys::kill(pid, sig) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// `timeout_ns` of `u64::MAX` blocks indefinitely.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_futex_wait(
    addr: *const u32,
    expected: u32,
    timeout_ns: u64,
) -> i32 {
    let ts;
    let timeout = if timeout_ns == u64::MAX {
        core::ptr::null()
    } else {
        ts = crate::time::timespec_from_nanos(timeout_ns);
        &raw const ts
    };
    match Sys::futex_wait(addr, expected, timeout) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_futex_wake(addr: *const u32, count: u32) -> i32 {
    match Sys::futex_wake(addr, count) {
        Ok(n) => n,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_get_cpu_count() -> i32 {
    match Sys::get_cpu_count() {
        Ok(n) => n as i32,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_get_current_cpu() -> i32 {
    match Sys::get_current_cpu() {
        Ok(n) => n as i32,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_set_cpu_affinity(target: u32, affinity: u32) -> i32 {
    match Sys::set_cpu_affinity(target, affinity) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_clock_gettime(clk_id: u64, sec: *mut i64, nsec: *mut i64) -> i32 {
    let mut raw = [0u8; 16];
    match Sys::clock_gettime(clk_id, raw.as_mut_ptr()) {
        Ok(()) => {
            if !sec.is_null() {
                unsafe {
                    *sec = i64::from_le_bytes([
                        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
                    ]);
                }
            }
            if !nsec.is_null() {
                unsafe {
                    *nsec = i64::from_le_bytes([
                        raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
                    ]);
                }
            }
            0
        }
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_get_time_ms() -> u64 {
    Sys::get_time_ms()
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_sleep_ms(ms: u64) {
    Sys::sleep_ms(ms);
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_yield() {
    Sys::yield_now();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_mmap(
    addr: *mut u8,
    len: usize,
    prot: u64,
    flags: u64,
    fd: i32,
    offset: u64,
) -> *mut u8 {
    match Sys::mmap(addr, len, prot, flags, fd, offset) {
        Ok(ptr) => ptr,
        Err(_) => core::ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_munmap(addr: *mut u8, len: usize) -> i32 {
    match Sys::munmap(addr, len) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_msync(addr: *mut u8, len: usize, flags: i32) -> i32 {
    match Sys::msync(addr, len, flags as u64) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_socket(domain: i32, sock_type: i32, protocol: i32) -> i32 {
    match Sys::socket(domain, sock_type, protocol) {
        Ok(fd) => fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_bind(fd: i32, addr: *const u8, addrlen: u32) -> i32 {
    match Sys::bind(fd, addr, addrlen) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_listen(fd: i32, backlog: i32) -> i32 {
    match Sys::listen(fd, backlog) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_accept(fd: i32, addr: *mut u8, addrlen: *mut u32) -> i32 {
    match Sys::accept(fd, addr, addrlen) {
        Ok(new_fd) => new_fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_connect(fd: i32, addr: *const u8, addrlen: u32) -> i32 {
    match Sys::connect(fd, addr, addrlen) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize {
    match Sys::send(fd, buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize {
    match Sys::recv(fd, buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: i32,
    addr: *const u8,
    addrlen: u32,
) -> isize {
    match Sys::sendto(fd, buf, len, flags, addr, addrlen) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: i32,
    addr: *mut u8,
    addrlen: *mut u32,
) -> isize {
    match Sys::recvfrom(fd, buf, len, flags, addr, addrlen) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_setsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *const u8,
    optlen: u32,
) -> i32 {
    match Sys::setsockopt(fd, level, optname, optval, optlen) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_getsockopt(
    fd: i32,
    level: i32,
    optname: i32,
    optval: *mut u8,
    optlen: *mut u32,
) -> i32 {
    match Sys::getsockopt(fd, level, optname, optval, optlen) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_shutdown(fd: i32, how: i32) -> i32 {
    match Sys::shutdown(fd, how) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_getpeername(fd: i32, addr: *mut u8, addrlen: *mut u32) -> i32 {
    match Sys::getpeername(fd, addr, addrlen) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_getsockname(fd: i32, addr: *mut u8, addrlen: *mut u32) -> i32 {
    match Sys::getsockname(fd, addr, addrlen) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_poll(fds: *mut u8, nfds: u32, timeout: i32) -> i32 {
    match Sys::poll(fds, nfds, timeout) {
        Ok(n) => n,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_ioctl(fd: i32, request: u64, arg: u64) -> i32 {
    match Sys::ioctl(fd, request, arg) {
        Ok(n) => n,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fcntl(fd: i32, cmd: i32, arg: u64) -> i32 {
    match Sys::fcntl(fd, cmd, arg) {
        Ok(n) => n,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_rmdir(path: *const u8) -> i32 {
    match Sys::rmdir(path) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_symlink(target: *const u8, link_path: *const u8) -> i32 {
    match Sys::symlink(target, link_path) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// Answers the byte count and never NUL-terminates, per POSIX.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_readlink(path: *const u8, buf: *mut u8, buf_len: usize) -> isize {
    match Sys::readlink(path, buf, buf_len) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_link(old: *const u8, new: *const u8) -> i32 {
    match Sys::link(old, new) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_truncate(path: *const u8, length: u64) -> i32 {
    match Sys::truncate(path, length) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_ftruncate(fd: i32, length: u64) -> i32 {
    match Sys::ftruncate(fd, length) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_chmod(path: *const u8, mode: u32) -> i32 {
    match Sys::chmod(path, mode) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fchmod(fd: i32, mode: u32) -> i32 {
    match Sys::fchmod(fd, mode) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_fchmodat(
    dirfd: i32,
    path: *const u8,
    mode: u32,
    flags: u32,
) -> i32 {
    match Sys::fchmodat(dirfd, path, mode, flags) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_access(path: *const u8, mode: u32) -> i32 {
    match Sys::access(path, mode) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_flock(fd: i32, operation: u32) -> i32 {
    match Sys::flock(fd, operation) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_openat(dirfd: i32, path: *const u8, flags: i32, mode: u32) -> i32 {
    match Sys::openat(dirfd, path, flags, mode) {
        Ok(fd) => fd,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_mkdirat(dirfd: i32, path: *const u8, mode: u32) -> i32 {
    match Sys::mkdirat(dirfd, path, mode) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_unlinkat(dirfd: i32, path: *const u8, flags: u32) -> i32 {
    match Sys::unlinkat(dirfd, path, flags) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_renameat(
    olddirfd: i32,
    old: *const u8,
    newdirfd: i32,
    new: *const u8,
) -> i32 {
    match Sys::renameat(olddirfd, old, newdirfd, new) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_readlinkat(
    dirfd: i32,
    path: *const u8,
    buf: *mut u8,
    buf_len: usize,
) -> isize {
    match Sys::readlinkat(dirfd, path, buf, buf_len) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_symlinkat(
    target: *const u8,
    newdirfd: i32,
    link_path: *const u8,
) -> i32 {
    match Sys::symlinkat(target, newdirfd, link_path) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_linkat(
    olddirfd: i32,
    old: *const u8,
    newdirfd: i32,
    new: *const u8,
    flags: u32,
) -> i32 {
    match Sys::linkat(olddirfd, old, newdirfd, new, flags) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_faccessat(
    dirfd: i32,
    path: *const u8,
    mode: u32,
    flags: u32,
) -> i32 {
    match Sys::faccessat(dirfd, path, mode, flags) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// `times` is `[atime, mtime]`; a null pointer sets both to now.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_utimensat(
    dirfd: i32,
    path: *const u8,
    times: *const Timespec,
    flags: u32,
) -> i32 {
    match Sys::utimensat(dirfd, path, times as *const [Timespec; 2], flags) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// Packed `UserDirent64` records; 0 means the directory is exhausted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_getdents64(fd: i32, buf: *mut u8, buf_len: usize) -> isize {
    match Sys::getdents64(fd, buf, buf_len) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_pread(fd: i32, buf: *mut u8, count: usize, offset: i64) -> isize {
    match Sys::pread64(fd, buf, count, offset) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_pwrite(
    fd: i32,
    buf: *const u8,
    count: usize,
    offset: i64,
) -> isize {
    match Sys::pwrite64(fd, buf, count, offset) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_readv(fd: i32, iov: *const UserIovec, iovcnt: i32) -> isize {
    match Sys::readv(fd, iov, iovcnt) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_writev(fd: i32, iov: *const UserIovec, iovcnt: i32) -> isize {
    match Sys::writev(fd, iov, iovcnt) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_uname(out: *mut UserUtsname) -> i32 {
    if out.is_null() {
        return -EFAULT.raw();
    }
    match Sys::uname(out) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_clock_settime(clk_id: u64, sec: i64, nsec: i64) -> i32 {
    let ts = Timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    };
    match Sys::clock_settime(clk_id, &raw const ts) {
        Ok(()) => 0,
        Err(e) => -(e.raw()),
    }
}

/// A short fill is legal; a caller wanting a full buffer loops.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_getrandom(buf: *mut u8, len: usize, flags: u32) -> isize {
    match Sys::getrandom(buf, len, flags) {
        Ok(n) => n as isize,
        Err(e) => -(e.raw() as isize),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn slopos_gettid() -> i32 {
    Sys::gettid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_waitpid(pid: i32, status: *mut i32, options: i32) -> i32 {
    match Sys::waitpid(pid, status, options) {
        Ok(reaped) => reaped,
        Err(e) => -(e.raw()),
    }
}

/// Nothing runs in the child between fork and exec, so a lock held by a
/// sibling of the spawning thread cannot deadlock it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_spawn_path(
    path: *const u8,
    path_len: usize,
    argv: *const *const u8,
    argc: u32,
    attrs: *const SpawnAttrs,
) -> i32 {
    match Sys::spawn_path(path, path_len, argv, argc, attrs) {
        Ok(tid) => tid,
        Err(e) => -(e.raw()),
    }
}
