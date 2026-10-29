pub mod accounts;
pub mod activation;
pub mod app_entry;
pub mod apps;
pub mod base_image;
pub mod base_watch;
pub mod boot_disk;
pub mod disk_names;
pub mod gfx;
pub mod held;
pub mod init_control;
pub mod keymap;
pub mod kmsg;
pub mod net;
pub mod net_query;
pub mod privd;
pub mod program_registry;
pub mod readiness;
pub mod root_layout;
pub mod runtime;
pub mod sealed;
pub mod secret;
pub mod selfhost;
pub mod sessions;
pub mod syscall;
pub mod tls;
pub mod tree;

// Process entry lives in `slopos-crt0`, not here: `crt0.o` carries the naked
// `_start` that hands the raw initial stack pointer to slibc's
// `__slibc_start`, exactly as a C program's does, and
// `scripts/build_userland.sh` links that object into every binary. A second
// `_start` in this rlib would be a duplicate symbol against it.
