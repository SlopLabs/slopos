//! SlopOS Graphics Algorithms
//!
//! Generic drawing primitives, font rendering, and damage tracking
//! algorithms that operate on any `DrawTarget` implementation.
//!
//! This crate sits between the ABI trait definitions (in `slopos-abi`)
//! and the concrete implementations (in `video` and `userland`).

#![no_std]
#![forbid(unsafe_code)]

pub mod damage;
pub mod font_render;
pub mod primitives;

pub use damage::{DamageTracker, InternalDamageTracker};
