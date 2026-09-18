//! The C entry points that are neither file metadata nor a subsystem of their
//! own: `read`/`write`/`open`/`close`, the break, and the allocator.

#[allow(dead_code)]
pub(crate) mod shim;
pub mod syscalls;
pub mod tests;

use core::ffi::{c_char, c_int, c_void};

use crate::errno::{ENOMEM, errno_set};
use crate::mem::malloc as heap;
use crate::pal::{Pal, Sys};

#[allow(non_camel_case_types)]
pub type ssize_t = isize;
#[allow(non_camel_case_types)]
pub type size_t = usize;

/// The `open(2)` flags, as `c_int`. Taken from the kernel ABI rather than
/// restated: a second set of magic numbers is a second thing to drift.
pub const O_RDONLY: c_int = slopos_abi::fs::O_RDONLY as c_int;
pub const O_WRONLY: c_int = slopos_abi::fs::O_WRONLY as c_int;
pub const O_RDWR: c_int = slopos_abi::fs::O_RDWR as c_int;
pub const O_CREAT: c_int = slopos_abi::fs::O_CREAT as c_int;
pub const O_EXCL: c_int = slopos_abi::fs::O_EXCL as c_int;
pub const O_TRUNC: c_int = slopos_abi::fs::O_TRUNC as c_int;
pub const O_APPEND: c_int = slopos_abi::fs::O_APPEND as c_int;
pub const O_DIRECTORY: c_int = slopos_abi::fs::O_DIRECTORY as c_int;
pub const O_NONBLOCK: c_int = slopos_abi::syscall::O_NONBLOCK as c_int;
pub const O_CLOEXEC: c_int = slopos_abi::syscall::O_CLOEXEC as c_int;

/// `O_TMPFILE` is `O_DIRECTORY | 0o20000000`: it creates an unnamed file, so
/// like `O_CREAT` it takes a mode. The kernel does not implement it, but
/// `open` still has to know that such a call carries a third argument — the
/// variadic decision has to be right even when the call then fails.
const O_TMPFILE: c_int = 0o20_200_000;

/// True when `oflag` names a call that creates, and therefore carries a
/// `mode_t` third argument.
#[inline]
pub(crate) fn oflag_creates(oflag: c_int) -> bool {
    let bits = oflag as u32;
    bits & slopos_abi::fs::O_CREAT != 0 || (oflag & O_TMPFILE) == O_TMPFILE
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t {
    match Sys::read(fd, buf as *mut u8, count) {
        Ok(n) => n as ssize_t,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t {
    match Sys::write(fd, buf as *const u8, count) {
        Ok(n) => n as ssize_t,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `open(2)`, variadic as C has it: the mode argument exists only for a call
/// that can create, and reading one the caller never pushed is what the old
/// hard-coded `0o666` was standing in for.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const c_char, oflag: c_int, mut args: ...) -> c_int {
    let mode = if oflag_creates(oflag) {
        args.next_arg::<crate::types::mode_t>()
    } else {
        0
    };
    match Sys::open(path as *const u8, oflag, mode) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Safe rather than `unsafe`: unlike `read`/`write` this touches no
/// caller-provided memory, so there is no precondition for a caller to uphold.
#[unsafe(no_mangle)]
pub extern "C" fn close(fd: c_int) -> c_int {
    match Sys::close(fd) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `brk(2)` as C declares it: 0 or -1, not the resulting break. A kernel that
/// answers a break below the request could not satisfy it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn brk(addr: *mut c_void) -> c_int {
    match Sys::brk(addr as *mut u8) {
        Ok(got) if got >= addr as *mut u8 => 0,
        Ok(_) => {
            errno_set(ENOMEM.raw());
            -1
        }
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `sbrk(3)`: answers the *old* break, or `(void *) -1` with `errno` set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sbrk(increment: isize) -> *mut c_void {
    let failure = usize::MAX as *mut c_void;
    let current = match Sys::brk(core::ptr::null_mut()) {
        Ok(p) => p,
        Err(e) => {
            errno_set(e.raw());
            return failure;
        }
    };
    if increment == 0 {
        return current as *mut c_void;
    }
    let want = if increment > 0 {
        (current as usize).wrapping_add(increment as usize)
    } else {
        (current as usize).wrapping_sub(increment.unsigned_abs())
    };
    match Sys::brk(want as *mut u8) {
        Ok(got) if got as usize == want => current as *mut c_void,
        Ok(_) => {
            errno_set(ENOMEM.raw());
            failure
        }
        Err(e) => {
            errno_set(e.raw());
            failure
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: size_t) -> *mut c_void {
    heap::alloc(size)
}

#[unsafe(no_mangle)]
pub extern "C" fn free(ptr: *mut c_void) {
    heap::dealloc(ptr)
}

#[unsafe(no_mangle)]
pub extern "C" fn realloc(ptr: *mut c_void, size: size_t) -> *mut c_void {
    heap::realloc(ptr, size)
}

#[unsafe(no_mangle)]
pub extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut c_void {
    heap::calloc(nmemb, size)
}

/// `posix_memalign(3)`. Answers the errno rather than setting it, which is the
/// one allocator entry point that does. `align` must be a power of two and a
/// multiple of `sizeof(void *)`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn posix_memalign(
    memptr: *mut *mut c_void,
    align: size_t,
    size: size_t,
) -> c_int {
    if memptr.is_null() {
        return crate::errno::EINVAL.raw();
    }
    if align < size_of::<*mut c_void>() || !align.is_power_of_two() {
        return crate::errno::EINVAL.raw();
    }
    if size == 0 {
        *memptr = core::ptr::null_mut();
        return 0;
    }
    let p = heap::memalign(align, size);
    if p.is_null() {
        return ENOMEM.raw();
    }
    *memptr = p as *mut c_void;
    0
}

/// `memalign(3)`. Obsolete beside `posix_memalign`, and still what some
/// callers reach for, so it is exported rather than aliased away.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memalign(align: size_t, size: size_t) -> *mut c_void {
    if align == 0 || !align.is_power_of_two() {
        errno_set(crate::errno::EINVAL.raw());
        return core::ptr::null_mut();
    }
    let p = heap::memalign(align, size);
    if p.is_null() {
        errno_set(ENOMEM.raw());
    }
    p as *mut c_void
}

/// `malloc_usable_size(3)`: the bytes actually available in `ptr`'s block,
/// which is at least what was asked for.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc_usable_size(ptr: *mut c_void) -> size_t {
    heap::malloc_usable_size(ptr as *mut u8)
}
