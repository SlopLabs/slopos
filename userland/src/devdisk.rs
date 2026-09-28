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
/// developer runs it: executed directly, so its `#!/bin/sh` picks the shell.
pub fn selfhost(root: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(format!("{root}/scripts/selfhost.sh"));
    cmd.args(args)
        .current_dir(root)
        .env("KERNEL_CARGO_TIMINGS", "1")
        .stdin(Stdio::null());
    cmd
}

/// `git <args>` in `dir`, with the git the tree at `root` carries in its
/// toolchain, where `selfhost.sh` finds cargo.
pub fn git(root: &str, dir: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(format!("{root}/third_party/rust-slopos/bin/git"));
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
}

/// What `cmd` printed, or its failure with what it said about it.
pub fn stdout_of(mut cmd: Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim_end()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Check out the host checkout's `HEAD` in the tree at `root`, as its
/// developer takes the host's commits, and answer it. A tree with uncommitted
/// edits is refused: they would be built with it.
pub fn take_host_head(root: &str) -> Result<String, String> {
    let edits = stdout_of(
        git(
            root,
            root,
            &["status", "--porcelain", "--untracked-files=no"],
        ),
        "git status",
    )?;
    if !edits.is_empty() {
        return Err(format!("the tree carries uncommitted edits:\n{edits}"));
    }
    stdout_of(
        git(root, root, &["fetch", "-q", "origin", "HEAD"]),
        "git fetch origin HEAD",
    )?;
    stdout_of(
        git(root, root, &["checkout", "-q", "--detach", "FETCH_HEAD"]),
        "git checkout FETCH_HEAD",
    )?;
    stdout_of(
        git(root, root, &["rev-parse", "HEAD"]),
        "git rev-parse HEAD",
    )
    .map(|commit| commit.trim().to_owned())
}
