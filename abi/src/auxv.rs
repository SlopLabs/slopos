//! ELF auxiliary vector definitions (kernel-userland ABI).
//!
//! The auxiliary vector is placed on the user stack by the kernel during
//! exec(). It provides runtime information that the C library startup code
//! (crt0 / __libc_start_main) needs to initialize properly.
//!
//! Stack layout after exec:
//!   [argc] [argv0..argvN] [NULL] [env0..envN] [NULL] [auxv entries] [AT_NULL,0]

/// Auxiliary vector entry (two u64 words).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AuxEntry {
    pub a_type: u64,
    pub a_val: u64,
}

// =============================================================================
// Auxiliary vector type constants (from ELF spec / Linux ABI)
// =============================================================================

/// End of auxiliary vector.
pub const AT_NULL: u64 = 0;

/// Entry point of the program (not the interpreter).
pub const AT_ENTRY: u64 = 9;

/// Address of program headers in memory.
pub const AT_PHDR: u64 = 3;

/// Size of each program header entry.
pub const AT_PHENT: u64 = 4;

/// Number of program headers.
pub const AT_PHNUM: u64 = 5;

/// System page size.
pub const AT_PAGESZ: u64 = 6;

/// Base address of the interpreter (0 for static binaries).
pub const AT_BASE: u64 = 7;

/// Flags (unused, set to 0).
pub const AT_FLAGS: u64 = 8;

/// UID of the process.
pub const AT_UID: u64 = 11;

/// Effective UID.
pub const AT_EUID: u64 = 12;

/// GID of the process.
pub const AT_GID: u64 = 13;

/// Effective GID.
pub const AT_EGID: u64 = 14;

/// Secure mode boolean (0 = normal).
pub const AT_SECURE: u64 = 23;

/// String identifying the real platform.
pub const AT_RANDOM: u64 = 25;
