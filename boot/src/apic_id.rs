use slopos_ostd::arch::x86_64::cpuid::CPUID_LEAF_FEATURES;

pub fn read_bsp_apic_id() -> u32 {
    let (_, ebx, _, _) = slopos_ostd::arch::x86_64::cpuid::cpuid(CPUID_LEAF_FEATURES);
    ((ebx >> 24) & 0xFF) as u32
}
