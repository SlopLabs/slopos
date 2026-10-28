//! SlopOS installed from the install medium onto the disk `just
//! test-installer` attaches, then booted from that disk with the medium gone,
//! and taken once around the loop a person runs there. Each boot runs this
//! test again; a non-volatile UEFI variable says which stage the last boot
//! reached. The disk says what kind of install it is: one with no partition
//! table is erased, one with a partition named [`REUSED_NAME`] has it reused as
//! the root, and any other gets SlopOS in its free space beside what it holds.
//!
//! 0 (the live system, the medium at `/media/install`): on a blank disk, find
//!   the installer's entry among the dock's and the boot's desktop opening
//!   its welcome; on a disk another system is on, give it the firmware entry
//!   that system would have; run the installer, on a blank disk from the
//!   shell answering its questions, a person named and a password typed
//!   among them, beside the other system with every answer a flag and a
//!   root size, and onto the partition to reuse as a window does, through
//!   privd's `installer --serve`.
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
//! the root. In the medium's rescue shell it checks the root, reads the file
//! through a mount and rolls the disk's base back through `bootctl --boot`,
//! and at a terminal interrupts what a rescue shell runs. Then it reinstalls
//! over the same partitions, keeping the root:
//!
//! 3 (`slopos-a` again): the file and the root's people are there, and what
//!   the slots said of the system they held is gone.
//!
//! Without the medium and with no stage set, it has nothing to do and passes.

use slopos_userland as _;

#[path = "support/sessions.rs"]
mod sessions;

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use slopos_abi::fs::SESSION_UID;
use slopos_abi::syscall::{O_CLOEXEC, UserTaskEntry, UserWinsize};
use slopos_abi::task::MAX_TASKS;
use slopos_boot_core::gpt::{self, Entry, Header, Location, Partition, Piece};
use slopos_boot_core::install::Role;
use slopos_boot_core::layout::{self, ESP_TYPE, ROOT_TYPE};
use slopos_boot_core::{base, bli, device_path, load_option, variables};
use slopos_installer_core::protocol::{self, Reply as EngineReply, Request as EngineRequest};
use slopos_installer_core::{
    Choices, Event, Failure, Issue, Mode, Part, Step, Tag, root_min, short,
};
use slopos_priv_core::wire::{Exit, Reply as PrivdReply, Request as PrivdRequest};
use slopos_remote_core::Deframer;
use slopos_slibc::test_harness::note;
use slopos_userland::accounts::Store;
use slopos_userland::app_entry;
use slopos_userland::apps::installer::not_offered;
use slopos_userland::base_image::retagged;
use slopos_userland::boot_disk::{
    BootDisk, boot_current, boot_order, firmware_entry, medium_disk_guid, open_fat, partition_node,
    set_loader_var, table_of, whole_disks,
};
use slopos_userland::init_control::{self, Command as InitCommand};
use slopos_userland::privd::{Connection, request_id};
use slopos_userland::selfhost::{selfhost_install, workspace};
use slopos_userland::syscall::core::{clock_gettime_ns, process_list, uname};
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use slopos_userland::syscall::fs::{self, mount, sync, umount2};
use slopos_userland::syscall::{OwnedFd, UserUtsname};
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
/// How long the rescue shell at a terminal, or the installer, may take to
/// answer, under TCG.
const TERMINAL_DEADLINE: Duration = Duration::from_secs(120);
/// What the install keeps of itself on the root it made or refilled.
const INSTALL_LOG: &str = "/var/log/installer.log";
const INSTALL_ACTION: &str = "org.slopos.install";
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// What the installed system boots with: this test again, and a slot that
/// panics falls back.
const INSTALLED_CMDLINE: &str = "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=off roulette=skip panic=reboot watchdog.miss_threshold=300 tests.run=*ext4_aaa*,*installer*";

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
        match not_offered(&node, medium) {
            Some(why) => println!("INSTALLER-NOT-OFFERED {node}: {why:?}"),
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

/// Write `header` and `array` as both copies of `disk`'s table.
fn write_table(disk: &str, header: Header, array: &[u8]) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(disk)
        .map_err(|e| format!("{disk}: {e}"))?;
    let geometry = header.geometry();
    for piece in header.write_order() {
        let (bytes, lba) = match piece {
            Piece::Array(copy) => (array.to_vec(), header.array_lba(copy)),
            Piece::Header(copy) => {
                let mut sector = vec![0u8; geometry.block() as usize];
                header.encode(copy, &mut sector);
                let lba = match copy {
                    Location::Primary => gpt::PRIMARY_LBA,
                    Location::Backup => geometry.backup_lba(),
                };
                (sector, lba)
            }
        };
        file.write_all_at(&bytes, lba * geometry.block())
            .and_then(|()| file.sync_data())
            .map_err(|e| format!("{disk}: {e}"))?;
    }
    Ok(())
}

