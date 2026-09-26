use core::ffi::c_void;

use super::heap;

pub use heap::HeapStats;

/// Null with `ENOMEM`, as C's allocation failure reads.
#[cold]
fn exhausted<T>() -> *mut T {
    crate::errno::errno_set(crate::errno::ENOMEM.raw());
    core::ptr::null_mut()
}

#[inline]
pub fn alloc(size: usize) -> *mut c_void {
    let p = heap::alloc(size);
    if p.is_null() {
        return exhausted();
    }
    p.cast()
}

#[inline]
pub fn dealloc(ptr: *mut c_void) {
    // SAFETY: `free`'s contract: `ptr` is null or a live allocation.
    unsafe { heap::free(ptr.cast()) }
}

pub fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    // SAFETY: `realloc`'s contract: `ptr` is null or a live allocation.
    let p = unsafe { heap::realloc(ptr.cast(), size) };
    if p.is_null() && size != 0 {
        return exhausted();
    }
    p.cast()
}

pub fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let Some(total) = nmemb.checked_mul(size) else {
        return exhausted();
    };
    let p = heap::alloc_zeroed(total);
    if p.is_null() {
        return exhausted();
    }
    p.cast()
}

/// The C `memalign`/`posix_memalign`/`malloc_usable_size` entry points live in
/// [`crate::ffi`] beside `malloc`; these are the Rust-side helpers they and
/// the TLS allocator call. `alignment` must be a power of two.
pub fn memalign(alignment: usize, size: usize) -> *mut u8 {
    let p = heap::alloc_aligned(alignment, size);
    if p.is_null() {
        return exhausted();
    }
    p
}

pub fn malloc_usable_size(ptr: *mut u8) -> usize {
    // SAFETY: `malloc_usable_size`'s contract: `ptr` is null or live.
    unsafe { heap::usable_size(ptr) }
}

pub fn heap_stats() -> HeapStats {
    heap::heap_stats()
}
