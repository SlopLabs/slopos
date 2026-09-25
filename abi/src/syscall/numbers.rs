//! Syscall number definitions (kernel-userland ABI).
//!
//! **Numbers are Linux x86-64's.** A call that carries a Linux number carries
//! that call's Linux signature and semantics; where SlopOS cannot yet answer
//! one, it answers `ENOSYS` at that number rather than putting a different
//! operation there. Anything with no Linux analogue lives in the private range
//! at [`SYSCALL_PRIVATE_BASE`] — the discipline ARM takes with
//! `__ARM_NR_BASE`, at a base clear of both the allocated Linux space and the
//! x32 bit-30 marker.
//!
//! Arguments arrive in `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` (`arg0`..`arg5`),
//! the number in `rax`; unless noted, the return is a non-negative result or a
//! negated errno. `rcx` and `r11` are clobbered by the `syscall` instruction.
//! That is the Linux x86-64 convention exactly.
//!
//! `scripts/check_syscall_abi.sh` holds every constant below the private base
//! to Linux's own `syscall_64.tbl`, so a number here cannot drift from the name
//! it claims.

/// Highest number Linux has allocated for x86-64, and therefore the top of
/// the Linux-numbered dispatch table. A call Linux adds below this lands
/// without a table resize; anything above it is out of range.
pub const SYSCALL_LINUX_MAX: u64 = 472;

/// Size of the Linux-numbered dispatch table.
pub const SYSCALL_TABLE_SIZE: usize = (SYSCALL_LINUX_MAX as usize) + 1;

/// First number of the SlopOS-private range.
///
/// 1024 is clear of every number Linux has allocated (472 after three
/// decades) and of `__X32_SYSCALL_BIT` (bit 30), so a private number can
/// never be mistaken for either.
pub const SYSCALL_PRIVATE_BASE: u64 = 1024;

/// Size of the private dispatch table.
pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 48;

/// One past the last private number.
pub const SYSCALL_PRIVATE_END: u64 = SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);

/// `read(fd, buf: *mut u8, len) -> bytes read`.
pub const SYSCALL_READ: u64 = 0;

/// `write(fd, buf: *const u8, len) -> bytes written`.
pub const SYSCALL_WRITE: u64 = 1;

/// `open(path: *const u8, flags, mode) -> fd` — `openat(AT_FDCWD, ...)`.
pub const SYSCALL_OPEN: u64 = 2;

/// `close(fd)`.
pub const SYSCALL_CLOSE: u64 = 3;

/// `stat(path: *const u8, out: *mut UserFsStat)` — follows a final symlink.
pub const SYSCALL_STAT: u64 = 4;

/// `fstat(fd, out: *mut UserFsStat)`.
pub const SYSCALL_FSTAT: u64 = 5;

/// `lstat(path, out)` — `fstatat` with `AT_SYMLINK_NOFOLLOW`.
pub const SYSCALL_LSTAT: u64 = 6;

/// `poll(fds: *mut UserPollFd, nfds, timeout_ms)`. `nfds` is capped at
/// `SELECT_MAX_FDS`.
pub const SYSCALL_POLL: u64 = 7;

/// `lseek(fd, offset: i64, whence) -> new offset`.
pub const SYSCALL_LSEEK: u64 = 8;

/// `mmap(addr, len, prot, flags, fd, offset) -> mapping address`.
///
/// Anonymous (`fd == -1`), memfd and regular-file mappings; every mapping is
/// lazy and the fault installs one page.
pub const SYSCALL_MMAP: u64 = 9;

/// `mprotect(addr, len, prot)` — `addr` must be page-aligned. Splits a VMA
/// when the range is a sub-range.
pub const SYSCALL_MPROTECT: u64 = 10;

/// `munmap(addr, len)` — `addr` must be page-aligned.
pub const SYSCALL_MUNMAP: u64 = 11;

/// `brk(new_break) -> resulting break`. A lazy extent; the fault installs pages.
pub const SYSCALL_BRK: u64 = 12;

/// `rt_sigaction(signum, new: *const UserSigaction, old: *mut UserSigaction,
/// sigsetsize)` — `signum` is `1..=NSIG`, `sigsetsize` must be 8, and either
/// pointer may be 0.
pub const SYSCALL_RT_SIGACTION: u64 = 13;

/// `rt_sigprocmask(how, new: *const SigSet, old: *mut SigSet, sigsetsize)` —
/// `SIG_BLOCK` 0, `SIG_UNBLOCK` 1, `SIG_SETMASK` 2; `sigsetsize` must be 8.
pub const SYSCALL_RT_SIGPROCMASK: u64 = 14;

/// `rt_sigreturn()` — no register arguments; the signal frame is on the user
/// stack. Does not return to the caller.
pub const SYSCALL_RT_SIGRETURN: u64 = 15;

/// `ioctl(fd, request, arg)`. Only TTY descriptors carry requests; anything
/// else is `ENOTTY`.
pub const SYSCALL_IOCTL: u64 = 16;

