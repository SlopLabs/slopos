use core::ffi::c_int;
use core::ptr;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::{DisplayInfo, PixelFormat};
use slopos_lib::{IrqMutex, klog_debug, klog_warn};
use slopos_mm::hhdm::PhysAddrHhdm;

const MIN_FRAMEBUFFER_WIDTH: u32 = 320;
const MIN_FRAMEBUFFER_HEIGHT: u32 = 240;
const MAX_BUFFER_SIZE: u32 = 64 * 1024 * 1024;

#[derive(Copy, Clone)]
pub(crate) struct FbState {
    pub(crate) base: VirtAddr,
    pub(crate) info: DisplayInfo,
}

impl FbState {
    #[inline]
    pub(crate) fn width(&self) -> u32 {
        self.info.width
    }

    #[inline]
    pub(crate) fn height(&self) -> u32 {
        self.info.height
    }

    #[inline]
    pub(crate) fn pitch(&self) -> u32 {
        self.info.pitch
    }

    #[inline]
    pub(crate) fn bpp(&self) -> u8 {
        self.info.bytes_per_pixel() * 8
    }

    #[inline]
    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base.as_mut_ptr()
    }

    #[inline]
    pub(crate) fn draw_pixel_format(&self) -> PixelFormat {
        self.info.format
    }

    #[inline]
    pub(crate) fn buffer_size(&self) -> usize {
        self.info.buffer_size() as usize
    }

    #[inline]
    fn checked_offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let bytes_pp = self.info.bytes_per_pixel() as usize;
        let pitch = self.pitch() as usize;
        let offset = (y as usize)
            .checked_mul(pitch)?
            .checked_add((x as usize).checked_mul(bytes_pp)?)?;
        if offset < self.buffer_size() {
            Some(offset)
        } else {
            None
        }
    }

    #[inline]
    fn checked_ptr(&self, offset: usize, len: usize) -> Option<*mut u8> {
        let end = offset.checked_add(len)?;
        if end > self.buffer_size() {
            return None;
        }
        let base = self.base_ptr();
        if base.is_null() {
            return None;
        }
        // SAFETY: offset and len were bounds-checked against framebuffer size above.
        Some(unsafe { base.add(offset) })
    }
}

struct FramebufferState {
    fb: Option<FbState>,
}

impl FramebufferState {
    const fn new() -> Self {
        Self { fb: None }
    }
}

static FRAMEBUFFER: IrqMutex<FramebufferState> = IrqMutex::new(FramebufferState::new());
static FRAMEBUFFER_FLUSH: IrqMutex<Option<fn() -> c_int>> = IrqMutex::new(None);

fn init_state_from_raw(addr: u64, width: u32, height: u32, pitch: u32, bpp: u8) -> i32 {
    if addr == 0 || width < MIN_FRAMEBUFFER_WIDTH || width > DisplayInfo::MAX_DIMENSION {
        return -1;
    }
    if height < MIN_FRAMEBUFFER_HEIGHT || height > DisplayInfo::MAX_DIMENSION {
        return -1;
    }
    if bpp != 16 && bpp != 24 && bpp != 32 {
        return -1;
    }
    let _buffer_size = match pitch.checked_mul(height) {
        Some(sz) if sz > 0 && sz <= MAX_BUFFER_SIZE => sz,
        _ => return -1,
    };

    let mapped_base = if let Some(hhdm_base) = slopos_mm::hhdm::try_offset() {
        if addr >= hhdm_base {
            VirtAddr::try_new(addr).unwrap_or(VirtAddr::NULL)
        } else {
            PhysAddr::try_new(addr)
                .and_then(|phys| phys.to_virt_checked())
                .unwrap_or(VirtAddr::NULL)
        }
    } else {
        PhysAddr::try_new(addr)
            .and_then(|phys| phys.to_virt_checked())
            .unwrap_or(VirtAddr::NULL)
    };

    if mapped_base.is_null() {
        return -1;
    }

    let display_info = DisplayInfo::new(width, height, pitch, PixelFormat::from_bpp(bpp));

    let fb_state = FbState {
        base: mapped_base,
        info: display_info,
    };

    let mut guard = FRAMEBUFFER.lock();
    guard.fb = Some(fb_state);
    0
}

