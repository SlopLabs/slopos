//! The object layouts slibc exports to C, and the const asserts that pin them.
//!
//! Every shape here is the Linux x86-64 one the target's `libc` module
//! declares. Where the kernel's own struct differs — the 32-byte
//! `UserSigaction`, its 8-byte `SigSet`, the 24-byte `UserDirent64` header —
//! slibc translates at the syscall boundary and the kernel struct is left
//! alone. The divergence is therefore private to this file and to the
//! translators in [`crate::signal`] and [`crate::io::dir`].
//!
//! Types whose kernel definition *already* is the Linux one are re-exported
//! rather than restated: a second declaration is a second thing to drift.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_long, c_short, c_uint, c_ulong, c_void};

pub use slopos_abi::fs::UserFsStat as stat;
pub use slopos_abi::fs::UserIovec as iovec;
pub use slopos_abi::fs::UserStatfs as statfs;
pub use slopos_abi::quota::RLimit64 as rlimit;
pub use slopos_abi::signal::UserSigAltStack as stack_t;
pub use slopos_abi::syscall::{CmsgHdr as cmsghdr, MsgHdr as msghdr};
pub use slopos_abi::syscall::{Timespec as timespec, UserUtsname as utsname};

/// `struct stat64` is `struct stat` on a 64-bit target; the two names exist so
/// a `_LARGEFILE64_SOURCE` consumer links.
pub use slopos_abi::fs::UserFsStat as stat64;

pub type mode_t = c_uint;
pub type off_t = i64;
pub type nfds_t = c_ulong;
pub type socklen_t = u32;
pub type dev_t = u64;
pub type ino_t = u64;
pub type uid_t = u32;
pub type gid_t = u32;
pub type pid_t = i32;
pub type clockid_t = c_int;
pub type time_t = i64;
pub type rlim_t = u64;
pub type sighandler_t = usize;

/// `sigset_t` is 128 bytes — glibc's `_SIGSET_NWORDS`, and what the target's
/// `libc` declares — while the kernel's mask is a single `u64`. Only signals
/// `1..=31` exist (`NSIG` is 32), so every bit slibc can hand the kernel lives
/// in word 0; [`sigset_high_bits_set`] is what refuses the rest rather than
/// silently dropping it.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigset_t {
    pub __val: [u64; 16],
}

impl sigset_t {
    pub const fn empty() -> Self {
        Self { __val: [0; 16] }
    }

    /// Word 0, which is the whole mask the kernel understands.
    #[inline]
    pub const fn kernel_mask(&self) -> u64 {
        self.__val[0] & KERNEL_SIGSET_MASK
    }

    /// True when the caller set a bit above signal 31. Such a bit names a
    /// realtime signal, which this kernel does not have.
    #[inline]
    pub fn has_unsupported_bits(&self) -> bool {
        if self.__val[0] & !KERNEL_SIGSET_MASK != 0 {
            return true;
        }
        self.__val[1..].iter().any(|&w| w != 0)
    }

    #[inline]
    pub const fn from_kernel_mask(mask: u64) -> Self {
        let mut set = Self::empty();
        set.__val[0] = mask & KERNEL_SIGSET_MASK;
        set
    }
}

impl Default for sigset_t {
    fn default() -> Self {
        Self::empty()
    }
}

/// Bits the kernel's `SigSet` can carry: signal N is bit N-1, for
/// `1 <= N <= NSIG`. Everything above is a realtime signal, which this kernel
/// has not got.
pub const KERNEL_SIGSET_MASK: u64 = slopos_abi::signal::SIGNAL_MASK;

/// Highest signal number that exists. The kernel's `parse_signum` accepts
/// `1..=NSIG` inclusive, which is one more than Linux's reading of the same
/// name — a stated divergence, and the reason the range lives in one constant.
pub const NSIG: c_int = slopos_abi::signal::NSIG as c_int;

