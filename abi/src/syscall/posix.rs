/// Realtime clock — the wall clock anchored at boot from the RTC, adjustable
/// through `clock_settime`.
pub const CLOCK_REALTIME: u64 = 0;

/// Monotonic clock — nanoseconds since boot, never adjusted.
pub const CLOCK_MONOTONIC: u64 = 1;

/// CPU time consumed by the caller's whole thread group.
pub const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;

/// CPU time consumed by the calling task alone.
pub const CLOCK_THREAD_CPUTIME_ID: u64 = 3;

/// Socket option level: generic socket options.
pub const SOL_SOCKET: i32 = 1;
/// Socket option level: TCP protocol options.
pub const IPPROTO_TCP: i32 = 6;

/// Allow local address reuse.
pub const SO_REUSEADDR: i32 = 2;
/// Retrieve and clear pending socket error.
pub const SO_ERROR: i32 = 4;
/// Send buffer size in bytes.
pub const SO_SNDBUF: i32 = 7;
/// Receive buffer size in bytes.
pub const SO_RCVBUF: i32 = 8;
/// Enable keepalive probes.
pub const SO_KEEPALIVE: i32 = 9;
/// Receive timeout (value: [`Timeval`]).
pub const SO_RCVTIMEO: i32 = 20;
/// Send timeout (value: [`Timeval`]).
pub const SO_SNDTIMEO: i32 = 21;

/// POSIX `struct timeval` — the wire format for `SO_RCVTIMEO` / `SO_SNDTIMEO`.
/// Every layer (kernel, slibc, std) must use this layout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl Timeval {
    pub const fn from_millis(ms: u64) -> Self {
        Self {
            tv_sec: (ms / 1000) as i64,
            tv_usec: ((ms % 1000) * 1000) as i64,
        }
    }

    pub const fn as_millis(&self) -> u64 {
        (self.tv_sec as u64) * 1000 + (self.tv_usec as u64) / 1000
    }

    /// Zero value — represents "no timeout".
    pub const ZERO: Self = Self {
        tv_sec: 0,
        tv_usec: 0,
    };

    /// Interpret a byte slice as a `Timeval`.  Returns `None` if too short.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < core::mem::size_of::<Self>() {
            return None;
        }
        Some(Self {
            tv_sec: i64::from_ne_bytes(b[..8].try_into().ok()?),
            tv_usec: i64::from_ne_bytes(b[8..16].try_into().ok()?),
        })
    }

    /// Write self into a byte slice.  Returns `false` if too short.
    pub fn to_bytes(&self, b: &mut [u8]) -> bool {
        if b.len() < core::mem::size_of::<Self>() {
            return false;
        }
        b[..8].copy_from_slice(&self.tv_sec.to_ne_bytes());
        b[8..16].copy_from_slice(&self.tv_usec.to_ne_bytes());
        true
    }
}

/// Linux x86-64's `struct rusage`, as `wait4` reports a child's.
///
/// `ru_utime` holds all of the child's CPU time, since the kernel does not
/// split user from system time, including threads that exited before it;
/// `ru_maxrss` is its resident peak up to then, in KiB. Neither includes the
/// child's reaped children, as Linux's do, and every other field reads zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rusage {
    pub ru_utime: Timeval,
    pub ru_stime: Timeval,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
}

const _: () = assert!(core::mem::size_of::<Rusage>() == 144);

/// Disable Nagle's algorithm (TCP only).
pub const TCP_NODELAY: i32 = 1;

/// Disallow further receives.
pub const SHUT_RD: i32 = 0;
/// Disallow further sends.
pub const SHUT_WR: i32 = 1;
/// Disallow further sends and receives.
pub const SHUT_RDWR: i32 = 2;

pub const PROT_NONE: u64 = 0;
pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;

pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_FIXED: u64 = 0x10;
/// The caller takes the fault-time refusal instead of a commit reservation.
pub const MAP_NORESERVE: u64 = 0x4000;

pub const F_DUPFD: u64 = 0;
/// `F_DUPFD` with `FD_CLOEXEC` set on the new number, atomically.
pub const F_DUPFD_CLOEXEC: u64 = 1030;
pub const F_GETFD: u64 = 1;
pub const F_SETFD: u64 = 2;
pub const F_GETFL: u64 = 3;
pub const F_SETFL: u64 = 4;
/// Record-lock commands. `l_type` takes [`F_RDLCK`]/[`F_WRLCK`]/[`F_UNLCK`].
pub const F_GETLK: u64 = 5;
pub const F_SETLK: u64 = 6;
pub const F_SETLKW: u64 = 7;
pub const FD_CLOEXEC: u64 = 1;

