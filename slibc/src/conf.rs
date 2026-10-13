//! System configuration and the odds and ends that belong to no subsystem.

use core::ffi::{c_char, c_int, c_long};
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::errno::{EINVAL, EIO, ENAMETOOLONG, ERANGE, errno_set};
use crate::pal::raw::syscall6;
use crate::pal::{Pal, Sys};
use crate::thread::tcb::{STRERROR_BUF, Tcb};
use crate::types::{gid_t, group, passwd, uid_t, utsname as Utsname};

pub const _SC_ARG_MAX: c_int = 0;
pub const _SC_CLK_TCK: c_int = 2;
pub const _SC_OPEN_MAX: c_int = 4;
pub const _SC_PAGESIZE: c_int = 30;
pub const _SC_IOV_MAX: c_int = 60;
pub const _SC_GETPW_R_SIZE_MAX: c_int = 70;
pub const _SC_THREAD_STACK_MIN: c_int = 75;
pub const _SC_NPROCESSORS_CONF: c_int = 83;
pub const _SC_NPROCESSORS_ONLN: c_int = 84;
pub const _SC_PHYS_PAGES: c_int = 85;
pub const _SC_AVPHYS_PAGES: c_int = 86;
pub const _SC_SYMLOOP_MAX: c_int = 173;
pub const _SC_HOST_NAME_MAX: c_int = 180;

/// POSIX's floor on `ARG_MAX`, which a program falls back to when
/// `sysconf(_SC_ARG_MAX)` answers -1. The real budget is the kernel's, which
/// `sysconf` reports.
pub const _POSIX_ARG_MAX: c_long = 4096;

/// Longest hostname `gethostname` will report, NUL excluded. Linux's.
pub const HOST_NAME_MAX: usize = 64;

/// Symlinks a path resolution will follow. Matches `realpath`'s own bound.
pub const SYMLOOP_MAX: c_long = 40;

/// One row of the passwd database. The strings carry their NULs.
struct User {
    name: &'static [u8],
    uid: uid_t,
    gid: gid_t,
    dir: &'static [u8],
}

const PW_PASSWD: &[u8] = b"x\0";
const PW_SHELL: &[u8] = b"/bin/shell\0";

/// The system and the one person at the machine. Compiled in rather than read
/// from `/etc/passwd` until there is more than one person.
const USERS: [User; 2] = [
    User {
        name: b"root\0",
        uid: 0,
        gid: 0,
        dir: b"/root\0",
    },
    User {
        name: b"user\0",
        uid: 1000,
        gid: 1000,
        dir: b"/home/user\0",
    },
];

impl User {
    /// Its name doubles as its gecos field.
    const fn buf_len(&self) -> usize {
        2 * self.name.len() + PW_PASSWD.len() + self.dir.len() + PW_SHELL.len()
    }
}

/// Bytes `getpwuid_r` needs for any row, NULs included, so
/// `_SC_GETPW_R_SIZE_MAX` and the rows cannot drift.
const PASSWD_BUF_MIN: usize = {
    let mut max = 0;
    let mut i = 0;
    while i < USERS.len() {
        if USERS[i].buf_len() > max {
            max = USERS[i].buf_len();
        }
        i += 1;
    }
    max
};

fn user_named(name: &[u8]) -> Option<&'static User> {
    USERS
        .iter()
        .find(|user| &user.name[..user.name.len() - 1] == name)
}

