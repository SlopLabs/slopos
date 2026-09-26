//! UEFI variables, through the kernel's firmware thread. Both calls need
//! `TASK_FLAG_POWER`.

use super::error::{SyscallError, SyscallResult};
use super::numbers::{SYSCALL_EFIVAR_GET, SYSCALL_EFIVAR_SET};
use super::raw::{syscall5, syscall6};

/// The Boot Loader Interface's vendor GUID, 4a67b082-0a4c-41cf-b6c7-440b29bb8c4f,
/// in `EFI_GUID`'s in-memory layout.
pub const LOADER_GUID: [u8; 16] = [
    0x82, 0xb0, 0x67, 0x4a, 0x4c, 0x0a, 0xcf, 0x41, 0xb6, 0xc7, 0x44, 0x0b, 0x29, 0xbb, 0x8c, 0x4f,
];

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

/// A Boot Loader Interface string variable: UTF-16LE with a terminator.
pub fn loader_string(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .chain(core::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// A Boot Loader Interface string variable back to text; `None` for one that
/// is not well-formed UTF-16.
pub fn loader_string_value(raw: &[u8]) -> Option<String> {
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16(&units).ok()
}
