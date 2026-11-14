//! `/bin/bootctl` — install a kernel into a boot slot, replace the base every
//! slot boots, and choose what boots.
//!
//! The boot partition, on the disk the loader was started from, holds a
//! kernel per slot at `/boot/<slot>/kernel.elf`, each a Limine entry
//! `slopos-<slot>`, and beside them the bases every slot boots,
//! `/boot/base/current.img` and `previous.img`, the one that last ran. A base
//! is tried before it is kept: `base try` runs it from memory and writes
//! nothing, so any reset boots the disk's base again, and `base commit`
//! writes the base that runs. Given the kernel beside it, `base try` puts a
//! base the running kernel does not serve on the partition instead, and that
//! kernel into the spare slot, armed to boot once, so the base rides the
//! kernel's trial. `base install` writes and runs a base at once.
//!
//! A slot's kernel carries the machine's words, [`layout::MACHINE_CMDLINE`],
//! stamped into its [`kernel::CMDLINE_SECTION`] as it is written, so the
//! words are tried with the slot and kept at its commit.
//!
//! What boots is chosen through the Boot Loader Interface alone, and nothing
//! on the ESP is written. A new kernel is tried once, not adopted: `oneshot`
//! sets `LoaderEntryOneShot`, and `LoaderConfigTimeoutOneShot` to skip the
//! menu, which Limine consumes on the next boot, so a reset after a panic
//! boots the default again. `commit`, run once the tried
//! system is up, makes the entry Limine reports as booted
//! (`LoaderEntrySelected`) the default, `LoaderEntryDefault`.
//!
//! `--boot` names another boot partition, as a rescue from the install
//! medium does for the disk it repairs. Its loader has not run this boot, so
//! what it offers is what the configuration beside SlopOS's loader on that
//! disk renders, and what it boots by default and once is what the machine's
//! variables say.
//!
//! Every file write is copy-on-write (`slopos_fat_core`), so no kernel or base
//! is ever half replaced on the medium.
//!
//! `collect`, which init runs on every boot, moves the records the kernel's
//! crash store kept to `/var/log/crash` and notes what each slot's last boot
//! came to under `/var/lib/slopos/slots`, which `status` reports.
//!
//! Needs `RawBlock` for the raw partition and the crash records, `Power` for
//! the loader variables and the reboot, and `BaseUpdate` to run a base;
//! started without them, each command asks privd to run it as its
//! `org.slopos.boot.*` or `org.slopos.base.*` action. `collect` is init's,
//! which starts it holding both.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use std::ops::RangeInclusive;

use slopos_boot_core::base::{self, Which};
use slopos_boot_core::crash::Summary;
use slopos_boot_core::{bli, kernel, layout, limine};
use slopos_fat_core::{Error as FatError, Volume};

use crate::app_entry::read_bounded;
use crate::apps::coreutils::time::utc_from_epoch;
use crate::base_image::sha256;
use crate::boot_disk::{
    BlockFile, BootDisk, loader_entries, loader_var, open_fat, partition_node, set_loader_var,
    table_of, whole_disks,
};
use crate::privd::{self, Input};
use crate::syscall::UserSysInfo;
use crate::syscall::core as sys_core;
use crate::syscall::process;
use slopos_abi::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

const CRASH_STORE: &str = "/dev/crash";
const CRASH_LOG: &str = "/var/log/crash";
const CRASH_LOG_KEPT: usize = 64;
/// The index the next record saved under `CRASH_LOG` takes. Each name starts
/// with its index, so names sort by when they were saved, whatever the clock
/// said.
const CRASH_LOG_BOUNDS: &str = "bounds";
/// A file per slot: how its last recorded boot ended, `up` or `crashed`,
/// then the record its last crash left. A boot that died with no record
/// leaves the one before it as the last recorded.
const SLOT_STATE: &str = "/var/lib/slopos/slots";
/// What a running trial of a base replaced: the SHA-256 of its manifest, in
/// hex. The first `base try` of a trial writes it, and a commit, a rollback
/// or an install that runs a base from the partition ends it; in memory, so
/// any reset ends the trial too.
const TRIAL_DIR: &str = "/run/bootctl";
const TRIAL_FILE: &str = "base-trial";
const BASES: [(Which, &str); 2] = [
    (Which::Current, layout::BASE_CURRENT),
    (Which::Previous, layout::BASE_PREVIOUS),
];

fn boot_partition(named: Option<&str>) -> Result<String, String> {
    match named {
        Some(node) => Ok(node.to_owned()),
        None => BootDisk::find()
            .map(|disk| disk.boot_node)
            .map_err(|e| format!("{e}; pass --boot /dev/<node>")),
    }
}

/// What the loader of a boot partition offers, and what the system asked of it.
struct Loader {
    entries: Vec<String>,
    selected: Option<String>,
    default: Option<String>,
    one_shot: Option<String>,
    /// The loader is this boot's, and the records under [`SLOT_STATE`] its
    /// slots'.
    here: bool,
}

impl Loader {
    /// This boot's loader when no partition is `named` or the system booted
    /// from it, and the loader of the disk it names otherwise.
    fn of(named: Option<&str>) -> Result<Loader, String> {
        match named {
            Some(boot) if !booted_from(boot) => Ok(Loader {
                entries: configured_entries(boot)?,
                selected: None,
                default: loader_var(bli::ENTRY_DEFAULT)?,
                one_shot: loader_var(bli::ENTRY_ONE_SHOT)?,
                here: false,
            }),
            _ => Ok(Loader {
                entries: loader_entries()?,
                selected: loader_var(bli::ENTRY_SELECTED)?,
                default: loader_var(bli::ENTRY_DEFAULT)?,
                one_shot: loader_var(bli::ENTRY_ONE_SHOT)?,
                here: true,
            }),
        }
    }

    /// The entry a boot with nothing armed takes, or why it is not known.
    fn default_entry(&self) -> Result<Option<&str>, String> {
        let entries: Vec<&str> = self.entries.iter().map(String::as_str).collect();
        bli::default_entry(&entries, self.default.as_deref()).map_err(|_| {
            format!(
                "{} is {:?}, which names no entry the boot loader offers; \
                 bootctl set-default names one",
                bli::ENTRY_DEFAULT,
                self.default.as_deref().unwrap_or_default()
            )
        })
    }

    fn offered(&self, entry: &str) -> Result<(), String> {
        if self.entries.iter().any(|e| e == entry) {
            Ok(())
        } else {
            Err(format!(
                "the boot loader offers no entry {entry} (it offers: {})",
                self.entries.join(" ")
            ))
        }
    }
}

