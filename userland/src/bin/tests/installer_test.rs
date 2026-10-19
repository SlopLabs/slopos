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
//!   the installer, on a blank disk from the shell answering its questions,
//!   a person named and a password typed among them, on the others with
//!   every answer a flag.
//! 1 (`slopos-a` of the installed disk): the root is the disk's, its first
//!   administrator the person the installer was told of, the other system's
//!   entry is still in `BootOrder` and in Limine's menu, and the clone
//!   fetches from the remote it was given. With a toolchain at
//!   `/usr/local`, `scripts/selfhost.sh install tests` builds the system,
//!   installs its base and puts its kernel into slot b, as a person at the
//!   shell runs it; without one, slot a's kernel is cloned into b. Boot b
//!   once.
//! 2 (`slopos-b`): it runs the kernel and the base stage 1 built and tagged,
//!   and `bootctl commit` makes it the default.
//!
//! Booted from the medium again with SlopOS on the disk, it leaves a file in
//! the root and a base in slot a, as a boot partition laid out before the
//! bases moved beside the slots holds. In the medium's rescue shell it checks
//! the root, reads the file through a mount and rolls the disk's base back
//! through `bootctl --boot`, and at a terminal interrupts what a rescue shell
//! runs. Then it reinstalls over the same partitions, keeping the root:
//!
//! 3 (`slopos-a` again): the file and the root's people are there, and what
//!   the slots said of the system they held is gone.
//!
//! Without the medium and with no stage set, it has nothing to do and passes.

use slopos_userland as _;

use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

use slopos_abi::fs::SESSION_UID;
use slopos_abi::syscall::UserWinsize;
use slopos_boot_core::gpt::Partition;
use slopos_boot_core::layout::{self, ESP_TYPE, ROOT_TYPE};
use slopos_boot_core::{base, bli, device_path, load_option, variables};
use slopos_slibc::test_harness::note;
use slopos_userland::accounts::Store;
use slopos_userland::base_image::retagged;
use slopos_userland::boot_disk::{
    BootDisk, boot_current, boot_order, firmware_entry, medium_disk_guid, not_installable,
    open_fat, partition_node, set_loader_var, table_of, whole_disks,
};
use slopos_userland::selfhost::{selfhost_install, workspace};
use slopos_userland::syscall::UserUtsname;
use slopos_userland::syscall::core::{clock_gettime_ns, uname};
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs::{self, mount, sync, umount2};
use slopos_userland::syscall::{net, process};

const STAGE: &str = "SlopOSInstallerTestStage";
/// The build tag stage 1 gave the system it built, for stage 2 to find.
const TAG: &str = "SlopOSInstallerTestTag";
/// The other system's firmware entry, which stage 0 made.
const FOREIGN: &str = "SlopOSInstallerTestForeign";
/// Whether stage 0 asked for SlopOS first in `BootOrder`, so that its entry
/// is what boots the disk; otherwise the firmware's own may come first.
const FIRST: &str = "SlopOSInstallerTestFirst";
/// The first person stage 0 named to the installer.
const PERSON: &str = "SlopOSInstallerTestPerson";
/// Who the blank disk's install is for, and the password typed for them.
const TYPED_PERSON: &str = "ada";
const TYPED_PASSWORD: &str = "correct horse battery";

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
/// The default the installed system left in the machine's variables, which a
/// rescue from the medium finds there.
const RESCUE_DEFAULT: &str = "slopos-b";
/// The base a rescue rolls the installed disk back to.
const RESCUE_TAG: &str = "installer-test-rescued";
/// How long the rescue shell at a terminal may take to answer, under TCG.
const TERMINAL_DEADLINE: Duration = Duration::from_secs(120);
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

