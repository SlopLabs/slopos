//! SlopOS installed from the install medium onto the disk `just
//! test-installer` attaches, then booted from that disk with the medium gone,
//! and taken once around the loop a person runs there. Each boot runs this
//! test again; a non-volatile UEFI variable says which stage the last boot
//! reached. The disk says what kind of install it is: one with no partition
//! table is erased, one with a partition named [`REUSED_NAME`] has it reused as
//! the root, and any other gets SlopOS in its free space beside what it holds.
//!
//! 0 (the live system, the medium at `/media/install`): on a disk another
//!   system is on, give it the firmware entry that system would have; run
//!   `/bin/installer` with every answer a flag.
//! 1 (`slopos-a` of the installed disk): the root is the disk's, the other
//!   system's entry is still in `BootOrder` and in Limine's menu, and the
//!   clone fetches from the remote it was given. With a toolchain at
//!   `/usr/local`, `scripts/selfhost.sh install tests` builds the system and
//!   installs it into slot b, as a person at the shell runs it; without one,
//!   slot a is cloned into b. Boot b once.
//! 2 (`slopos-b`): it runs the build stage 1 tagged, and `bootctl commit`
//!   makes it the default.
//!
//! Booted from the medium again with SlopOS on the disk, it leaves a file in
//! the root and reinstalls over the same partitions, keeping the root:
//!
//! 3 (`slopos-a` again): the file is there, and what the slots said of the
//!   system they held is gone.
//!
//! Without the medium and with no stage set, it has nothing to do and passes.

use slopos_userland as _;

use std::process::Command;
use std::time::Instant;

use slopos_boot_core::gpt::Partition;
use slopos_boot_core::layout::{self, ESP_TYPE, ROOT_TYPE};
use slopos_boot_core::{device_path, load_option, variables};
use slopos_slibc::test_harness::note;
use slopos_userland::boot_disk::{
    BootDisk, boot_current, boot_order, firmware_entry, open_fat, partition_node, table_of,
    whole_disks,
};
use slopos_userland::selfhost::{selfhost, workspace};
use slopos_userland::syscall::UserUtsname;
use slopos_userland::syscall::core::{clock_gettime_ns, uname};
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs::{mount, sync, umount2};

const STAGE: &str = "SlopOSInstallerTestStage";
/// The build tag stage 1 gave the system it built, for stage 2 to find.
const TAG: &str = "SlopOSInstallerTestTag";
/// The other system's firmware entry, which stage 0 made.
const FOREIGN: &str = "SlopOSInstallerTestForeign";
/// Whether stage 0 asked for SlopOS first in `BootOrder`, so that its entry
/// is what boots the disk; otherwise the firmware's own may come first.
const FIRST: &str = "SlopOSInstallerTestFirst";

/// The partition a disk offers as the root to reuse.
const REUSED_NAME: &str = "installer-test-root";
/// The other system's loader on a disk SlopOS installs beside it, and its
/// firmware entry's description.
const FOREIGN_LOADER: &str = r"\EFI\other\BOOTX64.EFI";
const FOREIGN_TITLE: &str = "Other OS";
/// Not the medium's own remote, so the clone fetching from it shows the
/// installer pointed it there.
const REMOTE: &str = "https://git.slopos.invalid/slopos.git";
/// What a reinstall that keeps the root must find there afterwards.
const KEPT: &str = "/home/installer-kept";
const KEPT_TEXT: &str = "written before the reinstall\n";
/// Where the live system mounts the installed root to leave that file.
const KEPT_MOUNT: &str = "/tmp/installer-test-root";
/// What the installed system boots with: this test again, and a slot that
/// panics falls back.
const INSTALLED_CMDLINE: &str = "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=off roulette=skip panic=reboot watchdog.miss_threshold=300 tests.run=*ext2_aaa*,*installer*";

fn var(name: &str) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    let len = efivar_get(name, &variables::SLOPOS.0, &mut buf).ok()?;
    Some(buf[..len].to_vec())
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

fn stage() -> Option<u8> {
    var(STAGE).filter(|v| v.len() == 1).map(|v| v[0])
}