fn status(boot: &str, loader: &Loader) -> Result<(), String> {
    let show = |value: Option<&str>| value.unwrap_or("-").to_owned();
    println!("boot: {boot}");
    println!("booted: {}", show(loader.selected.as_deref()));
    match loader.default_entry() {
        Ok(entry) => println!("default: {}", show(entry)),
        Err(why) => println!("default: ? ({why})"),
    }
    println!("oneshot: {}", show(loader.one_shot.as_deref()));
    println!("entries: {}", loader.entries.join(" "));
    let mut volume = open_fat(boot)?;
    let running = running_manifest(boot);
    let mut on_disk = false;
    for (which, path) in BASES {
        let line = match volume.read_file(path) {
            Ok(image) => {
                let described = base::describe(&image);
                let interface = match described.interface {
                    Ok(n) => format!("interface {n}"),
                    Err(e) => e.to_string(),
                };
                let runs = running.is_some() && described.manifest == running.as_deref();
                on_disk |= runs;
                format!(
                    "{interface}, build tag {}, {} bytes{}",
                    described.build_tag.unwrap_or("-"),
                    image.len(),
                    if runs { ", running" } else { "" }
                )
            }
            Err(e) => format!("- ({e:?})"),
        };
        println!("base {}: {line}", which.name());
    }
    if running.is_some() && !on_disk {
        let live = |path: &str| fs::read_to_string(path).map(|text| text.trim().to_owned());
        println!(
            "base tried: interface {}, build tag {}, running; bootctl base commit keeps it, \
             any reset boots the partition's",
            live(base::INTERFACE_PATH).unwrap_or_else(|_| "?".into()),
            live(base::BUILD_TAG_PATH).unwrap_or_else(|_| "-".into()),
        );
    }
    if let Ok(slots) = volume.list(layout::SLOTS_DIR) {
        for slot in slots
            .iter()
            .filter(|e| e.is_dir && layout::valid_slot(&e.name))
        {
            let path = format!(
                "{}/{}/{}",
                layout::SLOTS_DIR,
                slot.name,
                layout::KERNEL_FILE
            );
            if let Ok(kernel) = volume.read_file(&path) {
                let last = if loader.here {
                    last_boot(&slot.name)
                } else {
                    String::new()
                };
                let served = kernel::served_interfaces(&kernel).map_or_else(
                    |e| format!("{e:?}"),
                    |s| format!("{}-{}", s.start(), s.end()),
                );
                let words = match kernel::cmdline(&kernel) {
                    Ok("") => "-".to_owned(),
                    Ok(words) => words.to_owned(),
                    Err(e) => format!("? ({e:?})"),
                };
                println!(
                    "slot {}: kernel {} bytes, base interfaces {served}, cmdline {words}{last}",
                    slot.name,
                    kernel.len()
                );
            }
        }
    }
    Ok(())
}

fn same_node(a: &str, b: &str) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

fn booted_from(boot: &str) -> bool {
    BootDisk::find().is_ok_and(|booted| same_node(&booted.boot_node, boot))
}

/// The entries the configuration beside SlopOS's loader offers on the disk
/// whose boot partition is `boot`.
fn configured_entries(boot: &str) -> Result<Vec<String>, String> {
    let path = layout::LOADER_CONFIG.replace('\\', "/");
    for disk in whole_disks()? {
        let Some(table) = table_of(&disk) else {
            continue;
        };
        let of_type = |kind| {
            table
                .partitions()
                .filter(move |p| p.entry.type_guid == kind)
        };
        let holds_boot = of_type(layout::BOOT_TYPE)
            .any(|p| partition_node(&disk, &p).is_ok_and(|node| same_node(&node, boot)));
        if !holds_boot {
            continue;
        }
        for esp in of_type(layout::ESP_TYPE) {
            let config = partition_node(&disk, &esp)
                .and_then(|node| open_fat(&node))
                .and_then(|mut volume| volume.read_file(&path).map_err(|e| format!("{e:?}")));
            if let Ok(config) = config {
                let config = String::from_utf8_lossy(&config);
                let ids = limine::offered_entries(&config).map(|id| id.map(char::from).collect());
                return Ok(ids.collect());
            }
        }
        return Err(format!("no ESP on {disk} holds {}", layout::LOADER_CONFIG));
    }
    Err(format!("{boot} is no disk's boot partition"))
}

/// The manifest of the base this system runs, when it booted from `boot`.
fn running_manifest(boot: &str) -> Option<Vec<u8>> {
    if !booted_from(boot) {
        return None;
    }
    fs::read(base::MANIFEST_PATH).ok()
}

fn read_base(volume: &mut Volume<BlockFile>, path: &str) -> Result<Vec<u8>, String> {
    volume.read_file(path).map_err(|e| match e {
        FatError::NotFound => format!("{path}: the boot partition holds no base there"),
        e => format!("{path}: {e:?}"),
    })
}

fn served_by(kernel_path: &str, kernel: &[u8]) -> Result<RangeInclusive<u32>, String> {
    kernel::served_interfaces(kernel).map_err(|e| {
        format!(
            "{kernel_path}: no {} note to say which bases it serves ({e:?})",
            kernel::NOTE_SECTION
        )
    })
}

/// A kernel goes into a slot only when it serves one of the bases the slots
/// boot.
fn check_serves_a_base(
    volume: &mut Volume<BlockFile>,
    served: &RangeInclusive<u32>,
) -> Result<(), String> {
    let mut found = Vec::new();
    for (which, path) in BASES {
        match base::interface(&read_base(volume, path)?) {
            Ok(n) if served.contains(&n) => return Ok(()),
            Ok(n) => found.push(format!("{} interface {n}", which.name())),
            Err(e) => found.push(format!("{} {e}", which.name())),
        }
    }
    Err(format!(
        "the kernel serves base interfaces {}-{}, and the boot partition holds {}",
        served.start(),
        served.end(),
        found.join(" and ")
    ))
}

/// The slot's directory, created if need be, once the loader is known to
/// offer it and not to boot it by default.
fn slot_dir(volume: &mut Volume<BlockFile>, loader: &Loader, slot: &str) -> Result<String, String> {
    if !layout::valid_slot(slot) {
        return Err(format!(
            "slot '{slot}': 1-8 lowercase letters or digits, other than base"
        ));
    }
    let entry = format!("{}{slot}", layout::ENTRY_PREFIX);
    loader.offered(&entry)?;
    if loader.default_entry()? == Some(entry.as_str()) {
        return Err(format!(
            "{entry} is the default; install into another slot and try it"
        ));
    }
    let dir = format!("{}/{slot}", layout::SLOTS_DIR);
    match volume.create_dir(&dir) {
        Ok(()) | Err(FatError::Exists) => Ok(dir),
        Err(e) => Err(format!("{dir}: {e:?}")),
    }
}

/// Reads back past the flush: a slot is only worth booting once the medium is
/// known to hold what was meant.
fn write_verified(volume: &mut Volume<BlockFile>, path: &str, bytes: &[u8]) -> Result<(), String> {
    volume
        .write_file(path, bytes)
        .map_err(|e| format!("{path}: {e:?}"))?;
    let back = volume
        .read_file(path)
        .map_err(|e| format!("{path}: reading back: {e:?}"))?;
    if back != bytes {
        return Err(format!("{path}: read back different bytes"));
    }
    println!("wrote {} bytes to {path}, read back", bytes.len());
    Ok(())
}

/// A one-shot already armed for the slot is cleared first, so a failed install
/// leaves nothing armed to boot it.
fn install_slot(
    volume: &mut Volume<BlockFile>,
    loader: &Loader,
    slot: &str,
    kernel: &[u8],
) -> Result<(), String> {
    let dir = slot_dir(volume, loader, slot)?;
    let entry = format!("{}{slot}", layout::ENTRY_PREFIX);
    if loader.one_shot.as_deref() == Some(entry.as_str()) {
        disarm(&entry)?;
    }
    write_verified(volume, &format!("{dir}/{}", layout::KERNEL_FILE), kernel)?;
    if loader.here {
        forget_last_boot(slot);
    }
    Ok(())
}

fn disarm(entry: &str) -> Result<(), String> {
    set_loader_var(bli::ENTRY_ONE_SHOT, None)?;
    set_loader_var(bli::TIMEOUT_ONE_SHOT, None)?;
    println!("{entry} is no longer armed to boot once");
    Ok(())
}

