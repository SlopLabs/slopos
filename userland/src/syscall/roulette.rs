//! Wheel of Fate syscalls.

use super::numbers::*;
use super::raw::{syscall0, syscall1};

#[inline(always)]
pub fn spin() -> u64 {
    unsafe { syscall0(SYSCALL_ROULETTE) }
}

#[inline(always)]
pub fn result(fate_packed: u64) {
    unsafe {
        syscall1(SYSCALL_ROULETTE_RESULT, fate_packed);
    }
}

#[inline(always)]
pub fn draw(fate: u32) -> i64 {
    unsafe { syscall1(SYSCALL_ROULETTE_DRAW, fate as u64) as i64 }
}
