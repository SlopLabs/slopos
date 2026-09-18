//! The `DIR` stream — `opendir`/`readdir`/`closedir` over `getdents64(2)`.
//!
//! This is where the kernel's directory-record shape stops being visible.
//! `getdents64` returns a `UserDirent64` header that `#[repr(C)]` tail-pads to
//! 24 bytes, so its name begins at 24; the `struct dirent` the target's `libc`
//! declares begins its name at 19. Rather than change either, `readdir`
//! re-packs one record at a time into a `dirent` the caller owns for as long
//! as the next `readdir` has not run — which is exactly the lifetime POSIX
//! gives it.

use core::ffi::{c_char, c_int, c_void};

use crate::errno::{EBADF, EINVAL, EIO, ENOMEM, ENOTDIR, errno_set};
use crate::io::dirent::{DIRENT_NAME_OFFSET, DirentIter};
use crate::mem::malloc;
use crate::pal::{Pal, Sys};
use crate::types::{DIR, DIRENT_D_NAME_OFFSET, dirent, dirent64, stat};

/// Bytes of raw kernel records buffered per refill. One page: large enough
/// that a typical directory is two or three syscalls, small enough that the
/// whole `DIR` is a single small allocation.
const DIR_BUF_SIZE: usize = 4096;

/// Longest name a record may carry, NUL excluded — `struct dirent`'s
/// `d_name[256]` minus the terminator.
const DIR_NAME_MAX: usize = 255;

/// The object `*mut DIR` points at. Heap-allocated, never moved, and opaque to
/// C: only this module knows the layout.
#[repr(C)]
struct Dir {
    fd: c_int,
    /// Bytes of `buf` the last `getdents64` filled.
    filled: usize,
    /// Read cursor into `buf`.
    pos: usize,
    /// The record the last `readdir` handed out. It has to live in the `DIR`
    /// rather than on the stack because the caller keeps the pointer.
    entry: dirent,
    buf: [u8; DIR_BUF_SIZE],
}

/// Wrap an already-open directory descriptor. Takes ownership of `fd`: a
/// failure here closes it, because the caller has no way to tell whether it
/// was consumed.
unsafe fn dir_from_fd(fd: c_int) -> *mut DIR {
    let raw = malloc::alloc(size_of::<Dir>()) as *mut Dir;
    if raw.is_null() {
        let _ = Sys::close(fd);
        errno_set(ENOMEM.raw());
        return core::ptr::null_mut();
    }
    (*raw).fd = fd;
    (*raw).filled = 0;
    (*raw).pos = 0;
    (*raw).entry = dirent::zeroed();
    // The record buffer is written before it is read, so it is left
    // uninitialised rather than page-zeroed on every `opendir`.
    raw as *mut DIR
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn opendir(path: *const c_char) -> *mut DIR {
    if path.is_null() {
        errno_set(EINVAL.raw());
        return core::ptr::null_mut();
    }
    let flags = (slopos_abi::fs::O_RDONLY | slopos_abi::fs::O_DIRECTORY) as i32
        | slopos_abi::syscall::O_CLOEXEC as i32;
    let fd = match Sys::open(path as *const u8, flags, 0) {
        Ok(fd) => fd,
        Err(e) => {
            errno_set(e.raw());
            return core::ptr::null_mut();
        }
    };
    dir_from_fd(fd)
}

/// Takes ownership of `fd`, as POSIX requires: after a successful call the
/// descriptor must only be used through the `DIR`, and `closedir` closes it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fdopendir(fd: c_int) -> *mut DIR {
    let mut st = stat::default();
    if let Err(e) = Sys::fstat(fd, &raw mut st as *mut u8) {
        errno_set(e.raw());
        return core::ptr::null_mut();
    }
    if !st.is_directory() {
        errno_set(ENOTDIR.raw());
        return core::ptr::null_mut();
    }
    dir_from_fd(fd)
}

