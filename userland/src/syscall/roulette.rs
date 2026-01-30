//! Wheel of Fate syscalls.

use super::numbers::*;
use super::raw::{syscall0, syscall1};

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn spin() -> u64 {
    unsafe { syscall0(SYSCALL_ROULETTE) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn result(fate_packed: u64) {
    unsafe {
        syscall1(SYSCALL_ROULETTE_RESULT, fate_packed);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn draw(fate: u32) -> i64 {
    unsafe { syscall1(SYSCALL_ROULETTE_DRAW, fate as u64) as i64 }
}
