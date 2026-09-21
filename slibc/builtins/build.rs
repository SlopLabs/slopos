//! As `slibc/staticlib/build.rs`: `src/lib.rs` names `target_os = "slopos"`,
//! which a stock host toolchain would warn about and `warnings = "deny"` would
//! then turn into an error.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"slopos\"))");
    println!("cargo::rerun-if-changed=build.rs");
}
