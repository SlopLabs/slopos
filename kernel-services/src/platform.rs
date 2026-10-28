use core::ffi::c_void;

slopos_service_core::define_service! {
    platform => PlatformServices {
        timer_ticks() -> u64;
        timer_frequency() -> u32;
        timer_poll_delay_ms(ms: u32);
        timer_enable_irq();
        timer_disable_irq();
        /// Arm a **one-shot** LAPIC timer to fire after `ms` milliseconds, so
        /// the tickless-idle path wakes on a sleep deadline rather than the
        /// next periodic boundary. Its own ISR restores periodic mode via
        /// [`timer_restore_periodic`].
        timer_program_next_wakeup_ms(ms: u32) -> bool;
        /// Re-arm the LAPIC timer in periodic mode at the default preempt-tick
        /// interval. Idempotent.
        timer_restore_periodic();

        /// Write to the console without taking any lock. For the fault and
        /// panic paths, which may already hold the one below.
        @no_wrapper console_puts(s: &[u8]);
        /// Write to the console atomically with respect to klog. Every other
        /// writer must use this: bypassing it byte-interleaves into concurrent
        /// klog output, which in a test boot is the KTAP wire format.
        @no_wrapper console_write_serialized(s: &[u8]);

        rng_next() -> u64;

        is_rsdp_available() -> bool;
        get_rsdp_address() -> *const c_void;
        get_rsdp_phys() -> u64;

        idt_get_gate(vector: u8) -> slopos_ostd::irq::IdtEntry;

        irq_mask_gsi(gsi: u32) -> bool;
        irq_unmask_gsi(gsi: u32) -> bool;

        @no_wrapper clock_monotonic_ns() -> u64;
    }
}

#[inline(always)]
pub fn console_puts(s: &[u8]) {
    (platform_services().console_puts)(s)
}

#[inline(always)]
pub fn console_write_serialized(s: &[u8]) {
    (platform_services().console_write_serialized)(s)
}

#[inline(always)]
pub fn get_time_ms() -> u64 {
    let ns = clock_monotonic_ns();
    if ns != 0 {
        return ns / 1_000_000;
    }
    let ticks = timer_ticks();
    let freq = timer_frequency();
    if freq == 0 {
        return 0;
    }
    (ticks * 1000) / freq as u64
}

#[inline(always)]
pub fn clock_monotonic_ns() -> u64 {
    if !is_platform_initialized() {
        return 0;
    }
    (platform_services().clock_monotonic_ns)()
}
