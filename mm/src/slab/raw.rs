//! Raw-pointer `kmalloc` / `kfree` over [`super::KERNEL_SLAB`], for callers that
//! drive the heap directly rather than through the `#[global_allocator]`.

use core::ffi::c_void;
use core::ptr::NonNull;

use slopos_ostd::mm::KernelHeapBackend;
use slopos_ostd::util::ptr_buf;

use super::MAX_ALLOC_SIZE;

/// Allocate `size` bytes of kernel heap memory, zeroed.
///
/// Returns null on `size == 0`, `size > MAX_ALLOC_SIZE`, or allocation
/// failure.
pub fn kmalloc(size: usize) -> *mut c_void {
    if size == 0 || size > MAX_ALLOC_SIZE {
        return core::ptr::null_mut();
    }
    let Some(ptr) = super::KERNEL_SLAB.alloc(size) else {
        return core::ptr::null_mut();
    };
    // Tail padding past `size` is never read by the caller, so the rounded
    // chunk is left unscrubbed.
    ptr_buf::with_buf_mut(ptr.as_ptr(), size, |bytes: &mut [u8]| bytes.fill(0));
    ptr.as_ptr() as *mut c_void
}

/// Return a previously [`kmalloc`]-ed pointer to the slab/large tier. Null is
/// a no-op; wild pointers are silently swallowed.
pub fn kfree(ptr_in: *mut c_void) {
    let Some(nn) = NonNull::new(ptr_in as *mut u8) else {
        return;
    };
    super::KERNEL_SLAB.dealloc(nn);
}