/// The one disk the installer would offer, and what kind of install it asks
/// for.
fn target() -> Result<(String, Kind, Vec<Partition>), String> {
    let medium = medium_disk_guid()?;
    let mut disks = Vec::new();
    for node in whole_disks()? {
        match not_installable(&node, medium) {
            Some(why) => println!("INSTALLER-NOT-OFFERED {node}: {why}"),
            None => disks.push(node),
        }
    }
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

/// Leave a base in slot a, where a boot partition laid out before the bases
/// moved beside the slots kept one, for the reinstall to remove.
fn leave_slot_base(disk: &str, partitions: &[Partition]) -> Result<(), String> {
    let boot = partitions
        .iter()
        .find(|p| p.entry.type_guid == layout::BOOT_TYPE)
        .ok_or("the disk has no boot partition")?;
    let path = format!("{}/a/base.img", layout::SLOTS_DIR);
    open_fat(&partition_node(disk, boot)?)?
        .write_file(&path, b"a base kept per slot\n")
        .map_err(|e| format!("{path}: {e:?}"))?;
    println!("INSTALLER-LEFT-SLOT-BASE {path}");
    Ok(())
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

/// What a person repairing the installed disk from the medium does in its
/// rescue shell: check the root, read it mounted, and roll the boot
/// partition's base back, with the default the installed system left and a
/// base it kept before the one it runs.
fn rescue(disk: &str, partitions: &[Partition], root: &str) -> Result<(), String> {
    let boot = partitions
        .iter()
        .find(|p| p.entry.type_guid == layout::BOOT_TYPE)
        .ok_or("the disk has no boot partition")?;
    let boot = partition_node(disk, boot)?;
    set_loader_var(bli::ENTRY_DEFAULT, Some(RESCUE_DEFAULT))?;
    let current = open_fat(&boot)?
        .read_file(layout::BASE_CURRENT)
        .map_err(|e| format!("{}: {e:?}", layout::BASE_CURRENT))?;
    let kept = retagged(&current, RESCUE_TAG, None)?;
    open_fat(&boot)?
        .write_file(layout::BASE_PREVIOUS, &kept)
        .map_err(|e| format!("{}: {e:?}", layout::BASE_PREVIOUS))?;
    let script = format!(
        "set -e\n\
         e2fsck -fn {root}\n\
         mkdir -p {KEPT_MOUNT}\n\
         sh -c 'mount -r {root} {KEPT_MOUNT}'\n\
         cat {KEPT_MOUNT}{KEPT}\n\
         umount {KEPT_MOUNT}\n\
         bootctl --boot {boot} status\n\
         bootctl --boot {boot} base rollback\n"
    );
    let mut shell = Command::new("/bin/shell")
        .arg("--rescue")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("shell --rescue: {e}"))?;
    shell
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(script.as_bytes())
        .map_err(|e| format!("shell --rescue: {e}"))?;
    let out = shell
        .wait_with_output()
        .map_err(|e| format!("shell --rescue: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout);
    let expected = [
        KEPT_TEXT,
        &format!("default: {RESCUE_DEFAULT}\n"),
        "entries: slopos-a slopos-b\n",
        &format!("build tag {RESCUE_TAG}"),
    ];
    if !out.status.success() || expected.iter().any(|line| !said.contains(line)) {
        return Err(format!("the rescue shell exited {:?}:\n{said}", out.status));
    }
    let mut volume = open_fat(&boot)?;
    if volume.read_file(layout::BASE_CURRENT).ok() != Some(kept)
        || volume.read_file(layout::BASE_PREVIOUS).ok() != Some(current)
    {
        return Err(format!(
            "the rollback through {boot} left the bases unswapped"
        ));
    }
    println!("INSTALLER-RESCUED {root} checked and read, the base on {boot} rolled back");
    Ok(())
}

/// The rescue shell asked for at a terminal runs on one of its own, as large
/// as this one and following it when it is resized: an interrupt typed there
/// ends the program it runs and not the shell, and this terminal is as it was
/// once the shell exits, as a shell around the request reads it.
fn rescue_at_a_terminal() -> Result<(), String> {
    let (master, _) = process::openpty().map_err(|e| format!("no pty: {e}"))?;
    let slave = fs::ioctl_tiocgptpeer(master.raw()).map_err(|e| format!("no pty slave: {e}"))?;
    let resize = |rows, cols| {
        let size = UserWinsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        fs::tiocswinsz(master.raw(), &size).map_err(|e| format!("resizing: {e}"))
    };
    resize(37, 101)?;
    let tty = || {
        fs::dup(slave.raw())
            // SAFETY: `dup` just opened this descriptor for us.
            .map(|dup| std::process::Stdio::from(unsafe { File::from_raw_fd(dup.into_raw()) }))
            .map_err(|e| format!("dup: {e}"))
    };
    let mut cmd = Command::new("/bin/shell");
    cmd.args([
        "-c",
        "echo before-$(stty -g)-; shell --rescue && echo after-$(stty -g)-",
    ])
    .stdin(tty()?)
    .stdout(tty()?)
    .stderr(tty()?);
    // SAFETY: the closure makes two system calls and allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if process::setsid() < 0 || fs::tiocsctty(0).is_err() {
                return Err(std::io::Error::other("no controlling terminal"));
            }
            Ok(())
        });
    }
    let mut shell = cmd.spawn().map_err(|e| format!("shell --rescue: {e}"))?;
    let _ = net::set_nonblocking(master.raw());
    let mut seen = Vec::new();
    let mut until = |marker: &str| {
        let deadline = Instant::now() + TERMINAL_DEADLINE;
        let mut buf = [0u8; 512];
        while Instant::now() < deadline {
            match fs::read_slice(master.raw(), &mut buf) {
                Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
            if String::from_utf8_lossy(&seen).contains(marker) {
                return Ok(());
            }
        }
        Err(format!(
            "no {marker:?} from the rescue shell's terminal, which showed {:?}",
            String::from_utf8_lossy(&seen)
        ))
    };
    let typed = |text: &[u8]| {
        fs::write_slice(master.raw(), text)
            .map(drop)
            .map_err(|e| format!("typing at the rescue shell: {e}"))
    };
    until("Rescue shell:")?;
    typed(b"stty size\r")?;
    until("37 101")?;
    resize(38, 102)?;
    std::thread::sleep(Duration::from_secs(1));
    typed(b"stty size\r")?;
    until("38 102")?;
    typed(b"echo ready-$((40+2)); sleep 600\r")?;
    until("ready-42")?;
    std::thread::sleep(Duration::from_secs(2));
    typed(b"\x03")?;
    typed(b"echo status-$?\r")?;
    until("status-130")?;
    typed(b"exit\r")?;
    until("after-")?;
    let deadline = Instant::now() + TERMINAL_DEADLINE;
    loop {
        match shell.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            other => {
                let _ = shell.kill();
                return Err(format!("the rescue shell at a terminal ended as {other:?}"));
            }
        }
    }
    let mut buf = [0u8; 512];
    while let Ok(n) = fs::read_slice(master.raw(), &mut buf)
        && n > 0
    {
        seen.extend_from_slice(&buf[..n]);
    }
    let shown = String::from_utf8_lossy(&seen);
    let settings = |label: &str| {
        let from = shown.find(label)? + label.len();
        shown[from..].split('-').next()
    };
    if settings("before-").is_none() || settings("before-") != settings("after-") {
        return Err(format!(
            "the terminal was not given back as it was: {shown:?}"
        ));
    }
    println!("INSTALLER-RESCUED at a terminal: an interrupt ended the program it ran");
    Ok(())
}

