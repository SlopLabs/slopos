//! `/bin/bootctl` — install a system into a boot slot and choose what boots.
//!
//! The boot disk's EFI system partition holds Limine, `/limine.conf` and, per
//! slot, a kernel at `/boot/<slot>/kernel.elf` and the base image it boots
//! with at `/boot/<slot>/base.img`; each slot is a Limine entry named
//! `slopos-<slot>`. A new system is tried once, not adopted: `oneshot` sets the
//! Boot Loader Interface's `LoaderEntryOneShot`, which Limine consumes on the
//! next boot, so a reset after a panic boots the `default_entry` again.
//! `commit`, run once the tried system is up, makes the entry Limine reports
//! as booted (`LoaderEntrySelected`) the default.
//!
//! Every file write is copy-on-write (`slopos_fat_core`), so no kernel, base
//! or configuration is ever half replaced on the medium.
//!
//! Holds `TASK_FLAG_MOUNT` for the raw partition and `TASK_FLAG_POWER` for the
//! loader variables and the reboot.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;

use slopos_fat_core::{Device, Error as FatError, Volume};

use crate::syscall::core as sys_core;
use crate::syscall::efi::{
    LOADER_GUID, efivar_get, efivar_set, loader_string, loader_string_value,
};
use crate::syscall::numbers::{
    EFI_VARIABLE_BOOTSERVICE_ACCESS, EFI_VARIABLE_NON_VOLATILE, EFI_VARIABLE_RUNTIME_ACCESS,
};
use crate::syscall::process;

const CONFIG: &str = "/limine.conf";
const ENTRY_PREFIX: &str = "slopos-";
const KERNEL: &str = "kernel.elf";
const BASE: &str = "base.img";
const CPIO_NEWC_MAGIC: &[u8] = b"070701";
const CPIO_TRAILER: &[u8] = b"TRAILER!!!\0";
/// Non-volatile, because the reset between arming and the loader clears a volatile variable.
const ONE_SHOT_ATTRIBUTES: u32 =
    EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS;

struct BlockFile {
    file: File,
    size: u64,
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
}

fn open_volume(path: &str) -> Result<Volume<BlockFile>, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("{path}: {e}"))?;
    let size = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
    Volume::open(BlockFile { file, size })
        .map_err(|e| format!("{path}: not a FAT32 volume ({e:?})"))
}

/// The first block node holding a FAT32 volume with a Limine configuration.
fn find_esp() -> Result<String, String> {
    let mut names: Vec<String> = std::fs::read_dir("/dev")
        .map_err(|e| format!("/dev: {e}"))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("vd"))
        .collect();
    names.sort();
    for name in names {
        let path = format!("/dev/{name}");
        if let Ok(mut volume) = open_volume(&path)
            && volume.stat(CONFIG).is_ok()
        {
            return Ok(path);
        }
    }
    Err("no EFI system partition with /limine.conf found; pass --esp /dev/<node>".into())
}

fn loader_var(name: &str) -> Result<Option<String>, String> {
    let mut buf = [0u8; 512];
    match efivar_get(name, &LOADER_GUID, &mut buf) {
        Ok(n) => Ok(loader_string_value(&buf[..n])),
        Err(e) if e == crate::syscall::error::SyscallError::ENOENT => Ok(None),
        Err(e) => Err(format!("reading {name}: {e:?}")),
    }
}

/// `default_entry` as the configuration names it.
fn default_entry(config: &str) -> Option<&str> {
    config
        .lines()
        .find_map(|l| l.trim().strip_prefix("default_entry:"))
        .map(str::trim)
}

