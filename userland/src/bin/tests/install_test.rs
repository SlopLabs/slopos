//! A kernel installed into a boot slot, tried once, committed, and a broken
//! one rolled back — across the reboots that takes, on a boot disk
//! `just test-install` attaches. Each boot runs this test again; a
//! non-volatile UEFI variable says which stage the last boot reached.
//!
//! 0 (booted `slopos-a`): with a dev disk at `/devel`, check out the host's
//!   `HEAD` there, build the tests kernel under a fresh build tag and install
//!   it into slot b; without one, clone slot a into b. Boot b once.
//! 1 (booted `slopos-b`, default still a): the running kernel carries the tag
//!   stage 0 built it with, if it built one, and then commits a change on the
//!   host's `HEAD` and pushes it to the host; commit b, boot `slopos-bad` once
//!   — a kernel whose command line panics it with `panic=reboot` — and reboot.
//! 2 (booted `slopos-b` again): the panic reset back to the default.
//!
//! Without a boot disk it has nothing to do and passes, as `devdisk_test`
//! does without a dev disk.

use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::devdisk::{DEVEL, git, selfhost, stdout_of, take_host_head, workspace};
use slopos_userland::syscall::UserUtsname;
use slopos_userland::syscall::core::{clock_gettime_ns, uname};
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::numbers::{
    EFI_VARIABLE_BOOTSERVICE_ACCESS, EFI_VARIABLE_NON_VOLATILE, EFI_VARIABLE_RUNTIME_ACCESS,
};
use std::process::Command;
use std::time::Instant;

/// A GUID of SlopOS's own for the stage counter:
/// 5a1b0b05-5105-4e57-a11e-0000000000a1.
const SLOPOS_GUID: [u8; 16] = [
    0x05, 0x0b, 0x1b, 0x5a, 0x05, 0x51, 0x57, 0x4e, 0xa1, 0x1e, 0, 0, 0, 0, 0, 0xa1,
];
const STAGE: &str = "SlopOSInstallTestStage";
/// The build tag stage 0 gave the kernel it built, for stage 1 to find in
/// `uname -v`.
const TAG: &str = "SlopOSInstallTestTag";
const ATTRS: u32 =
    EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS;

fn bootctl(args: &[&str]) -> Option<String> {
    let out = Command::new("/bin/bootctl").args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        note(&format!(
            "bootctl {}: {}{}",
            args.join(" "),
            stdout,
            String::from_utf8_lossy(&out.stderr)
        ));
        return None;
    }
    Some(stdout)
}

fn field<'a>(status: &'a str, key: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(": "))
}

fn stage() -> Option<u8> {
    let mut buf = [0u8; 4];
    match efivar_get(STAGE, &SLOPOS_GUID, &mut buf) {
        Ok(1) => Some(buf[0]),
        _ => None,
    }
}

/// Write a variable, or delete it with an empty `data`.
fn set_var(name: &str, data: &[u8]) -> bool {
    match efivar_set(name, &SLOPOS_GUID, ATTRS, data) {
        Ok(()) => true,
        Err(e) if data.is_empty() && e == SyscallError::ENOENT => true,
        Err(e) => {
            note(&format!("setting {name}: {e:?}"));
            false
        }
    }
}

fn set_stage(value: Option<u8>) -> bool {
    set_var(STAGE, value.as_slice())
}

fn built_tag() -> Option<String> {
    let mut buf = [0u8; 64];
    let len = efivar_get(TAG, &SLOPOS_GUID, &mut buf).ok()?;
    String::from_utf8(buf[..len].to_vec()).ok()
}

fn running_version() -> String {
    let mut uts = UserUtsname::default();
    if uname(&mut uts) != 0 {
        return String::new();
    }
    let len = uts
        .version
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(uts.version.len());
    String::from_utf8_lossy(&uts.version[..len]).into_owned()
}

/// Build the host's `HEAD` on the dev disk under a fresh tag and install it
/// into slot b; `None` when no dev disk is attached.
fn install_guest_build() -> Option<bool> {
    let root = match workspace() {
        Ok(root) => root,
        Err(why) => {
            note(&format!("{why}; cloning slot a instead of building"));
            return None;
        }
    };
    match take_host_head(&root) {
        Ok(commit) => println!("INSTALL-COMMIT {commit}"),
        Err(why) => {
            note(&why);
            return Some(false);
        }
    }
    let tag = format!("guest-{}", clock_gettime_ns());
    let _ = std::fs::remove_file(format!("{root}/builddir/kernel-tests.elf"));
    let started = Instant::now();
    let status = selfhost(&root, &["install", "tests"])
        .env("SLOPOS_BUILD_TAG", &tag)
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        note(&format!("selfhost.sh install tests: {status:?}"));
        return Some(false);
    }
    println!("INSTALL-BUILT {tag} in {} s", started.elapsed().as_secs());
    Some(set_var(TAG, tag.as_bytes()))
}

