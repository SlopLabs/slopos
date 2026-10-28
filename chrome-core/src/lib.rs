//! The desktop chrome's geometry and state, host-tested apart from the
//! compositor, whose crate builds no tests.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod clock;
pub mod gate;
pub mod indicator;
pub mod key;
pub mod menu;
pub mod netstate;
pub mod placement;
pub mod positioner;
pub mod status;