pub const F_RDLCK: i16 = 0;
pub const F_WRLCK: i16 = 1;
pub const F_UNLCK: i16 = 2;

/// `flock(2)` operations.
pub const LOCK_SH: u64 = 1;
pub const LOCK_EX: u64 = 2;
pub const LOCK_NB: u64 = 4;
pub const LOCK_UN: u64 = 8;

pub const O_NONBLOCK: u64 = 0x800;
pub const O_NOCTTY: u64 = 0x100;
pub const O_CLOEXEC: u64 = 0x80_000;

/// Ancillary data type: pass file descriptors.
pub const SCM_RIGHTS: i32 = 1;

/// Maximum number of file descriptors in a single sendmsg ancillary payload.
pub const SCM_MAX_FDS: usize = 4;

/// `msg_flags` out-bit: the control buffer could not hold every ancillary
/// item, so some were discarded. Linux's `MSG_CTRUNC`.
pub const MSG_CTRUNC: i32 = 0x08;

/// `sendmsg`/`recvmsg` message header. Linux x86-64 `struct msghdr`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MsgHdr {
    /// Optional peer address; 0 on a connected socket.
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub _pad0: u32,
    /// User VA of an array of [`UserIovec`](crate::fs::UserIovec) data
    /// segments — at most [`UIO_MAXIOV`](crate::fs::UIO_MAXIOV) of them.
    pub msg_iov: u64,
    pub msg_iovlen: u64,
    /// User VA of the ancillary buffer: a run of [`CmsgHdr`]-headed items.
    pub msg_control: u64,
    /// In: the ancillary buffer's capacity. Out: the bytes of it used.
    pub msg_controllen: u64,
    /// Out on `recvmsg`: [`MSG_CTRUNC`] when ancillary data was discarded.
    pub msg_flags: i32,
    pub _pad1: u32,
}

const _: () = assert!(
    core::mem::size_of::<MsgHdr>() == 56,
    "MsgHdr must match the Linux x86-64 struct msghdr size"
);
const _: () = assert!(core::mem::align_of::<MsgHdr>() == 8);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_name) == 0);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_namelen) == 8);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_iov) == 16);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_iovlen) == 24);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_control) == 32);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_controllen) == 40);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_flags) == 48);

/// One ancillary data item's header. Linux x86-64 `struct cmsghdr`; the
/// payload begins [`CMSG_DATA_OFFSET`] bytes past the header's start.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CmsgHdr {
    /// This item's header plus payload, unpadded — see [`cmsg_len`].
    pub cmsg_len: u64,
    /// Originating protocol ([`SOL_SOCKET`]).
    pub cmsg_level: i32,
    /// Protocol-specific type ([`SCM_RIGHTS`]).
    pub cmsg_type: i32,
}

const _: () = assert!(
    core::mem::size_of::<CmsgHdr>() == 16,
    "CmsgHdr must match the Linux x86-64 struct cmsghdr size"
);
const _: () = assert!(core::mem::align_of::<CmsgHdr>() == 8);
const _: () = assert!(core::mem::offset_of!(CmsgHdr, cmsg_len) == 0);
const _: () = assert!(core::mem::offset_of!(CmsgHdr, cmsg_level) == 8);
const _: () = assert!(core::mem::offset_of!(CmsgHdr, cmsg_type) == 12);

/// `CMSG_DATA`'s offset from its item's header — the header size rounded up
/// to the ancillary alignment, which on x86-64 needs no rounding.
pub const CMSG_DATA_OFFSET: usize = cmsg_align(core::mem::size_of::<CmsgHdr>());

const _: () = assert!(CMSG_DATA_OFFSET == 16);

/// `CMSG_ALIGN`: ancillary items start on a `size_t` boundary.
pub const fn cmsg_align(len: usize) -> usize {
    (len + 7) & !7
}

/// `CMSG_LEN`: the `cmsg_len` an item carrying `payload` bytes declares.
pub const fn cmsg_len(payload: usize) -> usize {
    CMSG_DATA_OFFSET + payload
}

