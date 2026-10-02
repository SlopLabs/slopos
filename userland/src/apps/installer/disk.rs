//! The disk an install goes onto: what it holds, its table written and
//! re-read, and its new partitions formatted.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::process::Command;

use slopos_boot_core::gpt::{self, Geometry, Header, Location, Partition, Piece};
use slopos_boot_core::install::Region;
use slopos_boot_core::layout::{self, BOOT_TYPE, CRASH_TYPE, ESP_TYPE, ROOT_TYPE};
use slopos_boot_core::{Guid, install};

use crate::boot_disk::{BlockFile, Table, geometry_of, partition_node, table_of, whole_disks};
use crate::syscall::error::SyscallError;
use crate::syscall::fs::reread_partitions;

const MKE2FS: &str = "/sbin/mke2fs";
const E2FSCK: &str = "/sbin/e2fsck";
/// What e2fsprogs is told its configuration is: a path in the sealed base,
/// which holds no such file, so the programs take their built-in profile and
/// nothing any process writes can stand in for it. They read the variables
/// naming it with a plain `getenv`, so `AT_SECURE` does not cover them.
const E2FSPROGS_PROFILE: &str = "/usr/share/slopos/e2fsprogs.conf";
/// The log a root carries, as the roots the host builds do.
const ROOT_JOURNAL_MIB: u32 = 64;
const EXT4_MAGIC: u16 = 0xEF53;
const EXT4_MAGIC_AT: u64 = 1024 + 56;
const BOOT_SIGNATURE_AT: usize = 510;
const BOOT_SIGNATURE: [u8; 2] = [0x55, 0xAA];
const MBR_ENTRIES_AT: usize = 446;
const MBR_ENTRY_BYTES: usize = 16;
const MBR_PROTECTIVE_TYPE: u8 = 0xEE;
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const FAT_LABEL_AT: usize = 71;
/// How much of a disk's start must read zero for it to count as blank.
const BLANK_PROBE_BYTES: usize = 1 << 20;

/// What a disk holds.
pub enum Contents {
    /// The GUID partition table [`Disk::table`] is.
    Gpt,
    Blank,
    /// An MBR table's partitions: their index, type byte and size in bytes.
    Mbr(Vec<(usize, u8, u64)>),
    /// A GUID partition table with more entries than SlopOS reads, or damaged
    /// in both copies.
    UnreadableGpt,
    Ext4,
    Data,
}

pub struct Disk {
    pub node: String,
    pub geometry: Geometry,
    pub table: Option<Table>,
    pub contents: Contents,
}

impl Disk {
    /// The whole disk `node`, which a partition's node is not.
    pub fn open(node: &str) -> Result<Disk, String> {
        if !whole_disks()?.iter().any(|disk| disk == node) {
            return Err(format!(
                "{node} is not a disk; SlopOS installs onto a whole disk, one of {}",
                whole_disks()?.join(", ")
            ));
        }
        let file = File::open(node).map_err(|e| format!("{node}: {e}"))?;
        let geometry = geometry_of(&file).ok_or_else(|| format!("{node}: not a disk"))?;
        let (table, contents) = match (table_of(node), unprotected_gpt(&file, geometry)) {
            (Some(_), Some(entries)) => (None, Contents::Mbr(entries)),
            (Some(table), None) => (Some(table), Contents::Gpt),
            (None, _) => (None, contents_of(node, &file, geometry)?),
        };
        Ok(Disk {
            node: node.to_owned(),
            geometry,
            table,
            contents,
        })
    }

    /// What erasing the disk loses, if anything.
    pub fn holds(&self) -> Option<String> {
        match &self.contents {
            Contents::Gpt => Some("a GUID partition table".to_owned()),
            Contents::Blank => None,
            Contents::Mbr(parts) => Some(format!(
                "an MBR partition table with {} partition(s)",
                parts.len()
            )),
            Contents::UnreadableGpt => Some(
                "a GUID partition table this installer cannot read: over 128 entries, or damaged in both copies"
                    .to_owned(),
            ),
            Contents::Ext4 => Some("an ext4 volume across the whole disk".to_owned()),
            Contents::Data => Some("data that starts no partition table".to_owned()),
        }
    }

    pub fn partitions(&self) -> Vec<Partition> {
        self.table
            .as_ref()
            .map(|t| t.partitions().collect())
            .unwrap_or_default()
    }

    pub fn regions(&self) -> Vec<Region> {
        self.table
            .as_ref()
            .map(|t| install::regions(&t.header.grown(), &t.array).collect())
            .unwrap_or_default()
    }

