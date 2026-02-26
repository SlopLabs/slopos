//! MCFG / ECAM regression tests for Phase 4A of the Legacy Modernisation Plan.
//!
//! These tests verify:
//! - MCFG discovery ran and populated ECAM state on QEMU q35
//! - Primary ECAM entry covers segment 0 with expected base address
//! - Cached entry fields are valid (non-zero base, sane bus ranges)
//! - `McfgEntry::region_size()` returns correct byte count
//! - `McfgEntry::ecam_offset()` computes correct BDF offsets
//! - `ecam_offset()` rejects out-of-range bus, device, and function values
//! - `pci_ecam_find_entry()` locates the right entry for a given segment/bus
//! - Lock-free accessors agree with mutex-protected state
//! - Multiple reads return deterministic results
//! - ECAM base address is page-aligned (4 KiB)
//!
//! All tests run after PCI enumeration in the test harness.  QEMU q35 exposes
//! a single MCFG entry for segment 0, buses 0–255, at physical address
//! 0xB000_0000 or 0xE000_0000 (depends on QEMU version/config).

use slopos_lib::testing::TestResult;
use slopos_lib::{fail, pass};

use crate::pci::{
    pci_ecam_available, pci_ecam_base, pci_ecam_entry, pci_ecam_entry_count, pci_ecam_find_entry,
};

// =============================================================================
// 1. MCFG discovery sanity
// =============================================================================

/// ECAM must be available on QEMU q35 — MCFG discovery should have succeeded.
pub fn test_ecam_available() -> TestResult {
    if !pci_ecam_available() {
        return fail!("pci_ecam_available() returned false — MCFG discovery failed on q35");
    }
    pass!()
}

/// At least one ECAM entry must have been cached.
pub fn test_ecam_entry_count_nonzero() -> TestResult {
    let count = pci_ecam_entry_count();
    if count == 0 {
        return fail!("pci_ecam_entry_count() is 0 — no MCFG entries cached");
    }
    pass!()
}

// =============================================================================
// 2. Primary entry validation
// =============================================================================

/// The primary ECAM base address (segment 0) must be non-zero.
pub fn test_ecam_base_nonzero() -> TestResult {
    let base = pci_ecam_base();
    if base == 0 {
        return fail!("pci_ecam_base() returned 0 — no segment 0 ECAM region");
    }
    pass!()
}

/// ECAM base address must be page-aligned (4 KiB boundary).
pub fn test_ecam_base_page_aligned() -> TestResult {
    let base = pci_ecam_base();
    if base == 0 {
        return fail!("pci_ecam_base() is 0, cannot check alignment");
    }
    if (base & 0xFFF) != 0 {
        return fail!(
            "ECAM base 0x{:x} is not 4K-aligned (bottom 12 bits: 0x{:x})",
            base,
            base & 0xFFF
        );
    }
    pass!()
}

/// The primary entry must cover bus 0 (every PCIe system starts enumeration
/// from bus 0 on segment 0).
pub fn test_primary_entry_covers_bus_zero() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_start > 0 {
        return fail!(
            "Primary ECAM entry bus_start={}, expected 0",
            entry.bus_start
        );
    }
    pass!()
}

// =============================================================================
// 3. Entry field validity
// =============================================================================

/// Every cached entry must have a non-zero base address.
pub fn test_all_entries_base_nonzero() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        let entry = match pci_ecam_entry(i) {
            Some(e) => e,
            None => return fail!("pci_ecam_entry({}) returned None but count={}", i, count),
        };
        if entry.base_phys == 0 {
            return fail!("ECAM entry {} has zero base_phys", i);
        }
    }
    pass!()
}

/// Every entry must have bus_start <= bus_end.
pub fn test_all_entries_bus_range_valid() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        let entry = match pci_ecam_entry(i) {
            Some(e) => e,
            None => continue,
        };
        if entry.bus_start > entry.bus_end {
            return fail!(
                "ECAM entry {} has inverted bus range: start={} > end={}",
                i,
                entry.bus_start,
                entry.bus_end
            );
        }
    }
    pass!()
}

// =============================================================================
// 4. McfgEntry::region_size() correctness
// =============================================================================

/// Region size must equal (bus_end - bus_start + 1) * 256 * 4096.
pub fn test_region_size_formula() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        let entry = match pci_ecam_entry(i) {
            Some(e) => e,
            None => continue,
        };
        let expected = (entry.bus_end as u64 - entry.bus_start as u64 + 1) * 256 * 4096;
        let actual = entry.region_size();
        if actual != expected {
            return fail!(
                "Entry {} region_size()={} expected {} (buses {}..{})",
                i,
                actual,
                expected,
                entry.bus_start,
                entry.bus_end
            );
        }
    }
    pass!()
}

