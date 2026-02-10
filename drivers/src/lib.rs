#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]

pub mod apic;
pub mod input_event;
pub mod interrupt_test;
pub mod ioapic;
pub mod irq;
pub mod pci;
pub mod pci_defs;
pub mod pic;
pub mod pit;
pub mod pit_tests;
pub mod ps2;
pub mod random;
pub mod serial;
pub mod syscall_services_init;
pub mod tty;
pub mod virtio;
pub mod virtio_blk;
#[cfg(feature = "xe-gpu")]
pub mod xe;

pub use ps2::keyboard;
pub use ps2::mouse;
