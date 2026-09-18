//! Teaches `cargo`'s `unexpected_cfgs` lint about `target_os = "slopos"`.
//!
//! `src/lib.rs` is `no_std` with a panic runtime for SlopOS and an empty `std`
//! shim everywhere else, so it names the target. A stock host toolchain has no
//! `slopos` in its target list and would warn — which the workspace's
//! `warnings = "deny"` turns into an error — while the `+slopos` toolchain
//! knows the name and would not. Declaring the value keeps both honest.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"slopos\"))");
    println!("cargo::rerun-if-changed=build.rs");
}
