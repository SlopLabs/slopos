//! The disk SlopOS booted from, as the loader names it, and the firmware's
//! entry for that loader.
//!
//! Limine publishes `LoaderDevicePartUUID`, the ESP it was started from. The
//! disk whose GPT lists that partition is the boot disk, and its partition of
//! the SlopOS boot type holds the slots, at the node the kernel made from the
//! same table by the same rule. Reading a whole disk beneath its filesystems
//! takes `TASK_FLAG_MOUNT`.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt};

use slopos_boot_core::gpt::{self, Geometry, Header, Partition};
use slopos_boot_core::limine::Listed;
use slopos_boot_core::load_option::LoadOption;
use slopos_boot_core::variables::{self, BootVariable, Held, PERSISTENT, Placement};
use slopos_boot_core::{Guid, bli, device_path, layout, load_option};
use slopos_fat_core::{Device, Error as FatError, Volume};

use crate::syscall::efi::{efivar_get, efivar_set};
use crate::syscall::error::SyscallError;

fn read_var(name: &str, guid: &Guid) -> Result<Option<Vec<u8>>, SyscallError> {
    let mut buf = vec![0u8; slopos_abi::syscall::EFIVAR_DATA_MAX];
    match efivar_get(name, &guid.0, &mut buf) {
        Ok(n) => {
            buf.truncate(n);
            Ok(Some(buf))
        }
        Err(SyscallError::ENOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

fn read_var_or_say(name: &str, guid: &Guid) -> Result<Option<Vec<u8>>, String> {
    read_var(name, guid).map_err(|e| format!("reading {name}: {e:?}"))
}

fn write_var(name: &str, guid: &Guid, attributes: u32, value: &[u8]) -> Result<(), String> {
    match efivar_set(name, &guid.0, attributes, value) {
        Ok(()) => Ok(()),
        Err(SyscallError::ENOENT) if value.is_empty() => Ok(()),
        Err(e) => Err(format!("writing {name}: {e:?}")),
    }
}

/// Lossy: these variables are machine-wide, so another loader may have
/// written one, and a value that decodes to no entry must still be shown and
/// replaced rather than block the command that replaces it.
fn text_of(string: bli::Utf16<'_>) -> String {
    string
        .chars()
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// A Boot Loader Interface string; `None` when the loader did not set it.
pub fn loader_var(name: &str) -> Result<Option<String>, String> {
    Ok(read_var_or_say(name, &variables::LOADER)?
        .and_then(|raw| bli::strings(&raw).next().map(text_of)))
}

/// Set a Boot Loader Interface string, or delete it.
pub fn set_loader_var(name: &str, value: Option<&str>) -> Result<(), String> {
    let raw: Vec<u8> = value.map(|v| bli::encode(v).collect()).unwrap_or_default();
    write_var(name, &variables::LOADER, PERSISTENT, &raw)
}

/// The entries the loader offered on this boot, in menu order.
pub fn loader_entries() -> Result<Vec<String>, String> {
    let raw = read_var_or_say(bli::ENTRIES, &variables::LOADER)?
        .ok_or("the boot loader published no LoaderEntries")?;
    Ok(bli::strings(&raw).map(text_of).collect())
}

/// The block size a regular file standing in for a partition is addressed in.
const IMAGE_FILE_BLOCK: u32 = 512;

/// A block node, or an image file standing in for one, as `fat-core` reads it.
pub struct BlockFile {
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

impl BlockFile {
    pub fn open(path: &str) -> Result<BlockFile, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("{path}: {e}"))?;
        let size = file.metadata().map_err(|e| format!("{path}: {e}"))?.len();
        let block = crate::syscall::fs::block_size(file.as_raw_fd()).unwrap_or(IMAGE_FILE_BLOCK);
        Ok(BlockFile { file, size, block })
    }
}

/// The FAT32 volume on the block node `path`.
pub fn open_fat(path: &str) -> Result<Volume<BlockFile>, String> {
    Volume::open(BlockFile::open(path)?).map_err(|e| format!("{path}: not a FAT32 volume ({e:?})"))
}

/// A disk's GUID Partition Table, as one of its copies holds it.
pub struct Table {
    pub header: Header,
    pub array: Vec<u8>,
}

impl Table {
    pub fn partitions(&self) -> impl Iterator<Item = Partition> + '_ {
        self.header.partitions(&self.array).filter_map(Result::ok)
    }
}

/// The geometry of the block node `file`.
pub fn geometry_of(file: &File) -> Option<Geometry> {
    let capacity = file.metadata().ok()?.len();
    let block = crate::syscall::fs::block_size(file.as_raw_fd()).ok()?;
    Geometry::new(capacity, u64::from(block))
}

/// The table `node` holds: the primary copy, or the backup where the primary
/// does not read whole; `None` for a disk without one.
pub fn table_of(node: &str) -> Option<Table> {
    let file = File::open(node).ok()?;
    let geometry = geometry_of(&file)?;
    let mut sector = vec![0u8; geometry.block() as usize];
    [gpt::PRIMARY_LBA, geometry.backup_lba()]
        .into_iter()
        .find_map(|lba| {
            file.read_exact_at(&mut sector, geometry.byte_of(lba)?)
                .ok()?;
            let header = Header::parse(&sector, lba, geometry).ok()?;
            let mut array = vec![0u8; header.array_bytes()];
            file.read_exact_at(&mut array, geometry.byte_of(header.entry_lba())?)
                .ok()?;
            header
                .array_matches(&array)
                .then_some(Table { header, array })
        })
}

/// The GPT disk GUID `node` carries, from either header copy, whether or not
/// its table is one [`table_of`] reads.
pub fn disk_guid_of(node: &str) -> Option<Guid> {
    let file = File::open(node).ok()?;
    let geometry = geometry_of(&file)?;
    let mut sector = vec![0u8; geometry.block() as usize];
    [gpt::PRIMARY_LBA, geometry.backup_lba()]
        .into_iter()
        .find_map(|lba| {
            file.read_exact_at(&mut sector, geometry.byte_of(lba)?)
                .ok()?;
            gpt::disk_guid(&sector, lba, geometry).ok()
        })
}

/// The GPT disk GUID of the medium SlopOS booted from, which the medium
/// records; `None` when no medium is served.
pub fn medium_disk_guid() -> Result<Option<Guid>, String> {
    let path = format!("{}/{}", layout::MEDIUM_DIR, layout::MEDIUM_DISK_GUID);
    match std::fs::read_to_string(&path) {
        Ok(text) => Guid::parse(text.trim())
            .map(Some)
            .ok_or_else(|| format!("{path} holds no GUID")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// Why nothing may be installed onto the whole disk `node`: it refuses
/// writes, or it is the medium whose GPT disk GUID is `medium`.
pub fn not_installable(node: &str, medium: Option<Guid>) -> Option<&'static str> {
    let protected = File::open(node)
        .ok()
        .and_then(|file| crate::syscall::fs::read_only(file.as_raw_fd()).ok())
        .unwrap_or(false);
    if protected {
        return Some("write-protected");
    }
    (medium.is_some() && disk_guid_of(node) == medium).then_some("the install medium")
}

/// A node's partitions, if it holds a GPT.
fn partitions_of(node: &str) -> Option<Vec<Partition>> {
    table_of(node).map(|table| table.partitions().collect())
}

/// The whole disks among the block nodes: those no other node names as its
/// disk by the rule [`partition_node`] names partitions with.
pub fn whole_disks() -> Result<Vec<String>, String> {
    let nodes = block_nodes()?;
    let is_partition = |node: &String| {
        nodes.iter().any(|disk| {
            let separator = if disk.ends_with(|c: char| c.is_ascii_digit()) {
                "p"
            } else {
                ""
            };
            node.strip_prefix(disk.as_str())
                .and_then(|rest| rest.strip_prefix(separator))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    Ok(nodes.iter().filter(|n| !is_partition(n)).cloned().collect())
}

fn block_nodes() -> Result<Vec<String>, String> {
    let mut nodes: Vec<String> = std::fs::read_dir("/dev")
        .map_err(|e| format!("/dev: {e}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_block_device()))
        .map(|e| format!("/dev/{}", e.file_name().to_string_lossy()))
        .collect();
    nodes.sort();
    Ok(nodes)
}

/// The node the kernel names `partition` of `disk` with, as Linux does: a `p`
/// between them when the disk's name ends in a digit. Held to the partition's
/// size, so a name that is some other window is refused.
pub fn partition_node(disk: &str, partition: &Partition) -> Result<String, String> {
    let separator = if disk.ends_with(|c: char| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    let node = format!("{disk}{separator}{}", partition.entry.number);
    let size = File::open(&node)
        .and_then(|file| file.metadata())
        .map_err(|e| format!("{node}: {e}"))?
        .len();
    if size != partition.len {
        return Err(format!(
            "{node} holds {size} bytes, not the {} its table entry names",
            partition.len
        ));
    }
    Ok(node)
}

#[derive(Debug)]
pub enum FindError {
    /// The loader named no ESP: not booted through Limine from a GPT disk.
    NoLoaderPartition,
    /// No disk this kernel drives lists the ESP the loader named.
    UnlistedEsp(Guid),
    /// The disk Limine came from carries no SlopOS boot partition.
    NoBootPartition(String),
    Failed(String),
}

impl core::fmt::Display for FindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FindError::NoLoaderPartition => {
                f.write_str("the boot loader named no ESP (LoaderDevicePartUUID)")
            }
            FindError::UnlistedEsp(esp) => write!(f, "no disk here lists the ESP {esp}"),
            FindError::NoBootPartition(disk) => write!(f, "{disk} has no SlopOS boot partition"),
            FindError::Failed(why) => f.write_str(why),
        }
    }
}

/// The disk the loader booted from.
pub struct BootDisk {
    pub esp: Partition,
    /// The boot partition's node.
    pub boot_node: String,
}

impl BootDisk {
    pub fn find() -> Result<BootDisk, FindError> {
        let esp_uuid = loader_var(bli::DEVICE_PART_UUID)
            .map_err(FindError::Failed)?
            .and_then(|text| Guid::parse(&text))
            .ok_or(FindError::NoLoaderPartition)?;
        let mut holders = block_nodes()
            .map_err(FindError::Failed)?
            .into_iter()
            .filter_map(|node| Some((partitions_of(&node)?, node)))
            .filter(|(parts, _)| parts.iter().any(|p| p.entry.unique == esp_uuid));
        let (partitions, disk) = holders.next().ok_or(FindError::UnlistedEsp(esp_uuid))?;
        if let Some((_, other)) = holders.next() {
            return Err(FindError::Failed(format!(
                "the ESP {esp_uuid} is listed by both {disk} and {other}"
            )));
        }
        let esp = *partitions
            .iter()
            .find(|p| p.entry.unique == esp_uuid)
            .ok_or(FindError::UnlistedEsp(esp_uuid))?;
        let mut boots = partitions
            .iter()
            .filter(|p| p.entry.type_guid == layout::BOOT_TYPE);
        let boot = boots
            .next()
            .ok_or_else(|| FindError::NoBootPartition(disk.clone()))?;
        if boots.next().is_some() {
            return Err(FindError::Failed(format!(
                "{disk} carries more than one SlopOS boot partition"
            )));
        }
        let boot_node = partition_node(&disk, boot).map_err(FindError::Failed)?;
        Ok(BootDisk { esp, boot_node })
    }

    /// The ESP as a device path names it.
    pub fn esp_device_path(&self) -> device_path::HardDrive {
        device_path::HardDrive {
            partition_number: self.esp.entry.number,
            start_lba: self.esp.entry.first_lba,
            blocks: self.esp.blocks(),
            partition: self.esp.entry.unique,
        }
    }
}

fn option_name(number: u16) -> String {
    String::from_utf8_lossy(&BootVariable::option_name(number)).into_owned()
}

fn write_boot_option(number: u16, value: &[u8]) -> Result<(), String> {
    write_var(&option_name(number), &variables::GLOBAL, PERSISTENT, value)
}

/// `BootOrder`, empty when the firmware has none.
pub fn boot_order() -> Result<Vec<u16>, String> {
    Ok(read_var_or_say("BootOrder", &variables::GLOBAL)?
        .map(|raw| variables::boot_order(&raw).collect())
        .unwrap_or_default())
}

fn write_boot_order(order: impl Iterator<Item = u16>) -> Result<(), String> {
    let raw: Vec<u8> = order.flat_map(u16::to_le_bytes).collect();
    write_var("BootOrder", &variables::GLOBAL, PERSISTENT, &raw)
}

/// Each option `order` lists, as Limine's `efi_boot_entry` would find it.
pub fn listed_options(order: &[u16]) -> Result<Vec<ListedOption>, String> {
    order
        .iter()
        .map(
            |&number| match read_var(&option_name(number), &variables::GLOBAL) {
                Ok(None) => Ok(ListedOption::Absent),
                Ok(Some(raw)) => Ok(ListedOption::Value(raw)),
                Err(SyscallError::ENOBUFS | SyscallError::EIO) => Ok(ListedOption::Unreadable),
                Err(e) => Err(format!("reading {}: {e:?}", option_name(number))),
            },
        )
        .collect()
}

/// [`Listed`], owning its value.
pub enum ListedOption {
    Absent,
    Unreadable,
    Value(Vec<u8>),
}

impl ListedOption {
    pub fn as_listed(&self) -> Listed<'_> {
        match self {
            ListedOption::Absent => Listed::Absent,
            ListedOption::Unreadable => Listed::Unreadable,
            ListedOption::Value(raw) => Listed::Value(raw),
        }
    }
}

/// The option the firmware booted this time.
pub fn boot_current() -> Result<Option<u16>, String> {
    Ok(read_var_or_say("BootCurrent", &variables::GLOBAL)?
        .and_then(|raw| Some(u16::from_le_bytes(raw.get(..2)?.try_into().ok()?))))
}

/// The load option the firmware starts SlopOS's loader with.
fn loader_option(disk: &BootDisk) -> Result<Vec<u8>, String> {
    let mut path = vec![0u8; device_path::loader_len(layout::LOADER)];
    device_path::loader(&disk.esp_device_path(), layout::LOADER, &mut path)
        .map_err(|e| format!("the loader's device path does not encode: {e:?}"))?;
    let mut option = vec![0u8; load_option::encoded_len(layout::FIRMWARE_ENTRY, &path)];
    load_option::encode(
        load_option::ACTIVE,
        layout::FIRMWARE_ENTRY,
        &path,
        &mut option,
    )
    .map_err(|e| format!("the load option does not encode: {e:?}"))?;
    Ok(option)
}

/// What `Boot<number>` holds for SlopOS's loader on `disk`. One too large or
/// too damaged to read is someone's all the same.
fn held(number: u16, disk: &BootDisk) -> Result<Held, String> {
    Ok(match read_var(&option_name(number), &variables::GLOBAL) {
        Ok(None) => Held::Absent,
        Ok(Some(raw)) => {
            if LoadOption::parse(&raw)
                .is_ok_and(|o| o.starts(&disk.esp.entry.unique, layout::LOADER))
            {
                Held::Ours
            } else {
                Held::Other
            }
        }
        Err(SyscallError::ENOBUFS | SyscallError::EIO) => Held::Other,
        Err(e) => return Err(format!("reading {}: {e:?}", option_name(number))),
    })
}

fn placement(disk: &BootDisk, order: &[u16]) -> Result<Placement, String> {
    variables::place_entry(order, |number| held(number, disk))?
        .ok_or_else(|| "every Boot#### option number is in use".to_owned())
}

/// The firmware's entry for SlopOS's loader on `disk`, if one is registered.
pub fn firmware_entry(disk: &BootDisk) -> Result<Option<u16>, String> {
    Ok(match placement(disk, &boot_order()?)? {
        Placement::Existing(number) => Some(number),
        Placement::New(_) => None,
    })
}

/// Register SlopOS's loader with the firmware, where `variables::place_entry`
/// puts it, bringing an existing entry up to date. It goes first in
/// `BootOrder` when `first`; otherwise it keeps its place, or joins the end.
/// Every other entry keeps its place either way.
pub fn register_firmware_entry(disk: &BootDisk, first: bool) -> Result<u16, String> {
    let option = loader_option(disk)?;
    let order = boot_order()?;
    let number = match placement(disk, &order)? {
        Placement::Existing(number) => number,
        Placement::New(number) => number,
    };
    let written = read_var_or_say(&option_name(number), &variables::GLOBAL)?;
    if written.as_deref() != Some(option.as_slice()) {
        write_boot_option(number, &option)?;
    }
    let wanted: Vec<u16> = variables::place_in_order(&order, number, first).collect();
    if wanted != order {
        write_boot_order(wanted.iter().copied())?;
    }
    let written = read_var_or_say(&option_name(number), &variables::GLOBAL)?;
    if written.as_deref() != Some(option.as_slice()) || boot_order()? != wanted {
        return Err(format!(
            "Boot{number:04X} or BootOrder did not read back as written"
        ));
    }
    Ok(number)
}
