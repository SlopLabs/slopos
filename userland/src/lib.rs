#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod apps;
pub mod compositor;
pub mod gfx;
pub mod init_process;
pub mod libc;
pub mod program_registry;
pub mod roulette;
pub mod runtime;
pub mod shell;
pub mod syscall;
pub mod theme;
pub mod ui_utils;

/// Initializes userland runtime.
///
/// This function performs minimal crate-level setup required to prepare the userland
/// runtime for operation.
///
/// # Examples
///
/// ```
/// userland::init();
/// ```
pub fn init() {
    // Userland init remains lightweight.
}
