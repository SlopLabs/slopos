use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::devdisk::{selfhost, workspace};
use std::fs;
use std::time::Instant;

/// The dev disk's kernel build, timed; with no dev disk there is nothing to
/// build and the test passes by saying so. `clean` starts from an empty target
/// directory and no symbol table, as the host's reference build does, so the
/// time measures a whole build rather than whatever the last boot left behind.
fn guest_builds(variant: &str, clean: bool) -> bool {
    let root = match workspace() {
        Ok(root) => root,
        Err(why) => {
            note(why);
            return true;
        }
    };
    let elf = format!("{root}/builddir/kernel-{variant}.elf");
    let _ = fs::remove_file(&elf);
    if clean {
        let _ = fs::remove_dir_all(format!("{root}/builddir/target"));
        for v in ["dev", "tests"] {
            let _ = fs::remove_file(format!("{root}/builddir/kallsyms-{v}.rs"));
        }
    }
    let started = Instant::now();
    let status = selfhost(&root, &["build", variant]).status();
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            note(&format!("spawning selfhost.sh: {e}"));
            return false;
        }
    };
    if !status.success() {
        note(&format!(
            "selfhost.sh build {variant} exited {:?}",
            status.code()
        ));
        return false;
    }
    match fs::metadata(&elf) {
        Ok(meta) => {
            note(&format!(
                "{variant} kernel, {} bytes, in {:.1} s",
                meta.len(),
                started.elapsed().as_secs_f64()
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
    guest_builds("dev", true)
}

fn guest_builds_the_tests_kernel() -> bool {
    guest_builds("tests", false)
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