/// `CMSG_SPACE`: the control-buffer bytes an item carrying `payload` bytes
/// occupies, including the padding up to the next item.
pub const fn cmsg_space(payload: usize) -> usize {
    CMSG_DATA_OFFSET + cmsg_align(payload)
}

/// `CMSG_FIRSTHDR`: the offset of the first item in a control buffer of
/// `controllen` bytes, or `None` when it cannot hold a header.
///
/// Offsets rather than pointers: the buffer is user memory the kernel reads a
/// field at a time through its own copy-in, and userland indexes its own
/// buffer the same way.
pub const fn cmsg_firsthdr(controllen: usize) -> Option<usize> {
    if controllen >= core::mem::size_of::<CmsgHdr>() {
        Some(0)
    } else {
        None
    }
}

/// `CMSG_NXTHDR`: the offset of the item after the one at `offset` whose
/// header declared `len`, or `None` once no whole header is left.
pub const fn cmsg_nxthdr(controllen: usize, offset: usize, len: usize) -> Option<usize> {
    if len < core::mem::size_of::<CmsgHdr>() {
        return None;
    }
    let next = match offset.checked_add(cmsg_align(len)) {
        Some(n) => n,
        None => return None,
    };
    match controllen.checked_sub(next) {
        Some(left) if left >= core::mem::size_of::<CmsgHdr>() => Some(next),
        _ => None,
    }
}

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

pub const POLLIN: u16 = 0x0001;
pub const POLLPRI: u16 = 0x0002;
pub const POLLOUT: u16 = 0x0004;
pub const POLLERR: u16 = 0x0008;
pub const POLLHUP: u16 = 0x0010;
pub const POLLNVAL: u16 = 0x0020;

pub const FDSET_WORD_BITS: usize = 64;

// Clone flag values follow the Linux ABI.

/// Child and parent share the same virtual address space.
pub const CLONE_VM: u64 = 0x0000_0100;
/// Child and parent share the same filesystem information (cwd, root).
pub const CLONE_FS: u64 = 0x0000_0200;
/// Child and parent share the same file descriptor table.
pub const CLONE_FILES: u64 = 0x0000_0400;
/// Child and parent share the same signal handler table.
pub const CLONE_SIGHAND: u64 = 0x0000_0800;
/// Write the child's TID into the parent's memory at `parent_tid`.
pub const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
/// Write the child's TID into the child's memory at `child_tid`.
pub const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
/// Clear the child's TID at `child_tid` on exit (for futex-based join).
pub const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
/// Set the TLS (FS_BASE) for the new thread.
pub const CLONE_SETTLS: u64 = 0x0008_0000;
/// New thread shares the parent's thread group (POSIX thread semantics).
pub const CLONE_THREAD: u64 = 0x0001_0000;

pub const CLONE_SUPPORTED_MASK: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_PARENT_SETTID
    | CLONE_CHILD_SETTID
    | CLONE_CHILD_CLEARTID
    | CLONE_SETTLS
    | CLONE_THREAD;

// Linux futex ABI: the command is the low byte of `op`, the options above it.
pub const FUTEX_WAIT: u64 = 0;
pub const FUTEX_WAKE: u64 = 1;
pub const FUTEX_REQUEUE: u64 = 3;
pub const FUTEX_CMP_REQUEUE: u64 = 4;
pub const FUTEX_WAIT_BITSET: u64 = 9;
pub const FUTEX_WAKE_BITSET: u64 = 10;

/// Accepted and ignored: every futex here is keyed on a virtual address, so
/// all of them are already private. What it must not do is make the call
/// `ENOSYS` — glibc- and std-shaped callers always set it.
pub const FUTEX_PRIVATE_FLAG: u64 = 128;
/// Measure an absolute timeout against `CLOCK_REALTIME` instead of
/// `CLOCK_MONOTONIC`. Only legal with [`FUTEX_WAIT_BITSET`].
pub const FUTEX_CLOCK_REALTIME: u64 = 256;
pub const FUTEX_CMD_MASK: u64 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

/// The bitset that makes `FUTEX_WAIT_BITSET` behave as `FUTEX_WAIT`.
pub const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;
/// arch_prctl sub-commands (Linux-compatible values)
pub const ARCH_SET_FS: u64 = 0x1002;
pub const ARCH_GET_FS: u64 = 0x1003;
