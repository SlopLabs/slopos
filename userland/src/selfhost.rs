//! The self-hosting workspace as a test sees it: the clone the host seeds at
//! `/src/slopos`, built with the toolchain image the root is offered.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use slopos_abi::fs::{DEFAULT_PATH, SESSION_GID, SESSION_UID};
use slopos_extension_core::OFFERED;
use slopos_extension_core::declare::mount_point;
use slopos_extension_core::sidecar::Sidecar;

use crate::apps::extctl::store::Store;

pub const SOURCE: &str = "/src/slopos";
/// Room on the root disk; `/tmp` is memory.
pub const SCRATCH: &str = "/var/tmp";
const TOOLCHAIN: &str = "slopos-toolchain";
static TOOLCHAIN_MOUNT: LazyLock<String> = LazyLock::new(|| mount_point(TOOLCHAIN));
static TOOLCHAIN_SHOWN: LazyLock<Result<Option<&'static str>, String>> =
    LazyLock::new(shown_toolchain);

/// `/opt/slopos-toolchain`, once it shows the image the root is offered;
/// `None` on a root offered none.
pub fn toolchain() -> Result<Option<&'static str>, String> {
    TOOLCHAIN_SHOWN.clone()
}

fn shown_toolchain() -> Result<Option<&'static str>, String> {
    let meta = format!("{OFFERED}/{TOOLCHAIN}.meta");
    let text = match std::fs::read(&meta) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{meta}: {e}")),
    };
    let want = Sidecar::parse(&text)
        .filter(|sidecar| sidecar.name == TOOLCHAIN)
        .ok_or_else(|| format!("{meta}: no sidecar of {TOOLCHAIN}"))?
        .sha256;
    let mount = TOOLCHAIN_MOUNT.as_str();
    if shown_root() != Some(want) {
        return Err(format!(
            "{mount} does not show the image {meta} names, which init installs at boot"
        ));
    }
    Ok(Some(mount))
}

/// The root of the toolchain image the store keeps current, once its
/// activation shows it.
fn shown_root() -> Option<[u8; 32]> {
    let record = Store::open(false).ok()?.record(TOOLCHAIN).ok()??;
    crate::activation::shown(&record).ok()?;
    record.current.map(|image| image.sha256)
}

/// The workspace, or why there is none to build.
pub fn workspace() -> Result<&'static str, &'static str> {
    if !Path::new(SOURCE).join(".git").is_dir() {
        return Err("no clone at /src/slopos");
    }
    match toolchain() {
        Ok(Some(_)) => Ok(SOURCE),
        Ok(None) => Err("no toolchain on this root"),
        Err(_) => Err("the toolchain image this root is offered is not shown"),
    }
}

/// `just <args>` in the tree at `root`, run as the guest's developer runs it:
/// as the session, whose clone it is, with the `PATH` their shell exports.
/// bash exports none it was not given, and rustc hands a linker it finds no
/// `PATH` for only its own tools directories. cargo keeps its caches in the
/// clone, where the session can write.
pub fn just(root: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("just");
    cmd.uid(SESSION_UID)
        .gid(SESSION_GID)
        .args(args)
        .current_dir(root)
        .env("PATH", OsStr::from_bytes(DEFAULT_PATH.to_bytes()))
        .env_remove("LD_LIBRARY_PATH")
        .env("CARGO_HOME", format!("{root}/builddir/cargo-home"))
        .env("KERNEL_CARGO_TIMINGS", "1")
        .stdin(Stdio::null());
    cmd
}

/// `cmd`, a step that installs, with what `bootctl` asks privd asked through
/// a connection this process makes, so a root test's install is the test's
/// own and waits for nobody at the machine.
fn installing(mut cmd: Command, what: &str) -> Result<(), String> {
    let privd = crate::privd::Connection::open_within(crate::privd::SWITCH_WAIT)?;
    privd.hand_to(&mut cmd);
    let status = cmd.status();
    drop(privd);
    match status {
        Ok(status) if status.success() => Ok(()),
        status => Err(format!("{what}: {status:?}")),
    }
}

/// `just install <variant>`, which tries the base it builds, `bootctl base
/// commit` of that base once it runs, and `just install-kernel <variant>`,
/// under the build tag `tag`, each through a privd connection of its own: a
/// base that runs at once drains the privd the first one asked.
pub fn just_install(root: &str, variant: &str, tag: &str) -> Result<(), String> {
    let tagged = |recipe: &str| {
        let mut cmd = just(root, &[recipe, variant]);
        cmd.env("SLOPOS_BUILD_TAG", tag);
        cmd
    };
    installing(tagged("install"), &format!("just install {variant}"))?;
    let runs = std::fs::read_to_string(slopos_boot_core::base::BUILD_TAG_PATH)
        .is_ok_and(|running| running.trim() == tag);
    if runs {
        let base = if variant.ends_with("tests") {
            "initramfs-tests.cpio"
        } else {
            "initramfs.cpio"
        };
        let mut commit = Command::new("bootctl");
        commit
            .uid(SESSION_UID)
            .gid(SESSION_GID)
            .args(["base", "commit", &format!("{root}/builddir/{base}")])
            .stdin(Stdio::null());
        installing(commit, "bootctl base commit")?;
    }
    installing(
        tagged("install-kernel"),
        &format!("just install-kernel {variant}"),
    )
}

/// `git <args>` in `dir`, the git on the default search path, as the session,
/// whose clone it is: git refuses a repository another uid owns.
pub fn git(dir: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.uid(SESSION_UID)
        .gid(SESSION_GID)
        .args(args)
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
