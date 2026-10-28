//! Uncached kernel mappings of device MMIO.

use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::io_mem::{
    IoMem, IoMemCachePolicy, IoMemRegistry, PhysRange, register_io_mem_range,
};

/// Map `[phys, phys + size)` into a kernel virtual window with Uncacheable
/// page-table entries, registering the range with OSTD's dynamic-range
/// registry first. `None` on null phys, zero size, address-space overflow, or
/// registry / mapper failure.
pub fn map_uncached(phys: PhysAddr, size: usize) -> Option<IoMem> {
    if phys.is_null() || size == 0 {
        return None;
    }
    if phys.as_u64().checked_add(size as u64)? > PhysAddr::MAX.as_u64() {
        return None;
    }
    register_io_mem_range(PhysRange {
        base: phys,
        len: size,
    })
    .ok()?;
    IoMemRegistry::reserve(phys, size, IoMemCachePolicy::Uncacheable).ok()
}