/// The userspace `struct sigaction`: handler at 0, the 128-byte mask at 8,
/// `sa_flags` at 136, `sa_restorer` at 144. The kernel's `UserSigaction` is a
/// different 32-byte shape; [`crate::signal::sigaction`] converts.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigaction {
    pub sa_sigaction: sighandler_t,
    pub sa_mask: sigset_t,
    pub sa_flags: c_int,
    pub sa_restorer: Option<extern "C" fn()>,
}

impl sigaction {
    pub const fn zeroed() -> Self {
        Self {
            sa_sigaction: 0,
            sa_mask: sigset_t::empty(),
            sa_flags: 0,
            sa_restorer: None,
        }
    }
}

impl Default for sigaction {
    fn default() -> Self {
        Self::zeroed()
    }
}

/// `struct dirent`, with `d_name` at 19. The kernel's `UserDirent64` header is
/// tail-padded to 24, so [`crate::io::dir`] re-packs every record; that offset
/// is not visible through `readdir`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct dirent {
    pub d_ino: ino_t,
    pub d_off: off_t,
    pub d_reclen: u16,
    pub d_type: u8,
    pub d_name: [c_char; 256],
}

impl dirent {
    pub const fn zeroed() -> Self {
        Self {
            d_ino: 0,
            d_off: 0,
            d_reclen: 0,
            d_type: 0,
            d_name: [0; 256],
        }
    }
}

/// `struct dirent64` is `struct dirent` here, as it is on any target whose
/// `ino_t` and `off_t` are already 64-bit.
pub type dirent64 = dirent;

/// Byte offset of `d_name` within the libc `struct dirent`. Deliberately
/// spelled as a constant so the pin below reads as an assertion about the
/// number rather than about the compiler.
pub const DIRENT_D_NAME_OFFSET: usize = 19;

/// `struct statvfs`. Derived from the kernel's `statfs` by
/// [`crate::io::stat_vfs_from_statfs`]: the two carry the same facts under
/// different names.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct statvfs {
    pub f_bsize: c_ulong,
    pub f_frsize: c_ulong,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_favail: u64,
    pub f_fsid: c_ulong,
    pub f_flag: c_ulong,
    pub f_namemax: c_ulong,
    pub __f_spare: [c_int; 6],
}

/// `struct timeval`.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

/// `struct rusage`. Linux x86-64's 144-byte shape. This kernel accounts none
/// of it, so [`crate::process::getrusage`] fails rather than answering zeros.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct rusage {
    pub ru_utime: timeval,
    pub ru_stime: timeval,
    pub ru_maxrss: c_long,
    pub ru_ixrss: c_long,
    pub ru_idrss: c_long,
    pub ru_isrss: c_long,
    pub ru_minflt: c_long,
    pub ru_majflt: c_long,
    pub ru_nswap: c_long,
    pub ru_inblock: c_long,
    pub ru_oublock: c_long,
    pub ru_msgsnd: c_long,
    pub ru_msgrcv: c_long,
    pub ru_nsignals: c_long,
    pub ru_nvcsw: c_long,
    pub ru_nivcsw: c_long,
}

/// `struct passwd`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct passwd {
    pub pw_name: *mut c_char,
    pub pw_passwd: *mut c_char,
    pub pw_uid: uid_t,
    pub pw_gid: gid_t,
    pub pw_gecos: *mut c_char,
    pub pw_dir: *mut c_char,
    pub pw_shell: *mut c_char,
}

/// `DIR` is opaque to C. The definition lives in [`crate::io::dir`].
pub enum DIR {}

/// A `*mut c_void` that is not a mapping. `mmap` answers this on failure.
pub const MAP_FAILED: *mut c_void = usize::MAX as *mut c_void;

// ---------------------------------------------------------------------------
// Layout pins.
//
// Each of these is a shape two independent declarations have to agree on: the
// kernel's `abi` crate, or the target's `libc` module, or both. A mismatch
// under this target is a miscompile rather than a link error, so it is caught
// here instead.
// ---------------------------------------------------------------------------

