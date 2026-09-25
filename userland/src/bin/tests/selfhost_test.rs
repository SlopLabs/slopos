use slopos_userland as _;

use slopos_slibc::test_harness::note;
use std::fs;
use std::process::{Command, Stdio};
use std::time::Instant;

const DEVEL: &str = "/devel";
const MARKER: &str = "/devel/SLOPOS-DEVDISK";

/// The seeded source tree and the toolchain staged inside it, when the boot
/// mounted a dev disk carrying both. `Err` is the verdict otherwise: a run
/// with no dev disk attached passes by saying so.
fn workspace() -> Result<(String, String), bool> {
    let Ok(text) = fs::read_to_string(MARKER) else {
        note("no dev disk at /devel");
        return Err(true);
    };
    let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key));
    match (field("source "), field("toolchain ")) {
        (Some(source), Some(toolchain)) => {
            Ok((format!("{DEVEL}/{source}"), format!("{DEVEL}/{toolchain}")))
        }
        _ => {
            note("the dev disk carries no source tree and toolchain");
            Err(true)
        }
    }
}

/// `scripts/build_kernel.sh` run by `/bin/shell` over the dev disk's tree,
/// with nothing on `PATH` but the staged toolchain and the coreutils. Its
/// output streams to the console: the build takes hours under emulation, and
/// the last crate cargo named is where a stuck one stopped.
fn guest_builds(variant: &str, features: &[&str]) -> bool {
    let (root, prefix) = match workspace() {
        Ok(w) => w,
        Err(verdict) => return verdict,
    };
    let elf = format!("{root}/builddir/kernel-{variant}.elf");
    let _ = fs::remove_file(&elf);
    let started = Instant::now();
    let status = Command::new("/bin/shell")
        .args(["scripts/build_kernel.sh", "builddir", "builddir/target"])
        .args(features)
        .current_dir(&root)
        .env("PATH", format!("{prefix}/bin:/bin"))
        .env("CARGO_HOME", format!("{DEVEL}/cargo-home"))
        .env("KERNEL_CARGO_TIMINGS", "1")
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .status();
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            note(&format!("spawning /bin/shell: {e}"));
            return false;
        }
    };
    if !status.success() {
        note(&format!("build_kernel.sh exited {:?}", status.code()));
        return false;
    }
    match fs::metadata(&elf) {
        Ok(meta) => {
            note(&format!(
                "{variant} kernel, {} bytes, in {} s",
                meta.len(),
                started.elapsed().as_secs()
            ));
            true
        }
        Err(e) => {
            note(&format!("{elf}: {e}"));
            false
        }
    }
}

fn guest_builds_the_dev_kernel() -> bool {
    guest_builds("dev", &[])
}

fn guest_builds_the_tests_kernel() -> bool {
    guest_builds("tests", &["slopos-testing/qemu-exit kernel/tests"])
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("guest_builds_the_dev_kernel", guest_builds_the_dev_kernel),
        (
            "guest_builds_the_tests_kernel",
            guest_builds_the_tests_kernel,
        ),
    ]);
}
