use core::ffi::c_void;

use crate::{idt, limine_protocol, shutdown};
use slopos_drivers::{apic, hpet, ioapic, random, serial};
use slopos_kernel_services::platform::{PlatformServices, register_platform_services};

fn get_rsdp_address_fn() -> *const c_void {
    limine_protocol::get_rsdp_address()
}

fn get_rsdp_phys_fn() -> u64 {
    limine_protocol::get_rsdp_phys_address()
}

static PLATFORM_SERVICES: PlatformServices = PlatformServices {
    timer_ticks: || slopos_core::irq::get_timer_ticks(),
    // Only the BSP's ISR advances the counter, so 100 Hz is its real rate.
    timer_frequency: || 100,
    timer_poll_delay_ms: |ms| hpet::delay_ms(ms),
    timer_enable_irq: || apic::timer::unmask(),
    timer_disable_irq: || apic::timer::mask(),
    timer_program_next_wakeup_ms: |ms| {
        apic::timer::set_oneshot_ms(slopos_ostd::irq::idt::LAPIC_TIMER_VECTOR, ms)
    },
    timer_restore_periodic: || {
        const LAPIC_TIMER_PERIOD_MS: u32 = 10;
        let _ = apic::timer::set_periodic_ms(
            slopos_ostd::irq::idt::LAPIC_TIMER_VECTOR,
            LAPIC_TIMER_PERIOD_MS,
        );
    },
    console_puts: |s| {
        for &c in s {
            serial::serial_putc_com1(c);
        }
    },
    console_write_serialized: serial::serial_locked_write_bytes,
    rng_next: || random::random_next(),
    is_rsdp_available: limine_protocol::is_rsdp_available,
    get_rsdp_address: get_rsdp_address_fn,
    get_rsdp_phys: get_rsdp_phys_fn,
    idt_get_gate: idt::idt_get_gate,
    irq_mask_gsi: |gsi| ioapic::mask_gsi(gsi),
    irq_unmask_gsi: |gsi| ioapic::unmask_gsi(gsi),
    clock_monotonic_ns: slopos_drivers::tsc_clock::monotonic_ns,
};

pub fn register_boot_services() {
    register_platform_services(&PLATFORM_SERVICES);
    // OSTD owns the *authority* for power -- the witness and the single choke
    // point -- while the sequence stays here, where the ACPI and UEFI state it
    // needs lives. `boot` sits above OSTD, so the mechanism is registered in
    // rather than called out to.
    slopos_ostd::platform::power::register(slopos_ostd::platform::power::PowerOps {
        shutdown: shutdown::kernel_shutdown,
        reboot: shutdown::kernel_reboot,
    });
}
