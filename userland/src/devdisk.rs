//! The dev disk as a test sees it: the source tree and toolchain
//! `build_devdisk.sh` seeded, mounted at `/devel` by the boot's `mount=`.

use std::process::{Command, Stdio};

pub const DEVEL: &str = "/devel";
const MARKER: &str = "/devel/SLOPOS-DEVDISK";

/// The dev disk's source tree, or why there is none to build.
pub fn workspace() -> Result<String, &'static str> {
    let text = std::fs::read_to_string(MARKER).map_err(|_| "no dev disk at /devel")?;
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key));
    match (field("source "), field("toolchain ")) {
        (Some(source), Some(_)) => Ok(format!("{DEVEL}/{source}")),
        _ => Err("the dev disk carries no source tree and toolchain"),
    }
}

/// `scripts/selfhost.sh <args>` in the tree at `root`, run as the guest's
/// developer runs it.
pub fn selfhost(root: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("/bin/shell");
    cmd.arg("scripts/selfhost.sh")
        .args(args)
        .current_dir(root)
        .env("KERNEL_CARGO_TIMINGS", "1")
        .stdin(Stdio::null());
    cmd
}
