//! File descriptor RAII wrapper.

use crate::syscall::error::SyscallResult;
use crate::syscall::{fs, RawFd};

pub struct FdGuard {
    fd: RawFd,
}

impl FdGuard {
    #[inline]
    pub fn open(path: &core::ffi::CStr, flags: u32) -> SyscallResult<Self> {
        fs::open_cstr(path, flags).map(|fd| Self { fd })
    }

    #[inline]
    pub const unsafe fn from_raw(fd: RawFd) -> Self {
        Self { fd }
    }

    #[inline]
    pub const fn as_raw(&self) -> RawFd {
        self.fd
    }

    #[inline]
    pub fn read(&self, buf: &mut [u8]) -> SyscallResult<usize> {
        fs::read_slice(self.fd, buf)
    }

    #[inline]
    pub fn write(&self, buf: &[u8]) -> SyscallResult<usize> {
        fs::write_slice(self.fd, buf)
    }

    #[inline]
    pub fn into_raw(self) -> RawFd {
        let fd = self.fd;
        core::mem::forget(self);
        fd
    }
}

impl Drop for FdGuard {
    #[inline]
    fn drop(&mut self) {
        let _ = fs::close_fd(self.fd);
    }
}
