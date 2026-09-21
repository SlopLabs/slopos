//! I/O operations — poll, select, and miscellaneous POSIX file ops.

pub const STDIN_FILENO: core::ffi::c_int = 0;
pub const STDOUT_FILENO: core::ffi::c_int = 1;
pub const STDERR_FILENO: core::ffi::c_int = 2;

pub mod dir;
pub mod dirent;
pub mod misc;
pub mod poll;
#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;