/// A person typing `installer` at the shell, then `typed` to its questions:
/// started holding nothing, the installer asks privd, which runs it with the
/// answers on the standard input it was sent.
fn answer_on_stdin(args: &[String], typed: &str) -> std::io::Result<std::process::ExitStatus> {
    let line = std::iter::once("installer".to_owned())
        .chain(args.iter().map(|arg| format!("'{arg}'")))
        .collect::<Vec<_>>()
        .join(" ");
    let mut child = Command::new("/bin/shell")
        .args(["-c", &line])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("no stdin"))?
        .write_all(typed.as_bytes())?;
    child.wait()
}

/// The payload the kernel mounted stays as it mounted it: nothing unmounts
/// it, covers it or writes it.
fn payload_held() -> Result<(), String> {
    let dir = layout::PAYLOAD_DIR;
    let path = std::ffi::CString::new(dir).map_err(|_| "a NUL in a path")?;
    match umount2(path.as_ptr(), 0) {
        Err(SyscallError::EPERM) => {}
        other => return Err(format!("umount {dir}: {other:?}, not EPERM")),
    }
    let under = format!("{dir}/usr");
    match mount(b"", under.as_bytes(), b"ramfs", 0) {
        Err(SyscallError::EPERM) => {}
        other => return Err(format!("mounting over {under}: {other:?}, not EPERM")),
    }
    match std::fs::File::create(format!("{dir}/written")) {
        Err(e) if e.raw_os_error() == Some(SyscallError::EROFS.errno()) => Ok(()),
        other => Err(format!("creating a file in {dir}: {other:?}, not EROFS")),
    }
}

