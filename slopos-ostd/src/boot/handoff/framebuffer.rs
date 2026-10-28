//! The pixel-store helpers that fold raw stores into the bootloader-mapped
//! framebuffer inside OSTD.

/// Unaligned `u32` store at `ptr`: some firmware lands a row offset that is
/// not 4-byte aligned.
#[inline]
pub fn fb_write_u32_unaligned(ptr: *mut u8, value: u32) {
    // SAFETY: caller certifies `ptr` is within a pre-mapped
    // framebuffer; `write_unaligned` is sound for any byte alignment.
    unsafe {
        core::ptr::write_unaligned(ptr as *mut u32, value);
    }
}

/// Copy `src` to `base + byte_offset`; the vconsole shadow-buffer blit.
#[inline]
pub fn fb_blit_bytes(base: u64, byte_offset: usize, src: &[u8]) {
    // SAFETY: caller certifies the destination range
    // `[byte_offset, byte_offset + src.len())` lies inside the
    // pre-mapped framebuffer and source/destination do not alias.
    unsafe {
        let dst = (base as *mut u8).add(byte_offset);
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
    }
}

/// Solid-color fast path for the `bytes_per_pixel == 4`, all-bytes-identical
/// case.
#[inline]
pub fn fb_fill_u8_bulk(base_ptr: *mut u8, byte_value: u8, byte_count: usize) {
    // SAFETY: caller certifies `[base_ptr, base_ptr + byte_count)`
    // lies inside a pre-mapped framebuffer; `write_bytes` does not
    // require alignment.
    unsafe {
        core::ptr::write_bytes(base_ptr, byte_value, byte_count);
    }
}

/// Volatile `u32` store at an already-advanced `ptr`.
#[inline]
pub fn fb_write_u32_at(ptr: *mut u8, value: u32) {
    // SAFETY: caller certifies `ptr..ptr+4` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr as *mut u32, value);
    }
}

/// Volatile `u64` store, for an 8-byte-aligned destination.
#[inline]
pub fn fb_write_u64_at(ptr: *mut u64, value: u64) {
    // SAFETY: caller certifies `ptr..ptr+8` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

/// Volatile `u16` store at `ptr`.
#[inline]
pub fn fb_write_u16_at(ptr: *mut u8, value: u16) {
    // SAFETY: caller certifies `ptr..ptr+2` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr as *mut u16, value);
    }
}

/// Volatile `u8` store at `ptr`.
#[inline]
pub fn fb_write_u8_at(ptr: *mut u8, value: u8) {
    // SAFETY: caller certifies `ptr..ptr+1` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

/// Advance `base_ptr` by `byte_offset`, folding `(*mut u8).add` inside OSTD.
#[inline]
pub fn fb_ptr_add(base_ptr: *mut u8, byte_offset: usize) -> *mut u8 {
    // SAFETY: caller certifies the resulting address remains within
    // the pre-mapped framebuffer (and within the same allocation —
    // the FB is a single mapping).
    unsafe { base_ptr.add(byte_offset) }
}