pub fn init_with_display_info(address: *mut u8, info: &DisplayInfo) -> i32 {
    let rc = init_state_from_raw(
        address as u64,
        info.width,
        info.height,
        info.pitch,
        info.bytes_per_pixel() * 8,
    );

    if rc == 0 {
        if let Some(fb) = FRAMEBUFFER.lock().fb {
            klog_debug!(
                "Framebuffer init: phys=0x{:x} virt=0x{:x} {}x{} pitch={} bpp={}",
                address as u64,
                fb.base.as_u64(),
                fb.width(),
                fb.height(),
                fb.pitch(),
                fb.bpp()
            );
        } else {
            klog_warn!("Framebuffer init: state missing after init");
        }
    } else {
        klog_warn!(
            "Framebuffer init failed: phys=0x{:x} {}x{} pitch={} bpp={}",
            address as u64,
            info.width,
            info.height,
            info.pitch,
            info.bytes_per_pixel() * 8
        );
    }

    rc
}
pub fn get_display_info() -> Option<DisplayInfo> {
    FRAMEBUFFER.lock().fb.map(|fb| fb.info)
}

pub fn framebuffer_is_initialized() -> i32 {
    FRAMEBUFFER.lock().fb.is_some() as i32
}

pub fn framebuffer_clear(color: u32) {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return,
    };

    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let converted = fb.draw_pixel_format().convert_color(color);
    let base = fb.base_ptr();
    let pitch = fb.pitch() as usize;
    let width = fb.width() as usize;
    let height = fb.height() as usize;

    if bytes_pp == 4 {
        let b0 = (converted & 0xFF) as u8;
        let b1 = ((converted >> 8) & 0xFF) as u8;
        let b2 = ((converted >> 16) & 0xFF) as u8;
        let b3 = ((converted >> 24) & 0xFF) as u8;

        if b0 == b1 && b1 == b2 && b2 == b3 {
            // Fast path: all bytes identical (black, white, grey) - bulk memset per row
            let row_bytes = width * 4;
            for y in 0..height {
                let row_ptr = unsafe { base.add(y * pitch) };
                unsafe { ptr::write_bytes(row_ptr, b0, row_bytes) };
            }
        } else {
            // 64-bit writes (2 pixels at a time) for non-uniform colors
            let color64 = (converted as u64) | ((converted as u64) << 32);
            let pairs = width / 2;
            let remainder = width % 2;

            for y in 0..height {
                let row_ptr = unsafe { base.add(y * pitch) };
                unsafe {
                    let mut ptr64 = row_ptr as *mut u64;
                    for _ in 0..pairs {
                        ptr64.write_volatile(color64);
                        ptr64 = ptr64.add(1);
                    }
                    if remainder > 0 {
                        (ptr64 as *mut u32).write_volatile(converted);
                    }
                }
            }
        }
    } else {
        // Fallback for 2bpp/3bpp
        for y in 0..height {
            let row_ptr = unsafe { base.add(y * pitch) };
            for x in 0..width {
                let pixel_ptr = unsafe { row_ptr.add(x * bytes_pp) };
                unsafe {
                    match bytes_pp {
                        2 => ptr::write_volatile(pixel_ptr as *mut u16, converted as u16),
                        3 => {
                            ptr::write_volatile(pixel_ptr, ((converted >> 16) & 0xFF) as u8);
                            ptr::write_volatile(pixel_ptr.add(1), ((converted >> 8) & 0xFF) as u8);
                            ptr::write_volatile(pixel_ptr.add(2), (converted & 0xFF) as u8);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

pub fn framebuffer_set_pixel(x: u32, y: u32, color: u32) {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return,
    };

    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let converted = fb.draw_pixel_format().convert_color(color);
    let Some(offset) = fb.checked_offset(x, y) else {
        return;
    };
    let Some(pixel_ptr) = fb.checked_ptr(offset, bytes_pp) else {
        return;
    };

    unsafe {
        match bytes_pp {
            2 => ptr::write_volatile(pixel_ptr as *mut u16, converted as u16),
            3 => {
                ptr::write_volatile(pixel_ptr, ((converted >> 16) & 0xFF) as u8);
                ptr::write_volatile(pixel_ptr.add(1), ((converted >> 8) & 0xFF) as u8);
                ptr::write_volatile(pixel_ptr.add(2), (converted & 0xFF) as u8);
            }
            4 => ptr::write_volatile(pixel_ptr as *mut u32, converted),
            _ => {}
        }
    }
}

pub fn framebuffer_get_pixel(x: u32, y: u32) -> u32 {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return 0,
    };

    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let Some(offset) = fb.checked_offset(x, y) else {
        return 0;
    };
    let Some(pixel_ptr) = fb.checked_ptr(offset, bytes_pp) else {
        return 0;
    };

    let mut color = 0u32;
    unsafe {
        match bytes_pp {
            2 => color = ptr::read_volatile(pixel_ptr as *const u16) as u32,
            3 => {
                let b0 = ptr::read_volatile(pixel_ptr) as u32;
                let b1 = ptr::read_volatile(pixel_ptr.add(1)) as u32;
                let b2 = ptr::read_volatile(pixel_ptr.add(2)) as u32;
                color = (b0 << 16) | (b1 << 8) | b2;
            }
            4 => color = ptr::read_volatile(pixel_ptr as *const u32),
            _ => {}
        }
    }

    fb.draw_pixel_format().convert_color(color)
}

pub fn framebuffer_blit(
    src_x: i32,
    src_y: i32,
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
) -> c_int {
    if width <= 0 || height <= 0 {
        return -1;
    }
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return -1,
    };
    let bpp = fb.bpp() as usize;
    if bpp == 0 {
        return -1;
    }
    let bytes_per_pixel = bpp.div_ceil(8);
    if bytes_per_pixel == 0 {
        return -1;
    }
    let fb_width = fb.width() as i32;
    let fb_height = fb.height() as i32;
    if src_x < 0
        || src_y < 0
        || dst_x < 0
        || dst_y < 0
        || src_x.saturating_add(width) > fb_width
        || src_y.saturating_add(height) > fb_height
        || dst_x.saturating_add(width) > fb_width
        || dst_y.saturating_add(height) > fb_height
    {
        return -1;
    }

    let row_bytes = width as usize * bytes_per_pixel;
    let src_pitch = fb.pitch() as usize;
    let base = fb.base_ptr();
    if base.is_null() {
        return -1;
    }

    if dst_y > src_y {
        for row in (0..height).rev() {
            let src_offset = (src_y + row) as usize * src_pitch + src_x as usize * bytes_per_pixel;
            let dst_offset = (dst_y + row) as usize * src_pitch + dst_x as usize * bytes_per_pixel;
            unsafe {
                ptr::copy(base.add(src_offset), base.add(dst_offset), row_bytes);
            }
        }
    } else {
        for row in 0..height {
            let src_offset = (src_y + row) as usize * src_pitch + src_x as usize * bytes_per_pixel;
            let dst_offset = (dst_y + row) as usize * src_pitch + dst_x as usize * bytes_per_pixel;
            unsafe {
                ptr::copy(base.add(src_offset), base.add(dst_offset), row_bytes);
            }
        }
    }
    0
}

pub fn framebuffer_get_width() -> u32 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.width()).unwrap_or(0)
}

pub fn framebuffer_get_height() -> u32 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.height()).unwrap_or(0)
}

