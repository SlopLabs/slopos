use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_abi::damage::DamageRect;
use slopos_abi::DisplayInfo;

use super::scanout::Present;

/// Task ID of the compositor process, set when it acquires the screen.
static COMPOSITOR_TASK_ID: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn set_compositor_task_id(task_id: u32) {
    COMPOSITOR_TASK_ID.store(task_id, Ordering::Relaxed);
}

/// Recorded compositor task ID; 0 if none.
#[inline]
pub fn compositor_task_id() -> u32 {
    COMPOSITOR_TASK_ID.load(Ordering::Relaxed)
}

slopos_service_core::define_service! {
    video => VideoServices {
        get_display_info() -> Option<DisplayInfo>;
        @no_wrapper fb_flip(phys_addr: PhysAddr, size: usize, damage: &[DamageRect]) -> Present;
        hw_cursor_available() -> bool;
        @no_wrapper cursor_set_image(image: *const u8, len: usize, hot_x: u32, hot_y: u32) -> bool;
        cursor_move(x: u32, y: u32) -> bool;
        set_display_mode(width: u32, height: u32) -> bool;
    }
}

/// Upload a hardware-cursor image (validated kernel buffer) with its hotspot.
#[inline(always)]
pub fn cursor_set_image(image: &[u8], hot_x: u32, hot_y: u32) -> bool {
    (video_services().cursor_set_image)(image.as_ptr(), image.len(), hot_x, hot_y)
}

#[inline(always)]
pub fn fb_flip_from_shm(phys_addr: PhysAddr, size: usize, damage: &[DamageRect]) -> Present {
    (video_services().fb_flip)(phys_addr, size, damage)
}