    pub fn describe(&self) -> String {
        let mut out = format!("{} — {}", self.node, human(self.geometry.capacity()));
        match &self.table {
            None => {
                out.push_str(&format!(
                    ", {}\n",
                    self.holds().unwrap_or_else(|| "blank".to_owned())
                ));
                if let Contents::Mbr(parts) = &self.contents {
                    for (index, kind, bytes) in parts {
                        out.push_str(&format!(
                            "  MBR partition {}: type {kind:#04x}, {}\n",
                            index + 1,
                            human(*bytes)
                        ));
                    }
                }
            }
            Some(_) => {
                out.push('\n');
                for p in self.partitions() {
                    out.push_str(&format!(
                        "  partition {}: {}, {} at {}{}\n",
                        p.entry.number,
                        type_name(&p.entry.type_guid),
                        human(p.len),
                        human(p.start),
                        match name_of(&p) {
                            name if name.is_empty() => String::new(),
                            name => format!(", \"{name}\""),
                        }
                    ));
                }
                for (i, r) in self.regions().iter().enumerate() {
                    out.push_str(&format!(
                        "  free region {}: {} at {}\n",
                        i + 1,
                        human(r.blocks() * self.geometry.block()),
                        human(r.first_lba * self.geometry.block())
                    ));
                }
            }
        }
        out
    }

    /// The partition the node `named` is, by its path or its number.
    pub fn partition_named(&self, named: &str) -> Result<Partition, String> {
        let parts = self.partitions();
        named
            .parse::<u32>()
            .ok()
            .and_then(|n| parts.iter().find(|p| p.entry.number == n))
            .or_else(|| {
                parts
                    .iter()
                    .find(|p| partition_node(&self.node, p).is_ok_and(|node| node == named))
            })
            .copied()
            .ok_or_else(|| format!("{named} is no partition of {}", self.node))
    }
}

/// What a partition type GUID names, as `fdisk` would say it.
pub fn type_name(guid: &Guid) -> String {
    const KNOWN: [(&str, &str); 10] = [
        ("0fc63daf-8483-4772-8e79-3d69d8477de4", "Linux filesystem"),
        (
            "4f68bce3-e8cd-4db1-96e7-fbcaf984b709",
            "Linux root (x86-64)",
        ),
        ("933ac7e1-2eb4-4f13-b844-0e14e2aef915", "Linux home"),
        ("0657fd6d-a4ab-43c4-84e5-0933c84b4f4f", "Linux swap"),
        (
            "bc13c2ff-59e6-4262-a352-b275fd6f7172",
            "Linux extended boot",
        ),
        ("e6d6d379-f507-44c2-a23c-238f2a3df928", "Linux LVM"),
        ("a19d880f-05fc-4d3b-a006-743f0f84911e", "Linux RAID"),
        (
            "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7",
            "Microsoft basic data",
        ),
        ("e3c9e316-0b5c-4db8-817d-f92df00215ae", "Microsoft reserved"),
        ("de94bba4-06d1-4d40-a16a-bfd50179d6ac", "Windows recovery"),
    ];
    match *guid {
        ESP_TYPE => "EFI system".to_owned(),
        BOOT_TYPE => "SlopOS boot".to_owned(),
        ROOT_TYPE => "SlopOS root".to_owned(),
        CRASH_TYPE => "SlopOS crash".to_owned(),
        _ => KNOWN
            .iter()
            .find(|(spelling, _)| Guid::parse(spelling) == Some(*guid))
            .map_or_else(|| guid.to_string(), |(_, name)| (*name).to_owned()),
    }
}

pub fn name_of(p: &Partition) -> String {
    char::decode_utf16(p.entry.name.iter().copied().take_while(|&u| u != 0))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn random_bytes() -> Result<[u8; 16], String> {
    let mut bytes = [0u8; 16];
    if crate::syscall::core::getrandom(&mut bytes) != bytes.len() as isize {
        return Err("getrandom gave too few bytes".to_owned());
    }
    Ok(bytes)
}

pub fn random_guid() -> Result<Guid, String> {
    random_bytes().map(Guid::random)
}

/// Write `header` and `array` as the disk's table, each piece behind a flush
/// in [`Header::write_order`]. A fresh table gets its protective MBR first:
/// an erase cut short then leaves a disk only erase installs onto, rather
/// than a GPT behind the old MBR, which other systems take for MBR.
pub fn write_table(disk: &Disk, header: &Header, array: &[u8], fresh: bool) -> Result<(), String> {
    let node = &disk.node;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| format!("{node}: {e}"))?;
    let block = disk.geometry.block() as usize;
    let at = |lba: u64| lba * block as u64;
    let write = |bytes: &[u8], offset: u64| {
        file.write_all_at(bytes, offset)
            .and_then(|()| file.sync_data())
            .map_err(|e| format!("{node}: writing the partition table: {e}"))
    };
    if fresh {
        let mut mbr = [0u8; 512];
        gpt::protective_mbr(disk.geometry, &mut mbr);
        let mut sector = vec![0u8; block];
        sector[..512].copy_from_slice(&mbr);
        write(&sector, 0)?;
    }
    for piece in header.write_order() {
        match piece {
            Piece::Array(copy) => write(array, at(header.array_lba(copy)))?,
            Piece::Header(copy) => {
                let lba = match copy {
                    Location::Primary => gpt::PRIMARY_LBA,
                    Location::Backup => disk.geometry.backup_lba(),
                };
                let mut sector = vec![0u8; block];
                header.encode(copy, &mut sector);
                write(&sector, at(lba))?;
            }
        }
    }
    Ok(())
}