/// `sysconf(3)`.
///
/// An unrecognised name is `-1` with `EINVAL` rather than `-1` alone: a limit
/// this library does not know about must not be indistinguishable from one it
/// knows to be unbounded.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sysconf(name: c_int) -> c_long {
    match name {
        _SC_ARG_MAX => slopos_abi::spawn::EXEC_MAX_ARG_BYTES as c_long,
        _SC_PAGESIZE => slopos_abi::PAGE_SIZE as c_long,
        _SC_NPROCESSORS_CONF | _SC_NPROCESSORS_ONLN => match Sys::get_cpu_count() {
            // The affinity mask is what bounds a thread pool, and a mask that
            // reads as empty still leaves one CPU running this code.
            Ok(0) | Err(_) => 1,
            Ok(n) => n as c_long,
        },
        _SC_PHYS_PAGES => crate::sysinfo::phys_pages(),
        _SC_AVPHYS_PAGES => crate::sysinfo::avphys_pages(),
        _SC_OPEN_MAX => {
            let mut lim = crate::types::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if crate::process::rlimit::getrlimit(slopos_abi::quota::RLIMIT_NOFILE, &mut lim) != 0 {
                return -1;
            }
            if lim.rlim_cur == slopos_abi::quota::RLIM64_INFINITY {
                -1
            } else {
                lim.rlim_cur.min(c_long::MAX as u64) as c_long
            }
        }
        _SC_IOV_MAX => slopos_abi::fs::UIO_MAXIOV as c_long,
        _SC_GETPW_R_SIZE_MAX => PASSWD_BUF_MIN as c_long,
        _SC_THREAD_STACK_MIN => crate::thread::PTHREAD_STACK_MIN as c_long,
        // The scheduler runs on a millisecond timer, so a clock tick is 1 ms.
        _SC_CLK_TCK => 1000,
        _SC_SYMLOOP_MAX => SYMLOOP_MAX,
        _SC_HOST_NAME_MAX => HOST_NAME_MAX as c_long,
        _ => {
            errno_set(EINVAL.raw());
            -1
        }
    }
}

/// `syscall(2)`, the escape hatch.
///
/// Six arguments are always read and always passed. On x86-64 the C ABI spills
/// all six integer registers into the variadic save area, so reading a slot
/// the caller did not fill is defined; and the kernel reads only the arguments
/// the handler for `num` declares, so the surplus registers are ignored
/// exactly as they are for a hand-written `syscall` instruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall(num: c_long, mut args: ...) -> c_long {
    let a0 = args.next_arg::<c_long>() as u64;
    let a1 = args.next_arg::<c_long>() as u64;
    let a2 = args.next_arg::<c_long>() as u64;
    let a3 = args.next_arg::<c_long>() as u64;
    let a4 = args.next_arg::<c_long>() as u64;
    let a5 = args.next_arg::<c_long>() as u64;

    let ret = syscall6(num as u64, a0, a1, a2, a3, a4, a5);
    match crate::demux(ret) {
        Ok(v) => v as c_long,
        Err(e) => {
            errno_set(e.errno());
            -1
        }
    }
}

/// `gethostname(2)`, from `uname`'s `nodename` — the only name the kernel
/// keeps.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gethostname(name: *mut c_char, len: usize) -> c_int {
    if name.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let mut uts = Utsname::new();
    if let Err(e) = Sys::uname(&raw mut uts) {
        errno_set(e.raw());
        return -1;
    }
    let node = &uts.nodename;
    let node_len = crate::string::u_strnlen(node.as_ptr(), node.len());
    if node_len + 1 > len {
        errno_set(ENAMETOOLONG.raw());
        return -1;
    }
    core::ptr::copy_nonoverlapping(node.as_ptr(), name as *mut u8, node_len);
    *(name as *mut u8).add(node_len) = 0;
    0
}

/// `getpwuid_r(3)`. A uid with no row is "no such entry", which POSIX spells
/// as success with a null `*result` — not an error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwuid_r(
    uid: uid_t,
    pwd: *mut passwd,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut passwd,
) -> c_int {
    if pwd.is_null() || buf.is_null() || result.is_null() {
        return EINVAL.raw();
    }
    *result = core::ptr::null_mut();
    let Some(user) = USERS.iter().find(|user| user.uid == uid) else {
        return 0;
    };
    if buflen < user.buf_len() {
        return ERANGE.raw();
    }

    let base = buf as *mut u8;
    let mut at = 0usize;
    for (field, text) in [
        (&raw mut (*pwd).pw_name, user.name),
        (&raw mut (*pwd).pw_passwd, PW_PASSWD),
        (&raw mut (*pwd).pw_gecos, user.name),
        (&raw mut (*pwd).pw_dir, user.dir),
        (&raw mut (*pwd).pw_shell, PW_SHELL),
    ] {
        let dst = base.add(at);
        core::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
        *field = dst as *mut c_char;
        at += text.len();
    }
    (*pwd).pw_uid = user.uid;
    (*pwd).pw_gid = user.gid;
    *result = pwd;
    0
}

