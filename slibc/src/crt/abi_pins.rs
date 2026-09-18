//! Layout pins for the shared libc types the generated C headers hard-code.
//!
//! `slibc/build.rs` emits the assertions from the same table it renders
//! `slibc/include/**` from, so a `slopos-abi` struct that stops matching the
//! header is a compile error here rather than a header that quietly lies to
//! every C consumer. Nothing in this module is called; the assertions are the
//! content.

include!(concat!(env!("OUT_DIR"), "/abi_layout_pins.rs"));
