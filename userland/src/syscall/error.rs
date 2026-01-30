//! Syscall error handling with errno-compatible representation.
//!
//! This module provides the single point of error conversion for all syscalls.
//! The `demux()` function converts raw kernel return values to `Result<u64, SyscallError>`.

use core::fmt;

/// Syscall error with errno-compatible representation.
///
/// Values correspond to standard POSIX errno codes for compatibility.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(transparent)]
pub struct SyscallError(i32);

impl SyscallError {
    /// Operation not permitted
    pub const EPERM: Self = Self(1);
    /// No such file or directory
    pub const ENOENT: Self = Self(2);
    /// No such process
    pub const ESRCH: Self = Self(3);
    /// Interrupted system call
    pub const EINTR: Self = Self(4);
    /// I/O error
    pub const EIO: Self = Self(5);
    /// No such device or address
    pub const ENXIO: Self = Self(6);
    /// Argument list too long
    pub const E2BIG: Self = Self(7);
    /// Exec format error
    pub const ENOEXEC: Self = Self(8);
    /// Bad file descriptor
    pub const EBADF: Self = Self(9);
    /// No child processes
    pub const ECHILD: Self = Self(10);
    /// Try again / Resource temporarily unavailable
    pub const EAGAIN: Self = Self(11);
    /// Out of memory
    pub const ENOMEM: Self = Self(12);
    /// Permission denied
    pub const EACCES: Self = Self(13);
    /// Bad address
    pub const EFAULT: Self = Self(14);
    /// Device or resource busy
    pub const EBUSY: Self = Self(16);
    /// File exists
    pub const EEXIST: Self = Self(17);
    /// Cross-device link
    pub const EXDEV: Self = Self(18);
    /// No such device
    pub const ENODEV: Self = Self(19);
    /// Not a directory
    pub const ENOTDIR: Self = Self(20);
    /// Is a directory
    pub const EISDIR: Self = Self(21);
    /// Invalid argument
    pub const EINVAL: Self = Self(22);
    /// Too many open files
    pub const EMFILE: Self = Self(24);
    /// File too large
    pub const EFBIG: Self = Self(27);
    /// No space left on device
    pub const ENOSPC: Self = Self(28);
    /// Illegal seek
    pub const ESPIPE: Self = Self(29);
    /// Read-only file system
    pub const EROFS: Self = Self(30);
    /// Broken pipe
    pub const EPIPE: Self = Self(32);
    /// Function not implemented
    pub const ENOSYS: Self = Self(38);
    /// Connection refused
    pub const ECONNREFUSED: Self = Self(111);

    /// Create a SyscallError from a raw errno value.
    #[inline]
    pub const fn from_errno(errno: i32) -> Self {
        Self(errno)
    }

    /// Get the raw errno value.
    #[inline]
    pub const fn errno(self) -> i32 {
        self.0
    }

    /// Get a human-readable description of the error.
    pub const fn as_str(self) -> &'static str {
        match self.0 {
            1 => "Operation not permitted",
            2 => "No such file or directory",
            3 => "No such process",
            4 => "Interrupted system call",
            5 => "I/O error",
            6 => "No such device or address",
            7 => "Argument list too long",
            8 => "Exec format error",
            9 => "Bad file descriptor",
            10 => "No child processes",
            11 => "Resource temporarily unavailable",
            12 => "Out of memory",
            13 => "Permission denied",
            14 => "Bad address",
            16 => "Device or resource busy",
            17 => "File exists",
            18 => "Cross-device link",
            19 => "No such device",
            20 => "Not a directory",
            21 => "Is a directory",
            22 => "Invalid argument",
            24 => "Too many open files",
            27 => "File too large",
            28 => "No space left on device",
            29 => "Illegal seek",
            30 => "Read-only file system",
            32 => "Broken pipe",
            38 => "Function not implemented",
            111 => "Connection refused",
            _ => "Unknown error",
        }
    }
}

impl fmt::Debug for SyscallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SyscallError({}: {})", self.0, self.as_str())
    }
}

impl fmt::Display for SyscallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Result type for syscall operations.
pub type SyscallResult<T> = Result<T, SyscallError>;

/// Convert raw syscall return value to Result (SINGLE CONVERSION POINT).
///
/// Linux/SlopOS convention: negative values in the range [-4095, -1] indicate errors,
/// with the negated value being the errno.
///
/// # Examples
///
/// ```ignore
/// let result = unsafe { syscall1(SYSCALL_FS_CLOSE, fd) };
/// match demux(result) {
///     Ok(_) => println!("closed successfully"),
///     Err(e) => println!("error: {}", e),
/// }
/// ```
#[inline]
pub fn demux(value: u64) -> SyscallResult<u64> {
    let signed = value as i64;
    if signed >= -4095 && signed < 0 {
        Err(SyscallError((-signed) as i32))
    } else {
        Ok(value)
    }
}

/// Convert Result to raw syscall return value (for kernel use).
///
/// This is the inverse of `demux()`.
#[inline]
pub fn mux(result: SyscallResult<u64>) -> u64 {
    match result {
        Ok(v) => v,
        Err(e) => (-e.0 as i64) as u64,
    }
}
