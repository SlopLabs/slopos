//! What boots the new system: both slots on the boot partition, Limine and
//! its configuration in `\EFI\SlopOS\` on the ESP, the slot the loader picks,
//! and the firmware's entry for the loader.

use slopos_boot_core::gpt::Partition;
use slopos_boot_core::limine::{self, Config, MenuEntry};
use slopos_boot_core::{Guid, bli, layout};
use slopos_fat_core::{Error as FatError, Volume};

use super::Medium;
use crate::boot_disk::{
    BlockFile, BootDisk, boot_order, listed_options, loader_var, open_fat, register_firmware_entry,
    set_loader_var,
};

/// Long enough to choose another entry, which a timeout of 0 never shows.
const MENU_TIMEOUT_SECS: u32 = 5;
/// Limine copies an offered title into this many UTF-16 units.
const TITLE_MAX: usize = 128;

/// `\EFI\SlopOS\limine.conf` as a FAT path.
fn fat_path(uefi: &str) -> String {
    uefi.replace('\\', "/")
}

fn make_dirs(volume: &mut Volume<BlockFile>, path: &str) -> Result<(), String> {
    let mut at = 0;
    while let Some(next) = path[at + 1..].find('/') {
        at += next + 1;
        make_dir(volume, &path[..at])?;
    }
    make_dir(volume, path)
}

fn make_dir(volume: &mut Volume<BlockFile>, path: &str) -> Result<(), String> {
    match volume.create_dir(path) {
        Ok(()) | Err(FatError::Exists) => Ok(()),
        Err(e) => Err(format!("{path}: {e:?}")),
    }
}

/// Reads back past the flush, as `bootctl` writes a slot.
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
    Ok(())
}

/// The medium's kernel and base into every slot, and its notices beside them.
pub fn write_slots(boot_node: &str, medium: &Medium) -> Result<(), String> {
    let kernel = medium.read(layout::MEDIUM_KERNEL)?;
    let base = medium.read(layout::MEDIUM_BASE)?;
    let mut volume = open_fat(boot_node)?;
    make_dir(&mut volume, layout::SLOTS_DIR)?;
    for slot in layout::SLOTS {
        let dir = format!("{}/{slot}", layout::SLOTS_DIR);
        make_dir(&mut volume, &dir)?;
        write_verified(&mut volume, &format!("{dir}/{}", layout::BASE_FILE), &base)?;
        write_verified(
            &mut volume,
            &format!("{dir}/{}", layout::KERNEL_FILE),
            &kernel,
        )?;
        println!(
            "installer:   slot {slot}: kernel {} bytes, base {} bytes",
            kernel.len(),
            base.len()
        );
    }
    if medium.has(layout::MEDIUM_NOTICE) {
        let notice = medium.read(layout::MEDIUM_NOTICE)?;
        write_verified(
            &mut volume,
            &format!("{}/NOTICE.md", layout::SLOTS_DIR),
            &notice,
        )?;
    }
    Ok(())
}

/// Each other system the firmware's `BootOrder` lists that Limine can boot
/// through `efi_boot_entry`, by the title it would match.
fn other_systems(own_esp: Option<&Guid>) -> Result<Vec<String>, String> {
    let order = boot_order()?;
    let options = listed_options(&order)?;
    let listed: Vec<_> = options.iter().map(|o| o.as_listed()).collect();
    let mut titles = Vec::new();
    for index in 0..listed.len() {
        let mut buf = [0u8; TITLE_MAX];
        if let Some(title) = limine::offered_title(&listed, index, own_esp, &mut buf) {
            titles.push(title.to_owned());
        }
    }
    Ok(titles)
}

/// The configuration the new disk boots from: both slots on `boot`, each
/// with `root` as its root and `cmdline` after it, then every other system
/// the firmware lists, but one an entry already in the menu collides with.
pub fn limine_conf(
    boot: Guid,
    root: Guid,
    cmdline: &str,
    own_esp: Option<&Guid>,
) -> Result<String, String> {
    let titles = other_systems(own_esp)?;
    let mut entries: Vec<MenuEntry<'_>> = layout::SLOTS
        .iter()
        .map(|&slot| MenuEntry::Slot { slot, cmdline: "" })
        .collect();
    for title in &titles {
        let entry = MenuEntry::Firmware { title };
        if entries.iter().any(|e| limine::collide(e, &entry)) {
            println!(
                "installer:   leaving \"{title}\" out of the menu: an entry there collides with it"
            );
            continue;
        }
        entries.push(entry);
    }
    let mut text = String::new();
    Config {
        timeout: MENU_TIMEOUT_SECS,
        serial: false,
        boot_partition: boot,
        root_partition: Some(root),
        cmdline,
        resolution: None,
        entries: &entries,
    }
    .render(&mut text)
    .map_err(|e| format!("limine.conf does not render: {e:?}"))?;
    Ok(text)
}

