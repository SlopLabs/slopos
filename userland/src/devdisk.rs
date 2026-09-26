//! The dev disk as a test sees it: the source tree and toolchain
//! `build_devdisk.sh` seeded, mounted at `/devel` by the boot's `mount=`.

use std::process::{Command, Stdio};

pub const DEVEL: &str = "/devel";
const MARKER: &str = "/devel/SLOPOS-DEVDISK";

/// `(source tree, toolchain prefix)`, or `None` with the reason when the
/// boot mounted no dev disk carrying both.
pub fn workspace() -> Result<(String, String), &'static str> {
    let text = std::fs::read_to_string(MARKER).map_err(|_| "no dev disk at /devel")?;
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key));
    match (field("source "), field("toolchain ")) {
        (Some(source), Some(toolchain)) => {
            Ok((format!("{DEVEL}/{source}"), format!("{DEVEL}/{toolchain}")))
        }
        _ => Err("the dev disk carries no source tree and toolchain"),
    }
}

/// `scripts/build_kernel.sh` run by `/bin/shell` over the tree at `root`,
/// with nothing on `PATH` but the toolchain at `prefix` and the coreutils;
/// the kernel lands at `<root>/builddir/kernel-<variant>.elf`. Output is
/// inherited, so the last crate cargo named is where a stuck build stopped.
pub fn kernel_build(root: &str, prefix: &str, features: &[&str]) -> Command {
    let mut cmd = Command::new("/bin/shell");
    cmd.args(["scripts/build_kernel.sh", "builddir", "builddir/target"])
        .args(features)
        .current_dir(root)
        .env("PATH", format!("{prefix}/bin:/bin"))
        .env("CARGO_HOME", format!("{DEVEL}/cargo-home"))
        .env("KERNEL_CARGO_TIMINGS", "1")
        // Pinned, as the host's reference build pins it: cargo's default
        // depends on `CI`, and the profile is hashed into every crate's
        // metadata.
        .env("CARGO_INCREMENTAL", "1")
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null());
    cmd
}

/// The features `build_kernel.sh` takes for the tests kernel.
pub const TESTS_FEATURES: &[&str] = &["slopos-testing/qemu-exit kernel/tests"];
