//! A kernel installed into a boot slot, tried once, committed, and a broken
//! one rolled back — across the reboots that takes, on a boot disk
//! `just test-install` attaches. Each boot runs this test again; a
//! non-volatile UEFI variable says which stage the last boot reached.
//!
//! 0 (booted `slopos-a`): with a dev disk at `/devel`, build the tests kernel
//!   there under a fresh build tag and install it into slot b; without one,
//!   clone slot a into b. Boot b once.
//! 1 (booted `slopos-b`, default still a): the running kernel carries the tag
//!   stage 0 built it with, if it built one; commit b, boot `slopos-bad` once —
//!   a kernel whose command line panics it with `panic=reboot` — and reboot.
//! 2 (booted `slopos-b` again): the panic reset back to the default.
//!
//! Without a boot disk it has nothing to do and passes, as `devdisk_test`
//! does without a dev disk.

use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::devdisk::{TESTS_FEATURES, kernel_build, workspace};
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

/// Build the tests kernel on the dev disk under a fresh tag and install it
/// into slot b; `None` when no dev disk is attached.
fn install_guest_build() -> Option<bool> {
    let (root, prefix) = match workspace() {
        Ok(w) => w,
        Err(why) => {
            note(&format!("{why}; cloning slot a instead of building"));
            return None;
        }
    };
    let tag = format!("guest-{}", clock_gettime_ns());
    let elf = format!("{root}/builddir/kernel-tests.elf");
    let _ = std::fs::remove_file(&elf);
    let started = Instant::now();
    let status = kernel_build(&root, &prefix, TESTS_FEATURES)
        .env("SLOPOS_BUILD_TAG", &tag)
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        note(&format!("the guest's kernel build: {status:?}"));
        return Some(false);
    }
    println!("INSTALL-BUILT {tag} in {} s", started.elapsed().as_secs());
    Some(set_var(TAG, tag.as_bytes()) && bootctl(&["install", "b", &elf]).is_some())
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
