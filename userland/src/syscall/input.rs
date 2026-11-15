//! Input event syscalls.

use super::numbers::*;
use super::raw::syscall2;
use slopos_abi::InputEvent;

/// Drain queued input events into `events`, returning how many were written.
/// A negative errno is clamped to 0: raw, it reads as a huge unsigned count and
/// panics the caller's `events[..count]` indexing.
#[inline(always)]
pub fn poll_batch(events: &mut [InputEvent]) -> usize {
    let raw = unsafe {
        syscall2(
            SYSCALL_INPUT_POLL_BATCH,
            events.as_mut_ptr() as u64,
            events.len() as u64,
        )
    } as i64;
    raw.clamp(0, events.len() as i64) as usize
}