fn forget_last_boot(slot: &str) {
    match fs::remove_file(format!("{SLOT_STATE}/{slot}")) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => eprintln!("bootctl: {SLOT_STATE}/{slot}: {e}; its last boot stays noted"),
    }
}

/// The words the machine's file holds; none without one. They can turn off
/// `quota=` and `lockdep=`, so the file counts only while uid 0 alone can
/// write it and its directory.
fn machine_words() -> Result<String, String> {
    let path = layout::MACHINE_CMDLINE;
    let dir = path.rsplit_once('/').map_or("/", |(dir, _)| dir);
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(format!("{path}: {e}")),
        Ok(_) => {}
    }
    let uid0_alone = |path: &str, file: bool| {
        fs::symlink_metadata(path).is_ok_and(|meta| {
            (if file { meta.is_file() } else { meta.is_dir() })
                && meta.uid() == 0
                && meta.mode() & 0o022 == 0
        })
    };
    if !uid0_alone(path, true) || !uid0_alone(dir, false) {
        return Err(format!(
            "{path}: only uid 0 may write it and {dir}, and bootctl cmdline set writes it so"
        ));
    }
    let text = read_bounded(Path::new(path), kernel::CMDLINE_BYTES as u64 * 4)
        .ok_or_else(|| format!("{path}: no regular file of a command line's size"))?;
    let words = String::from_utf8_lossy(&text)
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    kernel::cmdline_fits(&words).map_err(|e| format!("{path}: {}", words_error(e)))?;
    Ok(words)
}

fn words_error(e: kernel::CmdlineError) -> String {
    match e {
        kernel::CmdlineError::TooLong => format!(
            "the words take more than {} bytes",
            kernel::CMDLINE_BYTES - 1
        ),
        kernel::CmdlineError::BadWord => "a word holds other than printable ASCII".into(),
        e => format!(
            "no {} section to stamp words into ({e:?})",
            kernel::CMDLINE_SECTION
        ),
    }
}

/// `root=` is `limine.conf`'s, written at install for this disk's root, and
/// a stamped word comes after it and would win.
fn no_root(words: &str) -> Result<(), String> {
    if words
        .split_ascii_whitespace()
        .any(|word| word.starts_with("root="))
    {
        return Err("a kernel's words never name root=, which the loader's line holds".into());
    }
    Ok(())
}

/// `kernel` with the words a slot of this machine boots it with: the
/// machine's, then `extra`. Another disk's slot is that root's machine, so a
/// kernel going there keeps its own words unless `extra` names some.
fn with_words(mut kernel: Vec<u8>, here: bool, extra: Option<&str>) -> Result<Vec<u8>, String> {
    let words = match (here, extra) {
        (true, extra) => {
            let mine = machine_words()?;
            [mine.as_str(), extra.unwrap_or_default()]
                .into_iter()
                .filter(|words| !words.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        }
        (false, Some(extra)) => extra.to_owned(),
        (false, None) => return Ok(kernel),
    };
    no_root(&words)?;
    if words.is_empty() && kernel::cmdline(&kernel) == Err(kernel::CmdlineError::NoSection) {
        return Ok(kernel);
    }
    kernel::stamp_cmdline(&mut kernel, &words).map_err(words_error)?;
    Ok(kernel)
}

fn show_words(kernel: &[u8]) -> &str {
    match kernel::cmdline(kernel) {
        Ok("") | Err(_) => "-",
        Ok(words) => words,
    }
}

fn install(
    boot: &str,
    loader: &Loader,
    slot: &str,
    kernel_path: &str,
    digest: Option<&String>,
    extra: Option<&str>,
) -> Result<(), String> {
    let kernel = read_image(kernel_path, layout::BOOT_BYTES.min(bases_limit()), digest)?;
    let served = served_by(kernel_path, &kernel)?;
    let mut volume = open_fat(boot)?;
    check_serves_a_base(&mut volume, &served)?;
    let kernel = with_words(kernel, loader.here, extra)?;
    install_slot(&mut volume, loader, slot, &kernel)?;
    println!(
        "installed slot {slot} (entry {}{slot}), serving base interfaces {}-{}, cmdline {}",
        layout::ENTRY_PREFIX,
        served.start(),
        served.end(),
        show_words(&kernel)
    );
    Ok(())
}

fn clone_slot(boot: &str, loader: &Loader, from: &str, to: &str) -> Result<(), String> {
    if !layout::valid_slot(from) {
        return Err(format!(
            "slot '{from}': 1-8 lowercase letters or digits, other than base"
        ));
    }
    let mut volume = open_fat(boot)?;
    let path = format!("{}/{from}/{}", layout::SLOTS_DIR, layout::KERNEL_FILE);
    let kernel = volume
        .read_file(&path)
        .map_err(|e| format!("{path}: {e:?}"))?;
    let served = served_by(&path, &kernel)?;
    check_serves_a_base(&mut volume, &served)?;
    let kernel = with_words(kernel, loader.here, None)?;
    install_slot(&mut volume, loader, to, &kernel)?;
    println!(
        "copied slot {from}'s kernel to slot {to}, cmdline {}",
        show_words(&kernel)
    );
    Ok(())
}

/// A slot's kernel no base write may leave serving neither base: the
/// default's, what a failed try falls back on, the running one's and the one
/// armed to boot once.
struct Guarded {
    entry: String,
    slot: String,
    served: RangeInclusive<u32>,
    default: bool,
}

fn default_kernel(guarded: &[Guarded]) -> Option<&Guarded> {
    guarded.iter().find(|kernel| kernel.default)
}

/// When the base takes over: the default kernel is what boots next with
/// nothing armed.
fn when_taken(guarded: &[Guarded], interface: u32) -> String {
    match default_kernel(guarded) {
        Some(kernel) if kernel.served.contains(&interface) => {
            format!("the next boot of {} takes it", kernel.entry)
        }
        Some(kernel) => format!(
            "{}'s kernel serves base interfaces {}-{}, so only a boot of one that \
             serves {interface} takes it",
            kernel.entry,
            kernel.served.start(),
            kernel.served.end()
        ),
        None => format!("a boot of a kernel that serves interface {interface} takes it"),
    }
}

/// The [`Guarded`] kernels; an entry that is no slot's has none.
fn guarded_kernels(
    volume: &mut Volume<BlockFile>,
    loader: &Loader,
) -> Result<Vec<Guarded>, String> {
    let default = loader.default_entry()?.map(str::to_owned);
    let mut guarded: Vec<Guarded> = Vec::new();
    let entries = [&default, &loader.selected, &loader.one_shot];
    for entry in entries.into_iter().flatten() {
        let Some(slot) = entry
            .strip_prefix(layout::ENTRY_PREFIX)
            .filter(|slot| layout::valid_slot(slot))
        else {
            continue;
        };
        if guarded.iter().any(|seen| seen.entry == *entry) {
            continue;
        }
        let path = format!("{}/{slot}/{}", layout::SLOTS_DIR, layout::KERNEL_FILE);
        let kernel = volume
            .read_file(&path)
            .map_err(|e| format!("{path}: {e:?}"))?;
        guarded.push(Guarded {
            entry: entry.clone(),
            slot: slot.to_owned(),
            served: served_by(&path, &kernel)?,
            default: default.as_ref() == Some(entry),
        });
    }
    Ok(guarded)
}

/// The SHA-256 a command line may name after an image, in hex.
fn digest_arg(words: &[String]) -> Result<Option<&String>, String> {
    match words {
        [] => Ok(None),
        [digest] if digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Ok(Some(digest))
        }
        _ => Err(USAGE.into()),
    }
}