/// Have the kernel read the disk's table again, which it refuses while
/// anything on the disk is mounted or claimed, as every write to the table
/// would be.
pub fn reread(disk: &Disk) -> Result<(), String> {
    let node = &disk.node;
    let file = File::open(node).map_err(|e| format!("{node}: {e}"))?;
    reread_partitions(file.as_raw_fd()).map_err(|e| match e {
        SyscallError::EBUSY => {
            format!("something on {node} is mounted or in use; unmount it first")
        }
        e => format!("{node}: the kernel would not re-read its table ({e:?})"),
    })
}

/// Zero the first MiB of the free run `first_lba..=last_lba`, which a table
/// is about to name as a partition, so one an install cut short starts with
/// no stale volume a later run would take for a real one.
pub fn clear_start(disk: &Disk, first_lba: u64, last_lba: u64) -> Result<(), String> {
    let node = &disk.node;
    let block = disk.geometry.block();
    let len = ((last_lba - first_lba + 1) * block).min(1 << 20);
    let file = OpenOptions::new()
        .write(true)
        .open(node)
        .map_err(|e| format!("{node}: {e}"))?;
    file.write_all_at(&vec![0u8; len as usize], first_lba * block)
        .and_then(|()| file.sync_data())
        .map_err(|e| format!("{node}: clearing a new partition: {e}"))
}

/// Whether the FAT volume `node` holds carries `label` in its boot sector.
pub fn fat_label_is(node: &str, label: &str) -> bool {
    let mut sector = [0u8; 512];
    File::open(node)
        .and_then(|mut f| f.read_exact(&mut sector))
        .is_ok_and(|()| sector[FAT_LABEL_AT..FAT_LABEL_AT + 11] == fat_label(label))
}

/// `label` as a FAT volume label: eleven bytes, space-padded.
fn fat_label(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    for (slot, byte) in out.iter_mut().zip(label.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    out
}

pub fn format_fat(node: &str, label: &str) -> Result<(), String> {
    let mut dev = BlockFile::open(node)?;
    let size = slopos_fat_core::Device::size(&dev);
    let serial = u32::from_le_bytes(random_bytes()?[..4].try_into().unwrap_or_default());
    slopos_fat_core::format(&mut dev, size, &fat_label(label), serial)
        .map_err(|e| format!("{node}: formatting FAT32: {e:?}"))
}

/// Zero the whole partition: a crash partition another system left records in
/// must not read as this one's.
pub fn zero(node: &str) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .open(node)
        .map_err(|e| format!("{node}: {e}"))?;
    let len = file.metadata().map_err(|e| format!("{node}: {e}"))?.len();
    let zeros = vec![0u8; 1 << 20];
    let mut at = 0;
    while at < len {
        let n = (len - at).min(zeros.len() as u64) as usize;
        file.write_all_at(&zeros[..n], at)
            .map_err(|e| format!("{node}: {e}"))?;
        at += n as u64;
    }
    file.sync_data().map_err(|e| format!("{node}: {e}"))
}

/// Whether `node` holds an ext2, ext3 or ext4 volume.
pub fn holds_ext4(node: &str) -> bool {
    let mut magic = [0u8; 2];
    File::open(node)
        .and_then(|mut f| {
            f.seek(SeekFrom::Start(EXT4_MAGIC_AT))?;
            f.read_exact(&mut magic)
        })
        .is_ok_and(|()| u16::from_le_bytes(magic) == EXT4_MAGIC)
}

/// Whether `node` holds no volume anyone else made: its first MiB reads zero,
/// as an install that made the partition left it, or a SlopOS format was cut
/// short there, its boot sector not yet written and its backup, the sixth
/// sector, carrying SlopOS's label.
pub fn holds_no_volume(node: &str) -> bool {
    const BACKUP_BOOT_SECTOR: u64 = 6;
    let Ok(file) = File::open(node) else {
        return false;
    };
    let Some(geometry) = geometry_of(&file) else {
        return false;
    };
    let mut start = vec![0u8; BLANK_PROBE_BYTES.min(geometry.capacity() as usize)];
    if file.read_exact_at(&mut start, 0).is_err() {
        return false;
    }
    if start.iter().all(|&b| b == 0) {
        return true;
    }
    let mut backup = [0u8; 512];
    let signed = |sector: &[u8]| sector[BOOT_SIGNATURE_AT..BOOT_SIGNATURE_AT + 2] == BOOT_SIGNATURE;
    !signed(&start[..512])
        && file
            .read_exact_at(&mut backup, BACKUP_BOOT_SECTOR * geometry.block())
            .is_ok_and(|()| {
                signed(&backup)
                    && backup[FAT_LABEL_AT..FAT_LABEL_AT + 11] == fat_label(layout::ESP_LABEL)
            })
}

