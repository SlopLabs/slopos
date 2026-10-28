//! Safe wrappers over the stdio surface for use from tests.

use crate::pal::{Pal, Sys};

use super::{FILE, chars, file};

/// A handle to an open stdio stream.
///
/// `Send`/`Sync`: every operation goes through the per-stream lock, which is
/// the concurrent access POSIX §2.5.1 defines for a `FILE`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stream(*mut FILE);

unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}

impl Stream {
    fn from_raw(raw: *mut FILE) -> Option<Stream> {
        if raw.is_null() {
            None
        } else {
            Some(Stream(raw))
        }
    }
}

/// `fopen(path, mode)`. Both slices must be NUL-terminated.
pub fn fopen(path: &[u8], mode: &[u8]) -> Option<Stream> {
    Stream::from_raw(unsafe { file::fopen(path.as_ptr(), mode.as_ptr()) })
}

/// `fdopen(fd, mode)`. `mode` must be NUL-terminated.
pub fn fdopen(fd: i32, mode: &[u8]) -> Option<Stream> {
    Stream::from_raw(unsafe { file::fdopen(fd, mode.as_ptr()) })
}

/// `fclose(stream)`. The handle must not be used afterwards.
pub fn fclose(stream: Stream) -> i32 {
    unsafe { file::fclose(stream.0) }
}

/// `fflush(stream)`.
pub fn fflush(stream: Stream) -> i32 {
    unsafe { file::fflush(stream.0) }
}

/// `fflush(NULL)` — flush every open output stream.
pub fn fflush_all() -> i32 {
    unsafe { file::fflush(core::ptr::null_mut()) }
}

/// `fwrite(data, 1, data.len(), stream)` — returns bytes written.
pub fn fwrite(stream: Stream, data: &[u8]) -> usize {
    unsafe { file::fwrite(data.as_ptr(), 1, data.len(), stream.0) }
}

/// `fputc(c, stream)`.
pub fn fputc(c: u8, stream: Stream) -> i32 {
    unsafe { chars::fputc(c as i32, stream.0) }
}

/// `fgetc(stream)`.
pub fn fgetc(stream: Stream) -> i32 {
    unsafe { chars::fgetc(stream.0) }
}

/// `fgets(out, out.len(), stream)` — returns the bytes stored ahead of the
/// terminating NUL, or `None` at end of file.
pub fn fgets(stream: Stream, out: &mut [u8]) -> Option<usize> {
    if out.is_empty() {
        return None;
    }
    let cap = if out.len() > i32::MAX as usize {
        i32::MAX
    } else {
        out.len() as i32
    };
    if unsafe { chars::fgets(out.as_mut_ptr(), cap, stream.0) }.is_null() {
        return None;
    }
    Some(out.iter().position(|&b| b == 0).unwrap_or(out.len()))
}

/// `fputs(s, stream)`. `s` must be NUL-terminated.
pub fn fputs(stream: Stream, s: &[u8]) -> i32 {
    unsafe { chars::fputs(s.as_ptr(), stream.0) }
}

/// `fseek(stream, offset, whence)`.
pub fn fseek(stream: Stream, offset: i64, whence: i32) -> i32 {
    unsafe { file::fseek(stream.0, offset, whence) }
}

/// `ferror(stream)`.
pub fn ferror(stream: Stream) -> i32 {
    unsafe { file::ferror(stream.0) }
}

/// `clearerr(stream)`.
pub fn clearerr(stream: Stream) {
    unsafe { file::clearerr(stream.0) }
}

/// `fileno(stream)`.
pub fn fileno(stream: Stream) -> i32 {
    unsafe { file::fileno(stream.0) }
}

/// `setvbuf(stream, NULL, mode, 0)` — mode is `_IOFBF`, `_IOLBF` or `_IONBF`.
pub fn setvbuf_mode(stream: Stream, mode: i32) -> i32 {
    // SAFETY: a null buffer with size 0 is the "keep the internal buffer" request.
    unsafe { file::setvbuf(stream.0, core::ptr::null_mut(), mode, 0) }
}

/// `flockfile(stream)`.
pub fn flockfile(stream: Stream) {
    unsafe { file::flockfile(stream.0) }
}

/// `funlockfile(stream)`.
pub fn funlockfile(stream: Stream) {
    // SAFETY: the caller holds the lock.
    unsafe { file::funlockfile(stream.0) }
}

/// `open(path, O_RDWR|O_CREAT|O_TRUNC, 0666)` — a bare descriptor for
/// `fdopen` to adopt. `path` must be NUL-terminated. Returns -1 on failure.
pub fn open_rw_create(path: &[u8]) -> i32 {
    use crate::ffi::{O_CREAT, O_RDWR, O_TRUNC};
    match Sys::open(path.as_ptr(), O_RDWR | O_CREAT | O_TRUNC, 0o666) {
        Ok(fd) => fd,
        Err(_) => -1,
    }
}

/// `read(fd, out)`, bypassing stdio — the way to observe where a stream left
/// its descriptor. Returns the byte count, or -1.
pub fn read_fd(fd: i32, out: &mut [u8]) -> isize {
    match Sys::read(fd, out.as_mut_ptr(), out.len()) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}
