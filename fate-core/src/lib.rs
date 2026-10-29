//! The Wheel of Fate as data and pixels, host-tested apart from
//! `/bin/roulette`, whose crate builds no tests.
//!
//! The kernel decides the fate (`roulette`) and acts on it
//! (`roulette_result`); nothing here can change either. This crate only shows
//! it, on the boot splash's own ring: which pocket the fate lands in
//! ([`wheel`]), how the light gets there ([`spin`], a function of elapsed
//! time rather than of frames drawn), where everything sits on the screen
//! ([`layout`]), and the pixels of each frame ([`scene`]), drawn into a
//! buffer the caller presents.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod layout;
pub mod scene;
pub mod spin;
pub mod wheel;

#[cfg(test)]
mod tests;
