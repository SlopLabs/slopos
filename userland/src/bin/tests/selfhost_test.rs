use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::devdisk::{HostHead, check_out, selfhost, take_host_head, workspace};
use std::fs;
use std::sync::LazyLock;
use std::time::Instant;

/// The dev disk's tree with the host's `HEAD` checked out, taken once for
/// every build. With no dev disk the verdict is a pass.
static TREE: LazyLock<Result<(String, HostHead), (bool, String)>> = LazyLock::new(|| {
    let root = workspace().map_err(|why| (true, why.to_owned()))?;
    let head = take_host_head(&root).map_err(|why| (false, why))?;
    Ok((root, head))
});

fn guest_takes_the_host_head() -> bool {
    match &*TREE {
        Ok((_, head)) => {
            note(&format!("SELFHOST-COMMIT {}", head.commit));
            true
        }
        Err((verdict, why)) => {
            note(why);
            *verdict
        }
    }
}

/// The dev disk's kernel build, timed. `clean` drops the target directory and
/// symbol table so the time is a whole build.
fn guest_builds(variant: &str, clean: bool) -> bool {
    let root = match &*TREE {
        Ok((root, _)) => root,
        Err((verdict, why)) => {
            note(why);
            return *verdict;
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
    let status = selfhost(root, &["build", variant]).status();
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

/// The tree's developer finds it where they left it.
fn guest_returns_to_its_checkout() -> bool {
    match &*TREE {
        Ok((root, head)) => match check_out(root, &head.before) {
            Ok(()) => true,
            Err(why) => {
                note(&why);
                false
            }
        },
        Err((verdict, _)) => *verdict,
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("guest_takes_the_host_head", guest_takes_the_host_head),
        ("guest_builds_the_dev_kernel", guest_builds_the_dev_kernel),
        (
            "guest_builds_the_tests_kernel",
            guest_builds_the_tests_kernel,
        ),
        (
            "guest_returns_to_its_checkout",
            guest_returns_to_its_checkout,
        ),
    ]);
}
