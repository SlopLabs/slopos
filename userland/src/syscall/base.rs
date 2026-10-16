//! The base: replacing it while the system runs, and hearing that it was.

use super::error::{SyscallError, SyscallResult};
use super::numbers::*;
use super::raw::{syscall0, syscall2};
use super::{OwnedFd, RawFd};

fn checked(raw: i64) -> SyscallResult<u64> {
    if raw < 0 {
        Err(SyscallError::from_errno((-raw) as i32))
    } else {
        Ok(raw as u64)
    }
}

/// Make the base archive `archive` the base; answers its generation.
pub fn base_activate(archive: &[u8]) -> SyscallResult<u32> {
    let raw = unsafe {
        syscall2(
            SYSCALL_BASE_ACTIVATE,
            archive.as_ptr() as u64,
            archive.len() as u64,
        ) as i64
    };
    checked(raw).map(|generation| generation as u32)
}

/// A descriptor that turns readable when the base's generation changes.
pub fn base_monitor() -> SyscallResult<OwnedFd> {
    let raw = unsafe { syscall0(SYSCALL_BASE_MONITOR) as i64 };
    // SAFETY: a non-negative return is an fd the kernel just allocated to us.
    checked(raw).map(|fd| unsafe { OwnedFd::from_raw(fd as i32) })
}

/// The generation `monitor` answers, which it then measures changes from.
pub fn base_generation(monitor: RawFd) -> SyscallResult<u32> {
    let mut bytes = [0u8; 8];
    match super::fs::read_slice(monitor, &mut bytes)? {
        8 => Ok(u64::from_le_bytes(bytes) as u32),
        _ => Err(SyscallError::EIO),
    }
}
