//! Teaches `cargo`'s `unexpected_cfgs` lint about `target_os = "slopos"`, as
//! `slibc/staticlib/build.rs` does: xtask builds on SlopOS too, where cargo
//! takes no `+TOOLCHAIN`, and a stock host toolchain does not know the name.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(target_os, values(\"slopos\"))");
    println!("cargo::rerun-if-changed=build.rs");
}
