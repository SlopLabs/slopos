//! The boot screen as data and pixels, drawn the same by the kernel's splash
//! and by `/bin/roulette`: a ring of twelve segments ([`dial`]) that the
//! splash lights clockwise as the kernel boots and the Wheel of Fate then
//! runs its light round, the logo under it ([`logo`]), and where both sit on
//! a screen ([`Layout`]). The boot's last frame is the wheel's first.
//!
//! Everything lies over black, as the firmware leaves the screen, in the
//! desktop theme's ink, and is measured with integers alone, since the kernel
//! is built soft-float.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod dial;
pub mod logo;

use slopos_abi::draw::Color32;
use slopos_theme_core as theme;

pub use dial::{ALL, Dial, SEGMENTS};
pub use logo::Logo;

pub const BACKDROP: Color32 = Color32::BLACK;

/// How much of [`lit`] an unlit segment shows, out of 255.
const UNLIT: u8 = 40;

/// The ring's radius, per mille of the screen's shorter side, and a
/// segment's width per mille of the radius.
const RADIUS: i32 = 135;
const WIDTH: i32 = 80;
/// The ring's centre, per mille of the screen's height down from the top:
/// above the middle, as the eye takes it, leaving the foot to the logo.
const CENTRE: i32 = 460;

/// A segment fully lit: the desktop's text.
pub fn lit() -> Color32 {
    theme::current().text
}

/// A segment unlit: a little of [`lit`] over the backdrop.
pub fn unlit() -> Color32 {
    over_backdrop(lit(), UNLIT)
}

/// `ink` over the backdrop, at `level` out of 255.
pub const fn over_backdrop(ink: Color32, level: u8) -> Color32 {
    theme::mix(BACKDROP, ink, level)
}

/// How lit segment `k` is, out of 255, `percent` of the way through boot:
/// each lights in turn over its twelfth, clockwise from the top.
pub const fn boot_level(percent: u32, k: usize) -> u8 {
    let percent = if percent > 100 { 100 } else { percent };
    let into = (percent * SEGMENTS as u32).saturating_sub(100 * k as u32);
    let into = if into > 100 { 100 } else { into };
    (into * 255 / 100) as u8
}

/// Each segment's ink `percent` of the way through boot.
pub fn boot_inks(percent: u32) -> [Color32; SEGMENTS] {
    let (unlit, lit) = (unlit(), lit());
    core::array::from_fn(|k| theme::mix(unlit, lit, boot_level(percent, k)))
}

/// The segments whose ink differs between `before` and `after` percent.
pub fn boot_changed(before: u32, after: u32) -> u16 {
    (0..SEGMENTS)
        .filter(|&k| boot_level(before, k) != boot_level(after, k))
        .fold(0, |mask, k| mask | 1 << k)
}

/// Where the ring and the logo sit on a screen.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    pub dial: Dial,
    pub logo: Logo,
}

impl Layout {
    pub fn for_screen(width: u32, height: u32) -> Layout {
        let (w, h) = (width as i32, height as i32);
        let unit = w.min(h);
        let radius = (unit * RADIUS / 1000).max(12);
        let width = (radius * WIDTH / 1000).max(3);
        Layout {
            dial: Dial::new(w / 2, h * CENTRE / 1000, radius, width),
            logo: Logo::for_screen(w, h, unit),
        }
    }
}

#[cfg(test)]
mod tests;