/// `pread64(fd, buf: *mut u8, len, offset)` — leaves the descriptor's own
/// position alone.
pub const SYSCALL_PREAD64: u64 = 17;

/// `pwrite64(fd, buf: *const u8, len, offset)`.
pub const SYSCALL_PWRITE64: u64 = 18;

/// `readv(fd, iov: *const UserIovec, iovcnt)` — at most `UIO_MAXIOV` segments.
pub const SYSCALL_READV: u64 = 19;

/// `writev(fd, iov: *const UserIovec, iovcnt)`.
pub const SYSCALL_WRITEV: u64 = 20;

/// `access(path: *const u8, mode)`.
pub const SYSCALL_ACCESS: u64 = 21;

/// `pipe(fds: *mut [i32; 2])`.
pub const SYSCALL_PIPE: u64 = 22;

/// `select(nfds, readfds, writefds, exceptfds, timeout: *mut UserTimeval)`.
/// The timeout is updated on return with the time left, as POSIX requires.
pub const SYSCALL_SELECT: u64 = 23;

/// `sched_yield()` — give up the rest of this slice.
pub const SYSCALL_SCHED_YIELD: u64 = 24;

/// `msync(addr, length, flags)` — write a shared file mapping's dirty pages
/// back through the filesystem. `MS_SYNC` waits for the device, `MS_ASYNC`
/// queues the writeback, `MS_INVALIDATE` is refused with `EINVAL`.
pub const SYSCALL_MSYNC: u64 = 26;

/// `dup(fd) -> lowest free fd`.
pub const SYSCALL_DUP: u64 = 32;

/// `dup2(oldfd, newfd)`.
pub const SYSCALL_DUP2: u64 = 33;

/// `nanosleep(req: *const Timespec, rem: *mut Timespec)`. `rem` may be 0 and is
/// written with the time left on `EINTR`.
pub const SYSCALL_NANOSLEEP: u64 = 35;

/// `getpid()` — the caller's thread-group id.
pub const SYSCALL_GETPID: u64 = 39;

/// `socket(domain, type, protocol) -> fd`. `protocol` 0 auto-selects.
pub const SYSCALL_SOCKET: u64 = 41;

/// `connect(fd, addr: *const SockAddrIn, addrlen)`.
pub const SYSCALL_CONNECT: u64 = 42;

/// `accept(fd, peer: *mut SockAddrIn, addrlen: *mut u32) -> fd`. `peer` and
/// `addrlen` may be 0; `addrlen` is read for the buffer's size and written with
/// the address's real length.
pub const SYSCALL_ACCEPT: u64 = 43;

/// `sendto(fd, buf: *const u8, len, flags, addr: *const SockAddrIn, addrlen)
/// -> bytes sent`. A null `addr` is `send(2)`.
pub const SYSCALL_SENDTO: u64 = 44;

/// `recvfrom(fd, buf: *mut u8, len, flags, src: *mut SockAddrIn,
/// srclen: *mut u32) -> bytes received`; 0 means the peer closed.
pub const SYSCALL_RECVFROM: u64 = 45;

/// `sendmsg(fd, msg: *const MsgHdr, flags) -> bytes sent`, optionally
/// carrying `SCM_RIGHTS` ancillary data.
pub const SYSCALL_SENDMSG: u64 = 46;

/// `recvmsg(fd, msg: *mut MsgHdr, flags) -> bytes received`, optionally
/// carrying `SCM_RIGHTS` ancillary data; 0 means the peer closed.
pub const SYSCALL_RECVMSG: u64 = 47;

/// `shutdown(fd, how)` — `SHUT_RD` 0, `SHUT_WR` 1, `SHUT_RDWR` 2.
pub const SYSCALL_SHUTDOWN: u64 = 48;

/// `bind(fd, addr: *const SockAddrIn, addrlen)`.
pub const SYSCALL_BIND: u64 = 49;

/// `listen(fd, backlog)` — honoured for `AF_INET`, ignored for `AF_UNIX`.
pub const SYSCALL_LISTEN: u64 = 50;

/// `getsockname(fd, addr: *mut u8, addrlen: *mut u32)`.
pub const SYSCALL_GETSOCKNAME: u64 = 51;

/// `getpeername(fd, addr: *mut u8, addrlen: *mut u32)`.
pub const SYSCALL_GETPEERNAME: u64 = 52;

/// `setsockopt(fd, level, optname, optval: *const u8, optlen)`.
pub const SYSCALL_SETSOCKOPT: u64 = 54;

/// `getsockopt(fd, level, optname, optval: *mut u8, optlen: *mut u32)` —
/// `optlen` is updated on return.
pub const SYSCALL_GETSOCKOPT: u64 = 55;

/// `clone(flags, child_stack, parent_tid: *mut, child_tid: *mut, tls) -> child
/// task id in the parent, 0 in the child`. A `child_stack` of 0 shares the
/// parent's, i.e. fork-like; `tls` is the new FS_BASE under `CLONE_SETTLS`.
pub const SYSCALL_CLONE: u64 = 56;