/// The image at `path`, a regular file of at most `limit` bytes, and with
/// `digest` the one whose SHA-256 it is: what was put there for this install
/// may have been replaced since.
fn read_image(path: &str, limit: u64, digest: Option<&String>) -> Result<Vec<u8>, String> {
    let bytes = read_bounded(Path::new(path), limit)
        .ok_or_else(|| format!("{path}: no regular file of at most {limit} bytes"))?;
    if let Some(want) = digest {
        let found = hex(&sha256(&bytes));
        if !found.eq_ignore_ascii_case(want) {
            return Err(format!("{path}: not the image whose sha256 is {want}"));
        }
    }
    Ok(bytes)
}

/// Every boot loads both bases: together they take at most half of what
/// this machine has, or no entry may load at all.
fn bases_limit() -> u64 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        return u64::MAX;
    }
    u64::from(info.total_pages) * 4096 / 2
}

/// A relayed signal between the two base writes would leave both files one
/// image, so none reaches this process once they begin.
fn shield_writes() {
    for signum in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
        let _ = process::ignore_signal(signum);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest_digest(manifest: &[u8]) -> String {
    hex(&sha256(manifest))
}

/// Whether only uid 0 can have put what is at `path` there.
fn uid0_alone(path: &str) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|meta| !meta.is_symlink() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

/// The digest [`TRIAL_FILE`] holds while a trial runs.
fn trial_replaced() -> Option<String> {
    let path = format!("{TRIAL_DIR}/{TRIAL_FILE}");
    if !uid0_alone(TRIAL_DIR) || !uid0_alone(&path) {
        return None;
    }
    let text = read_bounded(Path::new(&path), 65)?;
    let digest = std::str::from_utf8(&text).ok()?.trim_end();
    (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then(|| digest.into())
}

fn begin_trial(replaced: &str) -> Result<(), String> {
    system_dir(TRIAL_DIR)
        .and_then(|()| fs::set_permissions(TRIAL_DIR, fs::Permissions::from_mode(0o755)))
        .map_err(|e| format!("{TRIAL_DIR}: {e}"))?;
    write_durably(
        TRIAL_DIR,
        TRIAL_FILE,
        format!("{replaced}\n").as_bytes(),
        0o644,
    )
    .map(drop)
}

fn end_trial() {
    match fs::remove_file(format!("{TRIAL_DIR}/{TRIAL_FILE}")) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => eprintln!("bootctl: {TRIAL_DIR}/{TRIAL_FILE}: {e}; the trial stays noted"),
    }
}

/// The base `previous.img`'s rule counts as running, by its manifest's
/// digest: during a trial, the one the trial replaced.
fn last_ran(running: Option<&[u8]>) -> Option<String> {
    running.map(|running| trial_replaced().unwrap_or_else(|| manifest_digest(running)))
}

fn ran(image: &[u8], last: Option<&str>) -> bool {
    last.is_some_and(|last| {
        base::describe(image)
            .manifest
            .is_some_and(|manifest| manifest_digest(manifest) == last)
    })
}

fn running_serves() -> Result<RangeInclusive<u32>, String> {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        return Err("sys_info did not say which bases the running kernel serves".into());
    }
    Ok(info.base_interface_min..=info.base_interface_max)
}

/// Write `image` over `current`, the partition's current base, keeping
/// beside it the base that last ran, `last`, or else the previous one.
/// Refused when a [`Guarded`] kernel would serve neither; any other slot's
/// kernel that would is replaced by the default's first. `spared` takes a
/// new kernel next, so neither holds for it.
fn put_base(
    volume: &mut Volume<BlockFile>,
    loader: &Loader,
    current: Vec<u8>,
    image: &[u8],
    interface: u32,
    last: Option<&str>,
    spared: Option<&str>,
) -> Result<Vec<Guarded>, String> {
    let guarded = guarded_kernels(volume, loader)?;
    if current == image {
        return Ok(guarded);
    }
    let (kept, kept_interface, kept_len) = if ran(&current, last) {
        let (interface, len) = (base::interface(&current), current.len());
        (Some(current), interface, len)
    } else {
        drop(current);
        let previous = read_base(volume, layout::BASE_PREVIOUS)?;
        (None, base::interface(&previous), previous.len())
    };
    if (image.len() + kept_len) as u64 > bases_limit() {
        return Err(
            "the base, with the one kept beside it, takes more than half this machine's \
             memory, and every boot loads both"
                .into(),
        );
    }
    for kernel in guarded
        .iter()
        .filter(|kernel| Some(kernel.slot.as_str()) != spared)
    {
        let served = &kernel.served;
        if !served.contains(&interface) && !kept_interface.is_ok_and(|n| served.contains(&n)) {
            return Err(format!(
                "{}'s kernel serves base interfaces {}-{}; with this base it would \
                 serve neither, so commit a kernel that serves {interface} first",
                kernel.entry,
                served.start(),
                served.end(),
            ));
        }
    }
    let pair = [Some(interface), kept_interface.ok()];
    rescue_stranded(volume, loader, &guarded, pair, spared)?;
    match kept {
        Some(current) => write_bases(volume, &guarded, image, &current)?,
        None => {
            shield_writes();
            write_verified(volume, layout::BASE_CURRENT, image)?;
        }
    }
    Ok(guarded)
}

/// A slot no boot asked for, whose kernel would serve neither of `bases`,
/// takes the default's kernel, so every slot the loader offers still boots.
fn rescue_stranded(
    volume: &mut Volume<BlockFile>,
    loader: &Loader,
    guarded: &[Guarded],
    bases: [Option<u32>; 2],
    spared: Option<&str>,
) -> Result<(), String> {
    let Some(default) = default_kernel(guarded) else {
        return Ok(());
    };
    let Ok(slots) = volume.list(layout::SLOTS_DIR) else {
        return Ok(());
    };
    let kernel_path = |slot: &str| format!("{}/{slot}/{}", layout::SLOTS_DIR, layout::KERNEL_FILE);
    let mut stranded = Vec::new();
    for slot in slots
        .iter()
        .filter(|e| e.is_dir && layout::valid_slot(&e.name))
        .map(|e| e.name.as_str())
    {
        let offered = loader
            .entries
            .iter()
            .any(|entry| entry.strip_prefix(layout::ENTRY_PREFIX) == Some(slot));
        if Some(slot) == spared || guarded.iter().any(|g| g.slot == slot) || !offered {
            continue;
        }
        let Ok(kernel) = volume.read_file(&kernel_path(slot)) else {
            continue;
        };
        let serves = kernel::served_interfaces(&kernel)
            .is_ok_and(|served| bases.iter().flatten().any(|n| served.contains(n)));
        if !serves {
            stranded.push(slot.to_owned());
        }
    }
    if stranded.is_empty() {
        return Ok(());
    }
    let from = kernel_path(&default.slot);
    let copy = volume
        .read_file(&from)
        .map_err(|e| format!("{from}: {e:?}"))?;
    for slot in stranded {
        write_verified(volume, &kernel_path(&slot), &copy)?;
        if loader.here {
            forget_last_boot(&slot);
        }
        println!(
            "slot {slot}'s kernel would serve neither base; copied {}'s kernel there first",
            default.entry
        );
    }
    Ok(())
}

