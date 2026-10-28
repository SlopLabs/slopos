//! `/bin/bootctl` — install a kernel into a boot slot, replace the base every
//! slot boots, and choose what boots.
//!
//! The boot partition, on the disk the loader was started from, holds a
//! kernel per slot at `/boot/<slot>/kernel.elf`, each a Limine entry
//! `slopos-<slot>`, and beside them the bases every slot boots,
//! `/boot/base/current.img` and `previous.img`, the one that last ran. A new
//! base runs at once when the running kernel serves it and the system booted
//! from that partition, and at the next boot otherwise.
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
//! the loader variables and the reboot, and `BaseUpdate` to run a base
//! installed; started without them, each
//! command asks privd to run it as its `org.slopos.boot.*` or
//! `org.slopos.base.*` action. `collect` is init's, which starts it holding
//! both.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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
    for (which, path) in BASES {
        let line = match volume.read_file(path) {
            Ok(image) => {
                let described = base::describe(&image);
                let interface = match described.interface {
                    Ok(n) => format!("interface {n}"),
                    Err(e) => e.to_string(),
                };
                let runs = running.is_some() && described.manifest == running.as_deref();
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
            if let Ok(kernel) = volume.stat(&path) {
                let last = if loader.here {
                    last_boot(&slot.name)
                } else {
                    String::new()
                };
                println!("slot {}: kernel {} bytes{last}", slot.name, kernel.size);
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
        set_loader_var(bli::ENTRY_ONE_SHOT, None)?;
        set_loader_var(bli::TIMEOUT_ONE_SHOT, None)?;
        println!("{entry} is no longer armed to boot once");
    }
    write_verified(volume, &format!("{dir}/{}", layout::KERNEL_FILE), kernel)?;
    if loader.here {
        forget_last_boot(slot);
    }
    Ok(())
}

fn forget_last_boot(slot: &str) {
    match fs::remove_file(format!("{SLOT_STATE}/{slot}")) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => eprintln!("bootctl: {SLOT_STATE}/{slot}: {e}; its last boot stays noted"),
    }
}

fn install(
    boot: &str,
    loader: &Loader,
    slot: &str,
    kernel_path: &str,
    digest: Option<&String>,
) -> Result<(), String> {
    let kernel = read_image(kernel_path, layout::BOOT_BYTES.min(bases_limit()), digest)?;
    let served = served_by(kernel_path, &kernel)?;
    let mut volume = open_fat(boot)?;
    check_serves_a_base(&mut volume, &served)?;
    install_slot(&mut volume, loader, slot, &kernel)?;
    println!(
        "installed slot {slot} (entry {}{slot}), serving base interfaces {}-{}",
        layout::ENTRY_PREFIX,
        served.start(),
        served.end()
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
    install_slot(&mut volume, loader, to, &kernel)?;
    println!("copied slot {from}'s kernel to slot {to}");
    Ok(())
}

/// A slot's kernel no base write may leave serving neither base: the
/// default's, what a failed try falls back on, the running one's and the one
/// armed to boot once.
struct Guarded {
    entry: String,
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
        let found: String = sha256(&bytes).iter().map(|b| format!("{b:02x}")).collect();
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

/// `previous` gets `current` only while `current` runs, so two bases installed
/// without a boot between them keep the one that last ran, and no
/// [`Guarded`] kernel is left serving neither base.
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
            Some(manifest) if manifest == new.manifest => Some("it runs already".to_owned()),
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
    let keeps_current =
        running.is_some() && base::describe(&current).manifest == running.as_deref();
    let (kept, kept_len) = if keeps_current {
        (base::interface(&current), current.len())
    } else {
        let previous = read_base(&mut volume, layout::BASE_PREVIOUS)?;
        (base::interface(&previous), previous.len())
    };
    if (image.len() + kept_len) as u64 > bases_limit() {
        return Err(format!(
            "{image_path}: with the base kept beside it, more than half this machine's \
             memory, and every boot loads both"
        ));
    }
    let guarded = guarded_kernels(&mut volume, &Loader::of(named)?)?;
    for kernel in &guarded {
        let served = &kernel.served;
        if !served.contains(&new.interface) && !kept.is_ok_and(|n| served.contains(&n)) {
            return Err(format!(
                "{}'s kernel serves base interfaces {}-{}; with this base it would \
                 serve neither, so commit a kernel that serves {} first",
                kernel.entry,
                served.start(),
                served.end(),
                new.interface
            ));
        }
    }
    if keeps_current {
        write_bases(&mut volume, &guarded, &image, &current)?;
    } else {
        drop(current);
        shield_writes();
        write_verified(&mut volume, layout::BASE_CURRENT, &image)?;
    }
    let taken = match running.as_deref() {
        Some(manifest) if manifest == new.manifest => "it runs already".to_owned(),
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

/// Make `image` the base this system runs, when it booted from the
/// partition just written and the running kernel serves `interface`; what
/// happened, with `later` saying when a boot takes it otherwise.
fn activate(
    booted_here: bool,
    image: &[u8],
    interface: u32,
    later: impl FnOnce() -> String,
) -> String {
    let mut info = UserSysInfo::default();
    let served = sys_core::sys_info(&mut info) == 0
        && (info.base_interface_min..=info.base_interface_max).contains(&interface);
    if !booted_here || !served {
        return later();
    }
    match crate::syscall::base::base_activate(image) {
        Ok(generation) => format!("the system runs it now (generation {generation})"),
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

fn base_rollback(boot: &str, named: Option<&str>) -> Result<(), String> {
    let mut volume = open_fat(boot)?;
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
    let described = base::describe(&previous);
    let taken = match running_manifest(boot) {
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
fn spare(loader: &Loader) -> Result<(), String> {
    let default = loader.default_entry()?;
    let slot = layout::SLOTS
        .iter()
        .find(|slot| default.and_then(|d| d.strip_prefix(layout::ENTRY_PREFIX)) != Some(**slot))
        .ok_or("every slot boots by default")?;
    println!("{slot}");
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
  install <slot> <kernel.elf> [<sha256>]
                              copy a kernel into /boot/<slot>/
  clone <from> <to>           copy one slot's kernel into another
  base install <base.img> [<sha256>]
                              make <base.img> the base every slot boots
  base rollback               go back to the base that ran before
  oneshot <entry>             boot <entry> once, on the next boot only
  set-default <entry>         make <entry> the default
  spare                       the slot a new kernel goes into: not the default
  commit                      make the entry that booted the default
  collect                     save the kernel's crash records under /var/log/crash
  reboot                      restart now";

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
        [cmd, slot, kernel, digest @ ..] if cmd == "install" => {
            install(&boot()?, &loader()?, slot, kernel, digest_arg(digest)?)
        }
        [cmd, from, to] if cmd == "clone" => clone_slot(&boot()?, &loader()?, from, to),
        [cmd, sub, image, digest @ ..] if cmd == "base" && sub == "install" => {
            base_install(&boot()?, named, image, digest_arg(digest)?)
        }
        [cmd, sub] if cmd == "base" && sub == "rollback" => base_rollback(&boot()?, named),
        [cmd, entry] if cmd == "oneshot" => oneshot(&loader()?, entry),
        [cmd, entry] if cmd == "set-default" => set_default(boot, &loader()?, entry),
        [cmd] if cmd == "spare" => spare(&loader()?),
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
            "install" => "org.slopos.base.install",
            "rollback" => "org.slopos.base.rollback",
            _ => return None,
        },
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
