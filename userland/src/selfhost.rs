//! The self-hosting workspace as a test sees it: the clone the host seeds at
//! `/src/slopos`, built with the toolchain it installs at `/usr/local`.

use std::path::Path;
use std::process::{Command, Stdio};

pub const SOURCE: &str = "/src/slopos";
/// Room on the root disk; `/tmp` is memory.
pub const SCRATCH: &str = "/var/tmp";

/// The workspace, or why there is none to build.
pub fn workspace() -> Result<&'static str, &'static str> {
    if !Path::new(SOURCE).join(".git").is_dir() {
        return Err("no clone at /src/slopos");
    }
    if !Path::new("/usr/local/bin/cargo").is_file() {
        return Err("no toolchain at /usr/local");
    }
    Ok(SOURCE)
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

/// `git <args>` in `dir`, the git on the default search path.
pub fn git(dir: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
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

/// The host's `HEAD`, checked out in a tree by [`take_host_head`], and what the
/// tree had checked out before: its branch, or its commit when detached.
pub struct HostHead {
    pub commit: String,
    pub before: String,
}

/// Check out the host checkout's `HEAD` in the tree at `root`, as its
/// developer takes the host's commits. A tree with uncommitted edits is
/// refused: they would be built with it.
pub fn take_host_head(root: &str) -> Result<HostHead, String> {
    let edits = stdout_of(
        git(root, &["status", "--porcelain", "--untracked-files=no"]),
        "git status",
    )?;
    if !edits.is_empty() {
        return Err(format!("the tree carries uncommitted edits:\n{edits}"));
    }
    let before = stdout_of(
        git(root, &["symbolic-ref", "-q", "--short", "HEAD"]),
        "git symbolic-ref HEAD",
    )
    .or_else(|_| stdout_of(git(root, &["rev-parse", "HEAD"]), "git rev-parse HEAD"))?;
    stdout_of(
        git(root, &["fetch", "-q", "origin", "HEAD"]),
        "git fetch origin HEAD",
    )?;
    stdout_of(
        git(root, &["checkout", "-q", "--detach", "FETCH_HEAD"]),
        "git checkout FETCH_HEAD",
    )?;
    let commit = stdout_of(git(root, &["rev-parse", "HEAD"]), "git rev-parse HEAD")?;
    Ok(HostHead {
        commit: commit.trim().to_owned(),
        before: before.trim().to_owned(),
    })
}

/// Check `what`, a [`HostHead::before`], out again in the tree at `root`.
pub fn check_out(root: &str, what: &str) -> Result<(), String> {
    stdout_of(git(root, &["checkout", "-q", what, "--"]), "git checkout").map(drop)
}