/// Commit a change on the tree's checkout in a scratch clone, which leaves
/// the developer's tree alone, and push it to the tree's `host` remote as
/// `install-test/<tag>`.
fn push_guest_commit(tag: &str) -> Result<String, String> {
    let root = workspace().map_err(str::to_owned)?;
    let clone = format!("{DEVEL}/ladder/install-push");
    let _ = std::fs::remove_dir_all(&clone);
    let url = stdout_of(
        git(&root, &root, &["remote", "get-url", "--push", "host"]),
        "git remote",
    )?;
    stdout_of(
        git(&root, &root, &["clone", "-q", "--shared", &root, &clone]),
        "git clone",
    )?;
    std::fs::write(format!("{clone}/GUEST-COMMIT"), format!("{tag}\n"))
        .map_err(|e| format!("writing GUEST-COMMIT: {e}"))?;
    stdout_of(git(&root, &clone, &["add", "GUEST-COMMIT"]), "git add")?;
    let message = format!("install_test: committed by the kernel built as {tag}");
    stdout_of(
        git(
            &root,
            &clone,
            &[
                "-c",
                "user.name=install_test",
                "-c",
                "user.email=install-test@slopos.invalid",
                "commit",
                "-q",
                "-m",
                &message,
            ],
        ),
        "git commit",
    )?;
    let refspec = format!("HEAD:refs/heads/install-test/{tag}");
    stdout_of(
        git(&root, &clone, &["push", "-q", url.trim(), &refspec]),
        "git push",
    )?;
    let commit = stdout_of(git(&root, &clone, &["rev-parse", "HEAD"]), "git rev-parse")?;
    let _ = std::fs::remove_dir_all(&clone);
    Ok(commit.trim().to_owned())
}

fn reboot_into(entry: &str, next: u8) -> bool {
    if bootctl(&["oneshot", entry]).is_none() || !set_stage(Some(next)) {
        return false;
    }
    println!("INSTALL-STAGE {next}: rebooting into {entry}");
    let _ = bootctl(&["reboot"]);
    note("bootctl reboot returned");
    false
}

fn boot_slot_install_commit_rollback() -> bool {
    let Some(status) = bootctl(&["status"]) else {
        return true;
    };
    let booted = field(&status, "booted").unwrap_or("-");
    let default = field(&status, "default").unwrap_or("-");
    let oneshot = field(&status, "oneshot").unwrap_or("-");
    println!(
        "INSTALL-STATUS stage={:?} booted={booted} default={default}",
        stage()
    );
    // Probed first: an image without UEFI runtime services answers ENODEV.
    let mut probe = [0u8; 1];
    if matches!(efivar_get(STAGE, &SLOPOS_GUID, &mut probe), Err(e) if e == SyscallError::ENODEV) {
        note("no UEFI runtime services; nothing to test");
        return true;
    }
    match stage() {
        None => {
            if booted != "slopos-a" || default != "slopos-a" {
                note(&format!("first boot is {booted} with default {default}"));
                return false;
            }
            let installed = match install_guest_build() {
                Some(built) => built,
                None => bootctl(&["clone", "a", "b"]).is_some(),
            };
            installed && reboot_into("slopos-b", 1)
        }
        Some(1) => {
            if booted != "slopos-b" || default != "slopos-a" || oneshot != "-" {
                note(&format!(
                    "the tried boot is {booted}, default {default}, oneshot {oneshot}"
                ));
                return false;
            }
            if let Some(tag) = built_tag() {
                let version = running_version();
                if !version.ends_with(&format!(" {tag}")) {
                    note(&format!(
                        "slot b runs {version:?}, not the build tagged {tag}"
                    ));
                    return false;
                }
                println!("INSTALL-BOOTED {version}");
                match push_guest_commit(&tag) {
                    Ok(commit) => println!("INSTALL-PUSHED {commit} install-test/{tag}"),
                    Err(why) => {
                        note(&why);
                        return false;
                    }
                }
            }
            let Some(committed) = bootctl(&["commit"]) else {
                return false;
            };
            if !committed.contains("default: slopos-b") {
                note(&format!("commit said {committed:?}"));
                return false;
            }
            reboot_into("slopos-bad", 2)
        }
        Some(2) => {
            let _ = set_stage(None);
            let _ = set_var(TAG, &[]);
            if booted != "slopos-b" || default != "slopos-b" || oneshot != "-" {
                note(&format!(
                    "after the broken slot: booted {booted}, default {default}, oneshot {oneshot}"
                ));
                return false;
            }
            note("slot b installed, tried, committed; a panicking slot rolled back");
            true
        }
        Some(other) => {
            note(&format!("unknown stage {other}"));
            let _ = set_stage(None);
            let _ = set_var(TAG, &[]);
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[(
        "boot_slot_install_commit_rollback",
        boot_slot_install_commit_rollback,
    )]);
}
