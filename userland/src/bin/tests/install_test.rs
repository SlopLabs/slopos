//! A kernel installed into a boot slot, tried once, committed, and a broken
//! one rolled back — across the reboots that takes, on a boot disk
//! `just test-install` attaches. Each boot runs this test again; a
//! non-volatile UEFI variable says which stage the last boot reached.
//!
//! 0 (booted `slopos-a`): clone slot a into b, boot b once, reboot.
//! 1 (booted `slopos-b`, default still a): commit b, boot `slopos-bad` once —
//!   a kernel whose command line panics it with `panic=reboot` — and reboot.
//! 2 (booted `slopos-b` again): the panic reset back to the default.
//!
//! Without a boot disk it has nothing to do and passes, as `devdisk_test`
//! does without a dev disk.

use slopos_userland as _;

use slopos_slibc::test_harness::note;
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::numbers::{
    EFI_VARIABLE_BOOTSERVICE_ACCESS, EFI_VARIABLE_NON_VOLATILE, EFI_VARIABLE_RUNTIME_ACCESS,
};
use std::process::Command;

/// A GUID of SlopOS's own for the stage counter:
/// 5a1b0b05-5105-4e57-a11e-0000000000a1.
const SLOPOS_GUID: [u8; 16] = [
    0x05, 0x0b, 0x1b, 0x5a, 0x05, 0x51, 0x57, 0x4e, 0xa1, 0x1e, 0, 0, 0, 0, 0, 0xa1,
];
const STAGE: &str = "SlopOSInstallTestStage";

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

fn set_stage(value: Option<u8>) -> bool {
    let attrs =
        EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS;
    let data: &[u8] = match &value {
        Some(v) => core::slice::from_ref(v),
        None => &[],
    };
    match efivar_set(STAGE, &SLOPOS_GUID, attrs, data) {
        Ok(()) => true,
        Err(e) if value.is_none() && e == SyscallError::ENOENT => true,
        Err(e) => {
            note(&format!("setting {STAGE}: {e:?}"));
            false
        }
    }
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
            bootctl(&["clone", "a", "b"]).is_some() && reboot_into("slopos-b", 1)
        }
        Some(1) => {
            if booted != "slopos-b" || default != "slopos-a" || oneshot != "-" {
                note(&format!(
                    "the tried boot is {booted}, default {default}, oneshot {oneshot}"
                ));
                return false;
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
