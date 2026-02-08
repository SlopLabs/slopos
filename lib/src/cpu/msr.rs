//! Model-Specific Register (MSR) read/write instructions.
//!
//! The `Msr` address type is defined in `slopos_abi::arch::msr`.
//! This module provides the RDMSR/WRMSR instruction wrappers.

use core::arch::asm;

/// Read a 64-bit value from the specified MSR.
#[inline(always)]
pub fn read_msr(msr: slopos_abi::arch::Msr) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!(
            "rdmsr",
            out("eax") low,
            out("edx") high,
            in("ecx") msr.address(),
            options(nomem, nostack, preserves_flags)
        );
    }
    ((high as u64) << 32) | (low as u64)
}

/// Write a 64-bit value to the specified MSR.
#[inline(always)]
pub fn write_msr(msr: slopos_abi::arch::Msr, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!(
            "wrmsr",
            in("eax") low,
            in("edx") high,
            in("ecx") msr.address(),
            options(nomem, nostack, preserves_flags)
        );
    }
}
