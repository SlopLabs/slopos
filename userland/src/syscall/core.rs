//! Core syscalls: yield, exit, sleep, time, CPU info.

use super::numbers::*;
use super::raw::{syscall0, syscall1};

#[inline(always)]
pub fn yield_now() {
    unsafe {
        syscall0(SYSCALL_YIELD);
    }
}

#[inline(always)]
pub fn exit() -> ! {
    unsafe {
        syscall1(SYSCALL_EXIT, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[inline(always)]
pub fn exit_with_code(code: i32) -> ! {
    unsafe {
        syscall1(SYSCALL_EXIT, code as u64);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[inline(always)]
pub fn sleep_ms(ms: u32) {
    unsafe {
        syscall1(SYSCALL_SLEEP_MS, ms as u64);
    }
}

#[inline(always)]
pub fn get_time_ms() -> u64 {
    unsafe { syscall0(SYSCALL_GET_TIME_MS) }
}

#[inline(always)]
pub fn get_cpu_count() -> u32 {
    unsafe { syscall0(SYSCALL_GET_CPU_COUNT) as u32 }
}

#[inline(always)]
pub fn get_current_cpu() -> u32 {
    unsafe { syscall0(SYSCALL_GET_CURRENT_CPU) as u32 }
}

#[inline(always)]
pub fn random_next() -> u32 {
    unsafe { syscall0(SYSCALL_RANDOM_NEXT) as u32 }
}

#[inline(always)]
pub fn sys_info(info: &mut UserSysInfo) -> i64 {
    unsafe { syscall1(SYSCALL_SYS_INFO, info as *mut _ as u64) as i64 }
}