/// `fork() -> child task id in the parent, 0 in the child`. Copy-on-write.
pub const SYSCALL_FORK: u64 = 57;

/// `execve(path: *const u8, argv: *const *const u8, envp: *const *const u8)`,
/// replacing the current process image. `argv`/`envp` are NUL-terminated arrays
/// of NUL-terminated strings, or 0 for none. Does not return on success.
pub const SYSCALL_EXECVE: u64 = 59;

/// `exit(code)` — terminate the calling task only.
pub const SYSCALL_EXIT: u64 = 60;

/// `wait4(pid, status: *mut i32, options, rusage: *mut Rusage) -> reaped pid`.
///
/// `status` is written `(code << 8) | sig`, and a non-null `rusage` the child's
/// usage as [`crate::syscall::posix::Rusage`] describes it. Process-group waits
/// (`pid == 0`, `pid < -1`) are `ESRCH`.
pub const SYSCALL_WAIT4: u64 = 61;

/// `kill(pid, signum)` — `pid > 0` fans out over the thread group, 0 targets the
/// caller's process group, -1 every permitted task, `< -1` that process group.
/// `signum` 0 only probes.
pub const SYSCALL_KILL: u64 = 62;

/// `uname(out: *mut UserUtsname)` — kernel identification, Linux's six
/// 65-byte fields including `domainname`.
pub const SYSCALL_UNAME: u64 = 63;

/// `fcntl(fd, cmd, arg)`.
pub const SYSCALL_FCNTL: u64 = 72;

/// `flock(fd, operation)` — whole-file advisory lock on the open file
/// description, so it survives `dup` and `fork`. `LOCK_NB` for the
/// non-blocking form.
pub const SYSCALL_FLOCK: u64 = 73;

/// `fsync(fd)` — commits the fd's inode: its data blocks and its on-disk
/// record. `EINVAL` on a descriptor with no backing store.
///
/// The directory entry naming the file is not covered, per POSIX; committing
/// that needs an `fsync` on a descriptor for the parent directory.
pub const SYSCALL_FSYNC: u64 = 74;

/// `fdatasync(fd)` — as [`SYSCALL_FSYNC`], omitting metadata a later read does
/// not need. ext2 keeps an inode's size, block pointers and timestamps in one
/// 128-byte record, so today the two commit the same blocks.
pub const SYSCALL_FDATASYNC: u64 = 75;

/// `truncate(path: *const u8, length)` — set a regular file's size, freeing
/// blocks past it or extending sparsely.
pub const SYSCALL_TRUNCATE: u64 = 76;

/// `ftruncate(fd, length)` — memfd or regular file.
pub const SYSCALL_FTRUNCATE: u64 = 77;

/// `getcwd(buf: *mut u8, size) -> length written, including the NUL`.
pub const SYSCALL_GETCWD: u64 = 79;

/// `chdir(path)` — resolves against the old cwd and stores the walked path.
pub const SYSCALL_CHDIR: u64 = 80;

/// `rename(old: *const u8, new: *const u8)` — atomic, same-device only
/// (`EXDEV`).
pub const SYSCALL_RENAME: u64 = 82;

/// `mkdir(path: *const u8, mode)`.
pub const SYSCALL_MKDIR: u64 = 83;

/// `rmdir(path)` — remove an empty directory. `ENOTDIR` on a non-directory,
/// `ENOTEMPTY` on one with entries, `EBUSY` on a mount point.
pub const SYSCALL_RMDIR: u64 = 84;

/// `link(old: *const u8, new: *const u8)`. `EPERM` for a directory, `EXDEV`
/// across mounts.
pub const SYSCALL_LINK: u64 = 86;

/// `unlink(path: *const u8)`.
pub const SYSCALL_UNLINK: u64 = 87;

/// `symlink(target: *const u8, link_path: *const u8)`. `target` is stored
/// verbatim and is not resolved; a dangling one is legal.
pub const SYSCALL_SYMLINK: u64 = 88;

/// `readlink(path: *const u8, buf: *mut u8, len) -> bytes written`. Never
/// NUL-terminates, per POSIX; a target longer than `len` is truncated.
pub const SYSCALL_READLINK: u64 = 89;

/// `chmod(path: *const u8, mode)` — permission bits only; the type nibble of
/// `st_mode` is not a caller's to change.
pub const SYSCALL_CHMOD: u64 = 90;

/// `fchmod(fd, mode)` — permission bits of an open descriptor.
pub const SYSCALL_FCHMOD: u64 = 91;

/// `getuid()` — always 0; SlopOS is single-user.
pub const SYSCALL_GETUID: u64 = 102;

/// `getgid()` — always 0.
pub const SYSCALL_GETGID: u64 = 104;

/// `geteuid()` — always 0.
pub const SYSCALL_GETEUID: u64 = 107;

