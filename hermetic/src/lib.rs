//! SlopOS hermetic-state framework: the linker-section registry of
//! `HermeticState` impls walked at scope enter/Drop, the [`BootCtx`] capability
//! token gating boot-time-only mutators, and the [`KernelStackTop<'a>`]
//! resource newtype.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

mod boot_ctx;
mod registry;
mod stack_top;

#[doc(hidden)]
pub use paste as __paste;

pub use boot_ctx::{
    clear_test_scope_after_panic, return_after_ap, return_after_boot, return_after_test,
    take_for_ap, take_for_boot, take_for_test, ApInit, BootCtx, BootKind, BspInit, CpuInitKind,
    TestInit,
};
pub use registry::{registry_iter, topo_order, HermeticVTable, RegistryError};
pub use stack_top::KernelStackTop;
