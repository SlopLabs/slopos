use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::selfhost::{HostHead, check_out, selfhost, take_host_head, workspace};
use std::fs;
use std::sync::{LazyLock, OnceLock};
use std::time::Instant;

static FREE_WHEN_CLEAN: OnceLock<u64> = OnceLock::new();

fn root_free_bytes() -> u64 {
    slopos_userland::syscall::fs::statfs_path(c"/".as_ptr())
        .map_or(0, |stats| stats.f_bavail.saturating_mul(stats.f_bsize))
}

/// The workspace with the host's `HEAD` checked out, taken once for every
/// build. A root with no workspace passes.
static TREE: LazyLock<Result<(&str, HostHead), (bool, String)>> = LazyLock::new(|| {
    let root = workspace().map_err(|why| (true, why.to_owned()))?;
    let head = take_host_head(root).map_err(|why| (false, why))?;
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

/// The workspace's build of a kernel, its userland and the base it boots with,
/// timed. `clean` drops the target directory and symbol tables so the time is
/// a whole build.
fn guest_builds(variant: &str, base: &str, clean: bool) -> bool {
    let root = match &*TREE {
        Ok((root, _)) => root,
        Err((verdict, why)) => {
            note(why);
            return *verdict;
        }
    };
    let elf = format!("{root}/builddir/kernel-{variant}.elf");
    let base = format!("{root}/builddir/{base}");
    let _ = fs::remove_file(&elf);
    let _ = fs::remove_file(&base);
    if clean {
        let _ = fs::remove_dir_all(format!("{root}/builddir/target"));
        for v in ["dev", "tests"] {
            let _ = fs::remove_file(format!("{root}/builddir/kallsyms-{v}.rs"));
        }
        let _ = FREE_WHEN_CLEAN.set(root_free_bytes());
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
    match (fs::metadata(&elf), fs::metadata(&base)) {
        (Ok(kernel), Ok(image)) => {
            let used = FREE_WHEN_CLEAN
                .get()
                .map_or(0, |clean| clean.saturating_sub(root_free_bytes()));
            note(&format!(
                "kernel {} bytes, base {} bytes, in {:.1} s; the builds hold {} MiB of the root",
                kernel.len(),
                image.len(),
                started.elapsed().as_secs_f64(),
                used >> 20
            ));
            true
        }
        (Err(e), _) => {
            note(&format!("{elf}: {e}"));
            false
        }
        (_, Err(e)) => {
            note(&format!("{base}: {e}"));
            false
        }
    }
}

fn guest_builds_the_dev_system() -> bool {
    guest_builds("dev", "initramfs.cpio", true)
}

fn guest_builds_the_tests_system() -> bool {
    guest_builds("tests", "initramfs-tests.cpio", false)
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
        ("guest_builds_the_dev_system", guest_builds_the_dev_system),
        (
            "guest_builds_the_tests_system",
            guest_builds_the_tests_system,
        ),
        (
            "guest_returns_to_its_checkout",
            guest_returns_to_its_checkout,
        ),
    ]);
}
