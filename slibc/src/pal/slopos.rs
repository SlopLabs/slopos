use crate::errno::Errno;
use crate::pal::Pal;
use crate::pal::raw::*;
use slopos_abi::fs::{UserFsStat, UserIovec};
use slopos_abi::signal::UserSigAltStack;
use slopos_abi::spawn::SpawnAttrs;
use slopos_abi::syscall::*;

pub struct Sys;

/// Signal restorer trampoline — the address userland installs as
/// `sa_restorer`. The kernel refuses to deliver a handler whose
/// `sa_restorer` is 0, so libc must always inject one.
///
/// The kernel pushes the restorer address as a separate stack word ahead of
/// the `SignalFrame`, so once the handler's `ret` pops it RSP points directly
/// at the frame and `rt_sigreturn` needs no stack adjustment. `ud2` traps if
/// `rt_sigreturn` ever returns.
#[unsafe(naked)]
extern "C" fn signal_restorer() {
    core::arch::naked_asm!(
        "mov eax, {sigreturn}",
        "syscall",
        "ud2",
        sigreturn = const SYSCALL_RT_SIGRETURN,
    );
}

/// Address of the [`signal_restorer`] trampoline, for use as `sa_restorer`.
pub(crate) fn signal_restorer_addr() -> u64 {
    signal_restorer as *const () as u64
}

#[inline]
fn to_result(ret: u64) -> Result<u64, Errno> {
    crate::demux(ret).map_err(|e| {
        let errno = Errno::from(e);
        crate::errno::errno_set(errno.raw());
        errno
    })
}

/// Bytes an affinity mask is marshalled through — the `unsigned long`
/// granularity `sched_setaffinity` expects, and wide enough for every CPU the
/// scheduler tracks.
const CPU_MASK_BYTES: usize = 8;

/// `memfd_create` validates its name and keeps nothing else about it.
const MEMFD_NAME: &[u8] = b"slopos\0";

/// `reboot(2)` with the magics it demands. It only returns on failure — a
/// task without `Capability::Power` — and there is nothing left to do then.
fn power_off_or_spin(cmd: u64) -> ! {
    unsafe {
        syscall4(
            SYSCALL_REBOOT,
            LINUX_REBOOT_MAGIC1,
            LINUX_REBOOT_MAGIC2,
            cmd,
            0,
        );
    }
    loop {
        core::hint::spin_loop();
    }
}