/// Whether the ESP is SlopOS's own, whose removable-media path an install
/// writes: that path holds Limine, as `\EFI\SlopOS\` or the medium carries
/// it, with a configuration that boots this disk's boot partition `boot`; or
/// nothing at all on a volume carrying SlopOS's label, as an install cut short
/// leaves it. Anything else there is another system's.
pub fn esp_is_ours(esp_node: &str, boot: &Guid, medium: &Medium) -> Result<bool, String> {
    let mut volume = open_fat(esp_node)?;
    let fallback = fat_path(layout::FALLBACK_LOADER);
    match volume.read_file(&fallback) {
        Err(FatError::NotFound) => {
            let dir = &fallback[..fallback.rfind('/').unwrap_or(0)];
            let empty = match volume.list(dir) {
                Ok(entries) => entries.iter().all(|e| e.name == "." || e.name == ".."),
                Err(FatError::NotFound) => true,
                Err(e) => return Err(format!("{dir}: {e:?}")),
            };
            Ok(empty && super::disk::fat_label_is(esp_node, layout::ESP_LABEL))
        }
        Err(e) => Err(format!("{fallback}: {e:?}")),
        Ok(loader) => {
            let limine = loader == medium.read(layout::MEDIUM_LOADER)?
                || volume.read_file(&fat_path(layout::LOADER)).ok() == Some(loader);
            let conf = volume
                .read_file(&fat_path(layout::FALLBACK_CONFIG))
                .unwrap_or_default();
            let boots_this_disk = String::from_utf8_lossy(&conf).contains(&format!("guid({boot})"));
            Ok(limine && boots_this_disk)
        }
    }
}

/// Limine and `conf` into `\EFI\SlopOS\`, and at the removable-media path on
/// an ESP SlopOS created.
pub fn write_loader(
    esp_node: &str,
    own_esp: bool,
    conf: &str,
    medium: &Medium,
) -> Result<(), String> {
    let loader = medium.read(layout::MEDIUM_LOADER)?;
    let license = medium.read(layout::MEDIUM_LOADER_LICENSE)?;
    let mut volume = open_fat(esp_node)?;
    make_dirs(&mut volume, &fat_path(layout::LOADER_DIR))?;
    write_verified(&mut volume, &fat_path(layout::LOADER), &loader)?;
    write_verified(
        &mut volume,
        &fat_path(layout::LOADER_CONFIG),
        conf.as_bytes(),
    )?;
    write_verified(
        &mut volume,
        &format!("{}/LICENSE.limine", fat_path(layout::LOADER_DIR)),
        &license,
    )?;
    if own_esp {
        let fallback = fat_path(layout::FALLBACK_LOADER);
        make_dirs(&mut volume, &fallback[..fallback.rfind('/').unwrap_or(0)])?;
        write_verified(&mut volume, &fallback, &loader)?;
        write_verified(
            &mut volume,
            &fat_path(layout::FALLBACK_CONFIG),
            conf.as_bytes(),
        )?;
    }
    Ok(())
}

/// Slot a boots by default, and nothing armed for a SlopOS entry outlives the
/// system it was armed for.
pub fn set_default() -> Result<String, String> {
    let entry = format!("{}{}", layout::ENTRY_PREFIX, layout::SLOTS[0]);
    if loader_var(bli::ENTRY_ONE_SHOT)?.is_some_and(|armed| armed.starts_with(layout::ENTRY_PREFIX))
    {
        set_loader_var(bli::ENTRY_ONE_SHOT, None)?;
    }
    set_loader_var(bli::ENTRY_DEFAULT, Some(&entry))?;
    Ok(entry)
}

/// The firmware entry for the loader on `esp`, first in `BootOrder` if asked.
pub fn register(esp: Partition, boot_node: String, first: bool) -> Result<u16, String> {
    register_firmware_entry(&BootDisk { esp, boot_node }, first)
}
