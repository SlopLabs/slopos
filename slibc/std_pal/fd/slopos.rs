//! SlopOS file descriptor abstraction, over slibc C functions.

#![unstable(reason = "not public", issue = "none", feature = "fd")]
#![deny(unsafe_op_in_unsafe_fn)]

use crate::cmp;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, Read};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use crate::sys::pal::cvt;
use crate::sys::{AsInner, FromInner, IntoInner};

unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn dup(oldfd: i32) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i64) -> i32;
    fn slopos_pread(fd: i32, buf: *mut u8, count: usize, offset: i64) -> isize;
    fn slopos_pwrite(fd: i32, buf: *const u8, count: usize, offset: i64) -> isize;
    fn slopos_readv(fd: i32, iov: *const Iovec, iovcnt: i32) -> isize;
    fn slopos_writev(fd: i32, iov: *const Iovec, iovcnt: i32) -> isize;
}

/// Linux `struct iovec`. `IoSlice` is not this layout without the unix
/// `io_slice` arm, so segments are rebuilt per call.
#[repr(C)]
#[derive(Copy, Clone)]
struct Iovec {
    iov_base: u64,
    iov_len: u64,
}

/// Stack-only segment cap. Truncating is safe: a vectored call may be short
/// and std callers loop.
const IOV_STACK_MAX: usize = 16;

/// slibc's wrappers answer a negated errno, not `-1`, so `cvt` misreads them.
fn cvt_count(ret: isize) -> io::Result<usize> {
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as usize)
    }
}

const READ_LIMIT: usize = isize::MAX as usize;

const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const FD_CLOEXEC: i32 = 1;
const O_NONBLOCK: i32 = 0x800;

#[derive(Debug)]
pub struct FileDesc(OwnedFd);

impl FileDesc {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        cvt_count(unsafe {
            read(self.as_raw_fd(), buf.as_mut_ptr(), cmp::min(buf.len(), READ_LIMIT))
        })
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut iov = [Iovec { iov_base: 0, iov_len: 0 }; IOV_STACK_MAX];
        let mut n = 0;
        for buf in bufs.iter_mut() {
            if n == IOV_STACK_MAX {
                break;
            }
            iov[n] = Iovec {
                iov_base: buf.as_mut_ptr() as u64,
                iov_len: buf.len() as u64,
            };
            n += 1;
        }
        if n == 0 {
            return Ok(0);
        }
        cvt_count(unsafe { slopos_readv(self.as_raw_fd(), iov.as_ptr(), n as i32) })
    }

    #[inline]
    pub fn is_read_vectored(&self) -> bool {
        true
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut me = self;
        (&mut me).read_to_end(buf)
    }

    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        cvt_count(unsafe {
            slopos_pread(
                self.as_raw_fd(),
                buf.as_mut_ptr(),
                cmp::min(buf.len(), READ_LIMIT),
                offset as i64,
            )
        })
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        let ret = cvt_count(unsafe {
            read(
                self.as_raw_fd(),
                cursor.as_mut().as_mut_ptr() as *mut u8,
                cmp::min(cursor.capacity(), READ_LIMIT),
            )
        })?;
        unsafe {
            cursor.advance(ret);
        }
        Ok(())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        cvt_count(unsafe {
            write(self.as_raw_fd(), buf.as_ptr(), cmp::min(buf.len(), READ_LIMIT))
        })
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut iov = [Iovec { iov_base: 0, iov_len: 0 }; IOV_STACK_MAX];
        let mut n = 0;
        for buf in bufs.iter() {
            if n == IOV_STACK_MAX {
                break;
            }
            iov[n] = Iovec {
                iov_base: buf.as_ptr() as u64,
                iov_len: buf.len() as u64,
            };
            n += 1;
        }
        if n == 0 {
            return Ok(0);
        }
        cvt_count(unsafe { slopos_writev(self.as_raw_fd(), iov.as_ptr(), n as i32) })
    }

    #[inline]
    pub fn is_write_vectored(&self) -> bool {
        true
    }

    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        cvt_count(unsafe {
            slopos_pwrite(
                self.as_raw_fd(),
                buf.as_ptr(),
                cmp::min(buf.len(), READ_LIMIT),
                offset as i64,
            )
        })
    }

    pub fn set_cloexec(&self) -> io::Result<()> {
        cvt(unsafe { fcntl(self.as_raw_fd(), F_SETFD, FD_CLOEXEC as i64) })?;
        Ok(())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let flags = cvt(unsafe { fcntl(self.as_raw_fd(), F_GETFL, 0) })?;
        let flags = if nonblocking {
            flags | O_NONBLOCK
        } else {
            flags & !O_NONBLOCK
        };
        cvt(unsafe { fcntl(self.as_raw_fd(), F_SETFL, flags as i64) })?;
        Ok(())
    }

    pub fn duplicate(&self) -> io::Result<FileDesc> {
        let fd = cvt(unsafe { dup(self.as_raw_fd()) })?;
        Ok(FileDesc(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

impl<'a> Read for &'a FileDesc {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read(buf)
    }
}

impl AsInner<OwnedFd> for FileDesc {
    #[inline]
    fn as_inner(&self) -> &OwnedFd {
        &self.0
    }
}

impl IntoInner<OwnedFd> for FileDesc {
    fn into_inner(self) -> OwnedFd {
        self.0
    }
}

impl FromInner<OwnedFd> for FileDesc {
    fn from_inner(owned_fd: OwnedFd) -> Self {
        Self(owned_fd)
    }
}

impl AsFd for FileDesc {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for FileDesc {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for FileDesc {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl FromRawFd for FileDesc {
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        Self(unsafe { OwnedFd::from_raw_fd(raw_fd) })
    }
}
