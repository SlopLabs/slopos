//! Gates over the two contracts an install depends on: the private call
//! surface a base is built against (`base-interface`) and `libc.so`'s exports
//! and header layouts, which programs outside the base link against
//! (`libc-exports`). Both run before `just install` writes anything.

pub mod base_interface;
pub mod libc_exports;