/// `getegid()` — always 0.
pub const SYSCALL_GETEGID: u64 = 108;

/// `setpgid(pid, pgid)`.
pub const SYSCALL_SETPGID: u64 = 109;

/// `getppid()`.
pub const SYSCALL_GETPPID: u64 = 110;

/// `setsid()`.
pub const SYSCALL_SETSID: u64 = 112;

/// `getpgid(pid)`.
pub const SYSCALL_GETPGID: u64 = 121;

/// `getsid(pid)`.
pub const SYSCALL_GETSID: u64 = 124;

/// `sigaltstack(new: *const UserSigAltStack, old: *mut UserSigAltStack)` —
/// nominate a stack for `SA_ONSTACK` handlers.
pub const SYSCALL_SIGALTSTACK: u64 = 131;

/// `statfs(path: *const u8, out: *mut UserStatfs)` — filesystem-wide counters
/// for the mount the path resolves through. `EOPNOTSUPP` from a filesystem
/// that keeps none.
pub const SYSCALL_STATFS: u64 = 137;

/// `fstatfs(fd, out: *mut UserStatfs)` — as [`SYSCALL_STATFS`] for the mount an
/// open descriptor lives on.
pub const SYSCALL_FSTATFS: u64 = 138;

/// `vhangup()` — revoke the caller's controlling terminal; every other fd
/// referencing that TTY then fails with `EIO`. `-EPERM` without a ctty.
pub const SYSCALL_VHANGUP: u64 = 153;

/// `arch_prctl(code, addr)` — `ARCH_SET_FS` takes the new FS_BASE value,
/// `ARCH_GET_FS` a `*mut u64` output.
pub const SYSCALL_ARCH_PRCTL: u64 = 158;

/// `sync()` — drive every mount's writeback to completion, in bounded chunks.
pub const SYSCALL_SYNC: u64 = 162;

/// `mount(source: *const u8, target: *const u8, fstype: *const u8, flags,
/// data: *const u8)` — attach a filesystem at a path. Gated on
/// `Capability::Mount`. The mountable set is closed and none of its members
/// takes options, so a non-null `data` is `EINVAL`.
pub const SYSCALL_MOUNT: u64 = 165;

/// `umount2(path: *const u8, flags)` — detach the filesystem mounted exactly at
/// `path`. `EBUSY` when a descriptor is still open on it, unless
/// `MNT_DETACH` is set. Gated on `Capability::Mount`.
pub const SYSCALL_UMOUNT2: u64 = 166;

/// `reboot(magic1, magic2, cmd, arg)` — `LINUX_REBOOT_MAGIC1`/`MAGIC2` must
/// match and `cmd` selects the action: `LINUX_REBOOT_CMD_RESTART`,
/// `_POWER_OFF` or `_HALT`. Gated on `Capability::Power`.
pub const SYSCALL_REBOOT: u64 = 169;

/// `gettid()` — the caller's own task id; `getpid` answers the thread-group
/// id.
pub const SYSCALL_GETTID: u64 = 186;

/// `futex(uaddr: *mut u32, op, val, timeout: *const Timespec, uaddr2, val3)`.
///
/// `op & FUTEX_CMD_MASK` selects `WAIT`, `WAKE`, `REQUEUE`, `CMP_REQUEUE`,
/// `WAIT_BITSET` or `WAKE_BITSET`; `FUTEX_PRIVATE_FLAG` is accepted. A null
/// `timeout` blocks forever; the wait's internal resolution is a millisecond.
/// For the requeue forms `timeout` is reinterpreted as `val2`.
pub const SYSCALL_FUTEX: u64 = 202;

/// `sched_setaffinity(pid, cpusetsize, mask: *const u8)` — `pid` 0 is the
/// caller.
pub const SYSCALL_SCHED_SETAFFINITY: u64 = 203;

/// `sched_getaffinity(pid, cpusetsize, mask: *mut u8) -> bytes written`.
pub const SYSCALL_SCHED_GETAFFINITY: u64 = 204;

/// `getdents64(fd, buf: *mut u8, len)` — packed
/// [`UserDirent64`](crate::fs::UserDirent64) records; the cursor lives in the
/// descriptor.
pub const SYSCALL_GETDENTS64: u64 = 217;

/// `clock_settime(clock_id, ts: *const Timespec)` — `CLOCK_REALTIME` only,
/// `EINVAL` otherwise. Gated on `Capability::Clock`.
pub const SYSCALL_CLOCK_SETTIME: u64 = 227;

/// `clock_gettime(clock_id, out: *mut Timespec)` — `CLOCK_REALTIME`,
/// `CLOCK_MONOTONIC`, `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`.
pub const SYSCALL_CLOCK_GETTIME: u64 = 228;

/// `exit_group(code)` — terminate every task in the caller's thread group.
pub const SYSCALL_EXIT_GROUP: u64 = 231;