/// `previous` gets `current` only while `current` runs, or ran before a
/// trial, so two bases installed without a boot between them keep the one
/// that last ran, and no [`Guarded`] kernel is left serving neither base.
fn base_install(
    boot: &str,
    named: Option<&str>,
    image_path: &str,
    digest: Option<&String>,
) -> Result<(), String> {
    let image = read_image(image_path, bases_limit(), digest)?;
    let new = base::check(&image, sha256).map_err(|e| format!("{image_path}: {e}"))?;
    let mut volume = open_fat(boot)?;
    let current = read_base(&mut volume, layout::BASE_CURRENT)?;
    let running = running_manifest(boot);
    if current == image {
        let taken = match running.as_deref() {
            Some(manifest) if manifest == new.manifest => {
                end_trial();
                Some("it runs already".to_owned())
            }
            Some(_) => Some(activate(true, &image, new.interface, || {
                Loader::of(named)
                    .and_then(|loader| guarded_kernels(&mut volume, &loader))
                    .map_or_else(|e| e, |guarded| when_taken(&guarded, new.interface))
            })),
            None => None,
        };
        println!(
            "{image_path} is the current base already{}",
            taken.map(|t| format!("; {t}")).unwrap_or_default()
        );
        return Ok(());
    }
    let last = last_ran(running.as_deref());
    let loader = Loader::of(named)?;
    let guarded = put_base(
        &mut volume,
        &loader,
        current,
        &image,
        new.interface,
        last.as_deref(),
        None,
    )?;
    let taken = match running.as_deref() {
        Some(manifest) if manifest == new.manifest => {
            end_trial();
            "it runs already".to_owned()
        }
        _ => activate(running.is_some(), &image, new.interface, || {
            when_taken(&guarded, new.interface)
        }),
    };
    println!(
        "base installed: interface {}, build tag {}; {taken}",
        new.interface,
        new.build_tag.unwrap_or("-"),
    );
    Ok(())
}

/// Run `image` from memory and write nothing, so any reset boots the
/// partition's base again. Given `kernel` too, a base the running kernel
/// does not serve goes to the partition beside the running one instead, and
/// the kernel into the spare slot, armed to boot once: the base rides the
/// kernel's trial, since the default kernel boots the base kept beside it.
fn base_try(
    named: Option<&str>,
    image_path: &str,
    digest: Option<&String>,
    kernel: Option<(&str, Option<&String>)>,
) -> Result<(), String> {
    if let Some(boot) = named
        && !booted_from(boot)
    {
        return Err(format!(
            "base try runs a base on the system that booted, not on {boot}; bootctl --boot \
             {boot} base install writes one there"
        ));
    }
    let image = read_image(image_path, bases_limit(), digest)?;
    let new = base::check(&image, sha256).map_err(|e| format!("{image_path}: {e}"))?;
    let running = running_serves()?;
    if running.contains(&new.interface) {
        try_live(image_path, digest, &image, &new)?;
        if let Some((kernel_path, _)) = kernel {
            println!(
                "{kernel_path} is left alone: the running kernel serves base interface {}",
                new.interface
            );
        }
        return Ok(());
    }
    let unserved = format!(
        "the running kernel serves base interfaces {}-{}, not {}",
        running.start(),
        running.end(),
        new.interface
    );
    let Some((kernel_path, kernel_digest)) = kernel else {
        return Err(format!(
            "{image_path}: {unserved}; --kernel <kernel.elf> names the kernel it needs"
        ));
    };
    let kernel = read_image(
        kernel_path,
        layout::BOOT_BYTES.min(bases_limit()),
        kernel_digest,
    )?;
    let served = served_by(kernel_path, &kernel)?;
    if !served.contains(&new.interface) {
        return Err(format!(
            "{image_path}: {unserved}, and {kernel_path} serves {}-{}; nothing written",
            served.start(),
            served.end()
        ));
    }
    let boot = boot_partition(named)?;
    let mut loader = Loader::of(named)?;
    let slot = spare_slot(&loader)?;
    let entry = format!("{}{slot}", layout::ENTRY_PREFIX);
    loader.offered(&entry)?;
    let kernel = with_words(kernel, loader.here, None)?;
    if loader.one_shot.as_deref() == Some(entry.as_str()) {
        disarm(&entry)?;
        loader.one_shot = None;
    }
    let mut volume = open_fat(&boot)?;
    let current = read_base(&mut volume, layout::BASE_CURRENT)?;
    let last = last_ran(running_manifest(&boot).as_deref());
    put_base(
        &mut volume,
        &loader,
        current,
        &image,
        new.interface,
        last.as_deref(),
        Some(slot),
    )?;
    install_slot(&mut volume, &loader, slot, &kernel)?;
    oneshot(&loader, &entry)?;
    println!(
        "base written for the next boot, interface {}, build tag {}, and slot {slot} given \
         the kernel that serves it: {unserved}, so it takes a reboot; bootctl reboot tries \
         both once, and bootctl commit there keeps them",
        new.interface,
        new.build_tag.unwrap_or("-"),
    );
    Ok(())
}

/// The first `try` of a trial notes the base it replaces, for a commit to
/// keep as the one that last ran and a rollback to run again.
fn try_live(
    image_path: &str,
    digest: Option<&String>,
    image: &[u8],
    new: &base::Checked<'_>,
) -> Result<(), String> {
    let running =
        fs::read(base::MANIFEST_PATH).map_err(|e| format!("{}: {e}", base::MANIFEST_PATH))?;
    if running == new.manifest {
        println!("{image_path} runs already");
        return Ok(());
    }
    let first = trial_replaced().is_none();
    if first {
        begin_trial(&manifest_digest(&running))?;
    }
    let generation = crate::syscall::base::base_activate(image).map_err(|e| {
        if first {
            end_trial();
        }
        format!("{image_path}: it could not run now ({e})")
    })?;
    println!(
        "base tried: interface {}, build tag {}; the system runs it now (generation \
         {generation}), and any reset boots the partition's again",
        new.interface,
        new.build_tag.unwrap_or("-"),
    );
    println!(
        "bootctl base commit {image_path}{} keeps it; bootctl base rollback ends the trial",
        digest.map(|d| format!(" {d}")).unwrap_or_default()
    );
    Ok(())
}

/// Write the base this system runs, as `base install` would; during a trial
/// the base it replaced is the one that last ran.
fn base_commit(
    boot: &str,
    named: Option<&str>,
    image_path: &str,
    digest: Option<&String>,
) -> Result<(), String> {
    let image = read_image(image_path, bases_limit(), digest)?;
    let new = base::check(&image, sha256).map_err(|e| format!("{image_path}: {e}"))?;
    let running = running_manifest(boot).ok_or(
        "commit keeps the base this system runs, and the system did not boot from the \
         partition --boot names",
    )?;
    if running != new.manifest {
        return Err(format!(
            "{image_path}: not the base this system runs; bootctl base try runs it first"
        ));
    }
    let mut volume = open_fat(boot)?;
    let current = read_base(&mut volume, layout::BASE_CURRENT)?;
    if current == image {
        end_trial();
        println!("{image_path} is the current base already");
        return Ok(());
    }
    let last = last_ran(Some(&running));
    let guarded = put_base(
        &mut volume,
        &Loader::of(named)?,
        current,
        &image,
        new.interface,
        last.as_deref(),
        None,
    )?;
    end_trial();
    println!(
        "base committed: interface {}, build tag {}; {}",
        new.interface,
        new.build_tag.unwrap_or("-"),
        when_taken(&guarded, new.interface)
    );
    Ok(())
}

