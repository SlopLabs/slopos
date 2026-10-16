//! A kernel installed into a boot slot, tried once, committed, and a broken
//! one rolled back; then a base installed and rolled back, and one the kernel
//! does not serve passed over for the previous — across the reboots that
//! takes, on a boot disk `just test-install` attaches, and through the
//! firmware entry an installer registers for SlopOS's loader. Each boot runs
//! this test again; a non-volatile UEFI variable says which stage the last
//! boot reached.
//!
//! 0 (booted `slopos-a` by the removable-media path, as a disk the firmware
//!   has no entry for is): register SlopOS's firmware entry first in
//!   `BootOrder`. With a workspace at `/src/slopos`, check out the host's
//!   `HEAD` there, build the tests kernel and base under a fresh build tag,
//!   install the base, put the kernel into slot b and return the tree to its
//!   own checkout;
//!   without one, clone slot a into b. Boot b once.
//! 1 (booted `slopos-b` through the firmware entry, default still a): slot b
//!   holds a kernel alone and boots the current base, which slot a booted
//!   too unless stage 0 built a new one; the running kernel and base carry
//!   the tag stage 0 built them with, if it built them, and the kernel then
//!   commits a change on the host's `HEAD` and pushes it to the host; commit
//!   b, boot `slopos-bad` once — a kernel whose command line panics it with
//!   `panic=reboot` — and reboot.
//! 2 (booted `slopos-b` again, through the firmware entry): the panic reset
//!   back to the default, and init's `bootctl collect` moved the record the
//!   panic left in the crash partition to `/var/log/crash`, which `bootctl
//!   status` reports as slot bad's last boot. Boot `slopos-abort` once — its
//!   kernel takes the format-free abort a lockup takes — and reboot.
//! 3 (booted `slopos-b` again): that abort left a record too. Install a base
//!   repacked from the current one under the tag [`TAG_B`], which moves the
//!   running base to previous, and reboot.
//! 4: the boot runs base b from current. Roll back, and reboot.
//! 5: the boot runs the first base from current again, b is previous.
//!   Install one tagged [`TAG_C`] naming an interface this kernel does not
//!   serve, which moves the running base to previous again, and reboot.
//! 6: the kernel passed base c over and runs previous. A base installed now
//!   replaces c alone, since previous runs; a kernel serving neither base is
//!   refused a slot, and with such a kernel the default, a base it would serve
//!   neither of is refused too.
//!
//! Without a boot disk it has nothing to do and passes, as `toolchain_test`
//! does without a toolchain.

use slopos_userland as _;

use slopos_boot_core::base::BUILD_TAG_PATH;
use slopos_boot_core::crash::Summary;
use slopos_boot_core::{layout, variables};
use slopos_slibc::test_harness::note;
use slopos_userland::base_image::{retagged, sha256};
use slopos_userland::boot_disk::{
    BootDisk, FindError, boot_current, boot_order, firmware_entry, open_fat,
    register_firmware_entry,
};
use slopos_userland::selfhost::{
    SCRATCH, check_out, git, selfhost_install, stdout_of, take_host_head, workspace,
};
use slopos_userland::syscall::UserUtsname;
use slopos_userland::syscall::core::{clock_gettime_ns, uname};
use slopos_userland::syscall::efi::{efivar_get, efivar_set};
use slopos_userland::syscall::error::SyscallError;
use std::process::Command;
use std::time::Instant;