/// `getpwnam_r(3)`. A name with no row is "no such entry", which POSIX spells
/// as success with a null `*result`.
///
/// # Safety
/// `name` is a NUL-terminated C string; `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwnam_r(
    name: *const c_char,
    pwd: *mut passwd,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut passwd,
) -> c_int {
    if name.is_null() || pwd.is_null() || buf.is_null() || result.is_null() {
        return EINVAL.raw();
    }
    let bytes = name as *const u8;
    let requested = crate::string::slice_from_cstr(bytes, crate::string::u_strlen(bytes));
    let Some(user) = user_named(requested) else {
        *result = core::ptr::null_mut();
        return 0;
    };
    getpwuid_r(user.uid, pwd, buf, buflen, result)
}

static mut PW_ROW: passwd = passwd {
    pw_name: core::ptr::null_mut(),
    pw_passwd: core::ptr::null_mut(),
    pw_uid: 0,
    pw_gid: 0,
    pw_gecos: core::ptr::null_mut(),
    pw_dir: core::ptr::null_mut(),
    pw_shell: core::ptr::null_mut(),
};
static mut PW_ROW_BUF: [u8; PASSWD_BUF_MIN] = [0; PASSWD_BUF_MIN];

/// `getpwnam(3)`: [`getpwnam_r`] into static storage, which POSIX lets the
/// next call overwrite. A name with no row is `NULL` with `errno` untouched.
///
/// # Safety
/// `name` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwnam(name: *const c_char) -> *mut passwd {
    let mut result = core::ptr::null_mut();
    let rc = getpwnam_r(
        name,
        &raw mut PW_ROW,
        (&raw mut PW_ROW_BUF).cast(),
        PASSWD_BUF_MIN,
        &mut result,
    );
    if rc != 0 {
        crate::errno::errno_set(rc);
        return core::ptr::null_mut();
    }
    result
}

/// `getpwuid(3)`: [`getpwuid_r`] into the same static storage as
/// [`getpwnam`]. A uid with no row is `NULL` with `errno` untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwuid(uid: uid_t) -> *mut passwd {
    let mut result = core::ptr::null_mut();
    let rc = getpwuid_r(
        uid,
        &raw mut PW_ROW,
        (&raw mut PW_ROW_BUF).cast(),
        PASSWD_BUF_MIN,
        &mut result,
    );
    if rc != 0 {
        errno_set(rc);
        return core::ptr::null_mut();
    }
    result
}

/// One row of the group database: each user's own group, which lists no
/// members.
struct Group {
    name: &'static [u8],
    gid: gid_t,
}

const GROUPS: [Group; 2] = [
    Group {
        name: b"root\0",
        gid: 0,
    },
    Group {
        name: b"user\0",
        gid: 1000,
    },
];

const GR_PASSWD: &[u8] = b"x\0";
const PTR: usize = size_of::<*mut c_char>();

/// Bytes `getgrgid_r` needs for any row: the NULL-terminated, empty member
/// list, the slack to align it, and the two strings.
const GROUP_BUF_MIN: usize = {
    let mut longest = 0;
    let mut i = 0;
    while i < GROUPS.len() {
        if GROUPS[i].name.len() > longest {
            longest = GROUPS[i].name.len();
        }
        i += 1;
    }
    PTR + (PTR - 1) + longest + GR_PASSWD.len()
};

/// `getgrgid_r(3)`. A gid with no row is success with a null `*result`.
///
/// # Safety
/// `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrgid_r(
    gid: gid_t,
    grp: *mut group,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut group,
) -> c_int {
    if grp.is_null() || buf.is_null() || result.is_null() {
        return EINVAL.raw();
    }
    *result = core::ptr::null_mut();
    let Some(row) = GROUPS.iter().find(|row| row.gid == gid) else {
        return 0;
    };
    let base = buf as *mut u8;
    let pad = base.align_offset(PTR);
    if pad > PTR - 1 || buflen < PTR + pad + row.name.len() + GR_PASSWD.len() {
        return ERANGE.raw();
    }

    let members = base.add(pad) as *mut *mut c_char;
    *members = core::ptr::null_mut();
    let mut at = pad + PTR;
    for (field, text) in [
        (&raw mut (*grp).gr_name, row.name),
        (&raw mut (*grp).gr_passwd, GR_PASSWD),
    ] {
        let dst = base.add(at);
        core::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
        *field = dst as *mut c_char;
        at += text.len();
    }
    (*grp).gr_gid = row.gid;
    (*grp).gr_mem = members;
    *result = grp;
    0
}