/// For full-range segment 0 (buses 0–255), region size = 256 MiB.
pub fn test_region_size_full_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    // QEMU q35 typically exposes buses 0–255
    if entry.bus_start == 0 && entry.bus_end == 255 {
        let size = entry.region_size();
        let expected = 256 * 256 * 4096; // 256 MiB
        if size != expected {
            return fail!(
                "Full-range region_size()={} expected {} (256 MiB)",
                size,
                expected
            );
        }
    }
    pass!()
}

// =============================================================================
// 5. McfgEntry::ecam_offset() correctness
// =============================================================================

/// Offset for bus 0, device 0, function 0 must be 0 (relative to bus_start).
pub fn test_ecam_offset_zero_bdf() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    let offset = entry.ecam_offset(entry.bus_start, 0, 0);
    match offset {
        None => return fail!("ecam_offset(bus_start, 0, 0) returned None"),
        Some(0) => {}
        Some(v) => return fail!("ecam_offset(bus_start, 0, 0) = 0x{:x}, expected 0", v),
    }
    pass!()
}

/// Offset calculation must match the formula: (bus<<20) | (dev<<15) | (func<<12).
pub fn test_ecam_offset_known_bdf() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    // Pick a BDF within range: bus_start+1, device 3, function 2
    let test_bus = entry.bus_start.saturating_add(1);
    if test_bus > entry.bus_end {
        // Single-bus entry — skip this test gracefully
        return pass!();
    }
    let relative_bus = (test_bus - entry.bus_start) as u64;
    let expected = (relative_bus << 20) | (3u64 << 15) | (2u64 << 12);
    let actual = match entry.ecam_offset(test_bus, 3, 2) {
        Some(v) => v,
        None => return fail!("ecam_offset({}, 3, 2) returned None unexpectedly", test_bus),
    };
    if actual != expected {
        return fail!(
            "ecam_offset({}, 3, 2) = 0x{:x} expected 0x{:x}",
            test_bus,
            actual,
            expected
        );
    }
    pass!()
}

// =============================================================================
// 6. ecam_offset() boundary checks
// =============================================================================

/// Bus below entry range must return None.
pub fn test_ecam_offset_bus_below_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_start == 0 {
        // Cannot go below bus 0 — this boundary is inherently satisfied.
        return pass!();
    }
    let below = entry.bus_start - 1;
    if entry.ecam_offset(below, 0, 0).is_some() {
        return fail!(
            "ecam_offset(bus={}, 0, 0) should return None (below bus_start={})",
            below,
            entry.bus_start
        );
    }
    pass!()
}

/// Bus above entry range must return None.
pub fn test_ecam_offset_bus_above_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.bus_end == 255 {
        // Cannot go above bus 255 with a u8 — but the function takes u8,
        // so we can't actually pass 256. This is inherently safe.
        return pass!();
    }
    let above = entry.bus_end + 1;
    if entry.ecam_offset(above, 0, 0).is_some() {
        return fail!(
            "ecam_offset(bus={}, 0, 0) should return None (above bus_end={})",
            above,
            entry.bus_end
        );
    }
    pass!()
}

/// Device >= 32 must return None.
pub fn test_ecam_offset_device_out_of_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.ecam_offset(entry.bus_start, 32, 0).is_some() {
        return fail!("ecam_offset(bus_start, device=32, 0) should return None");
    }
    pass!()
}

/// Function >= 8 must return None.
pub fn test_ecam_offset_function_out_of_range() -> TestResult {
    let entry = match pci_ecam_entry(0) {
        Some(e) => e,
        None => return fail!("pci_ecam_entry(0) returned None"),
    };
    if entry.ecam_offset(entry.bus_start, 0, 8).is_some() {
        return fail!("ecam_offset(bus_start, 0, function=8) should return None");
    }
    pass!()
}

// =============================================================================
// 7. pci_ecam_find_entry() lookup
// =============================================================================

/// Finding segment 0, bus 0 must succeed.
pub fn test_find_entry_segment0_bus0() -> TestResult {
    let entry = match pci_ecam_find_entry(0, 0) {
        Some(e) => e,
        None => return fail!("pci_ecam_find_entry(0, 0) returned None"),
    };
    if entry.segment != 0 {
        return fail!(
            "pci_ecam_find_entry(0, 0) returned segment {} (expected 0)",
            entry.segment
        );
    }
    pass!()
}