fn bootctl(args: &[&str]) -> Option<String> {
    let out = Command::new("/bin/bootctl").args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        note(&format!(
            "bootctl {}: {stdout}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ));
        return None;
    }
    Some(stdout)
}

fn field<'a>(status: &'a str, key: &str) -> &'a str {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(": "))
        .unwrap_or("-")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Erase,
    Free,
    Reuse,
    /// SlopOS is on it: install again over its partitions, keeping the root.
    Reinstall,
}

fn name_of(p: &Partition) -> String {
    String::from_utf16_lossy(&p.entry.name)
        .trim_end_matches('\0')
        .to_owned()
}

/// The one disk, and what kind of install it asks for.
fn target() -> Result<(String, Kind, Vec<Partition>), String> {
    let disks = whole_disks()?;
    let [disk] = disks.as_slice() else {
        return Err(format!(
            "the installer check attaches one disk, not {disks:?}"
        ));
    };
    let Some(table) = table_of(disk) else {
        return Ok((disk.clone(), Kind::Erase, Vec::new()));
    };
    let partitions: Vec<Partition> = table.partitions().collect();
    let kind = if partitions.iter().any(|p| p.entry.type_guid == ROOT_TYPE) {
        Kind::Reinstall
    } else if partitions.iter().any(|p| name_of(p) == REUSED_NAME) {
        Kind::Reuse
    } else {
        Kind::Free
    };
    Ok((disk.clone(), kind, partitions))
}

/// The firmware entry another system on the disk would have registered, at
/// the end of `BootOrder`.
fn register_foreign(esp: &Partition) -> Result<u16, String> {
    let hd = device_path::HardDrive {
        partition_number: esp.entry.number,
        start_lba: esp.entry.first_lba,
        blocks: esp.blocks(),
        partition: esp.entry.unique,
    };
    let mut path = vec![0u8; device_path::loader_len(FOREIGN_LOADER)];
    device_path::loader(&hd, FOREIGN_LOADER, &mut path).map_err(|e| format!("{e:?}"))?;
    let mut option = vec![0u8; load_option::encoded_len(FOREIGN_TITLE, &path)];
    load_option::encode(load_option::ACTIVE, FOREIGN_TITLE, &path, &mut option)
        .map_err(|e| format!("{e:?}"))?;
    let order = boot_order()?;
    let number = (0x0100u16..)
        .find(|n| {
            let name =
                String::from_utf8_lossy(&variables::BootVariable::option_name(*n)).into_owned();
            !order.contains(n)
                && matches!(
                    efivar_get(&name, &variables::GLOBAL.0, &mut [0u8; 8]),
                    Err(SyscallError::ENOENT)
                )
        })
        .ok_or("no free Boot#### number")?;
    let name = String::from_utf8_lossy(&variables::BootVariable::option_name(number)).into_owned();
    efivar_set(&name, &variables::GLOBAL.0, variables::PERSISTENT, &option)
        .map_err(|e| format!("writing {name}: {e:?}"))?;
    let raw: Vec<u8> = order
        .iter()
        .chain(core::iter::once(&number))
        .flat_map(|n| n.to_le_bytes())
        .collect();
    efivar_set(
        "BootOrder",
        &variables::GLOBAL.0,
        variables::PERSISTENT,
        &raw,
    )
    .map_err(|e| format!("writing BootOrder: {e:?}"))?;
    Ok(number)
}

/// Leave [`KEPT`] on the root partition `node`, as a person's work would be.
fn leave_kept_file(node: &str) -> Result<(), String> {
    std::fs::create_dir_all(KEPT_MOUNT).map_err(|e| format!("{KEPT_MOUNT}: {e}"))?;
    mount(node.as_bytes(), KEPT_MOUNT.as_bytes(), b"ext4", 0)
        .map_err(|e| format!("mounting {node}: {e:?}"))?;
    let written = std::fs::write(format!("{KEPT_MOUNT}{KEPT}"), KEPT_TEXT);
    let _ = sync();
    let target = std::ffi::CString::new(KEPT_MOUNT).map_err(|_| "a NUL in the mount point")?;
    umount2(target.as_ptr(), 0).map_err(|e| format!("unmounting {KEPT_MOUNT}: {e:?}"))?;
    written.map_err(|e| format!("{KEPT}: {e}"))
}

