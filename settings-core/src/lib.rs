//! The settings SlopOS keeps outside the apps that change them, as data: the
//! network's, which `/bin/netctl` keeps in [`network::PATH`] and applies, and
//! each person's preferences, which `/bin/prefs` keeps under their home and
//! the desktop reads. Settings changes both through the same code, so the app
//! and a terminal refuse the same values with the same words.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod background;
pub mod network;
pub mod prefs;
