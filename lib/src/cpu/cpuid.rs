//! CPUID instruction wrapper.

/// Execute CPUID with the given leaf.
/// Returns (eax, ebx, ecx, edx).
#[inline(always)]
#[allow(unused_unsafe)]
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let res = unsafe { core::arch::x86_64::__cpuid(leaf) };
    (res.eax, res.ebx, res.ecx, res.edx)
}