/// The installer asking its questions, and a person typing `typed`.
fn answer_on_stdin(args: &[String], typed: &str) -> std::io::Result<std::process::ExitStatus> {
    use std::io::Write;
    let mut child = Command::new("/bin/installer")
        .args(args)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?
        .write_all(typed.as_bytes())?;
    child.wait()
}

/// The blank disk is installed as a person answers the questions; the others
/// with every answer a flag.
fn install() -> bool {
    let (disk, kind, partitions) = match target() {
        Ok(found) => found,
        Err(why) => {
            note(&why);
            return false;
        }
    };
    let mut args = vec!["--cmdline".to_owned(), INSTALLED_CMDLINE.to_owned()];
    let flagged = [
        "--disk".to_owned(),
        disk.clone(),
        "--remote".to_owned(),
        REMOTE.to_owned(),
        "--yes".to_owned(),
    ];
    let mut typed = None;
    match kind {
        Kind::Erase => {
            let mut answers = String::from("\n");
            if std::path::Path::new(layout::MEDIUM_DIR)
                .join("src")
                .exists()
            {
                answers.push_str(REMOTE);
                answers.push('\n');
            }
            answers.push_str("yes\n");
            answers.push_str(disk.rsplit('/').next().unwrap_or(&disk));
            answers.push('\n');
            typed = Some(answers);
        }
        Kind::Free => {
            let Some(esp) = partitions.iter().find(|p| p.entry.type_guid == ESP_TYPE) else {
                note("the foreign disk has no ESP");
                return false;
            };
            match register_foreign(esp) {
                Ok(number) => {
                    println!("INSTALLER-FOREIGN-ENTRY Boot{number:04X}");
                    if !set_var(FOREIGN, &number.to_le_bytes()) {
                        return false;
                    }
                }
                Err(why) => {
                    note(&why);
                    return false;
                }
            }
            args.extend(flagged);
            args.extend(["--mode", "free", "--first"].map(str::to_owned));
        }
        Kind::Reuse | Kind::Reinstall => {
            let reinstall = kind == Kind::Reinstall;
            let Some(root) = partitions.iter().find(|p| {
                if reinstall {
                    p.entry.type_guid == ROOT_TYPE
                } else {
                    name_of(p) == REUSED_NAME
                }
            }) else {
                note("the disk has no root partition to install over");
                return false;
            };
            let node = match partition_node(&disk, root) {
                Ok(node) => node,
                Err(why) => {
                    note(&why);
                    return false;
                }
            };
            if reinstall && let Err(why) = leave_kept_file(&node) {
                note(&why);
                return false;
            }
            let root_choice = if reinstall {
                "--keep-root"
            } else {
                "--format-root"
            };
            args.extend(flagged);
            args.extend(
                [
                    "--mode",
                    "reuse",
                    "--root",
                    &node,
                    root_choice,
                    "--not-first",
                ]
                .map(str::to_owned),
            );
        }
    }
    let started = Instant::now();
    let status = match &typed {
        None => Command::new("/bin/installer").args(&args).status(),
        Some(answers) => answer_on_stdin(&args, answers),
    };
    if !matches!(status, Ok(s) if s.success()) {
        note(&format!("installer {}: {status:?}", args.join(" ")));
        return false;
    }
    println!(
        "INSTALLER-INSTALLED {kind:?} onto {disk} in {} s",
        started.elapsed().as_secs()
    );
    let first = matches!(kind, Kind::Erase | Kind::Free);
    if let Err(why) = registered_where_asked(&disk, first) {
        note(&why);
        return false;
    }
    set_var(FIRST, &[u8::from(first)])
        && set_var(STAGE, &[if kind == Kind::Reinstall { 3 } else { 1 }])
}

