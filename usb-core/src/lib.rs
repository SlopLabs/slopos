//! USB as data: the xHCI host controller's registers, rings, TRBs and
//! contexts, and its bring-up written once over a register-bus trait, so the
//! kernel supplies MMIO and DMA memory and host tests a simulated
//! controller. Section numbers are the xHCI 1.2 specification's.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod knob;
pub mod xhci;
