//! libslop - Minimal C runtime library for SlopOS userland.

pub mod crt0;
pub mod ffi;
pub mod free_list;
pub mod malloc;
pub mod syscall;

pub use crt0::{argc, argv, crt0_start, envp, get_arg, get_env, set_main};
pub use malloc::{alloc, calloc, dealloc, realloc};
pub use syscall::{sys_brk, sys_close, sys_exit, sys_open, sys_read, sys_sbrk, sys_write};
