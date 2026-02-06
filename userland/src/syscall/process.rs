//! Process management syscalls: spawn, exec, fork, halt.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall4};

#[inline(always)]
pub fn spawn_path(path: &[u8]) -> i32 {
    spawn_path_with_attrs(path, 5, 0)
}

#[inline(always)]
pub fn spawn_path_with_attrs(path: &[u8], priority: u8, flags: u16) -> i32 {
    unsafe {
        syscall4(
            SYSCALL_SPAWN_PATH,
            path.as_ptr() as u64,
            path.len() as u64,
            priority as u64,
            flags as u64,
        ) as i32
    }
}

#[inline(always)]
pub fn waitpid(task_id: u32) -> i32 {
    unsafe { syscall1(SYSCALL_WAITPID, task_id as u64) as i32 }
}

#[inline(always)]
pub fn exec(path: &[u8]) -> i64 {
    unsafe { syscall1(SYSCALL_EXEC, path.as_ptr() as u64) as i64 }
}

#[inline(always)]
pub fn fork() -> i32 {
    unsafe { syscall0(SYSCALL_FORK) as i32 }
}

#[inline(always)]
pub fn halt() -> ! {
    unsafe {
        syscall0(SYSCALL_HALT);
    }
    loop {
        core::hint::spin_loop();
    }
}
