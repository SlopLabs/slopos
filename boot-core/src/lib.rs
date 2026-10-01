//! The boot disk as data: GUIDs and the GUID Partition Table, UEFI load
//! options and device paths, the boot manager variables and what a write to
//! them must satisfy, the Boot Loader Interface's strings, the layout of a
//! SlopOS disk, the Limine configuration that boots it and the crash records
//! it keeps.
//!
//! Free of `alloc`, `unsafe` and I/O, so the kernel, `bootctl` and the host's
//! disk builder share one reading of every format, and the host tests it.
//! Layouts follow the UEFI Specification 2.10 and systemd's Boot Loader
//! Interface, taken as interface facts.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod bli;
pub mod crash;
pub mod crc32;
pub mod device_path;
pub mod gpt;
pub mod guid;
pub mod layout;
pub mod limine;
pub mod load_option;
pub mod variables;

pub use guid::Guid;
