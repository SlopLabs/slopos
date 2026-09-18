//! File, metadata, vectored-I/O and memory-mapping entry points, plus the
//! handful of calls that are SlopOS's own and keep a `slopos_` prefix because
//! no C name describes them.
//!
//! Every C entry point here follows the C convention: `-1`/`MAP_FAILED`/`NULL`
//! with `errno` set, never a negated errno as a return value.

use core::ffi::{c_char, c_int, c_uint, c_void};

use crate::errno::{EINVAL, ENOSYS, EPERM, errno_set};
use crate::pal::{Pal, Sys};
use crate::string::u_strlen;
use crate::types::{
    MAP_FAILED, dev_t, gid_t, iovec as Iovec, mode_t, off_t, pid_t, stat as Stat, statfs as Statfs,
    statvfs as Statvfs, timespec as Timespec, uid_t, utsname as Utsname,
};
use slopos_abi::fs::{AT_FDCWD, AT_SYMLINK_NOFOLLOW};
use slopos_abi::spawn::SpawnAttrs;

/// The kernel's `struct stat`, which is already Linux's, so `stat` and friends
/// are pass-throughs. Re-exported under its historical name because in-repo
/// callers and the test module name it.
pub use slopos_abi::fs::UserFsStat as SloposStat;

/// `uid_t`/`gid_t` value meaning "leave this one alone".
const OWNER_UNCHANGED: u32 = u32::MAX;

/// SlopOS is single-user: the only uid and gid that exist are 0. A `chown`
/// naming 0 (or `-1`, "no change") therefore succeeds once the target is known
/// to exist, and any other principal is `EPERM` — there is nothing to change
/// ownership *to*. This is the one place that reasoning is written down; the
/// four `*chown` entry points below all share it.
#[inline]
fn owner_change_permitted(uid: uid_t, gid: gid_t) -> bool {
    (uid == 0 || uid == OWNER_UNCHANGED) && (gid == 0 || gid == OWNER_UNCHANGED)
}

#[inline]
fn fail<T>(e: crate::errno::Errno, sentinel: T) -> T {
    errno_set(e.raw());
    sentinel
}