/// `getgrnam_r(3)`. A name with no row is success with a null `*result`.
///
/// # Safety
/// `name` is a NUL-terminated C string; `buf` addresses `buflen` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrnam_r(
    name: *const c_char,
    grp: *mut group,
    buf: *mut c_char,
    buflen: usize,
    result: *mut *mut group,
) -> c_int {
    if name.is_null() || grp.is_null() || buf.is_null() || result.is_null() {
        return EINVAL.raw();
    }
    let bytes = name as *const u8;
    let requested = crate::string::slice_from_cstr(bytes, crate::string::u_strlen(bytes));
    let Some(row) = GROUPS
        .iter()
        .find(|row| &row.name[..row.name.len() - 1] == requested)
    else {
        *result = core::ptr::null_mut();
        return 0;
    };
    getgrgid_r(row.gid, grp, buf, buflen, result)
}

/// `initgroups(3)`: the supplementary groups become `group` and every group
/// that lists `user` as a member. No row lists a member, so the set is
/// `{group}`, which only a holder of `SetId` may set.
///
/// # Safety
/// `user` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn initgroups(user: *const c_char, group: gid_t) -> c_int {
    if user.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    crate::process::ids::setgroups(1, &group)
}

static mut GR_ROW: group = group {
    gr_name: core::ptr::null_mut(),
    gr_passwd: core::ptr::null_mut(),
    gr_gid: 0,
    gr_mem: core::ptr::null_mut(),
};
static mut GR_ROW_BUF: [u8; GROUP_BUF_MIN] = [0; GROUP_BUF_MIN];

unsafe fn group_static(rc: c_int, result: *mut group) -> *mut group {
    if rc != 0 {
        errno_set(rc);
        return core::ptr::null_mut();
    }
    result
}

/// `getgrgid(3)`: [`getgrgid_r`] into static storage. A gid with no row is
/// `NULL` with `errno` untouched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrgid(gid: gid_t) -> *mut group {
    let mut result = core::ptr::null_mut();
    let rc = getgrgid_r(
        gid,
        &raw mut GR_ROW,
        (&raw mut GR_ROW_BUF).cast(),
        GROUP_BUF_MIN,
        &mut result,
    );
    group_static(rc, result)
}

/// `getgrnam(3)`: [`getgrnam_r`] into static storage. A name with no row is
/// `NULL` with `errno` untouched.
///
/// # Safety
/// `name` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrnam(name: *const c_char) -> *mut group {
    let mut result = core::ptr::null_mut();
    let rc = getgrnam_r(
        name,
        &raw mut GR_ROW,
        (&raw mut GR_ROW_BUF).cast(),
        GROUP_BUF_MIN,
        &mut result,
    );
    group_static(rc, result)
}

/// `getpagesize(3)`.
#[unsafe(no_mangle)]
pub extern "C" fn getpagesize() -> c_int {
    slopos_abi::PAGE_SIZE as c_int
}

static PW_NEXT: AtomicUsize = AtomicUsize::new(0);
static GR_NEXT: AtomicUsize = AtomicUsize::new(0);

/// `setpwent(3)`: rewind [`getpwent`].
#[unsafe(no_mangle)]
pub extern "C" fn setpwent() {
    PW_NEXT.store(0, Ordering::Relaxed);
}

/// `endpwent(3)`: as [`setpwent`]; there is no file to close.
#[unsafe(no_mangle)]
pub extern "C" fn endpwent() {
    setpwent();
}

/// `getpwent(3)`: the passwd database's rows in turn, then `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpwent() -> *mut passwd {
    match USERS.get(PW_NEXT.fetch_add(1, Ordering::Relaxed)) {
        Some(user) => getpwuid(user.uid),
        None => core::ptr::null_mut(),
    }
}

