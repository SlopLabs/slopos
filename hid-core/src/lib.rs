//! HID as data, whatever carries it: report descriptors parsed into storage
//! the caller passes, the boot keyboard and mouse reports, and what a
//! keyboard's or a pointer's report says. Section numbers are HID 1.11's;
//! usages are the HID Usage Tables'.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod boot;
pub mod descriptor;
pub mod keyboard;
pub mod pointer;
pub mod usage;

pub use descriptor::{Descriptor, Element, Error, Field, Kind, Parsed, parse};

#[cfg(test)]
pub(crate) mod build;
