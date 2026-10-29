//! Window and surface management syscalls.

use super::numbers::*;
use super::raw::{syscall1, syscall2, syscall3};
use slopos_abi::DisplayInfo;
use slopos_abi::damage::DamageRect;
pub use slopos_abi::window::{SEAT_COMPOSITOR_PRIMARY, SEAT_VIRTCON};

/// Take the framebuffer seat, returning a non-duplicable descriptor naming it.
///
/// Must be held before `fb_flip`, the cursor calls or `set_display_mode`
/// will act. `-EBUSY` when a seat of equal or higher rank is
/// held by a live task. The fd is neither inherited across `fork` nor kept
/// across `exec`.
#[inline(always)]
pub fn screen_acquire(seat: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SCREEN_ACQUIRE, seat as u64) as i64 }
}

/// Give the screen and the input sink to the compositor `task_id`, whoever
/// holds them; init's alone.
#[inline(always)]
pub fn seat_switch(task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SEAT_SWITCH, u64::from(task_id)) as i64 }
}

/// As [`screen_acquire`], for the raw input stream `input_poll_batch` drains.
#[inline(always)]
pub fn input_sink_acquire(seat: u32) -> i64 {
    unsafe { syscall1(SYSCALL_INPUT_SINK_ACQUIRE, seat as u64) as i64 }
}

#[inline(always)]
pub fn fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall1(SYSCALL_FB_INFO, out as *mut _ as u64) as i64 }
}

#[inline(always)]
pub fn fb_flip(memfd_fd: u32) -> i64 {
    unsafe { syscall3(SYSCALL_FB_FLIP, memfd_fd as u64, 0, 0) as i64 }
}

#[inline(always)]
pub fn fb_flip_damage(memfd_fd: u32, damage: &[DamageRect]) -> i64 {
    if damage.is_empty() {
        return fb_flip(memfd_fd);
    }
    unsafe {
        syscall3(
            SYSCALL_FB_FLIP,
            memfd_fd as u64,
            damage.as_ptr() as u64,
            damage.len() as u64,
        ) as i64
    }
}

/// Upload a 64×64 BGRA hardware-cursor image; `hot_x`/`hot_y` are the hotspot
/// within it. Returns 0, or negative when there is no hardware cursor and the
/// caller must composite in software.
#[inline(always)]
pub fn cursor_set_image(image: &[u8], hot_x: u32, hot_y: u32) -> i64 {
    let hotspot = ((hot_x & 0xFFFF) << 16) | (hot_y & 0xFFFF);
    unsafe {
        syscall3(
            SYSCALL_CURSOR_SET_IMAGE,
            image.as_ptr() as u64,
            image.len() as u64,
            hotspot as u64,
        ) as i64
    }
}

/// Move the hardware cursor to absolute display coordinates.
#[inline(always)]
pub fn cursor_move(x: u32, y: u32) -> i64 {
    let pos = ((x & 0xFFFF) << 16) | (y & 0xFFFF);
    unsafe { syscall1(SYSCALL_CURSOR_MOVE, pos as u64) as i64 }
}

#[inline(always)]
pub fn set_display_mode(width: u32, height: u32) -> i64 {
    unsafe { syscall2(SYSCALL_SET_DISPLAY_MODE, width as u64, height as u64) as i64 }
}