impl Pal for Sys {
    fn open(path: *const u8, flags: i32, mode: u32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_OPEN, path as u64, flags as u64, mode as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn close(fd: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_CLOSE, fd as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn read(fd: i32, buf: *mut u8, count: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_READ, fd as u64, buf as u64, count as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn write(fd: i32, buf: *const u8, count: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_WRITE, fd as u64, buf as u64, count as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn lseek(fd: i32, offset: i64, whence: i32) -> Result<i64, Errno> {
        let ret = unsafe { syscall3(SYSCALL_LSEEK, fd as u64, offset as u64, whence as u64) };
        let val = to_result(ret)?;
        Ok(val as i64)
    }

    fn fstat(fd: i32, stat_buf: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FSTAT, fd as u64, stat_buf as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn stat(path: *const u8, stat_buf: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_STAT, path as u64, stat_buf as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn mkdir(path: *const u8, mode: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_MKDIR, path as u64, mode as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn unlink(path: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_UNLINK, path as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn rename(old: *const u8, new: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_RENAME, old as u64, new as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn dup(fd: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall1(SYSCALL_DUP, fd as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn dup2(old: i32, new: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall2(SYSCALL_DUP2, old as u64, new as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn dup3(old: i32, new: i32, flags: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_DUP3, old as u64, new as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn fcntl(fd: i32, cmd: i32, arg: u64) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_FCNTL, fd as u64, cmd as u64, arg) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn pipe(fds: *mut [i32; 2]) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_PIPE, fds as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn pipe2(fds: *mut [i32; 2], flags: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_PIPE2, fds as u64, flags as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn poll(fds: *mut u8, nfds: u32, timeout: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_POLL, fds as u64, nfds as u64, timeout as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn select(
        nfds: i32,
        readfds: *mut u8,
        writefds: *mut u8,
        exceptfds: *mut u8,
        timeout: *mut u8,
    ) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_SELECT,
                nfds as u64,
                readfds as u64,
                writefds as u64,
                exceptfds as u64,
                timeout as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn ioctl(fd: i32, request: u64, arg: u64) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_IOCTL, fd as u64, request, arg) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn rmdir(path: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_RMDIR, path as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn symlink(target: *const u8, link_path: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_SYMLINK, target as u64, link_path as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn readlink(path: *const u8, buf: *mut u8, buf_len: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_READLINK, path as u64, buf as u64, buf_len as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn truncate(path: *const u8, length: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_TRUNCATE, path as u64, length) };
        to_result(ret)?;
        Ok(())
    }

    fn chmod(path: *const u8, mode: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_CHMOD, path as u64, mode as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn openat(dirfd: i32, path: *const u8, flags: i32, mode: u32) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_OPENAT,
                dirfd as u64,
                path as u64,
                flags as u64,
                mode as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn mkdirat(dirfd: i32, path: *const u8, mode: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_MKDIRAT, dirfd as u64, path as u64, mode as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn unlinkat(dirfd: i32, path: *const u8, flags: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_UNLINKAT, dirfd as u64, path as u64, flags as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn renameat(olddirfd: i32, old: *const u8, newdirfd: i32, new: *const u8) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_RENAMEAT,
                olddirfd as u64,
                old as u64,
                newdirfd as u64,
                new as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn fstatat(
        dirfd: i32,
        path: *const u8,
        stat_buf: *mut UserFsStat,
        flags: u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_NEWFSTATAT,
                dirfd as u64,
                path as u64,
                stat_buf as u64,
                flags as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn readlinkat(
        dirfd: i32,
        path: *const u8,
        buf: *mut u8,
        buf_len: usize,
    ) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_READLINKAT,
                dirfd as u64,
                path as u64,
                buf as u64,
                buf_len as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn symlinkat(target: *const u8, newdirfd: i32, link: *const u8) -> Result<(), Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_SYMLINKAT,
                target as u64,
                newdirfd as u64,
                link as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn fchmodat(dirfd: i32, path: *const u8, mode: u32, flags: u32) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_FCHMODAT2,
                dirfd as u64,
                path as u64,
                mode as u64,
                flags as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn faccessat(dirfd: i32, path: *const u8, mode: u32, flags: u32) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_FACCESSAT2,
                dirfd as u64,
                path as u64,
                mode as u64,
                flags as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn access(path: *const u8, mode: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_ACCESS, path as u64, mode as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn link(old: *const u8, new: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_LINK, old as u64, new as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn linkat(
        olddirfd: i32,
        old: *const u8,
        newdirfd: i32,
        new: *const u8,
        flags: u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_LINKAT,
                olddirfd as u64,
                old as u64,
                newdirfd as u64,
                new as u64,
                flags as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn utimensat(
        dirfd: i32,
        path: *const u8,
        times: *const [Timespec; 2],
        flags: u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_UTIMENSAT,
                dirfd as u64,
                path as u64,
                times as u64,
                flags as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn getdents64(fd: i32, buf: *mut u8, buf_len: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_GETDENTS64, fd as u64, buf as u64, buf_len as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn pread64(fd: i32, buf: *mut u8, count: usize, offset: i64) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_PREAD64,
                fd as u64,
                buf as u64,
                count as u64,
                offset as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn pwrite64(fd: i32, buf: *const u8, count: usize, offset: i64) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_PWRITE64,
                fd as u64,
                buf as u64,
                count as u64,
                offset as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn readv(fd: i32, iov: *const UserIovec, iovcnt: i32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_READV, fd as u64, iov as u64, iovcnt as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn writev(fd: i32, iov: *const UserIovec, iovcnt: i32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_WRITEV, fd as u64, iov as u64, iovcnt as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn fchmod(fd: i32, mode: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FCHMOD, fd as u64, mode as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn flock(fd: i32, operation: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FLOCK, fd as u64, operation as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn brk(addr: *mut u8) -> Result<*mut u8, Errno> {
        let ret = unsafe { syscall1(SYSCALL_BRK, addr as u64) };
        let val = to_result(ret)?;
        Ok(val as *mut u8)
    }

    fn mmap(
        addr: *mut u8,
        len: usize,
        prot: u64,
        flags: u64,
        fd: i32,
        offset: u64,
    ) -> Result<*mut u8, Errno> {
        let ret = unsafe {
            syscall6(
                SYSCALL_MMAP,
                addr as u64,
                len as u64,
                prot,
                flags,
                fd as u64,
                offset,
            )
        };
        let val = to_result(ret)?;
        Ok(val as *mut u8)
    }

    fn munmap(addr: *mut u8, len: usize) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_MUNMAP, addr as u64, len as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn mprotect(addr: *mut u8, len: usize, prot: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_MPROTECT, addr as u64, len as u64, prot) };
        to_result(ret)?;
        Ok(())
    }

    fn msync(addr: *mut u8, len: usize, flags: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_MSYNC, addr as u64, len as u64, flags) };
        to_result(ret)?;
        Ok(())
    }

    fn fork() -> Result<i32, Errno> {
        let ret = unsafe { syscall0(SYSCALL_FORK) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn exec(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_EXECVE, path as u64, argv as u64, envp as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn waitpid(pid: i32, status: *mut i32, options: i32) -> Result<i32, Errno> {
        Self::wait4(pid, status, options, core::ptr::null_mut())
    }

    fn wait4(pid: i32, status: *mut i32, options: i32, rusage: *mut u8) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_WAIT4,
                pid as u64,
                status as u64,
                options as u64,
                rusage as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn exit(code: i32) -> ! {
        unsafe {
            syscall1(SYSCALL_EXIT, code as u64);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn getpid() -> i32 {
        unsafe { syscall0(SYSCALL_GETPID) as i32 }
    }

    fn getppid() -> i32 {
        unsafe { syscall0(SYSCALL_GETPPID) as i32 }
    }

    fn getuid() -> u32 {
        unsafe { syscall0(SYSCALL_GETUID) as u32 }
    }

    fn getgid() -> u32 {
        unsafe { syscall0(SYSCALL_GETGID) as u32 }
    }

    fn geteuid() -> u32 {
        unsafe { syscall0(SYSCALL_GETEUID) as u32 }
    }

    fn getegid() -> u32 {
        unsafe { syscall0(SYSCALL_GETEGID) as u32 }
    }

    fn setpgid(pid: i32, pgid: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_SETPGID, pid as u64, pgid as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getpgid(pid: i32) -> Result<i32, Errno> {
        let ret = unsafe { syscall1(SYSCALL_GETPGID, pid as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn setsid() -> Result<i32, Errno> {
        let ret = unsafe { syscall0(SYSCALL_SETSID) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn chdir(path: *const u8) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_CHDIR, path as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getcwd(buf: *mut u8, size: usize) -> Result<usize, Errno> {
        let ret = unsafe { syscall2(SYSCALL_GETCWD, buf as u64, size as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn clone(
        flags: u64,
        stack: *mut u8,
        parent_tid: *mut i32,
        child_tid: *mut i32,
        tls: u64,
    ) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_CLONE,
                flags,
                stack as u64,
                parent_tid as u64,
                child_tid as u64,
                tls,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn futex_wait(addr: *const u32, val: u32, timeout: *const Timespec) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_FUTEX,
                addr as u64,
                FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                val as u64,
                timeout as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn futex_wake(addr: *const u32, count: u32) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_FUTEX,
                addr as u64,
                FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
                count as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    /// The CPUs this task may run on, which is what a thread-count heuristic
    /// wants: an affinity-restricted task cannot use the ones it is fenced off
    /// from.
    fn get_cpu_count() -> Result<u32, Errno> {
        let mut mask = [0u8; CPU_MASK_BYTES];
        let ret = unsafe {
            syscall3(
                SYSCALL_SCHED_GETAFFINITY,
                0,
                mask.len() as u64,
                mask.as_mut_ptr() as u64,
            )
        };
        let written = (to_result(ret)? as usize).min(mask.len());
        Ok(mask[..written].iter().map(|b| b.count_ones()).sum())
    }

    fn get_current_cpu() -> Result<u32, Errno> {
        let mut cpu = 0u32;
        let ret = unsafe { syscall3(SYSCALL_GETCPU, (&mut cpu as *mut u32) as u64, 0, 0) };
        to_result(ret)?;
        Ok(cpu)
    }

    fn set_cpu_affinity(target: u32, affinity: u32) -> Result<(), Errno> {
        let mask = (affinity as u64).to_le_bytes();
        let ret = unsafe {
            syscall3(
                SYSCALL_SCHED_SETAFFINITY,
                target as u64,
                mask.len() as u64,
                mask.as_ptr() as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn sched_getaffinity(pid: i32, len: usize, mask: *mut u8) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_SCHED_GETAFFINITY,
                pid as u64,
                len as u64,
                mask as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn arch_prctl_set_fs(base: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_ARCH_PRCTL, ARCH_SET_FS, base) };
        to_result(ret)?;
        Ok(())
    }

    fn arch_prctl_get_fs() -> Result<u64, Errno> {
        let mut base = 0u64;
        let ret = unsafe {
            syscall2(
                SYSCALL_ARCH_PRCTL,
                ARCH_GET_FS,
                (&mut base as *mut u64) as u64,
            )
        };
        to_result(ret)?;
        Ok(base)
    }

    fn rt_sigaction(
        sig: i32,
        act: *const u8,
        oldact: *mut u8,
        sigsetsize: usize,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_RT_SIGACTION,
                sig as u64,
                act as u64,
                oldact as u64,
                sigsetsize as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn rt_sigprocmask(
        how: i32,
        set: *const u64,
        oldset: *mut u64,
        sigsetsize: usize,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_RT_SIGPROCMASK,
                how as u64,
                set as u64,
                oldset as u64,
                sigsetsize as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn kill(pid: i32, sig: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_KILL, pid as u64, sig as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn rt_sigreturn() -> ! {
        unsafe {
            syscall0(SYSCALL_RT_SIGRETURN);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn sigaltstack(new: *const UserSigAltStack, old: *mut UserSigAltStack) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_SIGALTSTACK, new as u64, old as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn socket(domain: i32, sock_type: i32, protocol: i32) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_SOCKET,
                domain as u64,
                sock_type as u64,
                protocol as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn bind(fd: i32, addr: *const u8, addrlen: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_BIND, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn listen(fd: i32, backlog: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_LISTEN, fd as u64, backlog as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn accept(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<i32, Errno> {
        let ret = unsafe { syscall3(SYSCALL_ACCEPT, fd as u64, addr as u64, addrlen as u64) };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn connect(fd: i32, addr: *const u8, addrlen: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_CONNECT, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    /// `send(2)` is `sendto` with no destination.
    fn send(fd: i32, buf: *const u8, len: usize, flags: i32) -> Result<usize, Errno> {
        Self::sendto(fd, buf, len, flags, core::ptr::null(), 0)
    }

    /// `recv(2)` is `recvfrom` declining the source address.
    fn recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> Result<usize, Errno> {
        Self::recvfrom(
            fd,
            buf,
            len,
            flags,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    }

    fn sendto(
        fd: i32,
        buf: *const u8,
        len: usize,
        flags: i32,
        addr: *const u8,
        addrlen: u32,
    ) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall6(
                SYSCALL_SENDTO,
                fd as u64,
                buf as u64,
                len as u64,
                flags as u64,
                addr as u64,
                addrlen as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn recvfrom(
        fd: i32,
        buf: *mut u8,
        len: usize,
        flags: i32,
        addr: *mut u8,
        addrlen: *mut u32,
    ) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall6(
                SYSCALL_RECVFROM,
                fd as u64,
                buf as u64,
                len as u64,
                flags as u64,
                addr as u64,
                addrlen as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn setsockopt(
        fd: i32,
        level: i32,
        optname: i32,
        optval: *const u8,
        optlen: u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_SETSOCKOPT,
                fd as u64,
                level as u64,
                optname as u64,
                optval as u64,
                optlen as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn getsockopt(
        fd: i32,
        level: i32,
        optname: i32,
        optval: *mut u8,
        optlen: *mut u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_GETSOCKOPT,
                fd as u64,
                level as u64,
                optname as u64,
                optval as u64,
                optlen as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn shutdown(fd: i32, how: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_SHUTDOWN, fd as u64, how as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getpeername(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_GETPEERNAME, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getsockname(fd: i32, addr: *mut u8, addrlen: *mut u32) -> Result<(), Errno> {
        let ret = unsafe { syscall3(SYSCALL_GETSOCKNAME, fd as u64, addr as u64, addrlen as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn resolve(hostname: *const u8, hostname_len: usize, result: *mut u8) -> Result<(), Errno> {
        let ret = unsafe {
            syscall3(
                SYSCALL_RESOLVE,
                hostname as u64,
                hostname_len as u64,
                result as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn net_query(what: u32, ifindex: u32, buf: *mut u8, buf_len: usize) -> Result<usize, Errno> {
        let ret = unsafe {
            syscall4(
                SYSCALL_NET_QUERY,
                what as u64,
                ifindex as u64,
                buf as u64,
                buf_len as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn clock_gettime(clk_id: u64, tp: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_CLOCK_GETTIME, clk_id, tp as u64) };
        to_result(ret)?;
        Ok(())
    }

    /// Monotonic milliseconds since boot. 0 stands in for the read failing,
    /// which a valid `Timespec` and a supported clock cannot provoke.
    fn get_time_ms() -> u64 {
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ret = unsafe {
            syscall2(
                SYSCALL_CLOCK_GETTIME,
                CLOCK_MONOTONIC,
                (&mut ts as *mut Timespec) as u64,
            )
        };
        if crate::demux(ret).is_err() {
            return 0;
        }
        (ts.tv_sec as u64) * 1_000 + (ts.tv_nsec as u64) / 1_000_000
    }

    /// Sleeps the whole interval: an interrupted sleep resumes on the
    /// remainder the kernel reports, so a caught signal does not shorten it.
    fn sleep_ms(ms: u64) {
        let mut req = Timespec {
            tv_sec: (ms / 1_000) as i64,
            tv_nsec: ((ms % 1_000) * 1_000_000) as i64,
        };
        loop {
            let mut rem = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let ret = unsafe {
                syscall2(
                    SYSCALL_NANOSLEEP,
                    (&req as *const Timespec) as u64,
                    (&mut rem as *mut Timespec) as u64,
                )
            };
            match crate::demux(ret) {
                Err(e) if Errno::from(e) == crate::errno::EINTR => {
                    if rem.tv_sec <= 0 && rem.tv_nsec <= 0 {
                        return;
                    }
                    req = rem;
                }
                _ => return,
            }
        }
    }

    fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_NANOSLEEP, req as u64, rem as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn clock_settime(clk_id: u64, tp: *const Timespec) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_CLOCK_SETTIME, clk_id, tp as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn uname(out: *mut UserUtsname) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_UNAME, out as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn getrandom(buf: *mut u8, len: usize, flags: u32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_GETRANDOM, buf as u64, len as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn gettid() -> i32 {
        unsafe { syscall0(SYSCALL_GETTID) as i32 }
    }

    fn exit_group(code: i32) -> ! {
        unsafe {
            syscall1(SYSCALL_EXIT_GROUP, code as u64);
        }
        loop {
            core::hint::spin_loop();
        }
    }

    fn spawn_path(
        path: *const u8,
        path_len: usize,
        argv: *const *const u8,
        argc: u32,
        attrs: *const SpawnAttrs,
    ) -> Result<i32, Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_SPAWN_PATH,
                path as u64,
                path_len as u64,
                argv as u64,
                argc as u64,
                attrs as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn yield_now() {
        unsafe {
            syscall0(SYSCALL_SCHED_YIELD);
        }
    }

    fn halt() -> ! {
        power_off_or_spin(LINUX_REBOOT_CMD_POWER_OFF)
    }

    fn reboot() -> ! {
        power_off_or_spin(LINUX_REBOOT_CMD_RESTART)
    }

    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_SENDMSG, fd as u64, msg as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> Result<usize, Errno> {
        let ret = unsafe { syscall3(SYSCALL_RECVMSG, fd as u64, msg as u64, flags as u64) };
        let val = to_result(ret)?;
        Ok(val as usize)
    }

    /// The name is only validated, never reported: there is no `/proc` for it
    /// to appear in.
    fn memfd_create(flags: u32) -> Result<i32, Errno> {
        if flags & !MFD_CLOEXEC != 0 {
            crate::errno::errno_set(crate::errno::EINVAL.raw());
            return Err(crate::errno::EINVAL);
        }
        let ret = unsafe {
            syscall2(
                SYSCALL_MEMFD_CREATE,
                MEMFD_NAME.as_ptr() as u64,
                flags as u64,
            )
        };
        let val = to_result(ret)?;
        Ok(val as i32)
    }

    fn ftruncate(fd: i32, size: u64) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FTRUNCATE, fd as u64, size) };
        to_result(ret)?;
        Ok(())
    }

    fn fsync(fd: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_FSYNC, fd as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn fdatasync(fd: i32) -> Result<(), Errno> {
        let ret = unsafe { syscall1(SYSCALL_FDATASYNC, fd as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn sync() -> Result<(), Errno> {
        let ret = unsafe { syscall0(SYSCALL_SYNC) };
        to_result(ret)?;
        Ok(())
    }

    fn statfs(path: *const u8, buf: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_STATFS, path as u64, buf as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn fstatfs(fd: i32, buf: *mut u8) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_FSTATFS, fd as u64, buf as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn mount(
        source: *const u8,
        target: *const u8,
        fstype: *const u8,
        flags: u32,
    ) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_MOUNT,
                source as u64,
                target as u64,
                fstype as u64,
                flags as u64,
                0,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn umount2(target: *const u8, flags: u32) -> Result<(), Errno> {
        let ret = unsafe { syscall2(SYSCALL_UMOUNT2, target as u64, flags as u64) };
        to_result(ret)?;
        Ok(())
    }

    fn test_report(status: u32, name: &[u8], msg: &[u8]) -> Result<(), Errno> {
        let ret = unsafe {
            syscall5(
                SYSCALL_TEST_REPORT,
                status as u64,
                name.as_ptr() as u64,
                name.len() as u64,
                msg.as_ptr() as u64,
                msg.len() as u64,
            )
        };
        to_result(ret)?;
        Ok(())
    }

    fn run_userland_tests() -> Result<(), Errno> {
        let ret = unsafe { syscall0(SYSCALL_RUN_USERLAND_TESTS) };
        to_result(ret)?;
        Ok(())
    }
}
