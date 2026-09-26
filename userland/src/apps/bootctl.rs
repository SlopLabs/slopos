//! `/bin/bootctl` — install a kernel into a boot slot and choose what boots.
//!
//! The boot disk's EFI system partition holds Limine, `/limine.conf` and one
//! kernel per slot at `/boot/<slot>/kernel.elf`; each slot is a Limine entry
//! named `slopos-<slot>`. A new kernel is tried once, not adopted:
//! `oneshot` sets the Boot Loader Interface's `LoaderEntryOneShot`, which
//! Limine consumes on the next boot, so a reset after a panic boots the
//! `default_entry` again. `commit`, run once the tried kernel is up, makes the
//! entry Limine reports as booted (`LoaderEntrySelected`) the default.
//!
//! Every file write is copy-on-write (`slopos_fat_core`), so neither a kernel
//! nor the configuration is ever half replaced on the medium.
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
            let kernel = format!("/boot/{}/kernel.elf", slot.name);
            if let Ok(entry) = volume.stat(&kernel) {
                println!("slot {}: {} bytes", slot.name, entry.size);
            }
        }
    }
    Ok(())
}

fn install(esp: &str, slot: &str, kernel_path: &str) -> Result<(), String> {
    if !valid_slot(slot) {
        return Err(format!("slot '{slot}': 1-8 lowercase letters or digits"));
    }
    let kernel = std::fs::read(kernel_path).map_err(|e| format!("{kernel_path}: {e}"))?;
    if !kernel.starts_with(b"\x7fELF") {
        return Err(format!("{kernel_path}: not an ELF file"));
    }
    let mut volume = open_volume(esp)?;
    let config = read_config(&mut volume)?;
    let entry = format!("{ENTRY_PREFIX}{slot}");
    if !has_entry(&config, &entry) {
        return Err(format!("{CONFIG} has no entry /{entry}"));
    }
    let dir = format!("/boot/{slot}");
    match volume.create_dir(&dir) {
        Ok(()) | Err(FatError::Exists) => {}
        Err(e) => return Err(format!("{dir}: {e:?}")),
    }
    let target = format!("{dir}/kernel.elf");
    volume
        .write_file(&target, &kernel)
        .map_err(|e| format!("{target}: {e:?}"))?;
    println!(
        "installed {} bytes as {target} (entry {entry})",
        kernel.len()
    );
    Ok(())
}

fn clone_slot(esp: &str, from: &str, to: &str) -> Result<(), String> {
    if !valid_slot(from) || !valid_slot(to) {
        return Err("slots are 1-8 lowercase letters or digits".into());
    }
    let mut volume = open_volume(esp)?;
    let source = format!("/boot/{from}/kernel.elf");
    let kernel = volume
        .read_file(&source)
        .map_err(|e| format!("{source}: {e:?}"))?;
    let dir = format!("/boot/{to}");
    match volume.create_dir(&dir) {
        Ok(()) | Err(FatError::Exists) => {}
        Err(e) => return Err(format!("{dir}: {e:?}")),
    }
    let target = format!("{dir}/kernel.elf");
    volume
        .write_file(&target, &kernel)
        .map_err(|e| format!("{target}: {e:?}"))?;
    println!("copied {source} to {target}");
    Ok(())
}

fn oneshot(entry: &str) -> Result<(), String> {
    efivar_set(
        "LoaderEntryOneShot",
        &LOADER_GUID,
        // Non-volatile: the reset between here and the loader clears the rest.
        EFI_VARIABLE_NON_VOLATILE | EFI_VARIABLE_BOOTSERVICE_ACCESS | EFI_VARIABLE_RUNTIME_ACCESS,
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
  install <slot> <kernel.elf> copy a kernel into /boot/<slot>/ on the ESP
  clone <from> <to>           copy one slot's kernel into another
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
        [cmd, slot, kernel] if cmd == "install" => install(&esp()?, slot, kernel),
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