/// Answers the next entry, or null at the end of the directory — at which
/// point `errno` is left alone, so a caller distinguishing end-of-directory
/// from failure zeroes `errno` first, exactly as POSIX describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readdir(dirp: *mut DIR) -> *mut dirent {
    let dir = dirp as *mut Dir;
    if dir.is_null() {
        errno_set(EBADF.raw());
        return core::ptr::null_mut();
    }

    loop {
        if (*dir).pos >= (*dir).filled {
            match Sys::getdents64((*dir).fd, (*dir).buf.as_mut_ptr(), DIR_BUF_SIZE) {
                Ok(0) => {
                    (*dir).filled = 0;
                    (*dir).pos = 0;
                    return core::ptr::null_mut();
                }
                Ok(n) => {
                    (*dir).filled = n.min(DIR_BUF_SIZE);
                    (*dir).pos = 0;
                }
                Err(e) => {
                    errno_set(e.raw());
                    return core::ptr::null_mut();
                }
            }
        }

        // Name and metadata are copied out before anything in `*dir` is
        // written, so the borrow of `buf` ends before `entry` is touched.
        let mut name = [0u8; DIR_NAME_MAX + 1];
        let (d_ino, d_off, d_type, name_len, consumed) = {
            let window = core::slice::from_raw_parts(
                (*dir).buf.as_ptr().add((*dir).pos),
                (*dir).filled - (*dir).pos,
            );
            let mut walker = DirentIter::new(window);
            let Some(record) = walker.next() else {
                // `d_reclen` is what says where the next record starts, so a
                // malformed one cannot be stepped over and the stream cannot
                // be resynchronised. That is an I/O error, not an end.
                errno_set(EIO.raw());
                return core::ptr::null_mut();
            };
            let len = record.name.len().min(DIR_NAME_MAX);
            name[..len].copy_from_slice(&record.name[..len]);
            (
                record.d_ino,
                record.d_off,
                record.d_type,
                len,
                walker.byte_offset(),
            )
        };
        (*dir).pos += consumed;

        let entry = &raw mut (*dir).entry;
        (*entry).d_ino = d_ino;
        (*entry).d_off = d_off;
        (*entry).d_type = d_type;
        (*entry).d_name = [0; 256];
        core::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*entry).d_name) as *mut u8,
            name_len,
        );
        // `d_reclen` describes *this* record in *this* struct: the libc header
        // plus the NUL-terminated name, rounded to the struct's alignment.
        (*entry).d_reclen = align_up(DIRENT_D_NAME_OFFSET + name_len + 1, 8) as u16;

        return entry;
    }
}

/// `struct dirent64` is `struct dirent` on a target whose `ino_t` and `off_t`
/// are already 64-bit, so this is the same stream under the name a
/// `_LARGEFILE64_SOURCE` consumer links against.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn readdir64(dirp: *mut DIR) -> *mut dirent64 {
    readdir(dirp)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn closedir(dirp: *mut DIR) -> c_int {
    let dir = dirp as *mut Dir;
    if dir.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let fd = (*dir).fd;
    malloc::dealloc(dir as *mut c_void);
    match Sys::close(fd) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Seek back to the start. The buffered records are dropped rather than
/// re-walked: the kernel's own cursor is what `lseek` moved.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rewinddir(dirp: *mut DIR) {
    let dir = dirp as *mut Dir;
    if dir.is_null() {
        return;
    }
    let _ = Sys::lseek((*dir).fd, 0, slopos_abi::syscall::SEEK_SET as i32);
    (*dir).filled = 0;
    (*dir).pos = 0;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn dirfd(dirp: *mut DIR) -> c_int {
    let dir = dirp as *mut Dir;
    if dir.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    (*dir).fd
}

#[inline]
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

// The re-pack exists precisely because these two differ. If they ever agree,
// `readdir` could memcpy and this module should be simplified rather than
// left doing pointless work.
const _: () = assert!(DIRENT_NAME_OFFSET != DIRENT_D_NAME_OFFSET);
const _: () = assert!(DIR_NAME_MAX + 1 == 256);