/// `openat(dirfd, path: *const u8, flags, mode)`. `AT_FDCWD` selects the
/// caller's cwd.
pub const SYSCALL_OPENAT: u64 = 257;

/// `mkdirat(dirfd, path: *const u8, mode)`.
pub const SYSCALL_MKDIRAT: u64 = 258;

/// `newfstatat(dirfd, path: *const u8, out: *mut UserFsStat, flags)`. The only
/// stat entry point that can decline a final symlink, so `AT_SYMLINK_NOFOLLOW`
/// is what makes `lstat` expressible.
pub const SYSCALL_NEWFSTATAT: u64 = 262;

/// `unlinkat(dirfd, path: *const u8, flags)` — `AT_REMOVEDIR` makes it
/// `rmdir`.
pub const SYSCALL_UNLINKAT: u64 = 263;

/// `renameat(olddirfd, old, newdirfd, new)`.
pub const SYSCALL_RENAMEAT: u64 = 264;

/// `linkat(olddirfd, old, newdirfd, new, flags)`.
pub const SYSCALL_LINKAT: u64 = 265;

/// `symlinkat(target: *const u8, newdirfd, link: *const u8)`.
pub const SYSCALL_SYMLINKAT: u64 = 266;

/// `readlinkat(dirfd, path: *const u8, buf: *mut u8, len)`.
pub const SYSCALL_READLINKAT: u64 = 267;

/// `fchmodat(dirfd, path: *const u8, mode)` —
/// [`SYSCALL_FCHMODAT2`] with no flags.
pub const SYSCALL_FCHMODAT: u64 = 268;

/// `faccessat(dirfd, path: *const u8, mode)` —
/// [`SYSCALL_FACCESSAT2`] with no flags.
pub const SYSCALL_FACCESSAT: u64 = 269;

/// `utimensat(dirfd, path: *const u8, times: *const [Timespec; 2], flags)`.
/// `UTIME_NOW`/`UTIME_OMIT` in `tv_nsec` select per-field behaviour; a NULL
/// `times` means both now.
pub const SYSCALL_UTIMENSAT: u64 = 280;

/// `signalfd4(fd, mask: *const SigSet, sizemask, flags) -> fd` that becomes
/// `POLLIN`-ready while a signal in `mask` is pending for the calling task, and
/// whose `read` drains one `SignalfdSiginfo`. `fd` must be -1 (this kernel
/// cannot re-arm an existing descriptor) and `sizemask` must be 8. Pair with
/// blocking those signals so they queue as in-band ring/poll events instead of
/// interrupting waits with `EINTR`.
pub const SYSCALL_SIGNALFD4: u64 = 289;

/// `dup3(oldfd, newfd, flags)`.
pub const SYSCALL_DUP3: u64 = 292;

/// `pipe2(fds: *mut [i32; 2], flags)` — `O_CLOEXEC` and `O_NONBLOCK`.
pub const SYSCALL_PIPE2: u64 = 293;

/// `prlimit64(pid, resource, new: *const RLimit64, old: *mut RLimit64)` —
/// `pid` must be 0 or the caller's own id.
///
/// The limits reported are the ones the kernel actually enforces, not
/// `RLIM64_INFINITY` placeholders: a caller that cannot query a real bound
/// cannot back off gracefully.
pub const SYSCALL_PRLIMIT64: u64 = 302;

/// `getcpu(cpu: *mut u32, node: *mut u32, unused)` — either pointer may be 0;
/// `node` always answers 0.
pub const SYSCALL_GETCPU: u64 = 309;

/// `getrandom(buf: *mut u8, len, flags) -> bytes written`, from the CSPRNG.
///
/// `GRND_NONBLOCK` and `GRND_RANDOM` are accepted and change nothing — the
/// pool is always seeded; any other flag is `EINVAL`.
pub const SYSCALL_GETRANDOM: u64 = 318;

/// `memfd_create(name: *const u8, flags) -> fd`. `name` is validated as a
/// NUL-terminated string shorter than 250 bytes and is otherwise unused; there
/// is no `/proc` for it to appear in. `flags` accepts `MFD_CLOEXEC`.
pub const SYSCALL_MEMFD_CREATE: u64 = 319;

/// `pidfd_open(pid, flags) -> fd` that becomes `POLLIN`-ready once the target
/// task exits. Not readable (`read` → `-EINVAL`); reap the status with
/// `wait4`. The target must be a child of the caller and `flags` must be 0.
pub const SYSCALL_PIDFD_OPEN: u64 = 434;

/// `faccessat2(dirfd, path: *const u8, mode, flags)` — existence and the
/// file's own mode bits. `AT_EACCESS` changes nothing: single-user uid 0.
pub const SYSCALL_FACCESSAT2: u64 = 439;

/// `fchmodat2(dirfd, path: *const u8, mode, flags)`.
pub const SYSCALL_FCHMODAT2: u64 = 452;

// ---------------------------------------------------------------------------
// SlopOS-private range. No Linux analogue; see the module header.
// ---------------------------------------------------------------------------