/// The partitions an MBR in `sector` lists, if it is one.
fn mbr_entries(sector: &[u8], geometry: Geometry) -> Option<Vec<(usize, u8, u64)>> {
    (sector[BOOT_SIGNATURE_AT..BOOT_SIGNATURE_AT + 2] == BOOT_SIGNATURE).then(|| {
        (0..4)
            .map(|i| &sector[MBR_ENTRIES_AT + i * MBR_ENTRY_BYTES..][..MBR_ENTRY_BYTES])
            .enumerate()
            .filter(|(_, e)| e[4] != 0)
            .map(|(i, e)| {
                let sectors = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
                (i, e[4], u64::from(sectors) * geometry.block())
            })
            .collect()
    })
}

/// An MBR listing partitions in front of a GPT without protecting it, which
/// other systems read rather than the GPT.
fn unprotected_gpt(file: &File, geometry: Geometry) -> Option<Vec<(usize, u8, u64)>> {
    let mut sector = vec![0u8; geometry.block() as usize];
    file.read_exact_at(&mut sector, 0).ok()?;
    mbr_entries(&sector, geometry).filter(|entries| {
        !entries.is_empty()
            && entries
                .iter()
                .all(|&(_, kind, _)| kind != MBR_PROTECTIVE_TYPE)
    })
}

fn contents_of(node: &str, file: &File, geometry: Geometry) -> Result<Contents, String> {
    let read = |offset: u64, len: usize| -> Result<Vec<u8>, String> {
        let mut bytes = vec![0u8; len];
        file.read_exact_at(&mut bytes, offset)
            .map_err(|e| format!("{node}: {e}"))?;
        Ok(bytes)
    };
    let block = geometry.block() as usize;
    let gpt_at = |lba: u64| -> Result<bool, String> {
        Ok(read(lba * geometry.block(), block)?.starts_with(GPT_SIGNATURE))
    };
    if gpt_at(gpt::PRIMARY_LBA)? || gpt_at(geometry.backup_lba())? {
        return Ok(Contents::UnreadableGpt);
    }
    match mbr_entries(&read(0, block)?, geometry) {
        Some(entries)
            if entries
                .iter()
                .any(|&(_, kind, _)| kind == MBR_PROTECTIVE_TYPE) =>
        {
            return Ok(Contents::UnreadableGpt);
        }
        Some(entries) if !entries.is_empty() => return Ok(Contents::Mbr(entries)),
        _ => {}
    }
    if holds_ext4(node) {
        return Ok(Contents::Ext4);
    }
    let probe = BLANK_PROBE_BYTES.min(geometry.capacity() as usize);
    if read(0, probe)?.iter().any(|&b| b != 0) {
        return Ok(Contents::Data);
    }
    Ok(Contents::Blank)
}

/// `program` with nothing of the installer's environment, which would reach
/// it with the raw-device right the installer hands it.
fn e2fsprogs(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .env("MKE2FS_CONFIG", E2FSPROGS_PROFILE)
        .env("E2FSCK_CONFIG", E2FSPROGS_PROFILE)
        .env("E2FSPROGS_UNDO_DIR", "none");
    command
}

fn run(mut command: Command, what: &str) -> Result<(), String> {
    let status = command.status().map_err(|e| format!("{what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} exited {:?}", status.code()))
    }
}

/// Format `node` as a root in the profile every SlopOS volume has.
pub fn mke2fs(node: &str) -> Result<(), String> {
    use slopos_ext4_core::profile;
    let mut command = e2fsprogs(MKE2FS);
    command
        .args([
            "-q",
            "-F",
            "-t",
            "ext4",
            "-O",
            "none",
            "-O",
            profile::features(),
        ])
        .args(["-I", &profile::inode_size().to_string()])
        .args(["-b", &profile::block_size().to_string()])
        .args(["-J", &format!("size={ROOT_JOURNAL_MIB}")])
        .args(["-E", "lazy_itable_init=1"])
        .arg(node);
    run(command, &format!("mke2fs {node}"))
}

/// `e2fsck -fn`: a full check that changes nothing.
pub fn e2fsck(node: &str) -> Result<(), String> {
    let mut command = e2fsprogs(E2FSCK);
    command.args(["-f", "-n", node]);
    run(command, &format!("e2fsck -fn {node}"))
}