/// Make `image` the base this system runs, when it booted from the
/// partition just written and the running kernel serves `interface`; what
/// happened, with `later` saying when a boot takes it otherwise. A base
/// from the partition that runs ends any trial.
fn activate(
    booted_here: bool,
    image: &[u8],
    interface: u32,
    later: impl FnOnce() -> String,
) -> String {
    if !booted_here || !running_serves().is_ok_and(|served| served.contains(&interface)) {
        return later();
    }
    match crate::syscall::base::base_activate(image) {
        Ok(generation) => {
            end_trial();
            format!("the system runs it now (generation {generation})")
        }
        Err(e) => format!("it could not run now ({e}); {}", later()),
    }
}

/// `current` and `previous` replace both bases, `previous` being the image
/// `current` holds now. Each write is whole but the pair is not: cut between
/// them, one image is on the partition twice, so `previous` is written first
/// only when the default kernel serves it.
fn write_bases(
    volume: &mut Volume<BlockFile>,
    guarded: &[Guarded],
    current: &[u8],
    previous: &[u8],
) -> Result<(), String> {
    let previous_first = default_kernel(guarded)
        .is_none_or(|kernel| base::interface(previous).is_ok_and(|n| kernel.served.contains(&n)));
    let writes = if previous_first {
        [
            (layout::BASE_PREVIOUS, previous),
            (layout::BASE_CURRENT, current),
        ]
    } else {
        [
            (layout::BASE_CURRENT, current),
            (layout::BASE_PREVIOUS, previous),
        ]
    };
    shield_writes();
    for (path, image) in writes {
        write_verified(volume, path, image)?;
    }
    Ok(())
}

/// During a trial, run again the base the trial replaced and write nothing;
/// otherwise swap the two bases.
fn base_rollback(boot: &str, named: Option<&str>) -> Result<(), String> {
    let mut volume = open_fat(boot)?;
    let running = running_manifest(boot);
    if let Some(replaced) = trial_replaced()
        && running
            .as_deref()
            .is_some_and(|running| manifest_digest(running) != replaced)
    {
        return back_from_trial(&mut volume, &replaced);
    }
    let previous = read_base(&mut volume, layout::BASE_PREVIOUS)?;
    let current = read_base(&mut volume, layout::BASE_CURRENT)?;
    if previous == current {
        println!("previous is the current base already; nothing to roll back");
        return Ok(());
    }
    let interface =
        base::interface(&previous).map_err(|e| format!("{}: {e}", layout::BASE_PREVIOUS))?;
    let guarded = guarded_kernels(&mut volume, &Loader::of(named)?)?;
    write_bases(&mut volume, &guarded, &previous, &current)?;
    if running.is_some() {
        end_trial();
    }
    let described = base::describe(&previous);
    let taken = match running {
        Some(running) if described.manifest == Some(&running[..]) => "it runs already".to_owned(),
        running => activate(running.is_some(), &previous, interface, || {
            when_taken(&guarded, interface)
        }),
    };
    println!(
        "rolled back to the base previous held: interface {interface}, build tag {}; {taken}",
        described.build_tag.unwrap_or("-"),
    );
    Ok(())
}

fn back_from_trial(volume: &mut Volume<BlockFile>, replaced: &str) -> Result<(), String> {
    for (which, path) in BASES {
        let image = read_base(volume, path)?;
        if !ran(&image, Some(replaced)) {
            continue;
        }
        let generation = crate::syscall::base::base_activate(&image).map_err(|e| {
            format!(
                "the base {} holds, which the trial replaced, could not run now ({e}); any \
                 reset boots it",
                which.name()
            )
        })?;
        end_trial();
        println!(
            "ended the trial: back to the base {} holds, build tag {}; the system runs it now \
             (generation {generation}), and nothing was written",
            which.name(),
            base::describe(&image).build_tag.unwrap_or("-"),
        );
        return Ok(());
    }
    Err(
        "the base the trial replaced is on the boot partition no more; any reset boots the \
         partition's"
            .into(),
    )
}

/// A boot armed here runs unattended, so it skips Limine's menu and its
/// timeout.
fn oneshot(loader: &Loader, entry: &str) -> Result<(), String> {
    loader.offered(entry)?;
    let armed = loader_var(bli::TIMEOUT_ONE_SHOT)?;
    // The timeout first: left armed alone it skips one menu, where the entry
    // left alone would go through it.
    set_loader_var(bli::TIMEOUT_ONE_SHOT, Some(bli::MENU_DISABLED))?;
    if let Err(e) = set_loader_var(bli::ENTRY_ONE_SHOT, Some(entry)) {
        let _ = set_loader_var(bli::TIMEOUT_ONE_SHOT, armed.as_deref());
        return Err(e);
    }
    println!("next boot: {entry}, once, without the menu");
    Ok(())
}

/// A slot becomes the default only while its kernel serves a base the boot
/// partition holds, since a failed try falls back on it.
fn set_default(
    boot: impl Fn() -> Result<String, String>,
    loader: &Loader,
    entry: &str,
) -> Result<(), String> {
    loader.offered(entry)?;
    if let Some(slot) = entry
        .strip_prefix(layout::ENTRY_PREFIX)
        .filter(|slot| layout::valid_slot(slot))
    {
        let mut volume = open_fat(&boot()?)?;
        let path = format!("{}/{slot}/{}", layout::SLOTS_DIR, layout::KERNEL_FILE);
        let kernel = volume
            .read_file(&path)
            .map_err(|e| format!("{path}: {e:?}"))?;
        check_serves_a_base(&mut volume, &served_by(&path, &kernel)?)?;
    }
    write_default(loader, entry)
}

fn write_default(loader: &Loader, entry: &str) -> Result<(), String> {
    if loader.default.as_deref() != Some(entry) {
        set_loader_var(bli::ENTRY_DEFAULT, Some(entry))?;
    }
    println!("default: {entry}");
    Ok(())
}

/// The first of the layout's slots that does not boot by default: where a
/// new system goes to be tried.
fn spare_slot(loader: &Loader) -> Result<&'static str, String> {
    let default = loader.default_entry()?;
    layout::SLOTS
        .iter()
        .copied()
        .find(|slot| default.and_then(|d| d.strip_prefix(layout::ENTRY_PREFIX)) != Some(*slot))
        .ok_or_else(|| "every slot boots by default".into())
}

fn cmdline_show() -> Result<(), String> {
    let words = machine_words()?;
    println!("{}", if words.is_empty() { "-" } else { &words });
    Ok(())
}

/// The words take effect as a slot's kernel is next written, and go with
/// that slot's trial and commit.
fn cmdline_set(words: &[String]) -> Result<(), String> {
    let words = words
        .iter()
        .flat_map(|word| word.split_ascii_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    kernel::cmdline_fits(&words).map_err(words_error)?;
    no_root(&words)?;
    let (dir, name) = layout::MACHINE_CMDLINE
        .rsplit_once('/')
        .ok_or("the machine's command line names no directory")?;
    write_durably(dir, name, format!("{words}\n").as_bytes(), 0o644)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("{dir}: {e}"))?;
    println!(
        "the machine's words: {}; the next kernel bootctl install or clone writes carries them",
        if words.is_empty() { "-" } else { &words }
    );
    Ok(())
}

#[derive(PartialEq)]
struct LastBoot {
    crashed: bool,
    last_crash: Option<String>,
}

