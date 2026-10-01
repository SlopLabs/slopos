//! A kernel installed into a boot slot, tried once, committed, and a broken
//! one rolled back — across the reboots that takes, on a boot disk
//! `just test-install` attaches, and through the firmware entry an installer
//! registers for SlopOS's loader. Each boot runs this test again; a
//! non-volatile UEFI variable says which stage the last boot reached.
//!
//! 0 (booted `slopos-a` by the removable-media path, as a disk the firmware
//!   has no entry for is): register SlopOS's firmware entry first in
//!   `BootOrder`. With a workspace at `/src/slopos`, check out the host's
//!   `HEAD` there, build the tests kernel and base under a fresh build tag,
//!   install them into slot b and return the tree to its own checkout;
//!   without one, clone slot a into b. Boot b once.
//! 1 (booted `slopos-b` through the firmware entry, default still a): the
//!   running kernel and base carry the tag stage 0 built them with, if it
//!   built them, and the kernel then commits a change on the host's `HEAD`
//!   and pushes it to the host; commit b, boot `slopos-bad` once — a kernel
//!   whose command line panics it with `panic=reboot` — and reboot.
//! 2 (booted `slopos-b` again, through the firmware entry): the panic reset
//!   back to the default.
//!
//! Without a boot disk it has nothing to do and passes, as `toolchain_test`
//! does without a toolchain.

use slopos_userland as _;

use slopos_boot_core::variables;
use slopos_slibc::test_harness::note;
use slopos_userland::boot_disk::{
    BootDisk, FindError, boot_current, boot_order, firmware_entry, register_firmware_entry,
};
use slopos_userland::selfhost::{
    SCRATCH, check_out, git, selfhost, stdout_of, take_host_head, workspace,
};
use slopos_userland::syscall::UserUtsname;
use slopos_userland::syscall::core::{clock_gettime_ns, uname};
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use std::process::Command;
use std::time::Instant;

const BASE_TAG_FILE: &str = "/usr/share/slopos/build-tag";

const STAGE: &str = "SlopOSInstallTestStage";
/// The build tag stage 0 gave the system it built, for stage 1 to find in
/// `uname -v` and in the base.
const TAG: &str = "SlopOSInstallTestTag";

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
    match efivar_get(STAGE, &variables::SLOPOS.0, &mut buf) {
        Ok(1) => Some(buf[0]),
        _ => None,
    }
}

