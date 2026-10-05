//! `/bin/bootctl` — install a system into a boot slot and choose what boots.
//!
//! The boot partition, on the disk the loader was started from, holds per slot
//! a kernel at `/boot/<slot>/kernel.elf` and the base image it boots with at
//! `/boot/<slot>/base.img`; each slot is a Limine entry named `slopos-<slot>`.
//! What boots is chosen through the Boot Loader Interface alone, and nothing
//! on the ESP is written. A new system is tried once, not adopted: `oneshot`
//! sets `LoaderEntryOneShot`, and `LoaderConfigTimeoutOneShot` to skip the
//! menu, which Limine consumes on the next boot, so a reset after a panic
//! boots the default again. `commit`, run once the tried
//! system is up, makes the entry Limine reports as booted
//! (`LoaderEntrySelected`) the default, `LoaderEntryDefault`.
//!
//! Every file write is copy-on-write (`slopos_fat_core`), so no kernel or base
//! is ever half replaced on the medium.
//!
//! `collect`, which init runs on every boot, moves the records the kernel's
//! crash store kept to `/var/log/crash` and notes what each slot's last boot
//! came to under `/var/lib/slopos/slots`, which `status` reports.
//!
//! Holds `TASK_FLAG_MOUNT` for the raw partition and the crash records, and
//! `TASK_FLAG_POWER` for the loader variables and the reboot.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;

use slopos_boot_core::crash::Summary;
use slopos_boot_core::{bli, layout};
use slopos_fat_core::{Error as FatError, Volume};

use crate::apps::coreutils::time::utc_from_epoch;
use crate::boot_disk::{BlockFile, BootDisk, loader_entries, loader_var, open_fat, set_loader_var};
use crate::syscall::core as sys_core;
use crate::syscall::process;

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
const CPIO_NEWC_MAGIC: &[u8] = b"070701";
const CPIO_TRAILER: &[u8] = b"TRAILER!!!\0";

fn boot_partition(named: Option<&str>) -> Result<String, String> {
    match named {
        Some(node) => Ok(node.to_owned()),
        None => BootDisk::find()
            .map(|disk| disk.boot_node)
            .map_err(|e| format!("{e}; pass --boot /dev/<node>")),
    }
}

/// What the loader reported on this boot, and what the system asked of it.
struct Loader {
    entries: Vec<String>,
    selected: Option<String>,
    default: Option<String>,
    one_shot: Option<String>,
}

