//! UEFI variables, through the kernel's firmware thread. Both calls need
//! `TASK_FLAG_POWER`, and the firmware's boot manager variables the
//! installer's role as well.

use super::error::{SyscallError, SyscallResult};
use super::numbers::{SYSCALL_EFIVAR_GET, SYSCALL_EFIVAR_SET};
use super::raw::{syscall5, syscall6};

fn checked(raw: i64) -> SyscallResult<u64> {
    if raw < 0 {
        Err(SyscallError::from_errno((-raw) as i32))
    } else {
        Ok(raw as u64)
    }
}

/// Read variable `name` into `buf`, answering how many bytes it holds.
pub fn efivar_get(name: &str, guid: &[u8; 16], buf: &mut [u8]) -> SyscallResult<usize> {
    let raw = unsafe {
        syscall5(
            SYSCALL_EFIVAR_GET,
            name.as_ptr() as u64,
            name.len() as u64,
            guid.as_ptr() as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        ) as i64
    };
    checked(raw).map(|n| n as usize)
}

/// Write variable `name`; an empty `data` deletes it.
pub fn efivar_set(name: &str, guid: &[u8; 16], attributes: u32, data: &[u8]) -> SyscallResult<()> {
    let raw = unsafe {
        syscall6(
            SYSCALL_EFIVAR_SET,
            name.as_ptr() as u64,
            name.len() as u64,
            guid.as_ptr() as u64,
            attributes as u64,
            data.as_ptr() as u64,
            data.len() as u64,
        ) as i64
    };
    checked(raw).map(|_| ())
}