// `struct stat`, straight from the kernel: no translation, so the pin is on
// the shared definition.
const _: () = assert!(size_of::<stat>() == 144);
const _: () = assert!(align_of::<stat>() == 8);
const _: () = assert!(core::mem::offset_of!(stat, st_mode) == 24);
const _: () = assert!(core::mem::offset_of!(stat, st_size) == 48);
const _: () = assert!(core::mem::offset_of!(stat, st_atim) == 72);
const _: () = assert!(core::mem::offset_of!(stat, st_mtim) == 88);
const _: () = assert!(core::mem::offset_of!(stat, st_ctim) == 104);

// `struct dirent` — 19, against the kernel record's 24.
const _: () = assert!(size_of::<dirent>() == 280);
const _: () = assert!(align_of::<dirent>() == 8);
const _: () = assert!(core::mem::offset_of!(dirent, d_off) == 8);
const _: () = assert!(core::mem::offset_of!(dirent, d_reclen) == 16);
const _: () = assert!(core::mem::offset_of!(dirent, d_type) == 18);
const _: () = assert!(core::mem::offset_of!(dirent, d_name) == DIRENT_D_NAME_OFFSET);
const _: () = assert!(DIRENT_D_NAME_OFFSET == 19);
// The kernel's header is five bytes longer. That difference is the whole
// reason `crate::io::dir` re-packs rather than memcpys.
const _: () = assert!(size_of::<slopos_abi::fs::UserDirent64>() == 24);
const _: () = assert!(size_of::<slopos_abi::fs::UserDirent64>() > DIRENT_D_NAME_OFFSET);

// `struct iovec`.
const _: () = assert!(size_of::<iovec>() == 16);
const _: () = assert!(align_of::<iovec>() == 8);
const _: () = assert!(core::mem::offset_of!(iovec, iov_len) == 8);

// `struct sigaction` — the userspace shape, not the kernel's.
const _: () = assert!(size_of::<sigaction>() == 152);
const _: () = assert!(align_of::<sigaction>() == 8);
const _: () = assert!(core::mem::offset_of!(sigaction, sa_sigaction) == 0);
const _: () = assert!(core::mem::offset_of!(sigaction, sa_mask) == 8);
const _: () = assert!(core::mem::offset_of!(sigaction, sa_flags) == 136);
const _: () = assert!(core::mem::offset_of!(sigaction, sa_restorer) == 144);
// The kernel's, which `crate::signal` narrows to.
const _: () = assert!(size_of::<slopos_abi::signal::UserSigaction>() == 32);

// `sigset_t` — 128 userspace bytes over an 8-byte kernel mask.
const _: () = assert!(size_of::<sigset_t>() == 128);
const _: () = assert!(align_of::<sigset_t>() == 8);
const _: () = assert!(size_of::<slopos_abi::signal::SigSet>() == 8);
const _: () = assert!(NSIG == 32);
// Signal 31 is the last one that fits; bit 31 is the last set bit of the mask.
const _: () = assert!(KERNEL_SIGSET_MASK == (1u64 << 32) - 1);

// `stack_t`.
const _: () = assert!(size_of::<stack_t>() == 24);
const _: () = assert!(core::mem::offset_of!(stack_t, ss_flags) == 8);
const _: () = assert!(core::mem::offset_of!(stack_t, ss_size) == 16);

// `struct msghdr` / `struct cmsghdr`, both from `abi`.
const _: () = assert!(size_of::<msghdr>() == 56);
const _: () = assert!(align_of::<msghdr>() == 8);
const _: () = assert!(core::mem::offset_of!(msghdr, msg_iov) == 16);
const _: () = assert!(core::mem::offset_of!(msghdr, msg_control) == 32);
const _: () = assert!(core::mem::offset_of!(msghdr, msg_flags) == 48);
const _: () = assert!(size_of::<cmsghdr>() == 16);
const _: () = assert!(align_of::<cmsghdr>() == 8);
const _: () = assert!(slopos_abi::syscall::CMSG_DATA_OFFSET == 16);

