pub mod apps;
pub mod gfx;
pub mod keymap;
pub mod net;
pub mod net_query;
pub mod program_registry;
pub mod readiness;
pub use slopos_rt as ring;
pub mod runtime;
pub mod syscall;
pub mod theme;
pub mod tls;
pub mod ui_utils;

pub use slopos_slibc as slibc;

pub fn init() {}

// Process entry lives in `slopos-crt0`, not here: `crt0.o` carries the naked
// `_start` that hands the raw initial stack pointer to slibc's
// `__slibc_start`, exactly as a C program's does, and
// `scripts/build_userland.sh` links that object into every binary. A second
// `_start` in this rlib would be a duplicate symbol against it.