/// The blank disk is installed as a person answers the questions; the others
/// with every answer a flag.
fn install() -> bool {
    let module = std::path::Path::new(layout::MEDIUM_DIR);
    if ["usr", "src"].iter().any(|tree| module.join(tree).exists()) {
        note("the module the loader carried holds the payload's trees");
        return false;
    }
    if std::path::Path::new(layout::PAYLOAD_DIR).exists()
        && let Err(why) = payload_held()
    {
        note(&why);
        return false;
    }
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
            if std::path::Path::new(layout::PAYLOAD_DIR)
                .join("src")
                .exists()
            {
                answers.push_str(REMOTE);
                answers.push('\n');
            }
            answers.push_str("yes\n");
            answers.push_str(&format!(
                "{TYPED_PERSON}\n{TYPED_PASSWORD}\n{TYPED_PASSWORD}\n"
            ));
            answers.push_str(disk.rsplit('/').next().unwrap_or(&disk));
            answers.push('\n');
            typed = Some(answers);
            if !set_var(PERSON, TYPED_PERSON.as_bytes()) {
                return false;
            }
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
            args.extend(["--person", "user", "--no-password"].map(str::to_owned));
            if !set_var(PERSON, b"user") {
                return false;
            }
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
            if reinstall
                && let Err(why) = leave_kept_file(&node)
                    .and_then(|()| leave_slot_base(&disk, &partitions))
                    .and_then(|()| rescue(&disk, &partitions, &node))
                    .and_then(|()| rescue_at_a_terminal())
            {
                note(&why);
                return false;
            }
            let root_choice = if reinstall {
                "--keep-root"
            } else {
                args.extend(["--person", "user", "--no-password"].map(str::to_owned));
                if !set_var(PERSON, b"user") {
                    return false;
                }
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
    if let Err(why) = first_person(TYPED_PERSON, Some(TYPED_PASSWORD)) {
        note(&format!("the kept root's people: {why}"));
        return false;
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

/// The root's first person is `name`, its first administrator, logged in by
/// the desktop, with `password` alone when one was typed.
fn first_person(name: &str, password: Option<&str>) -> Result<(), String> {
    let accounts = Store::read(std::path::Path::new("/"))
        .recorded()?
        .ok_or("the root holds no accounts record")?;
    let first = accounts.by_uid(SESSION_UID);
    let logged_in = accounts.autologin().map(|p| p.uid) == Some(SESSION_UID);
    let named = first
        .is_some_and(|p| p.name == name && p.admin && p.password.is_some() == password.is_some());
    if !named || !logged_in {
        return Err(format!("the record names {:?}", first.map(|p| &p.name)));
    }
    let Some(password) = password else {
        return Ok(());
    };
    let mut check = Command::new("/sbin/authcheck")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("authcheck: {e}"))?;
    if let Some(mut stdin) = check.stdin.take() {
        let _ = writeln!(stdin, "{name}\n{password}");
    }
    match check.wait() {
        Ok(status) if status.success() => Ok(()),
        other => Err(format!("authcheck refused the typed password: {other:?}")),
    }
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
    let person = var(PERSON).and_then(|v| String::from_utf8(v).ok());
    let password = (person.as_deref() == Some(TYPED_PERSON)).then_some(TYPED_PASSWORD);
    if let Err(why) = first_person(person.as_deref().unwrap_or("user"), password) {
        note(&format!("the installed root's people: {why}"));
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
            if let Err(why) = selfhost_install(root, "tests", &tag) {
                note(&why);
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
    let _ = set_var(PERSON, &[]);
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
        match std::fs::read_to_string(base::BUILD_TAG_PATH) {
            Ok(base) if base.trim() == tag => println!("INSTALLER-RUNS-BASE {tag}"),
            other => {
                note(&format!(
                    "the base runs {other:?}, not the build tagged {tag}"
                ));
                return false;
            }
        }
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
