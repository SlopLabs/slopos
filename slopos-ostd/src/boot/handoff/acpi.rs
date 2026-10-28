//! ACPI table region handoff: the `core::slice::from_raw_parts` over
//! HHDM-mapped firmware memory.

use slopos_abi::addr::PhysAddr;

use crate::boot::hhdm;

/// `len` bytes of HHDM-mapped firmware memory at `phys`, unvalidated, for
/// consumers that probe RSDP or SDT-header prefixes at several lengths before
/// borrowing a whole table. `None` until `crate::boot::hhdm::register_hhdm_offset`
/// has run, and for a zero `phys` or `len` (Limine never publishes ACPI tables
/// at physical 0).
pub fn acpi_region_bytes(phys: PhysAddr, len: usize) -> Option<&'static [u8]> {
    if len == 0 || phys.0 == 0 {
        return None;
    }
    let off = hhdm::hhdm_offset()?;
    let virt = phys.0.checked_add(off)?;
    let ptr = virt as *const u8;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: bootloader-published, HHDM-mapped, kernel-lifetime
    // backing. `len` is supplied by the caller and either fits inside
    // a probe size (e.g. `size_of::<SdtHeader>()`) or has been
    // validated against `hdr.length` by a higher-level checksum step.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}
