//! Architecture-specific definitions.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

// Re-export x86_64 types at arch level for convenience
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

/// Base vector for hardware IRQs (IRQ0 maps to this vector).
#[cfg(target_arch = "x86_64")]
pub use x86_64::idt::IRQ_BASE_VECTOR;

/// Syscall interrupt vector (int 0x80).
#[cfg(target_arch = "x86_64")]
pub use x86_64::idt::SYSCALL_VECTOR;