impl LastBoot {
    fn read(slot: &str) -> Option<LastBoot> {
        let state = fs::read_to_string(format!("{SLOT_STATE}/{slot}")).ok()?;
        let mut lines = state.lines();
        let crashed = match lines.next()? {
            "up" => false,
            "crashed" => true,
            _ => return None,
        };
        Some(LastBoot {
            crashed,
            last_crash: lines.next().map(str::to_owned),
        })
    }

    fn write(&self, slot: &str) -> Result<(), String> {
        if LastBoot::read(slot).as_ref() == Some(self) {
            return Ok(());
        }
        let mut state = String::from(if self.crashed { "crashed\n" } else { "up\n" });
        if let Some(record) = &self.last_crash {
            state.push_str(record);
            state.push('\n');
        }
        write_durably(SLOT_STATE, slot, state.as_bytes(), 0o644).map(|_| ())
    }

    fn status_tail(&self) -> String {
        match (self.crashed, &self.last_crash) {
            (true, Some(record)) => format!(", last boot crashed: {record}"),
            (true, None) => ", last boot crashed".to_owned(),
            (false, Some(record)) => format!(", last boot came up, last crash: {record}"),
            (false, None) => ", last boot came up".to_owned(),
        }
    }
}

fn last_boot(slot: &str) -> String {
    LastBoot::read(slot).map_or_else(String::new, |last| last.status_tail())
}

/// `dir` and each parent it lacks, made uid 0's: slot and crash records are
/// the system's whoever runs `bootctl`.
fn system_dir(dir: &str) -> std::io::Result<()> {
    let mut path = std::path::PathBuf::from("/");
    for part in std::path::Path::new(dir).components().skip(1) {
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => std::os::unix::fs::lchown(&path, Some(0), Some(0))?,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// `bytes` at `dir/name` through a sibling written and flushed first, so the
/// name holds all of it or nothing; the rename is made durable too.
fn write_durably(dir: &str, name: &str, bytes: &[u8], mode: u32) -> Result<String, String> {
    system_dir(dir).map_err(|e| format!("{dir}: {e}"))?;
    let temp = format!("{dir}/.{name}.{}", std::process::id());
    let path = format!("{dir}/{name}");
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temp)
        .and_then(|mut file| {
            std::os::unix::fs::fchown(&file, Some(0), Some(0))?;
            file.write_all(bytes).and_then(|()| file.sync_all())
        })
        .and_then(|()| fs::rename(&temp, &path))
        .and_then(|()| File::open(dir)?.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(format!("{path}: {e}"));
    }
    Ok(path)
}

/// `<UTC time>-<slot>`, or what of it the record says.
fn record_stem(summary: Option<&Summary<'_>>, name: &str) -> String {
    let when = summary.and_then(|s| s.time).map_or_else(
        || "unknown-time".to_owned(),
        |secs| {
            let t = utc_from_epoch(secs as i64);
            format!(
                "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
                t.year, t.month, t.day, t.hour, t.minute, t.second
            )
        },
    );
    let slot = summary.and_then(|s| s.slot()).unwrap_or("kernel");
    let torn = if name.ends_with("-torn") { "-torn" } else { "" };
    format!("{when}-{slot}{torn}")
}

fn index_of(name: &str) -> Option<u64> {
    let (index, rest) = name.split_once('-')?;
    if !rest.ends_with(".txt") {
        return None;
    }
    index.parse().ok()
}

/// The records saved under `CRASH_LOG`, oldest first.
fn saved_records() -> Vec<(u64, String)> {
    let mut saved: Vec<(u64, String)> = fs::read_dir(CRASH_LOG)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| Some((index_of(&name)?, name)))
        .collect();
    saved.sort();
    saved
}

/// Taken durably before the record is written, as `savecore` takes its
/// bounds, so a copy that failed never hands its index to another.
fn take_index() -> Result<u64, String> {
    let recorded = fs::read_to_string(format!("{CRASH_LOG}/{CRASH_LOG_BOUNDS}"))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok());
    let past_saved = saved_records().last().map_or(0, |(index, _)| index + 1);
    let index = recorded.unwrap_or(0).max(past_saved);
    let next = format!("{}\n", index + 1);
    write_durably(CRASH_LOG, CRASH_LOG_BOUNDS, next.as_bytes(), 0o644)?;
    Ok(index)
}

/// Save one record, note its slot crashed, and only then erase it: a note
/// that failed does not keep a saved record in the partition.
fn save_record(name: &str) -> Result<(), String> {
    let source = format!("{CRASH_STORE}/{name}");
    let text = fs::read(&source).map_err(|e| format!("{source}: {e}"))?;
    let lossy = String::from_utf8_lossy(&text);
    let summary = Summary::parse(&lossy);
    let file = format!(
        "{:06}-{}.txt",
        take_index()?,
        record_stem(summary.as_ref(), name)
    );
    let saved = write_durably(CRASH_LOG, &file, &text, 0o600)?;
    if let Some(slot) = summary.as_ref().and_then(Summary::slot) {
        let crashed = LastBoot {
            crashed: true,
            last_crash: Some(saved.clone()),
        };
        if let Err(why) = crashed.write(slot) {
            eprintln!("bootctl: {why}");
        }
    }
    fs::remove_file(&source).map_err(|e| format!("{source}: {e}"))?;
    println!(
        "bootctl: crash record {name} saved to {saved}: {}",
        summary.map_or("no summary", |s| s.panic)
    );
    Ok(())
}

/// Whether what is written under `dir` outlives the boot: a RAM root, which
/// a disk that mounted read-only leaves, does not.
fn on_disk(dir: &str) -> bool {
    let Ok(path) = CString::new(dir) else {
        return false;
    };
    crate::syscall::fs::statfs_path(path.as_ptr()).is_ok_and(|stats| {
        stats.f_type == slopos_abi::fs::EXT2_SUPER_MAGIC
            && stats.f_flags & slopos_abi::fs::ST_RDONLY == 0
    })
}

/// Drop the oldest records past [`CRASH_LOG_KEPT`], since a slot that
/// crashes on every boot would fill the root, and what a copy cut short left.
/// A record a slot names as its last crash stays.
fn prune_crash_log() {
    let named: Vec<String> = fs::read_dir(SLOT_STATE)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|slot| LastBoot::read(&slot)?.last_crash)
        .collect();
    let saved: Vec<(u64, String)> = saved_records()
        .into_iter()
        .filter(|(_, name)| !named.iter().any(|path| path.ends_with(name.as_str())))
        .collect();
    let surplus = saved.len().saturating_sub(CRASH_LOG_KEPT);
    let partial = fs::read_dir(CRASH_LOG)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with('.'));
    for name in saved[..surplus]
        .iter()
        .map(|(_, name)| name.clone())
        .chain(partial)
    {
        let _ = fs::remove_file(format!("{CRASH_LOG}/{name}"));
    }
}

/// Move every record the kernel's crash store holds to `CRASH_LOG`, oldest
/// first, then note the slot this boot came from as up. A boot with neither a
/// store nor a slot's state has nothing a note would change, and asks the
/// firmware nothing.
fn collect() -> Result<(), String> {
    let saved = save_records();
    if fs::metadata(CRASH_STORE).is_err() && fs::metadata(SLOT_STATE).is_err() {
        return saved;
    }
    let booted = loader_var(bli::ENTRY_SELECTED).ok().flatten();
    if let Some(slot) = booted
        .as_deref()
        .and_then(|entry| entry.strip_prefix(layout::ENTRY_PREFIX))
        .filter(|slot| layout::valid_slot(slot))
    {
        let last_crash = LastBoot::read(slot).and_then(|last| last.last_crash);
        LastBoot {
            crashed: false,
            last_crash,
        }
        .write(slot)?;
    }
    saved
}

