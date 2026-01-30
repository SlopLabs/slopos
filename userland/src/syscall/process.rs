//! Process management syscalls: spawn, exec, fork, halt.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2};

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn spawn(name: &[u8]) -> i32 {
    unsafe { syscall2(SYSCALL_SPAWN_TASK, name.as_ptr() as u64, name.len() as u64) as i32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn exec(path: &[u8]) -> i64 {
    unsafe { syscall1(SYSCALL_EXEC, path.as_ptr() as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn fork() -> i32 {
    unsafe { syscall0(SYSCALL_FORK) as i32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn halt() -> ! {
    unsafe {
        syscall0(SYSCALL_HALT);
    }
    loop {
        core::hint::spin_loop();
    }
}
