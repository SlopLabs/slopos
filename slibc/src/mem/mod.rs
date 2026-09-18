//! The heap. slibc owns the allocator and exports C's `malloc` family over
//! it, but registers no `#[global_allocator]`: `std` supplies its own over
//! those very entry points, and two `#[global_allocator]`s in one binary is a
//! hard link error.

pub mod bins;
pub mod chunk;
pub mod dlmalloc;
pub mod malloc;

pub use malloc::{alloc, calloc, dealloc, memalign, realloc};