/// The installer reads the disk again once the person confirms, and writes
/// nothing when it no longer proposes what they reviewed: here the other
/// system's partition is renamed while the installer waits for the answer.
fn refused_once_changed(disk: &str, args: &[String]) -> Result<(), String> {
    const ASKED: &str = "Type yes to write the disk";
    let table = table_of(disk).ok_or("the foreign disk has no table")?;
    let mut entry = table
        .partitions()
        .find(|p| p.entry.type_guid != ESP_TYPE)
        .ok_or("the foreign disk has no data partition")?
        .entry;
    entry.name = Entry::name_of("renamed under review");
    let mut renamed = table.array.clone();
    entry.encode(
        table
            .header
            .slot_mut(&mut renamed, entry.number)
            .ok_or("the data partition has no slot")?,
    );
    let mut installer = Command::new("/bin/installer")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("installer: {e}"))?;
    let stdout = installer.stdout.take().ok_or("no stdout")?;
    let _ = net::set_nonblocking(stdout.as_raw_fd());
    let deadline = Instant::now() + TERMINAL_DEADLINE;
    let mut shown = Vec::new();
    let mut buf = [0u8; 512];
    while !String::from_utf8_lossy(&shown).contains(ASKED) {
        let shown_so_far = || String::from_utf8_lossy(&shown).into_owned();
        match fs::read_slice(stdout.as_raw_fd(), &mut buf) {
            Ok(n) if n > 0 => shown.extend_from_slice(&buf[..n]),
            Ok(_) => {
                drop(installer.stdin.take());
                let said = installer
                    .wait_with_output()
                    .map(|out| String::from_utf8_lossy(&out.stderr).into_owned())
                    .unwrap_or_default();
                return Err(format!(
                    "the installer ended without asking to write the disk: {}{said}",
                    shown_so_far()
                ));
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                drop(installer.stdin.take());
                let _ = installer.kill();
                return Err(format!(
                    "the installer never asked to write the disk: {}",
                    shown_so_far()
                ));
            }
        }
    }
    let changed = write_table(disk, table.header.holding(&renamed), &renamed);
    let mut stdin = installer.stdin.take().ok_or("no stdin")?;
    // Unanswered, the installer reads the end of its input and writes nothing.
    let answered = match &changed {
        Ok(()) => stdin
            .write_all(b"yes\n")
            .map_err(|e| format!("answering the installer: {e}")),
        Err(_) => Ok(()),
    };
    drop((stdin, stdout));
    let out = installer
        .wait_with_output()
        .map_err(|e| format!("installer: {e}"));
    let after = table_of(disk).map(|t| t.array);
    let restored = write_table(disk, table.header, &table.array);
    changed.and(answered).and(restored)?;
    let out = out?;
    let said = String::from_utf8_lossy(&out.stderr);
    let refused = Failure::Refused(Issue::Changed(disk.to_owned())).to_string();
    if out.status.success() || !said.contains(&refused) {
        return Err(format!(
            "the installer, confirmed over a changed disk, exited {:?}: {said}",
            out.status
        ));
    }
    if after != Some(renamed) {
        return Err("the installer wrote to the disk it refused".to_owned());
    }
    println!("INSTALLER-REVIEWED-CHANGED {disk}: nothing written");
    Ok(())
}

/// `installer --serve` as privd runs it for this requester, which asks as
/// `root` and so is asked nothing, on two pipes this end holds the other
/// ends of.
struct Engine {
    privd: Connection,
    request: u64,
    to: OwnedFd,
    from: OwnedFd,
    replies: Deframer,
}