/// SlopOS's firmware entry for the disk, first in `BootOrder` just when the
/// install asked for that, before any boot lets the firmware reorder it.
fn registered_where_asked(disk: &str, first: bool) -> Result<(), String> {
    let table = table_of(disk).ok_or("the installed disk has no table")?;
    let of_type = |kind| table.partitions().find(|p| p.entry.type_guid == kind);
    let esp = of_type(ESP_TYPE).ok_or("the installed disk has no ESP")?;
    let boot = of_type(layout::BOOT_TYPE).ok_or("the installed disk has no boot partition")?;
    let boot_disk = BootDisk {
        esp,
        boot_node: partition_node(disk, &boot)?,
    };
    let entry = firmware_entry(&boot_disk)?.ok_or("no firmware entry boots the installed disk")?;
    let order = boot_order()?;
    let is_first = order.first() == Some(&entry);
    if is_first != first {
        return Err(format!(
            "Boot{entry:04X} is {}first in {order:04X?}, asked for {}",
            if is_first { "" } else { "not " },
            if first { "first" } else { "not first" }
        ));
    }
    println!("INSTALLER-REGISTERED Boot{entry:04X} in {order:04X?}");
    Ok(())
}

/// The installed disk once more, after a reinstall that kept its root.
fn reinstalled_boot(status: &str) -> bool {
    let _ = set_var(STAGE, &[]);
    let _ = set_var(FIRST, &[]);
    let (booted, default) = (field(status, "booted"), field(status, "default"));
    if booted != "slopos-a" || default != "slopos-a" {
        note(&format!(
            "the reinstalled disk booted {booted} with default {default}"
        ));
        return false;
    }
    match std::fs::read_to_string(KEPT) {
        Ok(text) if text == KEPT_TEXT => {}
        other => {
            note(&format!("{KEPT} after the reinstall: {other:?}"));
            return false;
        }
    }
    let forgotten = format!("/var/lib/slopos/slots/{}", layout::SLOTS[1]);
    if std::path::Path::new(&forgotten).exists() {
        note(&format!(
            "{forgotten} still speaks of the system slot b held"
        ));
        return false;
    }
    println!("INSTALLER-KEPT {KEPT}");
    true
}

/// The disk's ESP holds a configuration offering the other system.
fn menu_offers_foreign(disk: &str) -> Result<(), String> {
    let table = table_of(disk).ok_or("the disk has no table")?;
    let esp = table
        .partitions()
        .find(|p| p.entry.type_guid == ESP_TYPE)
        .ok_or("the disk has no ESP")?;
    let mut volume = open_fat(&partition_node(disk, &esp)?)?;
    let conf = volume
        .read_file(&layout::LOADER_CONFIG.replace('\\', "/"))
        .map_err(|e| format!("limine.conf: {e:?}"))?;
    let conf = String::from_utf8_lossy(&conf);
    if !conf.lines().any(|l| l == format!("/{FOREIGN_TITLE}")) {
        return Err(format!("limine.conf offers no {FOREIGN_TITLE}:\n{conf}"));
    }
    Ok(())
}

