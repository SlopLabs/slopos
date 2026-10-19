//! The base: replacing it while the system runs, and hearing that it was.
//! Extensions: showing one beside it, and taking one away.

use super::error::{SyscallError, SyscallResult};
use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3};
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

fn nul_terminated(name: &[u8]) -> SyscallResult<Vec<u8>> {
    if name.contains(&0) {
        return Err(SyscallError::EINVAL);
    }
    let mut owned = name.to_vec();
    owned.push(0);
    Ok(owned)
}

/// Activate the extension image `fd` reads, whose tree's root the caller
/// vouches is `digest`, and show it at `/opt/<name>`; answers its activation.
pub fn ext_activate(name: &[u8], fd: RawFd, digest: &[u8; 32]) -> SyscallResult<u32> {
    let name = nul_terminated(name)?;
    let raw = unsafe {
        syscall3(
            SYSCALL_EXT_ACTIVATE,
            name.as_ptr() as u64,
            fd as u64,
            digest.as_ptr() as u64,
        ) as i64
    };
    checked(raw).map(|activation| activation as u32)
}

/// Take away the extension shown at `/opt/<name>`.
pub fn ext_deactivate(name: &[u8]) -> SyscallResult<()> {
    let name = nul_terminated(name)?;
    let raw = unsafe { syscall1(SYSCALL_EXT_DEACTIVATE, name.as_ptr() as u64) as i64 };
    checked(raw).map(drop)
}
