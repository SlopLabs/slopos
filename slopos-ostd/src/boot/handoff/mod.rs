//! Bootloader-published memory boundary primitives.
//!
//! Every `core::slice::from_raw_parts` against bootloader-handed memory goes
//! through one of these.

pub mod acpi;
pub mod framebuffer;

pub use acpi::acpi_region_bytes;
