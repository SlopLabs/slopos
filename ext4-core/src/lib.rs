//! The ext4 and jbd2 on-disk formats as data: checksums, the superblock,
//! group descriptor, inode and directory-tail codecs, the extent tree over a
//! caller's block store, the journal's block formats and its recovery.
//!
//! Free of `alloc`, `unsafe` and I/O, so every decision the kernel's driver
//! makes about the format is host-testable against images e2fsprogs wrote.
//! Layouts follow kernel.org's "ext4 Data Structures and Algorithms", taken
//! as interface facts.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod bytes;
pub mod crc;
pub mod dir;
pub mod extent;
#[cfg(feature = "fixture")]
pub mod fixture;
pub mod group;
pub mod inode;
pub mod jbd2;
pub mod profile;
pub mod recovery;
pub mod superblock;
pub mod xattr;

pub use crc::{crc16, crc32c};