/// Without a store, which a disk with no crash partition or none on an NVMe
/// drive leaves, there is nothing to save; without a disk under `CRASH_LOG`
/// the records stay where they are.
fn save_records() -> Result<(), String> {
    let entries = match fs::read_dir(CRASH_STORE) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{CRASH_STORE}: {e}")),
    };
    system_dir(CRASH_LOG).map_err(|e| format!("{CRASH_LOG}: {e}"))?;
    if !on_disk(CRASH_LOG) {
        return Err(format!(
            "{CRASH_LOG} is on no writable disk; the crash records stay in {CRASH_STORE}"
        ));
    }
    let mut records: Vec<(u64, String)> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| Some((name.split('-').next()?.parse().ok()?, name)))
        .collect();
    records.sort();
    let mut failed = Ok(());
    for (_, name) in &records {
        if let Err(why) = save_record(name) {
            eprintln!("bootctl: {why}");
            failed = Err(format!("crash record {name} not collected"));
        }
    }
    prune_crash_log();
    failed
}

/// The entry that booted runs a base, so it needs no check.
fn commit(loader: &Loader) -> Result<(), String> {
    if !loader.here {
        return Err(
            "commit keeps what this boot booted, and the system did not boot from \
                    the partition --boot names"
                .into(),
        );
    }
    let booted = loader
        .selected
        .clone()
        .ok_or("the boot loader did not report the booted entry")?;
    write_default(loader, &booted)
}

const USAGE: &str = "usage: bootctl [--boot /dev/<node>] <command>
  status                      what booted, what is default, the bases, the slots
  install <slot> <kernel.elf> [<sha256>] [--cmdline <words>]
                              copy a kernel into /boot/<slot>/, with the machine's
                              words and <words> after them
  clone <from> <to>           copy one slot's kernel into another
  base try <base.img> [<sha256>] [--kernel <kernel.elf> [<sha256>]]
                              run <base.img> until the next reset, writing nothing;
                              one the running kernel does not serve goes to the
                              disk instead, and <kernel.elf> into the spare slot
  base commit <base.img> [<sha256>]
                              make the base this system runs the one every slot boots
  base install <base.img> [<sha256>]
                              make <base.img> the base every slot boots
  base rollback               end a trial, or go back to the base that ran before
  cmdline [set <words>...]    the words every kernel installed here starts with
  oneshot <entry>             boot <entry> once, on the next boot only
  set-default <entry>         make <entry> the default
  spare                       the slot a new kernel goes into: not the default
  commit                      make the entry that booted the default
  collect                     save the kernel's crash records under /var/log/crash
  reboot                      restart now";

/// `[<sha256>] [--cmdline <words>]` after an install's kernel.
fn install_tail(words: &[String]) -> Result<(Option<&String>, Option<&str>), String> {
    match words {
        [head @ .., flag, extra] if flag == "--cmdline" => Ok((digest_arg(head)?, Some(extra))),
        _ => Ok((digest_arg(words)?, None)),
    }
}

type KernelArg<'a> = Option<(&'a str, Option<&'a String>)>;

/// `[<sha256>] [--kernel <kernel.elf> [<sha256>]]` after a tried base.
fn try_tail(words: &[String]) -> Result<(Option<&String>, KernelArg<'_>), String> {
    let Some(at) = words.iter().position(|word| word == "--kernel") else {
        return Ok((digest_arg(words)?, None));
    };
    match &words[at + 1..] {
        [kernel, digest @ ..] => Ok((
            digest_arg(&words[..at])?,
            Some((kernel.as_str(), digest_arg(digest)?)),
        )),
        [] => Err(USAGE.into()),
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut rest = args;
    let mut named = None;
    if rest.first().map(String::as_str) == Some("--boot") {
        named = Some(rest.get(1).ok_or(USAGE)?.as_str());
        rest = &rest[2..];
    }
    let boot = || boot_partition(named);
    let loader = || Loader::of(named);
    match rest {
        [cmd] if cmd == "status" => status(&boot()?, &loader()?),
        [cmd, slot, kernel, tail @ ..] if cmd == "install" => {
            let (digest, extra) = install_tail(tail)?;
            install(&boot()?, &loader()?, slot, kernel, digest, extra)
        }
        [cmd, from, to] if cmd == "clone" => clone_slot(&boot()?, &loader()?, from, to),
        [cmd, sub, image, tail @ ..] if cmd == "base" && sub == "try" => {
            let (digest, kernel) = try_tail(tail)?;
            base_try(named, image, digest, kernel)
        }
        [cmd, sub, image, digest @ ..] if cmd == "base" && sub == "commit" => {
            base_commit(&boot()?, named, image, digest_arg(digest)?)
        }
        [cmd, sub, image, digest @ ..] if cmd == "base" && sub == "install" => {
            base_install(&boot()?, named, image, digest_arg(digest)?)
        }
        [cmd, sub] if cmd == "base" && sub == "rollback" => base_rollback(&boot()?, named),
        [cmd] if cmd == "cmdline" => cmdline_show(),
        [cmd, sub, words @ ..] if cmd == "cmdline" && sub == "set" => cmdline_set(words),
        [cmd, entry] if cmd == "oneshot" => oneshot(&loader()?, entry),
        [cmd, entry] if cmd == "set-default" => set_default(boot, &loader()?, entry),
        [cmd] if cmd == "spare" => {
            println!("{}", spare_slot(&loader()?)?);
            Ok(())
        }
        [cmd] if cmd == "commit" => commit(&loader()?),
        [cmd] if cmd == "collect" => collect(),
        [cmd] if cmd == "reboot" => process::reboot(),
        _ => Err(USAGE.into()),
    }
}

/// The action a command line belongs to, `None` for one privd runs nothing
/// for.
fn action_for(args: &[String]) -> Option<&'static str> {
    let words = match args {
        [flag, _, rest @ ..] if flag == "--boot" => rest,
        _ => args,
    };
    Some(match words.first()?.as_str() {
        "base" => match words.get(1)?.as_str() {
            "try" if words.iter().any(|word| word == "--kernel") => "org.slopos.base.pair",
            "try" => "org.slopos.base.try",
            "commit" => "org.slopos.base.commit",
            "install" => "org.slopos.base.install",
            "rollback" => "org.slopos.base.rollback",
            _ => return None,
        },
        "cmdline" if words.get(1).is_some_and(|sub| sub == "set") => "org.slopos.boot.cmdline",
        "status" => "org.slopos.boot.status",
        "install" => "org.slopos.boot.install",
        "clone" => "org.slopos.boot.clone",
        "oneshot" => "org.slopos.boot.oneshot",
        "set-default" => "org.slopos.boot.set-default",
        "spare" => "org.slopos.boot.spare",
        "commit" => "org.slopos.boot.commit",
        "reboot" => "org.slopos.boot.reboot",
        _ => return None,
    })
}

pub fn bootctl_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    privd::elevate(action_for(&args), &privd::own_args(), Input::Closed);
    if let Err(message) = run(&args) {
        eprintln!("bootctl: {message}");
        sys_core::exit_with_code(1);
    }
}
