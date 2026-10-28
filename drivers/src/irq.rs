use crate::ioapic::regs::{
    IOAPIC_FLAG_DELIVERY_FIXED, IOAPIC_FLAG_DEST_PHYSICAL, IOAPIC_FLAG_MASK,
    IOAPIC_FLAG_POLARITY_LOW, IOAPIC_FLAG_TRIGGER_LEVEL,
};
use slopos_kernel_services::driver_runtime::{
    IRQ_LINES, LEGACY_IRQ_COM1, irq_init, irq_is_masked, irq_set_route,
};
use slopos_ostd::cpu::x86_64 as cpu;
use slopos_ostd::irq::IRQ_BASE_VECTOR;
use slopos_ostd::klog_info;

use crate::{apic, ioapic};

pub(crate) fn program_ioapic_route(irq_line: u8) {
    if irq_line as usize >= IRQ_LINES {
        return;
    }

    if !apic::is_enabled() || !ioapic::is_ready() {
        panic!("IRQ: APIC/IOAPIC unavailable during route programming");
    }

    let Some(ioapic::IrqRoute {
        gsi,
        flags: legacy_flags,
    }) = ioapic::legacy_irq_route(irq_line)
    else {
        panic!("IRQ: Failed to translate legacy IRQ");
    };

    let vector = IRQ_BASE_VECTOR.wrapping_add(irq_line) as u8;
    let lapic_id = apic::get_id() as u8;
    let flags =
        IOAPIC_FLAG_DELIVERY_FIXED | IOAPIC_FLAG_DEST_PHYSICAL | legacy_flags | IOAPIC_FLAG_MASK;

    if !ioapic::config_irq(gsi, vector, lapic_id, flags) {
        panic!("IRQ: Failed to program IOAPIC route");
    }

    irq_set_route(irq_line, gsi);

    let masked = irq_is_masked(irq_line);

    let polarity = if legacy_flags & IOAPIC_FLAG_POLARITY_LOW != 0 {
        "active-low"
    } else {
        "active-high"
    };
    let trigger = if legacy_flags & IOAPIC_FLAG_TRIGGER_LEVEL != 0 {
        "level"
    } else {
        "edge"
    };

    klog_info!(
        "IRQ: IOAPIC route IRQ {} -> GSI {}, vector 0x{:x} ({}, {})",
        irq_line,
        gsi,
        vector,
        polarity,
        trigger
    );

    if masked {
        let _ = ioapic::mask_gsi(gsi);
    } else {
        let _ = ioapic::unmask_gsi(gsi);
    }
}

fn setup_ioapic_routes() {
    if !apic::is_enabled() || !ioapic::is_ready() {
        panic!("IRQ: APIC/IOAPIC not ready during dispatcher init");
    }

    program_ioapic_route(LEGACY_IRQ_COM1);
}

pub fn init() {
    irq_init();

    setup_ioapic_routes();

    cpu::enable_interrupts();
}