pub fn framebuffer_get_bpp() -> u8 {
    FRAMEBUFFER.lock().fb.map(|fb| fb.bpp()).unwrap_or(0)
}

pub fn framebuffer_convert_color(color: u32) -> u32 {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return color,
    };
    fb.draw_pixel_format().convert_color(color)
}

pub(crate) fn snapshot() -> Option<FbState> {
    FRAMEBUFFER.lock().fb
}

pub fn register_flush_callback(callback: fn() -> c_int) {
    let mut guard = FRAMEBUFFER_FLUSH.lock();
    *guard = Some(callback);
}

pub fn framebuffer_flush() -> c_int {
    let guard = FRAMEBUFFER_FLUSH.lock();
    if let Some(cb) = *guard { cb() } else { 0 }
}

fn copy_rect_from_shm(
    fb: &FbState,
    shm_virt: *const u8,
    shm_size: usize,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
) -> bool {
    let fb_width = fb.width() as i32;
    let fb_height = fb.height() as i32;
    let bytes_pp = fb.info.bytes_per_pixel() as usize;
    let fb_pitch = fb.pitch() as usize;

    let cx0 = x0.max(0);
    let cy0 = y0.max(0);
    let cx1 = x1.min(fb_width - 1);
    let cy1 = y1.min(fb_height - 1);
    if cx0 > cx1 || cy0 > cy1 {
        return true;
    }

    let row_bytes = (cx1 - cx0 + 1) as usize * bytes_pp;
    for row in cy0..=cy1 {
        let row_usize = row as usize;
        let src_off = row_usize
            .checked_mul(fb_pitch)
            .and_then(|v| v.checked_add(cx0 as usize * bytes_pp));
        let dst_off = src_off;
        let Some(src_off) = src_off else {
            return false;
        };
        let Some(dst_off) = dst_off else {
            return false;
        };

        let Some(src_end) = src_off.checked_add(row_bytes) else {
            return false;
        };
        if src_end > shm_size {
            return false;
        }

        let Some(dst_ptr) = fb.checked_ptr(dst_off, row_bytes) else {
            return false;
        };
        // SAFETY: src range is checked against shm_size, dst range checked by checked_ptr.
        unsafe {
            ptr::copy_nonoverlapping(shm_virt.add(src_off), dst_ptr, row_bytes);
        }
    }

    true
}