const BASE_TAG_FILE: &str = BUILD_TAG_PATH;
const TAG_B: &str = "install-test-base-b";
const TAG_C: &str = "install-test-base-c";
const TAG_D: &str = "install-test-base-d";
const TAG_E: &str = "install-test-base-e";
/// Where a repacked base waits for `bootctl`: memory, since the tests root
/// has no room for one.
const FIXTURE: &str = "/tmp/install-test-base.img";

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
    let installed = selfhost_install(root, "tests", &tag);
    let returned = check_out(root, &head.before);
    if let Err(why) = installed {
        note(&why);
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

/// The record the panic of slot `slot` left, collected: saved under
/// `/var/log/crash`, naming `panic` and holding `heading`'s report, named as
/// the slot's last boot by `status`, and gone from the crash store.
fn panic_collected(status: &str, slot: &str, panic: &str, heading: &str) -> bool {
    let saved = std::fs::read_dir("/var/log/crash")
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .find(|path| {
            let text = std::fs::read(path).unwrap_or_default();
            let text = String::from_utf8_lossy(&text);
            Summary::parse(&text)
                .is_some_and(|s| s.slot() == Some(slot) && s.panic.ends_with(panic))
                && text.contains(heading)
        });
    let Some(saved) = saved else {
        note(&format!(
            "/var/log/crash holds no record of slot {slot}'s panic"
        ));
        return false;
    };
    let saved = saved.display().to_string();
    let reported = format!(", last boot crashed: {saved}");
    let line = format!("slot {slot}:");
    if !status
        .lines()
        .any(|l| l.starts_with(&line) && l.ends_with(&reported))
    {
        note(&format!(
            "bootctl status does not report {saved}:\n{status}"
        ));
        return false;
    }
    match std::fs::read_dir("/dev/crash").map(Iterator::count) {
        Ok(0) => {}
        other => {
            note(&format!("the crash store still holds records: {other:?}"));
            return false;
        }
    }
    println!("INSTALL-CRASH-RECORD {saved}");
    true
}

/// The current base repacked as `tag` at [`FIXTURE`].
fn write_fixture(disk: &BootDisk, tag: &str, interface: Option<u32>) -> bool {
    let written = open_fat(&disk.boot_node)
        .and_then(|mut volume| {
            volume
                .read_file(layout::BASE_CURRENT)
                .map_err(|e| format!("{}: {e:?}", layout::BASE_CURRENT))
        })
        .and_then(|current| retagged(&current, tag, interface))
        .and_then(|image| std::fs::write(FIXTURE, image).map_err(|e| format!("{FIXTURE}: {e}")));
    if let Err(why) = written {
        note(&why);
        return false;
    }
    true
}

/// Install the current base repacked as `tag` through `bootctl`, naming its
/// SHA-256 as `remote.py` does, once another digest is refused.
fn install_repacked(disk: &BootDisk, tag: &str, interface: Option<u32>) -> Option<String> {
    if !write_fixture(disk, tag, interface) {
        return None;
    }
    let said = std::fs::read(FIXTURE)
        .map_err(|e| note(&format!("{FIXTURE}: {e}")))
        .ok()
        .and_then(|image| {
            let digest: String = sha256(&image).iter().map(|b| format!("{b:02x}")).collect();
            let other = "0".repeat(64);
            if !refused(
                &["base", "install", FIXTURE, &other],
                "not the image whose sha256 is",
            ) {
                note("bootctl installed a base whose sha256 is not the one named");
                return None;
            }
            bootctl(&["base", "install", FIXTURE, &digest])
        });
    let _ = std::fs::remove_file(FIXTURE);
    let said = said?;
    println!("INSTALL-BASE-INSTALLED {tag}");
    Some(said)
}

/// The base installed or rolled back to runs without a boot: the build tag
/// the base directories show is `tagged`, and `bootctl status` names
/// `current` and `previous`.
fn runs_live(
    stage: u8,
    tagged: impl Fn(&str) -> bool,
    current: (Option<&str>, bool),
    previous: (Option<&str>, bool),
) -> bool {
    let running = std::fs::read_to_string(BASE_TAG_FILE).unwrap_or_default();
    let tag = running.trim();
    if !tagged(tag) {
        note(&format!(
            "stage {stage}: the base running now is tagged {tag:?}"
        ));
        return false;
    }
    let Some(status) = bootctl(&["status"]) else {
        return false;
    };
    if !bases_are(&status, stage, current, previous) {
        return false;
    }
    println!("INSTALL-BASE-LIVE at stage {stage}: {tag:?}");
    true
}

fn refused(args: &[&str], saying: &str) -> bool {
    Command::new("/bin/bootctl")
        .args(args)
        .output()
        .is_ok_and(|out| {
            !out.status.success() && String::from_utf8_lossy(&out.stderr).contains(saying)
        })
}

/// With slot bad the default and `kernel` put there behind `bootctl`'s
/// back, a base install that would leave it serving neither base is
/// refused, and so is making it the default again.
fn default_kept_bootable(disk: &BootDisk, kernel: &[u8]) -> bool {
    if bootctl(&["set-default", "slopos-bad"]).is_none() {
        return false;
    }
    let path = format!("{}/bad/{}", layout::SLOTS_DIR, layout::KERNEL_FILE);
    let planted = open_fat(&disk.boot_node).and_then(|mut volume| {
        volume
            .write_file(&path, kernel)
            .map_err(|e| format!("{path}: {e:?}"))
    });
    if let Err(why) = planted {
        note(&why);
        return false;
    }
    if !write_fixture(disk, TAG_E, Some(slopos_abi::BASE_INTERFACE + 1)) {
        return false;
    }
    let base_refused = refused(
        &["base", "install", FIXTURE],
        "slopos-bad's kernel serves base interfaces 90-99",
    );
    let _ = std::fs::remove_file(FIXTURE);
    let restored = bootctl(&["set-default", "slopos-b"]).is_some();
    let default_refused = refused(
        &["set-default", "slopos-bad"],
        "the kernel serves base interfaces 90-99",
    );
    if !base_refused {
        note("bootctl installed a base that leaves the default kernel serving neither");
    }
    if !default_refused {
        note("bootctl made the default a kernel that serves neither base");
    }
    base_refused && restored && default_refused
}

/// What `bootctl status` says of base `which`: its interface, its build
/// tag, and whether it is the one running.
fn base_of(status: &str, which: &str) -> Option<(u32, String, bool)> {
    let line = field(status, &format!("base {which}"))?;
    let mut parts = line.split(", ");
    let interface = parts.next()?.strip_prefix("interface ")?.parse().ok()?;
    let tag = parts.find_map(|part| part.strip_prefix("build tag "))?;
    Some((interface, tag.to_owned(), line.ends_with(", running")))
}

/// The bases as `bootctl status` says this boot holds them: `(tag, running)`
/// for current and previous, each tag `None` for any.
fn bases_are(
    status: &str,
    stage: u8,
    current: (Option<&str>, bool),
    previous: (Option<&str>, bool),
) -> bool {
    let holds = |which, (tag, running): (Option<&str>, bool)| {
        base_of(status, which).is_some_and(|(interface, found, runs)| {
            let built_for = if [TAG_C, TAG_D].contains(&found.as_str()) {
                slopos_abi::BASE_INTERFACE + 1
            } else {
                slopos_abi::BASE_INTERFACE
            };
            tag.is_none_or(|tag| found == tag) && runs == running && interface == built_for
        })
    };
    if holds("current", current) && holds("previous", previous) {
        println!("INSTALL-BASES at stage {stage}: {current:?} then {previous:?}");
        return true;
    }
    note(&format!(
        "stage {stage}: wanted current {current:?} and previous {previous:?}:\n{status}"
    ));
    false
}

/// A kernel file whose note serves `min..=max` and that holds nothing else:
/// what `bootctl install` must refuse a slot when it serves neither base.
fn unserved_kernel(min: u32, max: u32) -> Vec<u8> {
    let mut note = Vec::new();
    for word in [
        7u32,
        8,
        slopos_abi::base_interface::NT_SLOPOS_BASE_INTERFACE,
    ] {
        note.extend_from_slice(&word.to_le_bytes());
    }
    note.extend_from_slice(b"SlopOS\0\0");
    note.extend_from_slice(&min.to_le_bytes());
    note.extend_from_slice(&max.to_le_bytes());
    let names = b"\0.note.slopos.base-interface\0.shstrtab\0";
    let mut file = vec![0u8; 64];
    file[..6].copy_from_slice(b"\x7fELF\x02\x01");
    let note_at = file.len() as u64;
    file.extend_from_slice(&note);
    let names_at = file.len() as u64;
    file.extend_from_slice(names);
    file.resize(file.len().next_multiple_of(8), 0);
    let table = file.len() as u64;
    let section = |name: u32, kind: u32, at: u64, len: usize| {
        let mut header = [0u8; 64];
        header[0..4].copy_from_slice(&name.to_le_bytes());
        header[4..8].copy_from_slice(&kind.to_le_bytes());
        header[24..32].copy_from_slice(&at.to_le_bytes());
        header[32..40].copy_from_slice(&(len as u64).to_le_bytes());
        header[48..56].copy_from_slice(&4u64.to_le_bytes());
        header
    };
    file.extend_from_slice(&[0u8; 64]);
    file.extend_from_slice(&section(1, 7, note_at, note.len()));
    file.extend_from_slice(&section(29, 3, names_at, names.len()));
    file[0x28..0x30].copy_from_slice(&table.to_le_bytes());
    file[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes());
    file[0x3C..0x3E].copy_from_slice(&3u16.to_le_bytes());
    file[0x3E..0x40].copy_from_slice(&2u16.to_le_bytes());
    file
}

fn reboot_default(next: u8) -> bool {
    if !set_stage(Some(next)) {
        return false;
    }
    println!("INSTALL-STAGE {next}: rebooting into the default");
    let _ = bootctl(&["reboot"]);
    note("bootctl reboot returned");
    false
}

/// Slot b's directory holds its kernel and nothing more.
fn slot_holds_a_kernel_alone(disk: &BootDisk) -> bool {
    let dir = format!("{}/b", layout::SLOTS_DIR);
    let names: Option<Vec<String>> = open_fat(&disk.boot_node).ok().and_then(|mut volume| {
        let entries = volume.list(&dir).ok()?;
        Some(
            entries
                .into_iter()
                .map(|e| e.name)
                .filter(|n| n != "." && n != "..")
                .collect(),
        )
    });
    if names.as_deref() != Some(&[layout::KERNEL_FILE.to_owned()]) {
        note(&format!("{dir} holds {names:?}, not a kernel alone"));
        return false;
    }
    true
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
            if !booted_through_entry(&disk, 1)
                || !slot_holds_a_kernel_alone(&disk)
                || !bases_are(&status, 1, (None, true), (None, built_tag().is_none()))
            {
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
                        note(&format!("the running base carries {other:?}, not {tag}"));
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
            let _ = set_var(TAG, &[]);
            if booted != "slopos-b" || default != "slopos-b" || oneshot != "-" {
                note(&format!(
                    "after the broken slot: booted {booted}, default {default}, oneshot {oneshot}"
                ));
                let _ = set_stage(None);
                return false;
            }
            if !booted_through_entry(&disk, 2)
                || !panic_collected(&status, "bad", "panic.boot=on", "=== KERNEL PANIC ===")
            {
                let _ = set_stage(None);
                return false;
            }
            reboot_into("slopos-abort", 3)
        }
        Some(3) => {
            if booted != "slopos-b" || default != "slopos-b" || oneshot != "-" {
                note(&format!(
                    "after the aborting slot: booted {booted}, default {default}, oneshot {oneshot}"
                ));
                let _ = set_stage(None);
                return false;
            }
            if !booted_through_entry(&disk, 3)
                || !panic_collected(&status, "abort", "panic.boot=abort", "=== KERNEL ABORT ===")
                || install_repacked(&disk, TAG_B, None).is_none()
            {
                let _ = set_stage(None);
                return false;
            }
            if !runs_live(3, |tag| tag == TAG_B, (Some(TAG_B), true), (None, false)) {
                let _ = set_stage(None);
                return false;
            }
            reboot_default(4)
        }
        Some(4) => {
            let running = std::fs::read_to_string(BASE_TAG_FILE).unwrap_or_default();
            let ok = booted == "slopos-b"
                && running.trim() == TAG_B
                && bases_are(&status, 4, (Some(TAG_B), true), (None, false))
                && bootctl(&["base", "rollback"]).is_some();
            if !ok {
                note(&format!(
                    "stage 4 runs {booted} on the base tagged {running:?}"
                ));
                let _ = set_stage(None);
                return false;
            }
            if !runs_live(4, |tag| tag != TAG_B, (None, true), (Some(TAG_B), false)) {
                let _ = set_stage(None);
                return false;
            }
            reboot_default(5)
        }
        Some(5) => {
            let running = std::fs::read_to_string(BASE_TAG_FILE).unwrap_or_default();
            let unserved = slopos_abi::BASE_INTERFACE + 1;
            let ok = booted == "slopos-b"
                && running.trim() != TAG_B
                && bases_are(&status, 5, (None, true), (Some(TAG_B), false))
                && install_repacked(&disk, TAG_C, Some(unserved)).is_some_and(|said| {
                    said.contains(&format!("only a boot of one that serves {unserved}"))
                });
            if !ok {
                note(&format!(
                    "stage 5 runs {booted} on the base tagged {running:?}"
                ));
                let _ = set_stage(None);
                return false;
            }
            reboot_default(6)
        }
        Some(6) => {
            let _ = set_stage(None);
            let running = std::fs::read_to_string(BASE_TAG_FILE).unwrap_or_default();
            if booted != "slopos-b"
                || running.trim() == TAG_C
                || !bases_are(&status, 6, (Some(TAG_C), false), (None, true))
            {
                note(&format!(
                    "stage 6 runs {booted} on the base tagged {running:?}"
                ));
                return false;
            }
            println!("INSTALL-PASSED-OVER {TAG_C}");
            if install_repacked(&disk, TAG_D, Some(slopos_abi::BASE_INTERFACE + 1)).is_none() {
                return false;
            }
            let Some(status) = bootctl(&["status"]) else {
                return false;
            };
            if !bases_are(&status, 6, (Some(TAG_D), false), (None, true)) {
                return false;
            }
            let kernel = "/tmp/install-test-unserved.elf";
            let unserved = unserved_kernel(90, 99);
            let refused = std::fs::write(kernel, &unserved).is_ok()
                && Command::new("/bin/bootctl")
                    .args(["install", "a", kernel])
                    .output()
                    .is_ok_and(|out| {
                        !out.status.success()
                            && String::from_utf8_lossy(&out.stderr)
                                .contains("the kernel serves base interfaces 90-99")
                    });
            let _ = std::fs::remove_file(kernel);
            if !refused {
                note("bootctl gave a slot a kernel that serves neither base");
                return false;
            }
            println!("INSTALL-REFUSED a kernel that serves neither base");
            if !default_kept_bootable(&disk, &unserved) {
                return false;
            }
            println!("INSTALL-REFUSED a base that leaves the default serving neither");
            if bootctl(&["base", "rollback"]).is_none() {
                return false;
            }
            note(
                "slot b installed, tried and committed; a panicking and an aborting slot rolled back, each leaving its record; a base installed, rolled back, and one the kernel does not serve passed over",
            );
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