impl Engine {
    fn start() -> Result<Engine, String> {
        let mut privd = Connection::open()?;
        let pipe = || fs::pipe2(O_CLOEXEC as u32).map_err(|e| format!("pipe: {e}"));
        let ((engine_in, to), (from, engine_out)) = (pipe()?, pipe()?);
        let request = PrivdRequest {
            id: request_id(),
            action: INSTALL_ACTION.into(),
            args: vec![b"--serve".to_vec()],
            cwd: false,
            terminal: false,
        };
        privd.send(&request, &[engine_in.raw(), engine_out.raw(), 2])?;
        drop((engine_in, engine_out));
        match privd.reply(request.id)? {
            Some(PrivdReply::Started { .. }) => {}
            other => return Err(format!("privd answered {other:?} for {INSTALL_ACTION}")),
        }
        let mut engine = Engine {
            privd,
            request: request.id,
            to,
            from,
            replies: protocol::replies(),
        };
        match engine.ask(EngineRequest::Hello {
            version: protocol::VERSION,
        })? {
            EngineReply::Hello { version } if version == protocol::VERSION => Ok(engine),
            other => Err(format!("the engine said {other:?} to Hello")),
        }
    }

    fn send(&self, request: &EngineRequest) -> Result<(), String> {
        let frame = request.frame();
        let mut sent = 0;
        while sent < frame.len() {
            match fs::write_slice(self.to.raw(), &frame[sent..]) {
                Ok(n) => sent += n,
                Err(SyscallError::EINTR) => {}
                Err(e) => return Err(format!("writing to the engine: {e}")),
            }
        }
        Ok(())
    }

    fn reply(&mut self) -> Result<EngineReply, String> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some((kind, payload)) = self.replies.next_frame().map_err(|e| e.to_string())? {
                return EngineReply::decode(kind, &payload).map_err(|e| e.to_string());
            }
            match fs::read_slice(self.from.raw(), &mut buf) {
                Ok(0) => return Err("the engine ended without answering".into()),
                Ok(n) => self.replies.push(&buf[..n]),
                Err(SyscallError::EINTR) => {}
                Err(e) => return Err(format!("reading the engine: {e}")),
            }
        }
    }

    fn ask(&mut self, request: EngineRequest) -> Result<EngineReply, String> {
        self.send(&request)?;
        self.reply()
    }

    /// Probe the machine and open the disk `node`: the partitions it offers
    /// as a root, each with whether it fits.
    fn open(&mut self, node: &str) -> Result<Vec<(u32, bool)>, String> {
        match self.ask(EngineRequest::Probe)? {
            EngineReply::System(system) if system.disk(node).is_some() => {}
            other => return Err(format!("the engine probed {other:?}")),
        }
        let opened = match self.ask(EngineRequest::Open { disk: node.into() })? {
            EngineReply::System(system) => system.disk(node).and_then(|d| d.opened().cloned()),
            other => return Err(format!("the engine opened {other:?}")),
        };
        let opened = opened.ok_or_else(|| format!("{node} came back unopened"))?;
        Ok(opened
            .roots
            .iter()
            .map(|(n, fit)| (*n, fit.is_ok()))
            .collect())
    }

    fn propose(&mut self, choices: &Choices) -> Result<EngineReply, String> {
        self.ask(EngineRequest::Propose(choices.clone()))
    }

    /// Close the engine's input and wait for privd to say how it exited.
    fn exited(mut self) -> Result<i32, String> {
        drop(self.to);
        let mut buf = [0u8; 4096];
        while matches!(fs::read_slice(self.from.raw(), &mut buf), Ok(n) if n > 0) {}
        loop {
            match self.privd.reply(self.request)? {
                Some(PrivdReply::Exited(Exit::Code(code))) => return Ok(code),
                Some(PrivdReply::Exited(Exit::Signal(signal))) => {
                    return Ok(128 + i32::from(signal));
                }
                Some(_) => {}
                None => return Err("privd closed the connection".into()),
            }
        }
    }
}