// ---------------------------------------------------------------------------
// Sequential and positional I/O
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lseek(fd: c_int, offset: off_t, whence: c_int) -> off_t {
    match Sys::lseek(fd, offset, whence) {
        Ok(pos) => pos,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pread(fd: c_int, buf: *mut c_void, count: usize, offset: off_t) -> isize {
    match Sys::pread64(fd, buf as *mut u8, count, offset) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwrite(
    fd: c_int,
    buf: *const c_void,
    count: usize,
    offset: off_t,
) -> isize {
    match Sys::pwrite64(fd, buf as *const u8, count, offset) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readv(fd: c_int, iov: *const Iovec, iovcnt: c_int) -> isize {
    match Sys::readv(fd, iov, iovcnt) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn writev(fd: c_int, iov: *const Iovec, iovcnt: c_int) -> isize {
    match Sys::writev(fd, iov, iovcnt) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

/// No `preadv` syscall exists, so this is a run of `pread`s at ascending
/// offsets — which is what `preadv` is defined to be, and which leaves the
/// file position alone exactly as the single syscall would.
///
/// A short read ends the call, as POSIX permits: the alternative is to keep
/// reading past a hole the caller has not been told about.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn preadv(
    fd: c_int,
    iov: *const Iovec,
    iovcnt: c_int,
    offset: off_t,
) -> isize {
    if iov.is_null() || iovcnt < 0 || iovcnt as usize > slopos_abi::fs::UIO_MAXIOV {
        return fail(EINVAL, -1);
    }
    let mut total = 0usize;
    let mut at = offset;
    for i in 0..iovcnt as usize {
        let seg = *iov.add(i);
        if seg.iov_len == 0 {
            continue;
        }
        let want = seg.iov_len as usize;
        match Sys::pread64(fd, seg.iov_base as *mut u8, want, at) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                at += n as off_t;
                if n < want {
                    break;
                }
            }
            Err(e) => {
                if total > 0 {
                    break;
                }
                return fail(e, -1);
            }
        }
    }
    total as isize
}

/// As [`preadv`], over `pwrite`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pwritev(
    fd: c_int,
    iov: *const Iovec,
    iovcnt: c_int,
    offset: off_t,
) -> isize {
    if iov.is_null() || iovcnt < 0 || iovcnt as usize > slopos_abi::fs::UIO_MAXIOV {
        return fail(EINVAL, -1);
    }
    let mut total = 0usize;
    let mut at = offset;
    for i in 0..iovcnt as usize {
        let seg = *iov.add(i);
        if seg.iov_len == 0 {
            continue;
        }
        let want = seg.iov_len as usize;
        match Sys::pwrite64(fd, seg.iov_base as *const u8, want, at) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                at += n as off_t;
                if n < want {
                    break;
                }
            }
            Err(e) => {
                if total > 0 {
                    break;
                }
                return fail(e, -1);
            }
        }
    }
    total as isize
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fsync(fd: c_int) -> c_int {
    match Sys::fsync(fd) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdatasync(fd: c_int) -> c_int {
    match Sys::fdatasync(fd) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// `sync(2)` returns nothing and cannot fail from the caller's point of view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sync() {
    let _ = Sys::sync();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ftruncate(fd: c_int, length: off_t) -> c_int {
    if length < 0 {
        return fail(EINVAL, -1);
    }
    match Sys::ftruncate(fd, length as u64) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn truncate(path: *const c_char, length: off_t) -> c_int {
    if length < 0 {
        return fail(EINVAL, -1);
    }
    match Sys::truncate(path as *const u8, length as u64) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn flock(fd: c_int, operation: c_int) -> c_int {
    match Sys::flock(fd, operation as u32) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// `openat(2)`, variadic: the mode argument exists only for a call that can
/// create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    path: *const c_char,
    oflag: c_int,
    mut args: ...
) -> c_int {
    let mode = if crate::ffi::oflag_creates(oflag) {
        args.next_arg::<mode_t>()
    } else {
        0
    };
    match Sys::openat(dirfd, path as *const u8, oflag, mode) {
        Ok(fd) => fd,
        Err(e) => fail(e, -1),
    }
}

/// `creat(path, mode)` is `open(path, O_WRONLY|O_CREAT|O_TRUNC, mode)`, which
/// is all it has ever been.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn creat(path: *const c_char, mode: mode_t) -> c_int {
    let flags =
        (slopos_abi::fs::O_WRONLY | slopos_abi::fs::O_CREAT | slopos_abi::fs::O_TRUNC) as c_int;
    match Sys::open(path as *const u8, flags, mode) {
        Ok(fd) => fd,
        Err(e) => fail(e, -1),
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstat(fd: c_int, buf: *mut Stat) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::fstat(fd, buf as *mut u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(path: *const c_char, buf: *mut Stat) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::stat(path as *const u8, buf as *mut u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(path: *const c_char, buf: *mut Stat) -> c_int {
    fstatat(AT_FDCWD, path, buf, AT_SYMLINK_NOFOLLOW as c_int)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut Stat,
    flags: c_int,
) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::fstatat(dirfd, path as *const u8, buf, flags as u32) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn statfs(path: *const c_char, buf: *mut Statfs) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::statfs(path as *const u8, buf as *mut u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatfs(fd: c_int, buf: *mut Statfs) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::fstatfs(fd, buf as *mut u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// `struct statvfs` carries the same facts as `struct statfs` under POSIX's
/// names, so it is derived rather than fetched separately. `f_favail` tracks
/// `f_ffree` and `f_bavail` tracks the kernel's own `f_bavail`: there is no
/// reserved-blocks notion here, so no second number to report.
fn statvfs_from_statfs(src: &Statfs) -> Statvfs {
    Statvfs {
        f_bsize: src.f_bsize,
        f_frsize: if src.f_frsize == 0 {
            src.f_bsize
        } else {
            src.f_frsize
        },
        f_blocks: src.f_blocks,
        f_bfree: src.f_bfree,
        f_bavail: src.f_bavail,
        f_files: src.f_files,
        f_ffree: src.f_ffree,
        f_favail: src.f_ffree,
        f_fsid: src.f_fsid,
        f_flag: src.f_flags,
        f_namemax: src.f_namelen,
        __f_spare: [0; 6],
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn statvfs(path: *const c_char, buf: *mut Statvfs) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    let mut raw = Statfs::default();
    match Sys::statfs(path as *const u8, &raw mut raw as *mut u8) {
        Ok(()) => {
            *buf = statvfs_from_statfs(&raw);
            0
        }
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatvfs(fd: c_int, buf: *mut Statvfs) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    let mut raw = Statfs::default();
    match Sys::fstatfs(fd, &raw mut raw as *mut u8) {
        Ok(()) => {
            *buf = statvfs_from_statfs(&raw);
            0
        }
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn faccessat(
    dirfd: c_int,
    path: *const c_char,
    mode: c_int,
    flags: c_int,
) -> c_int {
    match Sys::faccessat(dirfd, path as *const u8, mode as u32, flags as u32) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchmod(fd: c_int, mode: mode_t) -> c_int {
    match Sys::fchmod(fd, mode) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchmodat(
    dirfd: c_int,
    path: *const c_char,
    mode: mode_t,
    flags: c_int,
) -> c_int {
    match Sys::fchmodat(dirfd, path as *const u8, mode, flags as u32) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// The target must exist before the single-user answer is given: otherwise a
/// `chown` of a missing path would succeed. See [`owner_change_permitted`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chown(path: *const c_char, uid: uid_t, gid: gid_t) -> c_int {
    let mut probe = Stat::default();
    if let Err(e) = Sys::stat(path as *const u8, &raw mut probe as *mut u8) {
        return fail(e, -1);
    }
    if owner_change_permitted(uid, gid) {
        0
    } else {
        fail(EPERM, -1)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lchown(path: *const c_char, uid: uid_t, gid: gid_t) -> c_int {
    let mut probe = Stat::default();
    if let Err(e) = Sys::fstatat(
        AT_FDCWD,
        path as *const u8,
        &raw mut probe,
        AT_SYMLINK_NOFOLLOW,
    ) {
        return fail(e, -1);
    }
    if owner_change_permitted(uid, gid) {
        0
    } else {
        fail(EPERM, -1)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchown(fd: c_int, uid: uid_t, gid: gid_t) -> c_int {
    let mut probe = Stat::default();
    if let Err(e) = Sys::fstat(fd, &raw mut probe as *mut u8) {
        return fail(e, -1);
    }
    if owner_change_permitted(uid, gid) {
        0
    } else {
        fail(EPERM, -1)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchownat(
    dirfd: c_int,
    path: *const c_char,
    uid: uid_t,
    gid: gid_t,
    flags: c_int,
) -> c_int {
    let mut probe = Stat::default();
    if let Err(e) = Sys::fstatat(dirfd, path as *const u8, &raw mut probe, flags as u32) {
        return fail(e, -1);
    }
    if owner_change_permitted(uid, gid) {
        0
    } else {
        fail(EPERM, -1)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn utimensat(
    dirfd: c_int,
    path: *const c_char,
    times: *const Timespec,
    flags: c_int,
) -> c_int {
    match Sys::utimensat(
        dirfd,
        path as *const u8,
        times as *const [Timespec; 2],
        flags as u32,
    ) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// `utimensat` with a NULL path names the descriptor, which is precisely what
/// `futimens` is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn futimens(fd: c_int, times: *const Timespec) -> c_int {
    utimensat(fd, core::ptr::null(), times, 0)
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdir(path: *const c_char, mode: mode_t) -> c_int {
    match Sys::mkdir(path as *const u8, mode) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdirat(dirfd: c_int, path: *const c_char, mode: mode_t) -> c_int {
    match Sys::mkdirat(dirfd, path as *const u8, mode) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    match Sys::rmdir(path as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    match Sys::unlink(path as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlinkat(dirfd: c_int, path: *const c_char, flags: c_int) -> c_int {
    match Sys::unlinkat(dirfd, path as *const u8, flags as u32) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rename(old: *const c_char, new: *const c_char) -> c_int {
    match Sys::rename(old as *const u8, new as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn renameat(
    olddirfd: c_int,
    old: *const c_char,
    newdirfd: c_int,
    new: *const c_char,
) -> c_int {
    match Sys::renameat(olddirfd, old as *const u8, newdirfd, new as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn link(src: *const c_char, dst: *const c_char) -> c_int {
    match Sys::link(src as *const u8, dst as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn linkat(
    srcdirfd: c_int,
    src: *const c_char,
    dstdirfd: c_int,
    dst: *const c_char,
    flags: c_int,
) -> c_int {
    match Sys::linkat(
        srcdirfd,
        src as *const u8,
        dstdirfd,
        dst as *const u8,
        flags as u32,
    ) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn symlink(target: *const c_char, linkpath: *const c_char) -> c_int {
    match Sys::symlink(target as *const u8, linkpath as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn symlinkat(
    target: *const c_char,
    dirfd: c_int,
    linkpath: *const c_char,
) -> c_int {
    match Sys::symlinkat(target as *const u8, dirfd, linkpath as *const u8) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// Answers the byte count and never NUL-terminates, per POSIX.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readlink(path: *const c_char, buf: *mut c_char, bufsiz: usize) -> isize {
    match Sys::readlink(path as *const u8, buf as *mut u8, bufsiz) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn readlinkat(
    dirfd: c_int,
    path: *const c_char,
    buf: *mut c_char,
    bufsiz: usize,
) -> isize {
    match Sys::readlinkat(dirfd, path as *const u8, buf as *mut u8, bufsiz) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

/// Longest path any of these calls handles, NUL included. Linux's `PATH_MAX`.
const PATH_MAX: usize = slopos_abi::fs::USER_PATH_MAX;

/// Symlinks one `realpath` will expand before giving up with `ELOOP`.
const SYMLOOP_MAX: u32 = 40;

/// `realpath(3)`.
///
/// Resolved the only way available without a kernel-side canonicaliser:
/// component by component, `lstat`-ing each one so a missing or non-directory
/// prefix is reported where it happens, and re-reading through every symlink
/// until the count runs out. `resolved` may be null, in which case a
/// `PATH_MAX` buffer is allocated for the caller to `free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
    if path.is_null() {
        return fail(EINVAL, core::ptr::null_mut());
    }
    if *path == 0 {
        return fail(crate::errno::ENOENT, core::ptr::null_mut());
    }

    let mut out = [0u8; PATH_MAX];
    let out_len = match canonicalize(path as *const u8, &mut out) {
        Ok(n) => n,
        Err(e) => return fail(e, core::ptr::null_mut()),
    };

    let dst = if resolved.is_null() {
        let p = crate::mem::malloc::alloc(PATH_MAX) as *mut c_char;
        if p.is_null() {
            return fail(crate::errno::ENOMEM, core::ptr::null_mut());
        }
        p
    } else {
        resolved
    };
    core::ptr::copy_nonoverlapping(out.as_ptr(), dst as *mut u8, out_len);
    *(dst as *mut u8).add(out_len) = 0;
    dst
}

/// Walk `input` into `out`, answering the byte length written (no NUL).
unsafe fn canonicalize(
    input: *const u8,
    out: &mut [u8; PATH_MAX],
) -> Result<usize, crate::errno::Errno> {
    // `pending` holds the components still to resolve; a symlink replaces its
    // head, which is why it has to be a buffer rather than a cursor into the
    // caller's string.
    let mut pending = [0u8; PATH_MAX];
    let mut pending_len;

    let input_len = u_strlen(input);
    if input_len == 0 || input_len >= PATH_MAX {
        return Err(if input_len == 0 {
            crate::errno::ENOENT
        } else {
            crate::errno::ENAMETOOLONG
        });
    }

    let mut resolved_len;
    if *input == b'/' {
        out[0] = b'/';
        resolved_len = 1;
        core::ptr::copy_nonoverlapping(input, pending.as_mut_ptr(), input_len);
        pending_len = input_len;
    } else {
        // Straight into `out`: a relative path's answer starts at the working
        // directory, so there is no reason for a second `PATH_MAX` buffer on
        // a frame that already carries two.
        let n = Sys::getcwd(out.as_mut_ptr(), PATH_MAX)?;
        // `getcwd` answers a NUL-terminated string and counts the NUL.
        let cwd_len = crate::string::u_strnlen(out.as_ptr(), n.min(PATH_MAX));
        if cwd_len == 0 || cwd_len >= PATH_MAX {
            return Err(crate::errno::ENAMETOOLONG);
        }
        resolved_len = cwd_len;
        core::ptr::copy_nonoverlapping(input, pending.as_mut_ptr(), input_len);
        pending_len = input_len;
    }

    let mut cursor = 0usize;
    let mut links = 0u32;

    loop {
        // Skip separators.
        while cursor < pending_len && pending[cursor] == b'/' {
            cursor += 1;
        }
        if cursor >= pending_len {
            break;
        }
        let start = cursor;
        while cursor < pending_len && pending[cursor] != b'/' {
            cursor += 1;
        }
        let comp = &pending[start..cursor];

        if comp == b"." {
            continue;
        }
        if comp == b".." {
            while resolved_len > 1 && out[resolved_len - 1] != b'/' {
                resolved_len -= 1;
            }
            if resolved_len > 1 {
                resolved_len -= 1; // drop the separator itself
            }
            continue;
        }

        // Append "/component" to the resolved prefix.
        let sep = if resolved_len == 1 && out[0] == b'/' {
            0
        } else {
            1
        };
        if resolved_len + sep + comp.len() + 1 > PATH_MAX {
            return Err(crate::errno::ENAMETOOLONG);
        }
        let comp_at = resolved_len + sep;
        if sep == 1 {
            out[resolved_len] = b'/';
        }
        out[comp_at..comp_at + comp.len()].copy_from_slice(comp);
        let candidate_len = comp_at + comp.len();

        // `lstat` needs a NUL, which the trailing byte of `out` provides
        // because the length check above reserved it.
        out[candidate_len] = 0;
        let mut st = Stat::default();
        Sys::fstatat(AT_FDCWD, out.as_ptr(), &raw mut st, AT_SYMLINK_NOFOLLOW)?;

        if !st.is_symlink() {
            resolved_len = candidate_len;
            continue;
        }

        links += 1;
        if links > SYMLOOP_MAX {
            return Err(crate::errno::ELOOP);
        }

        let mut target = [0u8; PATH_MAX];
        let link_len = Sys::readlink(out.as_ptr(), target.as_mut_ptr(), target.len() - 1)?;
        if link_len == 0 || link_len >= PATH_MAX {
            return Err(crate::errno::ENAMETOOLONG);
        }

        // The link's target, then whatever of `pending` is left, become the
        // new work list. An absolute target also resets the resolved prefix.
        let rest = pending_len - cursor;
        let next_len = if rest > 0 {
            link_len + 1 + rest
        } else {
            link_len
        };
        if next_len >= PATH_MAX {
            return Err(crate::errno::ENAMETOOLONG);
        }
        // In place, tail first: `copy_within` copies through `ptr::copy`, so
        // the move is correct whichever direction the tail travels, and it
        // saves a third `PATH_MAX` buffer on this frame.
        if rest > 0 {
            pending.copy_within(cursor..pending_len, link_len + 1);
            pending[link_len] = b'/';
        }
        pending[..link_len].copy_from_slice(&target[..link_len]);
        pending_len = next_len;
        cursor = 0;

        if target[0] == b'/' {
            out[0] = b'/';
            resolved_len = 1;
        }
        // A relative target resolves against the directory holding the link,
        // which is exactly `resolved_len`: the component was written at
        // `comp_at` and only `candidate_len` advanced past it, so there is
        // nothing to drop. Stripping here consumed the *parent* instead, which
        // turned `/bin/ls -> coreutils` into `/coreutils`.
    }

    if resolved_len == 0 {
        out[0] = b'/';
        resolved_len = 1;
    }
    Ok(resolved_len)
}

/// SlopOS has no FIFO file kind, so there is nothing for `mkfifo` to create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkfifo(_path: *const c_char, _mode: mode_t) -> c_int {
    fail(ENOSYS, -1)
}

/// Only the regular-file case, which POSIX defines as equivalent to `creat`,
/// is expressible: there is no syscall that makes a device node or a FIFO.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mknod(path: *const c_char, mode: mode_t, dev: dev_t) -> c_int {
    let kind = mode & slopos_abi::fs::S_IFMT;
    if kind != 0 && kind != slopos_abi::fs::S_IFREG {
        return fail(ENOSYS, -1);
    }
    if dev != 0 {
        return fail(EINVAL, -1);
    }
    let flags =
        (slopos_abi::fs::O_WRONLY | slopos_abi::fs::O_CREAT | slopos_abi::fs::O_EXCL) as c_int;
    match Sys::open(path as *const u8, flags, mode & !slopos_abi::fs::S_IFMT) {
        Ok(fd) => {
            let _ = Sys::close(fd);
            0
        }
        Err(e) => fail(e, -1),
    }
}

/// Changing directory by descriptor needs either an `fchdir` syscall or a
/// `/proc/self/fd` to `readlink`; SlopOS has neither, and a descriptor cannot
/// be turned back into a path from userland.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fchdir(_fd: c_int) -> c_int {
    fail(ENOSYS, -1)
}

/// There is no per-process root: the mount table is global, so a `chroot`
/// would confine nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chroot(_path: *const c_char) -> c_int {
    fail(ENOSYS, -1)
}

// ---------------------------------------------------------------------------
// Memory mappings
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mmap(
    addr: *mut c_void,
    len: usize,
    prot: c_int,
    flags: c_int,
    fd: c_int,
    offset: off_t,
) -> *mut c_void {
    if offset < 0 {
        return fail(EINVAL, MAP_FAILED);
    }
    match Sys::mmap(
        addr as *mut u8,
        len,
        prot as u32 as u64,
        flags as u32 as u64,
        fd,
        offset as u64,
    ) {
        Ok(ptr) => ptr as *mut c_void,
        Err(e) => fail(e, MAP_FAILED),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn munmap(addr: *mut c_void, len: usize) -> c_int {
    match Sys::munmap(addr as *mut u8, len) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int {
    match Sys::mprotect(addr as *mut u8, len, prot as u32 as u64) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// Advice bits: the purely advisory ones are accepted and dropped, which is
/// what "advice" means. `MADV_DONTNEED` and `MADV_FREE` are *not* advisory —
/// a caller that gets success believes the pages now read as zero — and there
/// is no syscall that can do that, so they are refused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int {
    const MADV_DONTNEED: c_int = 4;
    const MADV_FREE: c_int = 8;
    if addr.is_null() && len != 0 {
        return fail(EINVAL, -1);
    }
    if advice == MADV_DONTNEED || advice == MADV_FREE {
        return fail(ENOSYS, -1);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn msync(addr: *mut c_void, len: usize, flags: c_int) -> c_int {
    match Sys::msync(addr as *mut u8, len, flags as u32 as u64) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

// ---------------------------------------------------------------------------
// System identity, randomness, ids
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub unsafe extern "C" fn uname(buf: *mut Utsname) -> c_int {
    if buf.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::uname(buf) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_settime(clk_id: c_int, tp: *const Timespec) -> c_int {
    if tp.is_null() {
        return fail(crate::errno::EFAULT, -1);
    }
    match Sys::clock_settime(clk_id as u64, tp) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
    }
}

/// A short fill is legal; a caller wanting a full buffer loops.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrandom(buf: *mut c_void, buflen: usize, flags: c_uint) -> isize {
    match Sys::getrandom(buf as *mut u8, buflen, flags) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gettid() -> pid_t {
    Sys::gettid()
}

// ---------------------------------------------------------------------------
// SlopOS-specific calls. No C name describes these, so they keep the prefix.
// ---------------------------------------------------------------------------

/// Packed `UserDirent64` records; 0 means the directory is exhausted. The
/// records are the kernel's shape, not `struct dirent`'s — `readdir` is the
/// call that translates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_getdents64(fd: i32, buf: *mut u8, buf_len: usize) -> isize {
    match Sys::getdents64(fd, buf, buf_len) {
        Ok(n) => n as isize,
        Err(e) => fail(e, -1),
    }
}

/// `timeout_ns` of `u64::MAX` blocks indefinitely. A nanosecond timeout is
/// what a futex needs and what `SYS_futex`'s `timespec` cannot express without
/// a second object, so this is the shape the SlopOS call keeps.
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
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_futex_wake(addr: *const u32, count: u32) -> i32 {
    match Sys::futex_wake(addr, count) {
        Ok(n) => n,
        Err(e) => fail(e, -1),
    }
}

/// CPUs this task may run on. `sysconf(_SC_NPROCESSORS_ONLN)` answers from
/// here; the raw call exists because affinity, not the machine's CPU count, is
/// what bounds a thread pool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_get_cpu_count() -> i32 {
    match Sys::get_cpu_count() {
        Ok(n) => n as i32,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_get_current_cpu() -> i32 {
    match Sys::get_current_cpu() {
        Ok(n) => n as i32,
        Err(e) => fail(e, -1),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn slopos_set_cpu_affinity(target: u32, affinity: u32) -> i32 {
    match Sys::set_cpu_affinity(target, affinity) {
        Ok(()) => 0,
        Err(e) => fail(e, -1),
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
        Ok(pid) => pid,
        Err(e) => fail(e, -1),
    }
}

// `off_t` and the kernel's file sizes are both 64-bit, so the only guard any
// call above needs is the negative-length check. If either ever narrows, the
// `as u64` casts in `truncate`/`ftruncate`/`mmap` become lossy and this fails
// first.
const _: () = assert!(size_of::<off_t>() == 8);
