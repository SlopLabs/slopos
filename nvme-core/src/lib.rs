//! The NVMe interface as data: controller registers, submission and
//! completion queue entries, identify structures and host memory buffer
//! sizing. Free of `alloc`, `unsafe` and I/O, so the driver's decisions are
//! host-testable. Offsets and fields follow the NVM Express Base
//! Specification 2.0 and the NVMe over PCIe Transport Specification 1.0.

#![no_std]
#![forbid(unsafe_code)]

pub mod command;
pub mod completion;
pub mod hmb;
pub mod identify;
pub mod regs;

pub use command::Command;
pub use completion::{Completion, Disposition, Status};
pub use identify::{ControllerInfo, NamespaceInfo};