/// The reuse disk installed as the window installs it, through privd's
/// `installer --serve`: a driver that leaves before `Install` writes
/// nothing, an install of a proposal it was not last given is refused with
/// nothing written and the engine serves on, and then the proposal for the
/// partition to reuse goes in. Its events are printed as the CLI prints them.
fn served_install(disk: &str, root: u32) -> Result<(), String> {
    let direct = Command::new("/bin/installer")
        .arg("--serve")
        .output()
        .map_err(|e| format!("installer --serve: {e}"))?;
    if direct.status.success()
        || !String::from_utf8_lossy(&direct.stderr).contains("--serve answers only")
    {
        return Err(format!(
            "installer --serve outside privd exited {:?}",
            direct.status
        ));
    }
    let mut choices = Choices::new(
        disk,
        Mode::Reuse {
            root,
            region: None,
            keep: false,
        },
    );
    choices.remote = Some(REMOTE.into());
    choices.cmdline = Some(INSTALLED_CMDLINE.into());

    let before = table_of(disk).map(|t| t.array);
    let mut engine = Engine::start()?;
    let roots = engine.open(disk)?;
    if !roots.contains(&(root, true)) {
        return Err(format!(
            "partition {root} is not offered as a root: {roots:?}"
        ));
    }
    let mut sized = choices.clone();
    sized.root_size = Some(GIB);
    match engine.propose(&sized)? {
        EngineReply::Issue(Issue::RootSize) => {}
        other => return Err(format!("a size for a reused root was answered {other:?}")),
    }
    let EngineReply::Proposal { proposal, .. } = engine.propose(&choices)? else {
        return Err("the reuse was not proposed".into());
    };
    match proposal.role(Role::Root) {
        Some((Tag::Reuse, Part::Existing { number, .. })) if *number == root => {}
        other => return Err(format!("the proposal does to the root {other:?}")),
    }
    match engine.exited()? {
        0 if table_of(disk).map(|t| t.array) == before => {
            println!("INSTALLER-SERVED-LEFT {disk}: nothing written")
        }
        code => {
            return Err(format!(
                "the engine left before Install exited {code}, or wrote"
            ));
        }
    }

    let mut engine = Engine::start()?;
    engine.open(disk)?;
    let EngineReply::Proposal { id, .. } = engine.propose(&choices)? else {
        return Err("the reuse was not proposed".into());
    };
    let stale = EngineRequest::Install {
        choices: choices.clone(),
        proposal: id + 1,
    };
    match engine.ask(stale)? {
        EngineReply::Failed(failure)
            if failure == Failure::Refused(Issue::Unproposed)
                && table_of(disk).map(|t| t.array) == before => {}
        other => return Err(format!("a stale install was answered {other:?}, or wrote")),
    }
    engine.open(disk)?;
    let EngineReply::Proposal { id, .. } = engine.propose(&choices)? else {
        return Err("the reuse was not proposed again".into());
    };
    engine.send(&EngineRequest::Install {
        choices,
        proposal: id,
    })?;
    loop {
        match engine.reply()? {
            EngineReply::Event(Event::Step(step)) => println!("installer: {step}"),
            EngineReply::Event(Event::Log { line }) => println!("installer:   {line}"),
            EngineReply::Event(Event::Progress { .. }) => {}
            EngineReply::Done(summary) => {
                println!("installer: {summary}");
                break;
            }
            other => return Err(format!("the install ended {other:?}")),
        }
    }
    match engine.exited()? {
        0 => Ok(()),
        code => Err(format!("the engine exited {code} after its install")),
    }
}

