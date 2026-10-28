//! What boots the new system: both kernel slots and the base on the boot
//! partition, Limine and its configuration in `\EFI\SlopOS\` on the ESP, the
//! slot the loader picks, and the firmware's entry for the loader.

use slopos_boot_core::gpt::Partition;
use slopos_boot_core::limine::{self, Config, MenuEntry};
use slopos_boot_core::{Guid, bli, layout};
use slopos_fat_core::{Error as FatError, Volume};

use super::engine::Events;
use super::medium::Medium;
use crate::boot_disk::{
    BlockFile, BootDisk, ListedOption, loader_var, open_fat, register_firmware_entry,
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

/// The medium's kernel into every slot, its base as both the current and the
/// previous base, which every entry loads, and its notices beside them.
pub fn write_boot(boot_node: &str, medium: &Medium, events: &mut dyn Events) -> Result<(), String> {
    let kernel = medium.read(layout::MEDIUM_KERNEL)?;
    let base = medium.read(layout::MEDIUM_BASE)?;
    let mut volume = open_fat(boot_node)?;
    make_dir(&mut volume, layout::SLOTS_DIR)?;
    make_dir(&mut volume, layout::BASE_DIR)?;
    for path in [layout::BASE_CURRENT, layout::BASE_PREVIOUS] {
        write_verified(&mut volume, path, &base)?;
    }
    events.log(format!("base: {} bytes, current and previous", base.len()));
    for slot in layout::SLOTS {
        let dir = format!("{}/{slot}", layout::SLOTS_DIR);
        make_dir(&mut volume, &dir)?;
        write_verified(
            &mut volume,
            &format!("{dir}/{}", layout::KERNEL_FILE),
            &kernel,
        )?;
        events.log(format!("slot {slot}: kernel {} bytes", kernel.len()));
    }
    let notice = medium.read(layout::MEDIUM_NOTICE)?;
    write_verified(
        &mut volume,
        &format!("{}/NOTICE.md", layout::SLOTS_DIR),
        &notice,
    )?;
    Ok(())
}

/// A system other than SlopOS that the firmware's `BootOrder` lists.
pub struct Other {
    /// The title Limine's `efi_boot_entry` would match.
    pub title: String,
    /// The partition its loader is on.
    pub partition: Guid,
}

/// Each other system `options` lists that Limine can boot.
pub fn other_systems(options: &[ListedOption], own_esp: Option<&Guid>) -> Vec<Other> {
    let listed: Vec<_> = options.iter().map(|o| o.as_listed()).collect();
    (0..listed.len())
        .filter_map(|index| {
            let mut buf = [0u8; TITLE_MAX];
            let (title, partition) = limine::offered_title(&listed, index, own_esp, &mut buf)?;
            Some(Other {
                title: title.to_owned(),
                partition,
            })
        })
        .collect()
}

pub struct Menu {
    pub conf: String,
    /// The other systems it offers.
    pub offered: Vec<Other>,
    /// The other systems left out, each colliding with an entry there.
    pub left_out: Vec<String>,
    /// The other systems whose loader the install removes.
    pub removed: Vec<String>,
}

/// The configuration the new disk boots from: both slots on `boot`, each
/// with `root` as its root and `cmdline` after it, then every other system
/// `options` lists, but one an entry already in the menu collides with and
/// one whose loader is on a partition in `gone`, which the install removes.
pub fn menu(
    boot: Guid,
    root: Guid,
    cmdline: &str,
    own_esp: Option<&Guid>,
    options: &[ListedOption],
    gone: &[Guid],
) -> Result<Menu, String> {
    let (removed, others): (Vec<_>, Vec<_>) = other_systems(options, own_esp)
        .into_iter()
        .partition(|other| gone.contains(&other.partition));
    let mut entries: Vec<MenuEntry<'_>> = layout::SLOTS
        .iter()
        .map(|&slot| MenuEntry::Slot { slot, cmdline: "" })
        .collect();
    let mut collides = Vec::new();
    for other in &others {
        let entry = MenuEntry::Firmware {
            title: &other.title,
        };
        let collided = entries.iter().any(|e| limine::collide(e, &entry));
        if !collided {
            entries.push(entry);
        }
        collides.push(collided);
    }
    let mut conf = String::new();
    Config {
        timeout: MENU_TIMEOUT_SECS,
        serial: false,
        boot_partition: boot,
        root_partition: Some(root),
        cmdline,
        resolution: None,
        entries: &entries,
    }
    .render(&mut conf)
    .map_err(|e| format!("limine.conf does not render: {e:?}"))?;
    let (left_out, offered): (Vec<_>, Vec<_>) = others
        .into_iter()
        .zip(collides)
        .partition(|(_, collided)| *collided);
    Ok(Menu {
        conf,
        offered: offered.into_iter().map(|(other, _)| other).collect(),
        left_out: left_out.into_iter().map(|(other, _)| other.title).collect(),
        removed: removed.into_iter().map(|other| other.title).collect(),
    })
}

/// What an ESP's removable-media path holds, which says whose the ESP is.
pub enum Fallback {
    /// No loader; whether nothing else is there either, on a volume carrying
    /// SlopOS's label, as an install cut short leaves it.
    Absent {
        ours: bool,
    },
    Loader {
        limine: bool,
        conf: String,
    },
}

impl Fallback {
    /// Limine is `loader`, or what `\EFI\SlopOS\` holds.
    pub fn read(
        volume: &mut Volume<BlockFile>,
        esp_node: &str,
        loader: &[u8],
    ) -> Result<Fallback, String> {
        let fallback = fat_path(layout::FALLBACK_LOADER);
        match volume.read_file(&fallback) {
            Err(FatError::NotFound) => {
                let dir = &fallback[..fallback.rfind('/').unwrap_or(0)];
                let empty = match volume.list(dir) {
                    Ok(entries) => entries.iter().all(|e| e.name == "." || e.name == ".."),
                    Err(FatError::NotFound) => true,
                    Err(e) => return Err(format!("{dir}: {e:?}")),
                };
                Ok(Fallback::Absent {
                    ours: empty && super::disk::fat_label_is(esp_node, layout::ESP_LABEL),
                })
            }
            Err(e) => Err(format!("{fallback}: {e:?}")),
            Ok(found) => Ok(Fallback::Loader {
                limine: found == loader
                    || volume.read_file(&fat_path(layout::LOADER)).ok() == Some(found),
                conf: String::from_utf8_lossy(
                    &volume
                        .read_file(&fat_path(layout::FALLBACK_CONFIG))
                        .unwrap_or_default(),
                )
                .into_owned(),
            }),
        }
    }

    /// Whether the ESP is SlopOS's own, whose removable-media path an install
    /// writes: Limine there, booting the boot partition `boot`, or nothing on
    /// a volume SlopOS labelled. Anything else is another system's.
    pub fn ours(&self, boot: &Guid) -> bool {
        match self {
            Fallback::Absent { ours } => *ours,
            Fallback::Loader { limine, conf } => *limine && conf.contains(&format!("guid({boot})")),
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
    let notices = medium.read(layout::MEDIUM_LOADER_NOTICES)?;
    let mut volume = open_fat(esp_node)?;
    make_dirs(&mut volume, &fat_path(layout::LOADER_DIR))?;
    write_verified(&mut volume, &fat_path(layout::LOADER), &loader)?;
    write_verified(
        &mut volume,
        &fat_path(layout::LOADER_CONFIG),
        conf.as_bytes(),
    )?;
    write_verified(&mut volume, &fat_path(layout::LOADER_LICENSE), &license)?;
    write_verified(&mut volume, &fat_path(layout::LOADER_NOTICES), &notices)?;
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
        set_loader_var(bli::TIMEOUT_ONE_SHOT, None)?;
    }
    set_loader_var(bli::ENTRY_DEFAULT, Some(&entry))?;
    Ok(entry)
}

/// The firmware entry for the loader on `esp`, first in `BootOrder` if asked.
pub fn register(esp: Partition, boot_node: String, first: bool) -> Result<u16, String> {
    register_firmware_entry(&BootDisk { esp, boot_node }, first)
}
