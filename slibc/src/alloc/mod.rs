//! Safe wrappers over slibc's heap allocator; the allocator itself lives in
//! `mem/`.

pub mod raw_buffer;

pub use raw_buffer::RawBuffer;