/// `config` with `default_entry` set to `entry`, every other line kept.
fn with_default_entry(config: &str, entry: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    for line in config.lines() {
        if !replaced && line.trim().starts_with("default_entry:") {
            out.push_str(&format!("default_entry: {entry}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out = format!("default_entry: {entry}\n{out}");
    }
    out
}

fn has_entry(config: &str, entry: &str) -> bool {
    config
        .lines()
        .any(|l| l.trim_start().strip_prefix('/').map(str::trim) == Some(entry))
}

fn valid_slot(slot: &str) -> bool {
    (1..=8).contains(&slot.len())
        && slot
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn read_config(volume: &mut Volume<BlockFile>) -> Result<String, String> {
    let raw = volume
        .read_file(CONFIG)
        .map_err(|e| format!("{CONFIG}: {e:?}"))?;
    String::from_utf8(raw).map_err(|_| format!("{CONFIG} is not UTF-8"))
}

fn set_default(volume: &mut Volume<BlockFile>, entry: &str) -> Result<(), String> {
    let config = read_config(volume)?;
    if !has_entry(&config, entry) {
        return Err(format!("{CONFIG} has no entry /{entry}"));
    }
    if default_entry(&config) == Some(entry) {
        return Ok(());
    }
    volume
        .write_file(CONFIG, with_default_entry(&config, entry).as_bytes())
        .map_err(|e| format!("{CONFIG}: {e:?}"))
}

fn status(esp: &str) -> Result<(), String> {
    let mut volume = open_volume(esp)?;
    let config = read_config(&mut volume)?;
    println!("esp: {esp}");
    println!(
        "booted: {}",
        loader_var("LoaderEntrySelected")?.unwrap_or_else(|| "-".into())
    );
    println!("default: {}", default_entry(&config).unwrap_or("-"));
    println!(
        "oneshot: {}",
        loader_var("LoaderEntryOneShot")?.unwrap_or_else(|| "-".into())
    );
    if let Ok(slots) = volume.list("/boot") {
        for slot in slots.iter().filter(|e| e.is_dir) {
            let mut size = |file: &str| {
                volume
                    .stat(&format!("/boot/{}/{file}", slot.name))
                    .map_or_else(|_| "-".to_owned(), |entry| entry.size.to_string())
            };
            let (kernel, base) = (size(KERNEL), size(BASE));
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

/// The slot's directory, created if need be, once `/limine.conf` is known to
/// boot it and not by default.
fn slot_dir(volume: &mut Volume<BlockFile>, slot: &str) -> Result<String, String> {
    if !valid_slot(slot) {
        return Err(format!("slot '{slot}': 1-8 lowercase letters or digits"));
    }
    let entry = format!("{ENTRY_PREFIX}{slot}");
    let config = read_config(volume)?;
    if !has_entry(&config, &entry) {
        return Err(format!("{CONFIG} has no entry /{entry}"));
    }
    // A slot is written as two files, so the one that boots by default is
    // never the one being written.
    if default_entry(&config) == Some(entry.as_str()) {
        return Err(format!(
            "{entry} is the default; install into another slot and try it"
        ));
    }
    let dir = format!("/boot/{slot}");
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
    let dir = slot_dir(volume, slot)?;
    disarm_one_shot(&format!("{ENTRY_PREFIX}{slot}"))?;
    write_verified(volume, &format!("{dir}/{BASE}"), base)?;
    write_verified(volume, &format!("{dir}/{KERNEL}"), kernel)
}

fn disarm_one_shot(entry: &str) -> Result<(), String> {
    if loader_var("LoaderEntryOneShot")?.as_deref() != Some(entry) {
        return Ok(());
    }
    efivar_set("LoaderEntryOneShot", &LOADER_GUID, ONE_SHOT_ATTRIBUTES, &[])
        .map_err(|e| format!("clearing LoaderEntryOneShot: {e:?}"))?;
    println!("{entry} is no longer armed to boot once");
    Ok(())
}

fn install(esp: &str, slot: &str, kernel_path: &str, base_path: &str) -> Result<(), String> {
    let kernel = std::fs::read(kernel_path).map_err(|e| format!("{kernel_path}: {e}"))?;
    if !kernel.starts_with(b"\x7fELF") {
        return Err(format!("{kernel_path}: not an ELF file"));
    }
    let base = std::fs::read(base_path).map_err(|e| format!("{base_path}: {e}"))?;
    if !is_base_image(&base) {
        return Err(format!("{base_path}: not a newc cpio archive"));
    }
    let mut volume = open_volume(esp)?;
    install_slot(&mut volume, slot, &kernel, &base)?;
    println!("installed slot {slot} (entry {ENTRY_PREFIX}{slot})");
    Ok(())
}

fn clone_slot(esp: &str, from: &str, to: &str) -> Result<(), String> {
    if !valid_slot(from) {
        return Err(format!("slot '{from}': 1-8 lowercase letters or digits"));
    }
    let mut volume = open_volume(esp)?;
    let mut read = |file: &str| {
        let path = format!("/boot/{from}/{file}");
        volume
            .read_file(&path)
            .map_err(|e| format!("{path}: {e:?}"))
    };
    let (kernel, base) = (read(KERNEL)?, read(BASE)?);
    install_slot(&mut volume, to, &kernel, &base)?;
    println!("copied slot {from} to slot {to}");
    Ok(())
}

fn oneshot(entry: &str) -> Result<(), String> {
    efivar_set(
        "LoaderEntryOneShot",
        &LOADER_GUID,
        ONE_SHOT_ATTRIBUTES,
        &loader_string(entry),
    )
    .map_err(|e| format!("setting LoaderEntryOneShot: {e:?}"))?;
    println!("next boot: {entry}, once");
    Ok(())
}

fn commit(esp: &str) -> Result<(), String> {
    let booted = loader_var("LoaderEntrySelected")?
        .ok_or("the boot loader did not report the booted entry")?;
    let mut volume = open_volume(esp)?;
    set_default(&mut volume, &booted)?;
    println!("default: {booted}");
    Ok(())
}

const USAGE: &str = "usage: bootctl [--esp /dev/<node>] <command>
  status                      what booted, what is default, the slots
  install <slot> <kernel.elf> <base.img>
                              copy a kernel and its base into /boot/<slot>/
  clone <from> <to>           copy one slot's kernel and base into another
  oneshot <entry>             boot <entry> once, on the next boot only
  set-default <entry>         make <entry> the default
  commit                      make the entry that booted the default
  reboot                      restart now";

fn run(args: &[String]) -> Result<(), String> {
    let mut rest = args;
    let mut esp = None;
    if rest.first().map(String::as_str) == Some("--esp") {
        esp = Some(rest.get(1).ok_or(USAGE)?.clone());
        rest = &rest[2..];
    }
    let esp = || esp.clone().map_or_else(find_esp, Ok);
    match rest {
        [cmd] if cmd == "status" => status(&esp()?),
        [cmd, slot, kernel, base] if cmd == "install" => install(&esp()?, slot, kernel, base),
        [cmd, from, to] if cmd == "clone" => clone_slot(&esp()?, from, to),
        [cmd, entry] if cmd == "oneshot" => oneshot(entry),
        [cmd, entry] if cmd == "set-default" => {
            let mut volume = open_volume(&esp()?)?;
            set_default(&mut volume, entry)
        }
        [cmd] if cmd == "commit" => commit(&esp()?),
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