fn first_boot(status: &str) -> bool {
    let (booted, default) = (field(status, "booted"), field(status, "default"));
    if booted != "slopos-a" || default != "slopos-a" {
        note(&format!(
            "the first boot is {booted} with default {default}"
        ));
        return false;
    }
    if std::path::Path::new(layout::MEDIUM_DIR).exists() {
        note("the installed system serves an install medium");
        return false;
    }
    let disk = match target() {
        Ok((disk, _, _)) => disk,
        Err(why) => {
            note(&why);
            return false;
        }
    };
    let registered = BootDisk::find()
        .map_err(|e| format!("{e:?}"))
        .and_then(|boot_disk| firmware_entry(&boot_disk));
    let first = var(FIRST).is_some_and(|v| v == [1]);
    match (registered, boot_current()) {
        (Ok(Some(entry)), Ok(Some(current))) if entry == current || !first => {
            println!("INSTALLER-BOOTED slopos-a of {disk} through Boot{current:04X}");
        }
        (registered, current) => {
            note(&format!(
                "the firmware booted {current:?}, not SlopOS's entry {registered:?}"
            ));
            return false;
        }
    }
    let foreign = var(FOREIGN).and_then(|v| Some(u16::from_le_bytes(v.get(..2)?.try_into().ok()?)));
    if let Some(number) = foreign {
        let order = boot_order().unwrap_or_default();
        if !order.contains(&number) {
            note(&format!(
                "the other system's entry Boot{number:04X} left BootOrder {order:04X?}"
            ));
            return false;
        }
        println!("INSTALLER-FOREIGN-KEPT Boot{number:04X} in {order:04X?}");
        if let Err(why) = menu_offers_foreign(&disk) {
            note(&why);
            return false;
        }
    }
    let config = std::fs::read_to_string("/src/slopos/.git/config").unwrap_or_default();
    if workspace().is_ok() && !config.contains(&format!("url = {REMOTE}")) {
        note(&format!(
            "the clone does not fetch from {REMOTE}:\n{config}"
        ));
        return false;
    }
    let built = match workspace() {
        Ok(root) => {
            let tag = format!("guest-{}", clock_gettime_ns());
            let started = Instant::now();
            let status = selfhost(root, &["install", "tests"])
                .env("SLOPOS_BUILD_TAG", &tag)
                .status();
            if !matches!(status, Ok(s) if s.success()) {
                note(&format!("selfhost.sh install tests: {status:?}"));
                return false;
            }
            println!("INSTALLER-BUILT {tag} in {} s", started.elapsed().as_secs());
            set_var(TAG, tag.as_bytes())
        }
        Err(why) => {
            note(&format!("{why}; cloning slot a instead of building"));
            bootctl(&["clone", "a", "b"]).is_some() && bootctl(&["oneshot", "slopos-b"]).is_some()
        }
    };
    if !built || !set_var(STAGE, &[2]) {
        return false;
    }
    println!("INSTALLER-STAGE 2: rebooting into slopos-b");
    let _ = bootctl(&["reboot"]);
    note("bootctl reboot returned");
    false
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

fn tried_boot(status: &str) -> bool {
    let tag = var(TAG).map(|t| String::from_utf8_lossy(&t).into_owned());
    let _ = set_var(STAGE, &[]);
    let _ = set_var(TAG, &[]);
    let _ = set_var(FOREIGN, &[]);
    let _ = set_var(FIRST, &[]);
    let (booted, default) = (field(status, "booted"), field(status, "default"));
    if booted != "slopos-b" || default != "slopos-a" {
        note(&format!(
            "the tried boot is {booted} with default {default}"
        ));
        return false;
    }
    if let Some(tag) = tag {
        let version = running_version();
        if !version.ends_with(&format!(" {tag}")) {
            note(&format!(
                "slot b runs {version:?}, not the build tagged {tag}"
            ));
            return false;
        }
        println!("INSTALLER-RUNS {version}");
    }
    match bootctl(&["commit"]) {
        Some(said) if said.contains("default: slopos-b") => {
            println!("INSTALLER-COMMITTED slopos-b");
            true
        }
        other => {
            note(&format!("commit said {other:?}"));
            false
        }
    }
}

fn installed_built_and_committed() -> bool {
    let mut probe = [0u8; 1];
    if matches!(efivar_get(STAGE, &variables::SLOPOS.0, &mut probe), Err(e) if e == SyscallError::ENODEV)
    {
        note("no UEFI runtime services; nothing to test");
        return true;
    }
    match stage() {
        None if std::path::Path::new(layout::MEDIUM_DIR).exists() => install(),
        None => {
            note("no install medium and no install under way; nothing to test");
            true
        }
        Some(stage) => {
            let Some(status) = bootctl(&["status"]) else {
                return false;
            };
            println!(
                "INSTALLER-STATUS stage={stage} booted={} default={}",
                field(&status, "booted"),
                field(&status, "default")
            );
            match stage {
                1 => first_boot(&status),
                2 => tried_boot(&status),
                3 => reinstalled_boot(&status),
                other => {
                    note(&format!("unknown stage {other}"));
                    let _ = set_var(STAGE, &[]);
                    false
                }
            }
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[(
        "installed_built_and_committed",
        installed_built_and_committed,
    )]);
}