impl Loader {
    fn read() -> Result<Loader, String> {
        Ok(Loader {
            entries: loader_entries()?,
            selected: loader_var(bli::ENTRY_SELECTED)?,
            default: loader_var(bli::ENTRY_DEFAULT)?,
            one_shot: loader_var(bli::ENTRY_ONE_SHOT)?,
        })
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

fn status(boot: &str) -> Result<(), String> {
    let loader = Loader::read()?;
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
    if let Ok(slots) = volume.list(layout::SLOTS_DIR) {
        for slot in slots.iter().filter(|e| e.is_dir) {
            let mut size = |file: &str| {
                volume
                    .stat(&format!("{}/{}/{file}", layout::SLOTS_DIR, slot.name))
                    .map_or_else(|_| "-".to_owned(), |entry| entry.size.to_string())
            };
            let (kernel, base) = (size(layout::KERNEL_FILE), size(layout::BASE_FILE));
            if kernel != "-" {
                println!(
                    "slot {}: kernel {kernel} bytes, base {base} bytes{}",
                    slot.name,
                    last_boot(&slot.name)
                );
            }
        }
    }
    Ok(())
}

fn is_base_image(image: &[u8]) -> bool {
    let tail = &image[image.len().saturating_sub(512)..];
    image.starts_with(CPIO_NEWC_MAGIC)
        && tail.windows(CPIO_TRAILER.len()).any(|w| w == CPIO_TRAILER)
}

/// The slot's directory, created if need be, once the loader is known to
/// offer it and not to boot it by default.
fn slot_dir(volume: &mut Volume<BlockFile>, loader: &Loader, slot: &str) -> Result<String, String> {
    if !layout::valid_slot(slot) {
        return Err(format!("slot '{slot}': 1-8 lowercase letters or digits"));
    }
    let entry = format!("{}{slot}", layout::ENTRY_PREFIX);
    loader.offered(&entry)?;
    // A slot is written as two files, so the one that boots by default is
    // never the one being written.
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
    slot: &str,
    kernel: &[u8],
    base: &[u8],
) -> Result<(), String> {
    let loader = Loader::read()?;
    let dir = slot_dir(volume, &loader, slot)?;
    let entry = format!("{}{slot}", layout::ENTRY_PREFIX);
    if loader.one_shot.as_deref() == Some(entry.as_str()) {
        set_loader_var(bli::ENTRY_ONE_SHOT, None)?;
        set_loader_var(bli::TIMEOUT_ONE_SHOT, None)?;
        println!("{entry} is no longer armed to boot once");
    }
    write_verified(volume, &format!("{dir}/{}", layout::BASE_FILE), base)?;
    write_verified(volume, &format!("{dir}/{}", layout::KERNEL_FILE), kernel)?;
    forget_last_boot(slot);
    Ok(())
}

fn forget_last_boot(slot: &str) {
    match fs::remove_file(format!("{SLOT_STATE}/{slot}")) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => eprintln!("bootctl: {SLOT_STATE}/{slot}: {e}; its last boot stays noted"),
    }
}

fn install(boot: &str, slot: &str, kernel_path: &str, base_path: &str) -> Result<(), String> {
    let kernel = std::fs::read(kernel_path).map_err(|e| format!("{kernel_path}: {e}"))?;
    if !kernel.starts_with(b"\x7fELF") {
        return Err(format!("{kernel_path}: not an ELF file"));
    }
    let base = std::fs::read(base_path).map_err(|e| format!("{base_path}: {e}"))?;
    if !is_base_image(&base) {
        return Err(format!("{base_path}: not a newc cpio archive"));
    }
    let mut volume = open_fat(boot)?;
    install_slot(&mut volume, slot, &kernel, &base)?;
    println!(
        "installed slot {slot} (entry {}{slot})",
        layout::ENTRY_PREFIX
    );
    Ok(())
}

fn clone_slot(boot: &str, from: &str, to: &str) -> Result<(), String> {
    if !layout::valid_slot(from) {
        return Err(format!("slot '{from}': 1-8 lowercase letters or digits"));
    }
    let mut volume = open_fat(boot)?;
    let mut read = |file: &str| {
        let path = format!("{}/{from}/{file}", layout::SLOTS_DIR);
        volume
            .read_file(&path)
            .map_err(|e| format!("{path}: {e:?}"))
    };
    let (kernel, base) = (read(layout::KERNEL_FILE)?, read(layout::BASE_FILE)?);
    install_slot(&mut volume, to, &kernel, &base)?;
    println!("copied slot {from} to slot {to}");
    Ok(())
}

/// A boot armed here runs unattended, so it skips Limine's menu: the laptop's
/// installed boots that went through the menu unedited hung black, while the
/// live ISO, which has none, always came up (plans/bare-metal.md).
fn oneshot(entry: &str) -> Result<(), String> {
    Loader::read()?.offered(entry)?;
    set_loader_var(bli::ENTRY_ONE_SHOT, Some(entry))?;
    set_loader_var(bli::TIMEOUT_ONE_SHOT, Some(bli::MENU_DISABLED))?;
    println!("next boot: {entry}, once, without the menu");
    Ok(())
}

fn set_default(entry: &str) -> Result<(), String> {
    let loader = Loader::read()?;
    loader.offered(entry)?;
    if loader.default.as_deref() != Some(entry) {
        set_loader_var(bli::ENTRY_DEFAULT, Some(entry))?;
    }
    println!("default: {entry}");
    Ok(())
}

/// The first of the layout's slots that does not boot by default: where a
/// new system goes to be tried.
fn spare() -> Result<(), String> {
    let loader = Loader::read()?;
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

/// `bytes` at `dir/name` through a sibling written and flushed first, so the
/// name holds all of it or nothing; the rename is made durable too.
fn write_durably(dir: &str, name: &str, bytes: &[u8], mode: u32) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    let temp = format!("{dir}/.{name}.{}", std::process::id());
    let path = format!("{dir}/{name}");
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&temp)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
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
    fs::create_dir_all(CRASH_LOG).map_err(|e| format!("{CRASH_LOG}: {e}"))?;
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

fn commit() -> Result<(), String> {
    let booted = loader_var(bli::ENTRY_SELECTED)?
        .ok_or("the boot loader did not report the booted entry")?;
    set_default(&booted)
}

const USAGE: &str = "usage: bootctl [--boot /dev/<node>] <command>
  status                      what booted, what is default, the slots
  install <slot> <kernel.elf> <base.img>
                              copy a kernel and its base into /boot/<slot>/
  clone <from> <to>           copy one slot's kernel and base into another
  oneshot <entry>             boot <entry> once, on the next boot only
  set-default <entry>         make <entry> the default
  spare                       the slot a new system goes into: not the default
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
    match rest {
        [cmd] if cmd == "status" => status(&boot()?),
        [cmd, slot, kernel, base] if cmd == "install" => install(&boot()?, slot, kernel, base),
        [cmd, from, to] if cmd == "clone" => clone_slot(&boot()?, from, to),
        [cmd, entry] if cmd == "oneshot" => oneshot(entry),
        [cmd, entry] if cmd == "set-default" => set_default(entry),
        [cmd] if cmd == "spare" => spare(),
        [cmd] if cmd == "commit" => commit(),
        [cmd] if cmd == "collect" => collect(),
        [cmd] if cmd == "reboot" => process::reboot(),
        _ => Err(USAGE.into()),
    }
}

pub fn bootctl_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(message) = run(&args) {
        eprintln!("bootctl: {message}");
        sys_core::exit_with_code(1);
    }
}
