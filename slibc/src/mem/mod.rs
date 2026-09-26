//! The heap. slibc owns the allocator and exports C's `malloc` family over
//! it, but registers no `#[global_allocator]`: `std` supplies its own over
//! those very entry points, and two `#[global_allocator]`s in one binary is a
//! hard link error.

pub mod heap;
pub mod malloc;
mod os;

pub use heap::{ForkGuard, fork_prepare};
pub use malloc::{alloc, calloc, dealloc, memalign, realloc};

/// Hand the finished calling thread's heap to the pool.
///
/// # Safety
/// `tcb` is the calling thread's own TCB, and the thread allocates nothing
/// afterwards.
pub unsafe fn release_thread_heap(tcb: *mut crate::thread::tcb::Tcb) {
    heap::release_thread_heap(&raw mut (*tcb).heap);
}
