#![no_std]
#![forbid(unsafe_code)]

pub mod apic_id;
pub mod base;
pub mod boot_drivers;
pub mod boot_impl;
pub mod boot_memory;
pub mod boot_services;
pub mod early_init;
pub mod exception;
pub mod ffi_boundary;
pub mod gdt;
pub use gdt::syscall_msr_init;
pub mod idt;
pub mod ist_stacks;
pub mod kconsole;
pub mod limine_protocol;
pub mod panic;
pub mod shutdown;
pub mod smp;
#[cfg(feature = "test-hooks")]
pub mod tests;
pub mod uefi_runtime;
pub mod user_fault;

pub use early_init::{
    boot_get_cmdline, boot_get_hhdm_offset, boot_init_run_all, boot_init_run_phase,
};
pub use ffi_boundary::kernel_main;
pub use limine_protocol::{
    BootFramebuffer, BootInfo, MemoryRegion, MemoryRegionKind, boot_info, memory_regions,
};
pub use panic::{panic_handler_impl, set_panic_cpu_state};
pub use shutdown::{
    execute_kernel, kernel_drain_serial_output, kernel_quiesce_interrupts, kernel_reboot,
    kernel_shutdown,
};
