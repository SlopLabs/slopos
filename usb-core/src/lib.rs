//! USB as data: xHCI's registers, rings, TRBs, contexts and bring-up over a
//! register-bus trait; the device framework; hubs; and a controller's tree
//! over a host trait. The kernel supplies MMIO, DMA and locks, the host tests
//! a simulator. Section numbers are xHCI 1.2's unless a USB spec is named.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod bus;
pub mod device;
pub mod hid;
pub mod hub;
pub mod knob;
pub mod storage;
pub mod xhci;
