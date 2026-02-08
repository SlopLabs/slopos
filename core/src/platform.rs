use core::ffi::{c_char, c_int, c_void};

slopos_lib::define_service! {
    /// Platform hardware abstraction layer.
    ///
    /// Registered once during early boot by the `boot` crate, which has visibility
    /// into both driver implementations and boot-specific callbacks.
    platform => PlatformServices {
        // -- Timer ----------------------------------------------------------
        timer_ticks() -> u64;
        timer_frequency() -> u32;
        timer_poll_delay_ms(ms: u32);
        timer_sleep_ms(ms: u32);
        timer_enable_irq();
        timer_disable_irq();

        // -- Console --------------------------------------------------------
        console_putc(c: u8);
        @no_wrapper console_puts(s: &[u8]);

        // -- RNG ------------------------------------------------------------
        rng_next() -> u64;

        // -- GDT ------------------------------------------------------------
        gdt_set_kernel_rsp0(rsp0: u64);

        // -- Lifecycle ------------------------------------------------------
        @no_wrapper kernel_shutdown(reason: *const c_char) -> !;
        @no_wrapper kernel_reboot(reason: *const c_char) -> !;

        // -- ACPI -----------------------------------------------------------
        is_rsdp_available() -> bool;
        get_rsdp_address() -> *const c_void;

        // -- Kernel state ---------------------------------------------------
        is_kernel_initialized() -> bool;
        idt_get_gate(vector: u8, entry: *mut c_void) -> c_int;

        // -- IRQ dispatch ---------------------------------------------------
        irq_send_eoi();
        irq_mask_gsi(gsi: u32) -> i32;
        irq_unmask_gsi(gsi: u32) -> i32;
    }
}

// -- Manual wrappers for @no_wrapper methods --------------------------------

/// Write a byte slice to the platform console.
#[inline(always)]
pub fn console_puts(s: &[u8]) {
    (platform_services().console_puts)(s)
}

/// Halt the kernel. Does not return.
#[inline(always)]
pub fn kernel_shutdown(reason: *const c_char) -> ! {
    (platform_services().kernel_shutdown)(reason)
}

/// Reboot the machine. Does not return.
#[inline(always)]
pub fn kernel_reboot(reason: *const c_char) -> ! {
    (platform_services().kernel_reboot)(reason)
}

// -- Computed helpers (not in the service table) -----------------------------

/// Wall-clock milliseconds derived from timer ticks and frequency.
#[inline(always)]
pub fn get_time_ms() -> u64 {
    let ticks = timer_ticks();
    let freq = timer_frequency();
    if freq == 0 {
        return 0;
    }
    (ticks * 1000) / freq as u64
}