/// `klog_write(buf: *const u8, len)` — write the serialized kernel console.
/// Not `write(2)`: there is no descriptor, and the destination is the log.
/// Linux's nearest analogue is a write to `/dev/kmsg`.
pub const SYSCALL_KLOG_WRITE: u64 = SYSCALL_PRIVATE_BASE + 0;

/// `ctty_read(buf: *mut u8, len)` — cooked read of the caller's controlling
/// terminal with no descriptor. `ENXIO` without a ctty.
pub const SYSCALL_CTTY_READ: u64 = SYSCALL_PRIVATE_BASE + 1;

/// `sys_info(out: *mut UserSysInfo)` — page-allocator, task and scheduler
/// counters. Deliberately not Linux's `sysinfo`, whose struct reports
/// something else.
pub const SYSCALL_SYS_INFO: u64 = SYSCALL_PRIVATE_BASE + 2;

/// `process_list(out: *mut UserTaskEntry, max) -> entries written`.
pub const SYSCALL_PROCESS_LIST: u64 = SYSCALL_PRIVATE_BASE + 3;

/// `cpu_info(out: *mut UserCpuInfo)`.
pub const SYSCALL_CPU_INFO: u64 = SYSCALL_PRIVATE_BASE + 4;

/// `percpu_stats(out: *mut UserPerCpuStats, max) -> entries written`.
pub const SYSCALL_PERCPU_STATS: u64 = SYSCALL_PRIVATE_BASE + 5;

/// `spawn_path(path: *const u8, path_len, argv: *const *const u8, argc,
/// attrs: *const SpawnAttrs) -> task id`, or a negated errno.
///
/// The child begins with an empty fd table; the `SpawnAttrs` action list
/// installs exactly the descriptors it inherits (`posix_spawn` file-actions).
/// Linux leaves this to libc over `clone3`/`execveat`; here it is one call so
/// nothing allocates between fork and exec.
pub const SYSCALL_SPAWN_PATH: u64 = SYSCALL_PRIVATE_BASE + 6;

/// `sigdefault(mask: SigSet)` — force every signal in the mask to `SIG_DFL`,
/// overriding a caught handler or `SIG_IGN`, in one call.
pub const SYSCALL_SIGDEFAULT: u64 = SYSCALL_PRIVATE_BASE + 7;

/// `resolve(host: *const u8, host_len, out: *mut [u8; 4])` via the in-kernel
/// DNS client. `host` is not NUL-terminated and must be at most 253 bytes.
pub const SYSCALL_RESOLVE: u64 = SYSCALL_PRIVATE_BASE + 8;

/// `net_query(what, ifindex, buf, len) -> bytes written`, as a
/// [`UserNetQueryHdr`](crate::net::UserNetQueryHdr) followed by `record_count`
/// fixed-stride records. Truncation is read from the header
/// (`total_count > record_count`), not the return value, so a header-sized
/// buffer is the sizing query and anything smaller is `EINVAL`. Unprivileged,
/// but `NET_Q_SOCKETS` names `owner_pid` only for the caller's own sockets
/// unless it holds `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_QUERY: u64 = SYSCALL_PRIVATE_BASE + 9;

/// `net_iface_ctl(ifindex, op, arg)` — admin up/down, MTU, DHCP lifecycle,
/// neighbour and address flushes, plus the global operations addressed to
/// `NET_IFINDEX_GLOBAL`. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_IFACE_CTL: u64 = SYSCALL_PRIVATE_BASE + 10;

/// `net_addr_ctl(op, ptr, len)`, where `op` is `NET_ADDROP_ADD`/`_DEL` and
/// `ptr` points at exactly one [`UserAddrReq`](crate::net::UserAddrReq) whose
/// size `len` must equal. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_ADDR_CTL: u64 = SYSCALL_PRIVATE_BASE + 11;

/// `net_route_ctl(op, ptr, len)`, where `op` is `NET_ROUTEOP_ADD`/`_DEL` and
/// `ptr` points at exactly one [`UserRouteReq`](crate::net::UserRouteReq) whose
/// size `len` must equal. Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_ROUTE_CTL: u64 = SYSCALL_PRIVATE_BASE + 12;

/// `net_resolver_set(ptr, len)` — exactly one
/// [`UserResolverReq`](crate::net::UserResolverReq) whose size `len` must
/// equal. Clearing the static override is a request naming zero servers.
/// Requires `TASK_FLAG_NET_ADMIN`.
pub const SYSCALL_NET_RESOLVER_SET: u64 = SYSCALL_PRIVATE_BASE + 13;

/// `net_monitor(mask, flags) -> fd` that becomes `POLLIN`-ready whenever the
/// stack's configuration changes and whose `read` drains whole
/// [`NetEvent`](crate::net::NetEvent) records. A dropped record is reported in
/// band, as a `NET_EV_OVERFLOW` ordered before the records that followed the
/// drop, so a reader never loses its position. Unprivileged.
pub const SYSCALL_NET_MONITOR: u64 = SYSCALL_PRIVATE_BASE + 14;