/// Write a variable, or delete it with an empty `data`.
fn set_var(name: &str, data: &[u8]) -> bool {
    match efivar_set(name, &variables::SLOPOS.0, variables::PERSISTENT, data) {
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
    let len = efivar_get(TAG, &variables::SLOPOS.0, &mut buf).ok()?;
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

/// Build the host's `HEAD` in the workspace under a fresh tag, install it
/// into slot b and check the tree's own checkout out again; `None` when the
/// root carries no workspace.
fn install_guest_build() -> Option<bool> {
    let root = match workspace() {
        Ok(root) => root,
        Err(why) => {
            note(&format!("{why}; cloning slot a instead of building"));
            return None;
        }
    };
    let head = match take_host_head(root) {
        Ok(head) => head,
        Err(why) => {
            note(&why);
            return Some(false);
        }
    };
    println!("INSTALL-COMMIT {}", head.commit);
    let tag = format!("guest-{}", clock_gettime_ns());
    for built in ["kernel-tests.elf", "initramfs-tests.cpio"] {
        let _ = std::fs::remove_file(format!("{root}/builddir/{built}"));
    }
    let started = Instant::now();
    let status = selfhost(root, &["install", "tests"])
        .env("SLOPOS_BUILD_TAG", &tag)
        .status();
    let returned = check_out(root, &head.before);
    if !matches!(status, Ok(s) if s.success()) {
        note(&format!("selfhost.sh install tests: {status:?}"));
        return Some(false);
    }
    if let Err(why) = returned {
        note(&why);
        return Some(false);
    }
    println!("INSTALL-BUILT {tag} in {} s", started.elapsed().as_secs());
    Some(set_var(TAG, tag.as_bytes()))
}

/// Commit a change on the host `HEAD` stage 0 fetched in a scratch clone,
/// which leaves the developer's tree alone, and push it to the tree's `host`
/// remote as `install-test/<tag>`.
fn push_guest_commit(tag: &str) -> Result<String, String> {
    let root = workspace().map_err(str::to_owned)?;
    let clone = format!("{SCRATCH}/install-push");
    let _ = std::fs::remove_dir_all(&clone);
    let url = stdout_of(
        git(root, &["remote", "get-url", "--push", "host"]),
        "git remote",
    )?;
    let fetched = stdout_of(git(root, &["rev-parse", "FETCH_HEAD"]), "git rev-parse")?;
    stdout_of(
        git(
            root,
            &["clone", "-q", "--shared", "--no-checkout", root, &clone],
        ),
        "git clone",
    )?;
    stdout_of(
        git(&clone, &["checkout", "-q", "--detach", fetched.trim()]),
        "git checkout",
    )?;
    std::fs::write(format!("{clone}/GUEST-COMMIT"), format!("{tag}\n"))
        .map_err(|e| format!("writing GUEST-COMMIT: {e}"))?;
    stdout_of(git(&clone, &["add", "GUEST-COMMIT"]), "git add")?;
    let message = format!("install_test: committed by the kernel built as {tag}");
    stdout_of(
        git(
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
        git(&clone, &["push", "-q", url.trim(), &refspec]),
        "git push",
    )?;
    let commit = stdout_of(git(&clone, &["rev-parse", "HEAD"]), "git rev-parse")?;
    let _ = std::fs::remove_dir_all(&clone);
    Ok(commit.trim().to_owned())
}

/// Register SlopOS's loader first in `BootOrder`, twice: the second time
/// must find the first's entry and change nothing.
fn register_loader(disk: &BootDisk) -> bool {
    if let Ok(Some(current)) = boot_current()
        && firmware_entry(disk).ok().flatten() == Some(current)
    {
        note(&format!(
            "Boot{current:04X}, SlopOS's entry, booted a disk nothing registered"
        ));
        return false;
    }
    let number = match register_firmware_entry(disk, true) {
        Ok(number) => number,
        Err(why) => {
            note(&why);
            return false;
        }
    };
    let order = boot_order().unwrap_or_default();
    match register_firmware_entry(disk, true) {
        Ok(again) if again == number && boot_order().unwrap_or_default() == order => {}
        other => {
            note(&format!(
                "registering again answered {other:?}, BootOrder {:04X?} became {:04X?}",
                order,
                boot_order()
            ));
            return false;
        }
    }
    println!("INSTALL-FIRMWARE-ENTRY Boot{number:04X} first in BootOrder {order:04X?}");
    true
}

/// The firmware started this boot from SlopOS's own entry, so Limine ran from
/// its vendor directory and read the configuration there.
fn booted_through_entry(disk: &BootDisk, stage: u8) -> bool {
    match (firmware_entry(disk), boot_current()) {
        (Ok(Some(entry)), Ok(Some(current))) if entry == current => {
            println!("INSTALL-THROUGH Boot{entry:04X} at stage {stage}");
            true
        }
        other => {
            note(&format!(
                "stage {stage} did not boot through SlopOS's firmware entry: {other:?}"
            ));
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
    // Probed first: an image without UEFI runtime services answers ENODEV.
    let mut probe = [0u8; 1];
    if matches!(efivar_get(STAGE, &variables::SLOPOS.0, &mut probe), Err(e) if e == SyscallError::ENODEV)
    {
        note("no UEFI runtime services; nothing to test");
        return true;
    }
    let mut order = [0u8; slopos_abi::syscall::EFIVAR_DATA_MAX];
    if matches!(
        efivar_get("BootOrder", &variables::GLOBAL.0, &mut order),
        Err(SyscallError::EPERM)
    ) {
        note("the installer's role does not reach BootOrder");
        return false;
    }
    let disk = match BootDisk::find() {
        Ok(disk) => disk,
        Err(
            why @ (FindError::NoLoaderPartition
            | FindError::UnlistedEsp(_)
            | FindError::NoBootPartition(_)),
        ) => {
            note(&format!("{why}; nothing to test"));
            return true;
        }
        Err(why) => {
            note(&why.to_string());
            return false;
        }
    };
    let Some(status) = bootctl(&["status"]) else {
        return false;
    };
    let booted = field(&status, "booted").unwrap_or("-");
    let default = field(&status, "default").unwrap_or("-");
    let oneshot = field(&status, "oneshot").unwrap_or("-");
    println!(
        "INSTALL-STATUS stage={:?} booted={booted} default={default} boot={}",
        stage(),
        disk.boot_node
    );
    match stage() {
        None => {
            if booted != "slopos-a" || default != "slopos-a" {
                note(&format!("first boot is {booted} with default {default}"));
                return false;
            }
            if !register_loader(&disk) {
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
            if !booted_through_entry(&disk, 1) {
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
                match std::fs::read_to_string(BASE_TAG_FILE) {
                    Ok(base) if base.trim() == tag => println!("INSTALL-BASE {tag}"),
                    other => {
                        note(&format!("slot b's base carries {other:?}, not {tag}"));
                        return false;
                    }
                }
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
            if !booted_through_entry(&disk, 2) {
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
