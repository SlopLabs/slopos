//! Intel's frequency and core-type interfaces as data: what CPUID leaves 6,
//! 7, 0xB and 0x1A report, the `IA32_HWP_*` fields, the energy-performance
//! preference names, effective frequency from `IA32_APERF`/`IA32_MPERF`, the
//! thermal status registers, the kernel command line's frequency and
//! placement knobs, and the score the scheduler places a task by. Free of
//! `alloc`, `unsafe` and I/O: the kernel reads and writes the registers,
//! userland decodes what it reports.
//!
//! Layouts follow the Intel SDM, volume 3B, "Power and Thermal Management".

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod config;
pub mod cpuid;
pub mod freq;
pub mod hwp;
pub mod place;
pub mod therm;

pub use config::{Config, Policy};
pub use cpuid::{CoreType, PowerFeatures, SmtTopology};
pub use hwp::{HwpCaps, HwpRequest, Limits};
pub use place::{Candidate, CpuClass, Placement};
pub use therm::ThermStatus;

#[cfg(test)]
mod tests;