/// `fb_info(out: *mut DisplayInfo)`.
pub const SYSCALL_FB_INFO: u64 = SYSCALL_PRIVATE_BASE + 15;

/// `fb_flip(seat_fd, damage: *const DamageRect, count)` — present the acquired
/// seat's back buffer.
pub const SYSCALL_FB_FLIP: u64 = SYSCALL_PRIVATE_BASE + 16;

/// `cursor_set_image(image: *const u8, len, hotspot)` — a 64x64 BGRA image,
/// `hotspot` packing `(hot_x << 16) | hot_y`. Compositor-only.
pub const SYSCALL_CURSOR_SET_IMAGE: u64 = SYSCALL_PRIVATE_BASE + 17;

/// `cursor_move(pos)` in absolute display coords, `pos` packing
/// `(x << 16) | y`. Compositor-only.
pub const SYSCALL_CURSOR_MOVE: u64 = SYSCALL_PRIVATE_BASE + 18;

/// `set_display_mode(width: u32, height: u32)`. Compositor-only.
pub const SYSCALL_SET_DISPLAY_MODE: u64 = SYSCALL_PRIVATE_BASE + 19;

/// `screen_acquire(seat_id)` — take the framebuffer seat, returning a
/// non-duplicable descriptor naming it.
///
/// `seat_id` is `slopos_ostd::seat::SeatId`: 0 compositor-primary, 1 virtcon.
/// `EBUSY` when a seat of equal or higher rank is held. Ownership is announced
/// here and never conferred by presenting a frame.
pub const SYSCALL_SCREEN_ACQUIRE: u64 = SYSCALL_PRIVATE_BASE + 20;

/// `input_sink_acquire(seat_id)` — as [`SYSCALL_SCREEN_ACQUIRE`], for the raw
/// input event stream that `input_poll_batch` drains.
pub const SYSCALL_INPUT_SINK_ACQUIRE: u64 = SYSCALL_PRIVATE_BASE + 21;

/// `input_poll_batch(out: *mut u8, max) -> events written`.
pub const SYSCALL_INPUT_POLL_BATCH: u64 = SYSCALL_PRIVATE_BASE + 22;

/// `clipboard_copy(buf: *const u8, len)`.
pub const SYSCALL_CLIPBOARD_COPY: u64 = SYSCALL_PRIVATE_BASE + 23;

/// `clipboard_paste(buf: *mut u8, len) -> bytes written`.
pub const SYSCALL_CLIPBOARD_PASTE: u64 = SYSCALL_PRIVATE_BASE + 24;

/// `font_set(data: *const u8, width, height, glyph_count, format)`.
///
/// Bitmap format (0): 1bpp MSB-first, one byte per row per glyph,
/// `glyph_count x height` bytes; width must be 8. Coverage format (1): 8-bit
/// alpha, one `width x height` cell per `slopos_font::GLYPH_RANGES` slot then
/// one replacement cell; `glyph_count` must equal `slopos_font::GLYPH_COUNT`.
pub const SYSCALL_FONT_SET: u64 = SYSCALL_PRIVATE_BASE + 25;

/// `keymap_load(data: *const u8, len)` — a serialised `LayoutTable` blob (see
/// `slopos_abi::input::layout`), `EINVAL` if malformed. Unprivileged: the
/// kernel-side binary validator is the safety boundary, and the kernel never
/// parses layout text.
pub const SYSCALL_KEYMAP_LOAD: u64 = SYSCALL_PRIVATE_BASE + 26;

/// `keymap_get_name(buf: *mut u8, buf_len) -> bytes written` of the active
/// layout's short name. Unprivileged.
pub const SYSCALL_KEYMAP_GET_NAME: u64 = SYSCALL_PRIVATE_BASE + 27;

/// SlopRing: create a submission/completion ring (SLOPRING SS 6.1).
/// `ring_setup(entries: u32, params: *mut RingParams) -> ring fd`. Maps the
/// shared ring region into the caller and writes [`crate::ring::RingParams`].
///
/// Private rather than io_uring's 425: the SQE, the params and the register
/// ops are SlopOS's own shapes, and a Linux number would promise otherwise.
pub const SYSCALL_RING_SETUP: u64 = SYSCALL_PRIVATE_BASE + 28;

/// SlopRing: submit and/or harvest ring completions (SLOPRING SS 6.2).
/// `ring_enter(ring_fd, to_submit, min_complete, flags) -> submissions`.
/// With `min_complete > 0` the calling task blocks on the in-flight resource
/// queues until that many CQEs are ready, a signal arrives, or the deadline
/// elapses.
pub const SYSCALL_RING_ENTER: u64 = SYSCALL_PRIVATE_BASE + 29;

