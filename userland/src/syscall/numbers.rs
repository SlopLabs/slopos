//! Syscall number re-exports from the ABI crate.
//!
//! This module simply re-exports all syscall numbers from `slopos_abi::syscall`
//! to provide a single source of truth for syscall numbers in userland.

pub use slopos_abi::syscall::*;
