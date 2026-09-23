use core::fmt;

/// Syscall error with errno-compatible representation.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(transparent)]
pub struct SyscallError(i32);

impl SyscallError {
    pub const EPERM: Self = Self(1);
    pub const ENOENT: Self = Self(2);
    pub const ESRCH: Self = Self(3);
    pub const EINTR: Self = Self(4);
    pub const EIO: Self = Self(5);
    pub const ENXIO: Self = Self(6);
    pub const E2BIG: Self = Self(7);
    pub const ENOEXEC: Self = Self(8);
    pub const EBADF: Self = Self(9);
    pub const ECHILD: Self = Self(10);
    pub const EAGAIN: Self = Self(11);
    pub const ENOMEM: Self = Self(12);
    pub const EACCES: Self = Self(13);
    pub const EFAULT: Self = Self(14);
    pub const EBUSY: Self = Self(16);
    pub const EEXIST: Self = Self(17);
    pub const EXDEV: Self = Self(18);
    pub const ENODEV: Self = Self(19);
    pub const ENOTDIR: Self = Self(20);
    pub const EISDIR: Self = Self(21);
    pub const EINVAL: Self = Self(22);
    pub const EMFILE: Self = Self(24);
    pub const EFBIG: Self = Self(27);
    pub const ENOSPC: Self = Self(28);
    pub const ESPIPE: Self = Self(29);
    pub const EROFS: Self = Self(30);
    pub const EPIPE: Self = Self(32);
    pub const ENOSYS: Self = Self(38);
    pub const ENETUNREACH: Self = Self(101);
    pub const ETIMEDOUT: Self = Self(110);
    pub const ECONNREFUSED: Self = Self(111);
    pub const EHOSTUNREACH: Self = Self(113);

    #[inline]
    pub const fn from_errno(errno: i32) -> Self {
        Self(errno)
    }

    #[inline]
    pub const fn errno(self) -> i32 {
        self.0
    }

    /// Every Linux x86-64 errno, 1 through 133, because `strerror_r` is
    /// expected to answer for all of them: a hole here reads as "Unknown
    /// error" for a condition the library or a program produces.
    pub const fn as_str(self) -> &'static str {
        match self.0 {
            0 => "Success",
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
            15 => "Block device required",
            16 => "Device or resource busy",
            17 => "File exists",
            18 => "Cross-device link",
            19 => "No such device",
            20 => "Not a directory",
            21 => "Is a directory",
            22 => "Invalid argument",
            23 => "Too many open files in system",
            24 => "Too many open files",
            25 => "Inappropriate ioctl for device",
            26 => "Text file busy",
            27 => "File too large",
            28 => "No space left on device",
            29 => "Illegal seek",
            30 => "Read-only file system",
            31 => "Too many links",
            32 => "Broken pipe",
            33 => "Numerical argument out of domain",
            34 => "Numerical result out of range",
            35 => "Resource deadlock avoided",
            36 => "File name too long",
            37 => "No locks available",
            38 => "Function not implemented",
            39 => "Directory not empty",
            40 => "Too many levels of symbolic links",
            42 => "No message of desired type",
            43 => "Identifier removed",
            44 => "Channel number out of range",
            45 => "Level 2 not synchronized",
            46 => "Level 3 halted",
            47 => "Level 3 reset",
            48 => "Link number out of range",
            49 => "Protocol driver not attached",
            50 => "No CSI structure available",
            51 => "Level 2 halted",
            52 => "Invalid exchange",
            53 => "Invalid request descriptor",
            54 => "Exchange full",
            55 => "No anode",
            56 => "Invalid request code",
            57 => "Invalid slot",
            59 => "Bad font file format",
            60 => "Device not a stream",
            61 => "No data available",
            62 => "Timer expired",
            63 => "Out of streams resources",
            64 => "Machine is not on the network",
            65 => "Package not installed",
            66 => "Object is remote",
            67 => "Link has been severed",
            68 => "Advertise error",
            69 => "Srmount error",
            70 => "Communication error on send",
            71 => "Protocol error",
            72 => "Multihop attempted",
            73 => "RFS specific error",
            74 => "Bad message",
            75 => "Value too large for defined data type",
            76 => "Name not unique on network",
            77 => "File descriptor in bad state",
            78 => "Remote address changed",
            79 => "Can not access a needed shared library",
            80 => "Accessing a corrupted shared library",
            81 => ".lib section in a.out corrupted",
            82 => "Attempting to link in too many shared libraries",
            83 => "Cannot exec a shared library directly",
            84 => "Invalid or incomplete multibyte or wide character",
            85 => "Interrupted system call should be restarted",
            86 => "Streams pipe error",
            87 => "Too many users",
            88 => "Socket operation on non-socket",
            89 => "Destination address required",
            90 => "Message too long",
            91 => "Protocol wrong type for socket",
            92 => "Protocol not available",
            93 => "Protocol not supported",
            94 => "Socket type not supported",
            95 => "Operation not supported",
            96 => "Protocol family not supported",
            97 => "Address family not supported by protocol",
            98 => "Address already in use",
            99 => "Cannot assign requested address",
            100 => "Network is down",
            101 => "Network is unreachable",
            102 => "Network dropped connection on reset",
            103 => "Software caused connection abort",
            104 => "Connection reset by peer",
            105 => "No buffer space available",
            106 => "Transport endpoint is already connected",
            107 => "Transport endpoint is not connected",
            108 => "Cannot send after transport endpoint shutdown",
            109 => "Too many references: cannot splice",
            110 => "Connection timed out",
            111 => "Connection refused",
            112 => "Host is down",
            113 => "No route to host",
            114 => "Operation already in progress",
            115 => "Operation now in progress",
            116 => "Stale file handle",
            117 => "Structure needs cleaning",
            118 => "Not a XENIX named type file",
            119 => "No XENIX semaphores available",
            120 => "Is a named type file",
            121 => "Remote I/O error",
            122 => "Disk quota exceeded",
            123 => "No medium found",
            124 => "Wrong medium type",
            125 => "Operation canceled",
            126 => "Required key not available",
            127 => "Key has expired",
            128 => "Key has been revoked",
            129 => "Key was rejected by service",
            130 => "Owner died",
            131 => "State not recoverable",
            132 => "Operation not possible due to RF-kill",
            133 => "Memory page has hardware error",
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

impl From<crate::errno::Errno> for SyscallError {
    #[inline]
    fn from(e: crate::errno::Errno) -> Self {
        SyscallError::from_errno(e.raw())
    }
}

pub type SyscallResult<T> = Result<T, SyscallError>;

/// Raw syscall returns in [-4095, -1] are errors carrying a negated errno.
#[inline]
pub fn demux(value: u64) -> SyscallResult<u64> {
    let signed = value as i64;
    if signed >= -4095 && signed < 0 {
        Err(SyscallError((-signed) as i32))
    } else {
        Ok(value)
    }
}

#[inline]
pub fn mux(result: SyscallResult<u64>) -> u64 {
    match result {
        Ok(v) => v,
        Err(e) => (-e.0 as i64) as u64,
    }
}
