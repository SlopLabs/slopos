use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::devdisk::{TESTS_FEATURES, kernel_build, workspace};
use std::fs;
use std::time::Instant;

/// The dev disk's kernel build, timed; with no dev disk there is nothing to
/// build and the test passes by saying so.
fn guest_builds(variant: &str, features: &[&str]) -> bool {
    let (root, prefix) = match workspace() {
        Ok(w) => w,
        Err(why) => {
            note(why);
            return true;
        }
    };
    let elf = format!("{root}/builddir/kernel-{variant}.elf");
    let _ = fs::remove_file(&elf);
    let started = Instant::now();
    let status = kernel_build(&root, &prefix, features).status();
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
    guest_builds("tests", TESTS_FEATURES)
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
