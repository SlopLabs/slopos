//! TTY/Console I/O syscalls (NOT file descriptor operations).

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2};

#[inline(always)]
pub fn write(buf: &[u8]) -> i64 {
    unsafe { syscall2(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
pub fn read(buf: &mut [u8]) -> i64 {
    unsafe { syscall2(SYSCALL_READ, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
pub fn read_char() -> i64 {
    unsafe { syscall0(SYSCALL_READ_CHAR) as i64 }
}

#[inline(always)]
pub fn set_focus(task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_TTY_SET_FOCUS, task_id as u64) as i64 }
}