pub fn fb_flip_from_shm(shm_phys: PhysAddr, size: usize) -> c_int {
    fb_flip_from_shm_damage(shm_phys, size, core::ptr::null(), 0)
}

pub fn fb_flip_from_shm_damage(
    shm_phys: PhysAddr,
    size: usize,
    damage: *const slopos_abi::damage::DamageRect,
    damage_count: u32,
) -> c_int {
    let fb = match FRAMEBUFFER.lock().fb {
        Some(fb) => fb,
        None => return -1,
    };

    let fb_size = fb.info.buffer_size();
    let copy_size = size.min(fb_size);
    if copy_size == 0 {
        return -1;
    }

    let shm_virt = match shm_phys.to_virt_checked() {
        Some(v) => v.as_u64(),
        None => return -1,
    };

    let shm_ptr = shm_virt as *const u8;

    if damage.is_null() || damage_count == 0 {
        let Some(dst_ptr) = fb.checked_ptr(0, copy_size) else {
            return -1;
        };
        // SAFETY: source and destination have been validated and are non-overlapping.
        unsafe {
            ptr::copy_nonoverlapping(shm_ptr, dst_ptr, copy_size);
        }
        return framebuffer_flush();
    }

    let max_regions = slopos_abi::damage::MAX_DAMAGE_REGIONS as u32;
    let region_count = damage_count.min(max_regions) as usize;
    // SAFETY: kernel syscall path validates this pointer and length before calling us.
    let regions = unsafe { core::slice::from_raw_parts(damage, region_count) };

    for rect in regions {
        if !rect.is_valid() {
            continue;
        }
        if !copy_rect_from_shm(&fb, shm_ptr, copy_size, rect.x0, rect.y0, rect.x1, rect.y1) {
            return -1;
        }
    }
    framebuffer_flush()
}