/// Finding a nonexistent segment (0xFFFF) must return None.
pub fn test_find_entry_nonexistent_segment() -> TestResult {
    if pci_ecam_find_entry(0xFFFF, 0).is_some() {
        return fail!("pci_ecam_find_entry(0xFFFF, 0) unexpectedly returned Some");
    }
    pass!()
}

// =============================================================================
// 8. Lock-free vs mutex consistency
// =============================================================================

/// pci_ecam_base() must match the base_phys of the first entry with segment 0.
pub fn test_ecam_base_matches_primary_entry() -> TestResult {
    let base = pci_ecam_base();
    let entry = match pci_ecam_find_entry(0, 0) {
        Some(e) => e,
        None => {
            if base == 0 {
                return pass!(); // Both agree: no segment 0
            }
            return fail!(
                "pci_ecam_base()=0x{:x} but find_entry(0,0) returned None",
                base
            );
        }
    };
    if base != entry.base_phys {
        return fail!(
            "pci_ecam_base()=0x{:x} != find_entry base_phys=0x{:x}",
            base,
            entry.base_phys
        );
    }
    pass!()
}

/// pci_ecam_entry_count() must match the number of entries retrievable by index.
pub fn test_entry_count_matches_indexable() -> TestResult {
    let count = pci_ecam_entry_count();
    for i in 0..count as usize {
        if pci_ecam_entry(i).is_none() {
            return fail!("pci_ecam_entry({}) is None but entry_count={}", i, count);
        }
    }
    // One past the end must be None
    if pci_ecam_entry(count as usize).is_some() {
        return fail!(
            "pci_ecam_entry({}) is Some but should be past-the-end",
            count
        );
    }
    pass!()
}

// =============================================================================
// 9. Deterministic reads
// =============================================================================

/// Two consecutive reads of all ECAM state must return identical results.
pub fn test_ecam_state_deterministic() -> TestResult {
    let base1 = pci_ecam_base();
    let count1 = pci_ecam_entry_count();
    let avail1 = pci_ecam_available();

    let base2 = pci_ecam_base();
    let count2 = pci_ecam_entry_count();
    let avail2 = pci_ecam_available();

    if base1 != base2 {
        return fail!("ECAM base changed: 0x{:x} vs 0x{:x}", base1, base2);
    }
    if count1 != count2 {
        return fail!("ECAM entry count changed: {} vs {}", count1, count2);
    }
    if avail1 != avail2 {
        return fail!("ECAM availability changed: {} vs {}", avail1, avail2);
    }

    // Also verify each entry is identical across reads
    for i in 0..count1 as usize {
        let e1 = pci_ecam_entry(i);
        let e2 = pci_ecam_entry(i);
        match (e1, e2) {
            (Some(a), Some(b)) => {
                if a.base_phys != b.base_phys
                    || a.segment != b.segment
                    || a.bus_start != b.bus_start
                    || a.bus_end != b.bus_end
                {
                    return fail!("ECAM entry {} changed between reads", i);
                }
            }
            (None, None) => {}
            _ => return fail!("ECAM entry {} availability changed between reads", i),
        }
    }
    pass!()
}

// =============================================================================
// Suite registration
// =============================================================================

slopos_lib::define_test_suite!(
    ecam,
    [
        // MCFG discovery sanity
        test_ecam_available,
        test_ecam_entry_count_nonzero,
        // Primary entry validation
        test_ecam_base_nonzero,
        test_ecam_base_page_aligned,
        test_primary_entry_covers_bus_zero,
        // Entry field validity
        test_all_entries_base_nonzero,
        test_all_entries_bus_range_valid,
        // region_size() correctness
        test_region_size_formula,
        test_region_size_full_range,
        // ecam_offset() correctness
        test_ecam_offset_zero_bdf,
        test_ecam_offset_known_bdf,
        // ecam_offset() boundary checks
        test_ecam_offset_bus_below_range,
        test_ecam_offset_bus_above_range,
        test_ecam_offset_device_out_of_range,
        test_ecam_offset_function_out_of_range,
        // find_entry() lookup
        test_find_entry_segment0_bus0,
        test_find_entry_nonexistent_segment,
        // Lock-free vs mutex consistency
        test_ecam_base_matches_primary_entry,
        test_entry_count_matches_indexable,
        // Deterministic reads
        test_ecam_state_deterministic,
    ]
);
