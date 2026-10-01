//! `/bin/bootctl` — install a system into a boot slot and choose what boots.
//!
//! The boot partition, on the disk the loader was started from, holds per slot
//! a kernel at `/boot/<slot>/kernel.elf` and the base image it boots with at
//! `/boot/<slot>/base.img`; each slot is a Limine entry named `slopos-<slot>`.
//! What boots is chosen through the Boot Loader Interface alone, and nothing
//! on the ESP is written. A new system is tried once, not adopted: `oneshot`
//! sets `LoaderEntryOneShot`, which Limine consumes on the next boot, so a
//! reset after a panic boots the default again. `commit`, run once the tried
//! system is up, makes the entry Limine reports as booted
//! (`LoaderEntrySelected`) the default, `LoaderEntryDefault`.
//!
//! Every file write is copy-on-write (`slopos_fat_core`), so no kernel or base
//! is ever half replaced on the medium.
//!
//! Holds `TASK_FLAG_MOUNT` for the raw partition and `TASK_FLAG_POWER` for the
//! loader variables and the reboot.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use slopos_boot_core::{bli, layout};
use slopos_fat_core::{Device, Error as FatError, Volume};

use crate::boot_disk::{BootDisk, loader_entries, loader_var, set_loader_var};
use crate::syscall::core as sys_core;
use crate::syscall::process;

/// The block size a regular file standing in for a partition is addressed in.
const IMAGE_FILE_BLOCK: u32 = 512;
const CPIO_NEWC_MAGIC: &[u8] = b"070701";
const CPIO_TRAILER: &[u8] = b"TRAILER!!!\0";

struct BlockFile {
    file: File,
    size: u64,
    block: u32,
}

impl Device for BlockFile {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), FatError> {
        self.file
            .read_exact_at(buf, offset)
            .map_err(|_| FatError::Io)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), FatError> {
        self.file
            .write_all_at(buf, offset)
            .map_err(|_| FatError::Io)
    }

    fn flush(&mut self) -> Result<(), FatError> {
        self.file.sync_data().map_err(|_| FatError::Io)
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        self.block
    }
}

fn open_volume(path: &str) -> Result<Volume<BlockFile>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let size = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
    let block = crate::syscall::fs::block_size(file.as_raw_fd()).unwrap_or(IMAGE_FILE_BLOCK);
    Volume::open(BlockFile { file, size, block })
        .map_err(|e| format!("{path}: not a FAT32 volume ({e:?})"))
}

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
    let mut volume = open_volume(boot)?;
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
                    "slot {}: kernel {kernel} bytes, base {base} bytes",
                    slot.name
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
        println!("{entry} is no longer armed to boot once");
    }
    write_verified(volume, &format!("{dir}/{}", layout::BASE_FILE), base)?;
    write_verified(volume, &format!("{dir}/{}", layout::KERNEL_FILE), kernel)
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
    let mut volume = open_volume(boot)?;
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
    let mut volume = open_volume(boot)?;
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

fn oneshot(entry: &str) -> Result<(), String> {
    Loader::read()?.offered(entry)?;
    set_loader_var(bli::ENTRY_ONE_SHOT, Some(entry))?;
    println!("next boot: {entry}, once");
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