/// The root size the foreign disk's install asks for: a GiB past the least
/// a root of this medium takes, so it is neither bound.
fn foreign_root_size() -> Result<u64, String> {
    let payload = std::path::Path::new(layout::PAYLOAD_DIR);
    let carried = ["usr/local", "src"]
        .iter()
        .any(|tree| payload.join(tree).exists());
    let bytes = if carried {
        let path = std::ffi::CString::new(layout::PAYLOAD_DIR).map_err(|_| "a NUL")?;
        let stats = fs::statfs_path(path.as_ptr())
            .map_err(|e| format!("{}: {e:?}", layout::PAYLOAD_DIR))?;
        Some(stats.f_blocks * stats.f_bsize)
    } else {
        None
    };
    Ok(root_min(bytes).div_ceil(MIB) * MIB + GIB)
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

/// The programs of the live processes carrying `tag`, with their uids.
fn programs(tag: u32) -> Vec<(String, u32)> {
    let mut tasks = vec![UserTaskEntry::default(); MAX_TASKS];
    let count = process_list(&mut tasks).clamp(0, MAX_TASKS as i64);
    tasks.truncate(count as usize);
    let live = init_control::processes(tag);
    tasks
        .iter()
        .filter(|task| live.contains(&task.task_id))
        .map(|task| {
            let end = task
                .name
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(task.name.len());
            (
                String::from_utf8_lossy(&task.name[..end]).into_owned(),
                task.uid,
            )
        })
        .collect()
}

/// The entries the live system's dock reads carry the installer's, and the
/// boot's desktop opens the installer's welcome, as the first person, in
/// place of a terminal, and its terminal once restarted.
fn welcomed() -> Result<(), String> {
    let entries = app_entry::pinned(None);
    if !entries
        .iter()
        .any(|entry| entry.exec == "/bin/installer-ui")
    {
        return Err(format!("the live system's dock holds {entries:?}"));
    }
    init_control::ask(InitCommand::RestartSession)?;
    let opened = welcomed_once();
    init_control::ask(InitCommand::EndSession)?;
    opened
}

fn welcomed_once() -> Result<(), String> {
    let desktop = sessions::in_front(SESSION_UID, sessions::SESSION_DEADLINE)
        .ok_or("the boot's desktop never came in front")?;
    let opens = |program: &str, not: &str| {
        let mut opened = Vec::new();
        let seen = sessions::until(sessions::SESSION_DEADLINE, || {
            opened = programs(desktop.tag);
            opened.contains(&(program.to_owned(), SESSION_UID))
                && !opened.iter().any(|(other, _)| other == not)
        });
        match seen {
            true => Ok(()),
            false => Err(format!(
                "the boot's desktop opened {opened:?}, not {program}"
            )),
        }
    };
    opens("installer-ui", "terminal")?;
    init_control::ask(InitCommand::Restart(desktop.tag))?;
    opens("terminal", "installer-ui")
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
    let mut served = None;
    match kind {
        Kind::Erase => {
            if let Err(why) = welcomed() {
                note(&why);
                return false;
            }
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
            answers.push_str(short(&disk));
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
            let size = match foreign_root_size() {
                Ok(size) => size,
                Err(why) => {
                    note(&why);
                    return false;
                }
            };
            println!("INSTALLER-ROOT-SIZE {size}");
            args.extend(flagged);
            args.extend(["--mode", "free", "--first"].map(str::to_owned));
            args.extend(["--root-size".to_owned(), format!("{size} bytes")]);
            args.extend(["--person", "user", "--no-password"].map(str::to_owned));
            if !set_var(PERSON, b"user") {
                return false;
            }
            let asked: Vec<String> = args.iter().filter(|arg| *arg != "--yes").cloned().collect();
            if let Err(why) = refused_once_changed(&disk, &asked) {
                note(&why);
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
                    .and_then(|()| rescue(&disk, &partitions, &node))
                    .and_then(|()| rescue_at_a_terminal())
            {
                note(&why);
                return false;
            }
            if reinstall {
                args.extend(flagged);
                args.extend(
                    [
                        "--mode",
                        "reuse",
                        "--root",
                        &node,
                        "--keep-root",
                        "--not-first",
                    ]
                    .map(str::to_owned),
                );
            } else {
                served = Some(root.entry.number);
                if !set_var(PERSON, b"user") {
                    return false;
                }
            }
        }
    }
    let started = Instant::now();
    let installed = match (&typed, served) {
        (_, Some(root)) => served_install(&disk, root),
        (None, None) => match Command::new("/bin/installer").args(&args).status() {
            Ok(status) if status.success() => Ok(()),
            other => Err(format!("installer {}: {other:?}", args.join(" "))),
        },
        (Some(answers), None) => match answer_on_stdin(&args, answers) {
            Ok(status) if status.success() => Ok(()),
            other => Err(format!("installer {}: {other:?}", args.join(" "))),
        },
    };
    if let Err(why) = installed {
        note(&why);
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
    if let Err(why) = target().and_then(|(disk, _, _)| install_log(&disk, true)) {
        note(&why);
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

/// The root holds the log of the install that made it, or of the reinstall
/// that kept it: its summary and every step before the check.
fn install_log(disk: &str, kept: bool) -> Result<(), String> {
    let log = std::fs::read_to_string(INSTALL_LOG).map_err(|e| format!("{INSTALL_LOG}: {e}"))?;
    let wanted = [
        format!("SlopOS will be installed on {disk}:"),
        if kept {
            "  Accounts: ".to_owned()
        } else {
            "the first administrator".to_owned()
        },
        Step::Default.to_string(),
        "  LoaderEntryDefault is slopos-a".to_owned(),
    ];
    match wanted.iter().find(|line| !log.contains(line.as_str())) {
        Some(missing) => Err(format!("{INSTALL_LOG} has no {missing:?}:\n{log}")),
        None => {
            println!("INSTALLER-LOG {INSTALL_LOG} of {disk}");
            Ok(())
        }
    }
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
    if let Err(why) = install_log(&disk, false) {
        note(&why);
        return false;
    }
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