/// SlopRing: register provided/fixed buffers with a ring (SLOPRING SS 13, ABI
/// v2). `ring_register(ring_fd, op, arg: u64, nr_args)`; the implemented ops
/// sit behind [`super::super::ring::SLOPRING_FEAT_REG_BUFFERS`] and an unknown
/// one is `-ENOSYS`.
pub const SYSCALL_RING_REGISTER: u64 = SYSCALL_PRIVATE_BASE + 30;

/// `roulette()` — spin the Wheel of Fate.
pub const SYSCALL_ROULETTE: u64 = SYSCALL_PRIVATE_BASE + 31;

/// `roulette_result(packed)` — report a spin's outcome.
pub const SYSCALL_ROULETTE_RESULT: u64 = SYSCALL_PRIVATE_BASE + 32;

/// `roulette_draw(fate: u32)` — draw the wheel for a fate.
pub const SYSCALL_ROULETTE_DRAW: u64 = SYSCALL_PRIVATE_BASE + 33;

/// `test_report(status, name: *const u8, name_len, msg: *const u8, msg_len)` —
/// one userland subtest result; `status` is 0 Pass / 1 Fail / 2 Skip. Name and
/// message are UTF-8 without NUL, truncated at `TEST_REPORT_NAME_MAX` /
/// `TEST_REPORT_MSG_MAX`.
pub const SYSCALL_TEST_REPORT: u64 = SYSCALL_PRIVATE_BASE + 34;

/// Drive the kernel-side userland-test phase: spawn each `TestKind::Userland`
/// entry of the `.test_registry`, drain its `SYSCALL_TEST_REPORT` ring, emit
/// KTAP, and merge with the kernel-phase summary.
///
/// Caller must be a real kernel-scheduled task (`/sbin/init` is the canonical
/// one) — `task_wait_for` requires a non-null `current_task`.
pub const SYSCALL_RUN_USERLAND_TESTS: u64 = SYSCALL_PRIVATE_BASE + 35;

/// Deliberately panic in syscall context to exercise the task-scoped
/// panic-recovery boundary. `ENOSYS` unless the `panic.recover_smoke` boot flag
/// is set, so production images expose no panic trigger; when armed the call
/// does not return.
pub const SYSCALL_TEST_PANIC: u64 = SYSCALL_PRIVATE_BASE + 36;

/// `getrandom` flags. Both are accepted and change nothing: the pool is
/// seeded before userland runs.
pub const GRND_NONBLOCK: u32 = 0x0001;
pub const GRND_RANDOM: u32 = 0x0002;

/// `memfd_create` flags.
pub const MFD_CLOEXEC: u32 = 0x0001;

/// `reboot` magics and commands, as Linux defines them. Linux accepts any of
/// the four second magics, so a caller written against any of them works.
pub const LINUX_REBOOT_MAGIC1: u64 = 0xfee1_dead;
pub const LINUX_REBOOT_MAGIC2: u64 = 672_274_793;
pub const LINUX_REBOOT_MAGIC2A: u64 = 85_072_278;
pub const LINUX_REBOOT_MAGIC2B: u64 = 369_367_448;
pub const LINUX_REBOOT_MAGIC2C: u64 = 537_993_216;
pub const LINUX_REBOOT_CMD_RESTART: u64 = 0x0123_4567;
pub const LINUX_REBOOT_CMD_HALT: u64 = 0xcdef_0123;
pub const LINUX_REBOOT_CMD_POWER_OFF: u64 = 0x4321_fedc;
pub const LINUX_REBOOT_CMD_CAD_ON: u64 = 0x89ab_cdef;
pub const LINUX_REBOOT_CMD_CAD_OFF: u64 = 0;

/// Whether `magic2` is one of the four values Linux's `reboot(2)` accepts.
pub const fn linux_reboot_magic2(magic2: u64) -> bool {
    matches!(
        magic2,
        LINUX_REBOOT_MAGIC2 | LINUX_REBOOT_MAGIC2A | LINUX_REBOOT_MAGIC2B | LINUX_REBOOT_MAGIC2C
    )
}

pub const FONT_FORMAT_BITMAP: u64 = 0;
pub const FONT_FORMAT_COVERAGE: u64 = 1;

pub const RING_REGISTER_PBUF_RING: u32 = 1;
pub const RING_REGISTER_BUFFERS: u32 = 2;
pub const RING_UNREGISTER_PBUF_RING: u32 = 3;
pub const RING_UNREGISTER_BUFFERS: u32 = 4;

/// Standard return value for unimplemented syscalls: -ENOSYS (negated errno 38).
pub const ENOSYS_RETURN: u64 = (-38i64) as u64;

const _: () = assert!(SYSCALL_PRIVATE_BASE > SYSCALL_LINUX_MAX);
const _: () = assert!(SYSCALL_PRIVATE_BASE & 0x4000_0000 == 0);

const _: () = assert!(
    SYSCALL_TEST_PANIC < SYSCALL_PRIVATE_END,
    "private syscall range is full; raise SYSCALL_PRIVATE_TABLE_SIZE",
);