/// `setgrent(3)`: rewind [`getgrent`].
#[unsafe(no_mangle)]
pub extern "C" fn setgrent() {
    GR_NEXT.store(0, Ordering::Relaxed);
}

/// `endgrent(3)`: as [`setgrent`].
#[unsafe(no_mangle)]
pub extern "C" fn endgrent() {
    setgrent();
}

/// `getgrent(3)`: the group database's rows in turn, then `NULL`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgrent() -> *mut group {
    match GROUPS.get(GR_NEXT.fetch_add(1, Ordering::Relaxed)) {
        Some(row) => getgrgid(row.gid),
        None => core::ptr::null_mut(),
    }
}

/// `getentropy(3)`. POSIX caps one call at 256 bytes, and the kernel's
/// generator never blocks, so a short read is a failure rather than a retry.
///
/// # Safety
/// `buf` addresses `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getentropy(buf: *mut core::ffi::c_void, len: usize) -> c_int {
    if len > 256 {
        errno_set(EIO.raw());
        return -1;
    }
    match Sys::getrandom(buf as *mut u8, len, 0) {
        Ok(got) if got == len => 0,
        Ok(_) => {
            errno_set(EIO.raw());
            -1
        }
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `strerror_r(3)`, the XSI form: 0, or an errno.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strerror_r(n: c_int, buf: *mut c_char, buflen: usize) -> c_int {
    if buf.is_null() || buflen == 0 {
        return EINVAL.raw();
    }
    let text = crate::error::SyscallError::from_errno(n)
        .as_str()
        .as_bytes();
    if text.len() + 1 > buflen {
        // Truncating as well as reporting ERANGE leaves a caller that ignores
        // the return with a usable string.
        let room = buflen - 1;
        core::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, room);
        *(buf as *mut u8).add(room) = 0;
        return ERANGE.raw();
    }
    core::ptr::copy_nonoverlapping(text.as_ptr(), buf as *mut u8, text.len());
    *(buf as *mut u8).add(text.len()) = 0;
    0
}

/// As `errno`: until `TLS_READY` flips there is no TCB to answer out of.
static mut STRERROR_FALLBACK: [u8; STRERROR_BUF] = [0; STRERROR_BUF];

/// `strerror(3)`. Never `NULL`; an unknown number reads as "Unknown error".
///
/// The buffer is per-thread: `as_str` hands back a `&'static str` with no
/// NUL, so the text must be copied, and one process-wide copy would let two
/// threads in `strerror` at once read each other's answer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strerror(n: c_int) -> *mut c_char {
    let text = crate::error::SyscallError::from_errno(n)
        .as_str()
        .as_bytes();
    let len = text.len().min(STRERROR_BUF - 1);
    let buf = if crate::thread::tls::tls_is_initialized() {
        (&raw mut (*Tcb::current()).strerror_buf).cast::<u8>()
    } else {
        (&raw mut STRERROR_FALLBACK).cast::<u8>()
    };
    core::ptr::copy_nonoverlapping(text.as_ptr(), buf, len);
    *buf.add(len) = 0;
    buf.cast()
}

pub const _PC_NAME_MAX: c_int = 3;
pub const _PC_PATH_MAX: c_int = 4;

/// `pathconf(3)`. Every limit here is a constant, so the path is checked for
/// existence and nothing else: POSIX wants `ENOENT` for one that is absent,
/// and a caller would otherwise size a buffer off a name it cannot open.
///
/// # Safety
/// `path` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pathconf(path: *const c_char, name: c_int) -> c_long {
    if crate::io::misc::access(path as *const u8, 0) != 0 {
        return -1;
    }
    limit(name)
}

/// `fpathconf(3)`. As [`pathconf`], with the descriptor checked in place of
/// the path.
#[unsafe(no_mangle)]
pub extern "C" fn fpathconf(fd: c_int, name: c_int) -> c_long {
    if let Err(e) = Sys::fcntl(fd, slopos_abi::syscall::F_GETFD as c_int, 0) {
        errno_set(e.raw());
        return -1;
    }
    limit(name)
}

fn limit(name: c_int) -> c_long {
    match name {
        _PC_NAME_MAX => 255,
        _PC_PATH_MAX => 4096,
        _ => {
            errno_set(EINVAL.raw());
            -1
        }
    }
}
