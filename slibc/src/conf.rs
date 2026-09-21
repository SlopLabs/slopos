//! System configuration and the odds and ends that belong to no subsystem.

use core::ffi::{c_char, c_int, c_long};

use crate::errno::{EINVAL, EIO, ENAMETOOLONG, ERANGE, errno_set};
use crate::pal::raw::syscall6;
use crate::pal::{Pal, Sys};
use crate::thread::tcb::{STRERROR_BUF, Tcb};
use crate::types::{passwd, uid_t, utsname as Utsname};

pub const _SC_ARG_MAX: c_int = 0;
pub const _SC_CLK_TCK: c_int = 2;
pub const _SC_OPEN_MAX: c_int = 4;
pub const _SC_PAGESIZE: c_int = 30;
pub const _SC_IOV_MAX: c_int = 60;
pub const _SC_GETPW_R_SIZE_MAX: c_int = 70;
pub const _SC_THREAD_STACK_MIN: c_int = 75;
pub const _SC_NPROCESSORS_CONF: c_int = 83;
pub const _SC_NPROCESSORS_ONLN: c_int = 84;
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

/// Bytes `getpwuid_r` needs for its one row, NULs included. Stated as a
/// constant so `_SC_GETPW_R_SIZE_MAX` and the row below cannot drift.
const PASSWD_BUF_MIN: usize =
    PW_NAME.len() + PW_PASSWD.len() + PW_GECOS.len() + PW_DIR.len() + PW_SHELL.len();

const PW_NAME: &[u8] = b"root\0";
const PW_PASSWD: &[u8] = b"x\0";
const PW_GECOS: &[u8] = b"root\0";
/// `/` rather than `/root`: that is what the shell's own `HOME` default is,
/// and the image has no per-user directory.
const PW_DIR: &[u8] = b"/\0";
const PW_SHELL: &[u8] = b"/bin/shell\0";

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

/// `getpwuid_r(3)`.
///
/// SlopOS is single-user, so there is exactly one row and it is uid 0. Any
/// other uid is "no such entry", which POSIX spells as success with a null
/// `*result` — not an error.
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
    if uid != 0 {
        return 0;
    }
    if buflen < PASSWD_BUF_MIN {
        return ERANGE.raw();
    }

    // `PASSWD_BUF_MIN` is the total of the five strings, so the check above
    // is the only bound this loop needs.
    let base = buf as *mut u8;
    let mut at = 0usize;
    for (field, text) in [
        (&raw mut (*pwd).pw_name, PW_NAME),
        (&raw mut (*pwd).pw_passwd, PW_PASSWD),
        (&raw mut (*pwd).pw_gecos, PW_GECOS),
        (&raw mut (*pwd).pw_dir, PW_DIR),
        (&raw mut (*pwd).pw_shell, PW_SHELL),
    ] {
        let dst = base.add(at);
        core::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
        *field = dst as *mut c_char;
        at += text.len();
    }
    (*pwd).pw_uid = 0;
    (*pwd).pw_gid = 0;
    *result = pwd;
    0
}

/// `getpwnam_r(3)`. The one row is named `root`; any other name is "no such
/// entry", which POSIX spells as success with a null `*result`.
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
    if requested != &PW_NAME[..PW_NAME.len() - 1] {
        *result = core::ptr::null_mut();
        return 0;
    }
    getpwuid_r(0, pwd, buf, buflen, result)
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
