//! The Realtek RTL8111/8168 as data and as a register sequence: the
//! register map, the table of MAC versions the driver brings up, the
//! descriptor format and ring bookkeeping, and the bring-up itself over a
//! register-bus trait. Free of `alloc`, `unsafe` and I/O, so the kernel
//! supplies MMIO and DMA memory and host tests supply a simulated chip.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod bus;
pub mod chip;
pub mod desc;
pub mod regs;
pub mod ring;
pub mod version;

#[cfg(test)]
mod sim;

pub use bus::{Error, RegisterBus, Wait};
pub use chip::{Link, Mac, ProbeError, Probed, RingAddresses, Speed, Stalls};
pub use desc::{Descriptor, RxError};
pub use ring::{DescriptorMemory, Received, RxRing, TxError, TxRing};
pub use version::{ChipVersion, identify, xid};
