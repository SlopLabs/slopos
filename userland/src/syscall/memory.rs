//! Memory management syscalls: brk, sbrk, shared memory.

use core::ffi::c_void;

use super::numbers::*;
use super::raw::{syscall1, syscall2};
use slopos_abi::PixelFormat;

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn brk(addr: *mut c_void) -> *mut c_void {
    unsafe { syscall1(SYSCALL_BRK, addr as u64) as *mut c_void }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sbrk(increment: isize) -> *mut c_void {
    unsafe {
        let current = syscall1(SYSCALL_BRK, 0) as usize;
        if increment == 0 {
            return current as *mut c_void;
        }
        let new_brk = if increment > 0 {
            current.wrapping_add(increment as usize)
        } else {
            current.wrapping_sub((-increment) as usize)
        };
        let result = syscall1(SYSCALL_BRK, new_brk as u64) as usize;
        if result == new_brk {
            current as *mut c_void
        } else {
            usize::MAX as *mut c_void
        }
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_create(size: u64, flags: u32) -> u32 {
    unsafe { syscall2(SYSCALL_SHM_CREATE, size, flags as u64) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_create_with_format(size: u64, format: PixelFormat) -> u32 {
    unsafe { syscall2(SYSCALL_SHM_CREATE_WITH_FORMAT, size, format as u64) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_map(token: u32, access: u32) -> u64 {
    unsafe { syscall2(SYSCALL_SHM_MAP, token as u64, access as u64) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn shm_unmap(virt_addr: u64) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_UNMAP, virt_addr) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_destroy(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_DESTROY, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_acquire(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_ACQUIRE, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_release(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_RELEASE, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_poll_released(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_POLL_RELEASED, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn shm_get_formats() -> u32 {
    unsafe { syscall0(SYSCALL_SHM_GET_FORMATS) as u32 }
}

use super::raw::syscall0;
