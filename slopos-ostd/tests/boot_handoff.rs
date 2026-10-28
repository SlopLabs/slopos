//! Host-side tests for `slopos_ostd::boot::handoff`, driving it from a
//! synthetic byte slice.

use std::sync::Mutex;

use slopos_abi::addr::PhysAddr;
use slopos_ostd::boot::handoff::acpi_region_bytes;
use slopos_ostd::boot::hhdm::{register_hhdm_offset, reset_hhdm_offset_for_tests};
use slopos_ostd::sync::{reset_bsp_token_for_tests, run_bsp_init};

/// Serialises every test touching the BSP-gated HHDM-offset registry: it is
/// process-global on host, and `cargo test` runs tests in parallel.
static BSP_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn acpi_region_bytes_requires_hhdm_registration() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_hhdm_offset_for_tests();
    reset_bsp_token_for_tests();
    assert!(acpi_region_bytes(PhysAddr::new(0x1000), 16).is_none());
}

#[test]
fn acpi_region_bytes_round_trips_raw_bytes() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_hhdm_offset_for_tests();
    reset_bsp_token_for_tests();

    let payload: Vec<u8> = (0..16u8).collect();
    let ptr = payload.as_ptr().expose_provenance() as u64;
    let phys_base = 0x1000_u64;
    let offset = ptr.wrapping_sub(phys_base);

    run_bsp_init(|tok| register_hhdm_offset(tok, offset));

    let bytes = acpi_region_bytes(PhysAddr::new(phys_base), payload.len())
        .expect("HHDM is registered and len > 0");
    assert_eq!(bytes, &payload[..]);
}

#[test]
fn acpi_region_bytes_rejects_null_phys_or_zero_len() {
    let _g = BSP_LOCK.lock().unwrap();
    reset_hhdm_offset_for_tests();
    reset_bsp_token_for_tests();
    run_bsp_init(|tok| register_hhdm_offset(tok, 0));
    assert!(acpi_region_bytes(PhysAddr::new(0), 16).is_none());
    assert!(acpi_region_bytes(PhysAddr::new(0x1000), 0).is_none());
}