// `struct utsname`.
const _: () = assert!(size_of::<utsname>() == 390);
const _: () = assert!(core::mem::offset_of!(utsname, nodename) == 65);

// `struct statvfs` and the `struct statfs` it is derived from.
const _: () = assert!(size_of::<statvfs>() == 112);
const _: () = assert!(align_of::<statvfs>() == 8);
const _: () = assert!(core::mem::offset_of!(statvfs, f_blocks) == 16);
const _: () = assert!(core::mem::offset_of!(statvfs, f_fsid) == 64);
const _: () = assert!(core::mem::offset_of!(statvfs, __f_spare) == 88);
const _: () = assert!(size_of::<statfs>() == 120);
const _: () = assert!(core::mem::offset_of!(statfs, f_namelen) == 64);

// `struct termios` is the one shape that is *not* the libc-declared one, and
// that is deliberate: the plan keeps `NCCS` divergent (19 here, 32 on Linux)
// because no std path reads it, and `ioctl(TCGETS)` therefore fills 44 bytes
// rather than 60. The pin exists so the divergence stays the stated one — if
// the kernel's `NCCS` ever moves, this fails instead of silently short-filling
// a caller's buffer.
const _: () = assert!(slopos_abi::syscall::NCCS == 19);
const _: () = assert!(size_of::<slopos_abi::syscall::UserTermios>() == 44);
const _: () = assert!(align_of::<slopos_abi::syscall::UserTermios>() == 4);

// `struct rlimit` / `struct rusage` / `struct passwd` / `struct timeval`.
const _: () = assert!(size_of::<rlimit>() == 16);
const _: () = assert!(size_of::<rusage>() == 144);
const _: () = assert!(align_of::<rusage>() == 8);
// 48, not 56: `pw_uid` and `pw_gid` are 4 bytes each and sit between two
// pointers, so they share one 8-byte slot with no padding.
const _: () = assert!(size_of::<passwd>() == 48);
const _: () = assert!(core::mem::offset_of!(passwd, pw_gecos) == 24);
const _: () = assert!(size_of::<timeval>() == 16);

// The pthread objects, whose sizes the target's `libc` fixes as opaque byte
// arrays. slibc picks the fields *inside* those sizes, so the sizes are the
// only thing both sides can disagree about.
const _: () = assert!(size_of::<crate::thread::pthread_attr_t>() == 56);
const _: () = assert!(align_of::<crate::thread::pthread_attr_t>() == 8);
const _: () = assert!(size_of::<crate::thread::pthread_mutex_t>() == 40);
const _: () = assert!(align_of::<crate::thread::pthread_mutex_t>() == 8);
const _: () = assert!(size_of::<crate::thread::mutex::pthread_mutexattr_t>() == 4);
const _: () = assert!(size_of::<crate::thread::pthread_cond_t>() == 48);
const _: () = assert!(align_of::<crate::thread::pthread_cond_t>() == 8);
const _: () = assert!(size_of::<crate::thread::condvar::pthread_condattr_t>() == 4);
const _: () = assert!(size_of::<crate::thread::pthread_rwlock_t>() == 56);
const _: () = assert!(align_of::<crate::thread::pthread_rwlock_t>() == 8);
const _: () = assert!(size_of::<crate::thread::rwlock::pthread_rwlockattr_t>() == 8);
const _: () = assert!(size_of::<crate::thread::pthread_t>() == 8);
const _: () = assert!(size_of::<crate::thread::keys::pthread_key_t>() == 4);

// `struct pollfd` and `fd_set`, which `poll`/`select` pass through untouched.
const _: () = assert!(size_of::<crate::io::poll::Pollfd>() == 8);
const _: () = assert!(size_of::<crate::io::poll::FdSet>() == 128);

// `struct timespec`, shared with the kernel.
const _: () = assert!(size_of::<timespec>() == 16);

const _: () = assert!(size_of::<mode_t>() == 4);
const _: () = assert!(size_of::<nfds_t>() == 8);
const _: () = assert!(size_of::<c_short>() == 2);
