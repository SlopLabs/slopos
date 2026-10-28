//! The disk an install goes onto: what it holds, its table written and
//! re-read, and its new partitions formatted.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use slopos_abi::caps::RAW_BLOCK;
use slopos_abi::fs::O_RDONLY;
use slopos_abi::syscall::O_CLOEXEC;
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};

use slopos_boot_core::disk_id::{self, Bus};
use slopos_boot_core::gpt::{self, Geometry, Header, Location, Partition, Piece};
use slopos_boot_core::install::Region;
use slopos_boot_core::layout;
use slopos_boot_core::{Guid, install};
use slopos_installer_core::{
    Contents, Layout, MbrPartition, NoTable, Refusal, disk_size, name_of, short, type_name,
};

use super::engine::Events;
use crate::boot_disk::{
    BlockFile, Table, disk_guid_of, geometry_of, partition_node, table_of, write_protected,
};
use crate::syscall::error::SyscallError;
use crate::syscall::fs::{open_cstr, pipe2, read_slice, reread_partitions};
use crate::syscall::process::{self, Endowment, WaitStatus};

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
const BY_ID: &str = "/dev/disk/by-id";

/// Why the whole disk `node` is never offered: it refuses writes, or it is
/// the medium whose GPT disk GUID is `medium`.
pub fn not_offered(node: &str, medium: Option<Guid>) -> Option<Refusal> {
    if write_protected(node) {
        return Some(Refusal::ReadOnly);
    }
    (medium.is_some() && disk_guid_of(node) == medium).then_some(Refusal::Medium)
}

/// What `/dev/disk/by-id` says of the whole disk `node`: its bus and what
/// its maker calls it.
pub fn by_id(node: &str) -> (Option<Bus>, Option<String>) {
    let target = format!("../../{}", short(node));
    let names: Vec<String> = std::fs::read_dir(BY_ID)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            std::fs::read_link(entry.path()).is_ok_and(|link| link.as_os_str() == target.as_str())
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let Some(described) = disk_id::describe(names.iter().map(String::as_str)) else {
        return (None, None);
    };
    let words: Vec<&str> = described.model().collect();
    (
        Some(described.bus),
        (!words.is_empty()).then(|| words.join(" ")),
    )
}

#[derive(Clone, PartialEq)]
pub struct Disk {
    pub node: String,
    pub geometry: Geometry,
    /// Its GPT, or what it holds without one.
    pub table: Result<Table, NoTable>,
}

impl Disk {
    pub fn open(node: &str) -> Result<Disk, String> {
        let file = File::open(node).map_err(|e| format!("{node}: {e}"))?;
        let geometry = geometry_of(&file).ok_or_else(|| format!("{node}: not a disk"))?;
        let table = match (table_of(node), unprotected_gpt(&file, geometry)) {
            (Some(_), Some(entries)) => Err(NoTable::Mbr(entries)),
            (Some(table), None) => Ok(table),
            (None, _) => Err(no_table(node, &file, geometry)?),
        };
        Ok(Disk {
            node: node.to_owned(),
            geometry,
            table,
        })
    }

    pub fn contents(&self) -> Contents {
        match &self.table {
            Ok(_) => Contents::Gpt,
            Err(held) => Contents::NoTable(held.clone()),
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

    /// The region a free-space install takes when none is named.
    pub fn largest_region(&self) -> Option<usize> {
        let table = self.table.as_ref().ok()?;
        install::largest_region(&table.header.grown(), &table.array)
    }

    pub fn layout(&self) -> Layout {
        match &self.table {
            Ok(t) => Layout::of(&self.node, &t.header.grown(), &t.array),
            Err(_) => Layout::empty(self.geometry.capacity()),
        }
    }

    /// The disk as the CLI lists it: its node, its maker's name and its
    /// size, then what it holds.
    pub fn describe(&self, model: Option<&str>) -> String {
        let size = disk_size(self.geometry.capacity());
        let mut out = match model {
            Some(model) => format!("{} — {model}, {size}", self.node),
            None => format!("{} — {size}", self.node),
        };
        match &self.table {
            Err(held) => {
                out.push_str(&format!(
                    ", {}\n",
                    held.holds().unwrap_or_else(|| "empty".to_owned())
                ));
                if let NoTable::Mbr(parts) = held {
                    for part in parts {
                        out.push_str(&format!(
                            "  MBR partition {}: type {:#04x}, {}\n",
                            part.index + 1,
                            part.kind,
                            disk_size(part.bytes)
                        ));
                    }
                }
            }
            Ok(_) => {
                out.push('\n');
                for p in self.partitions() {
                    let slopos = install::ROLES
                        .iter()
                        .any(|role| role.type_guid() == p.entry.type_guid);
                    out.push_str(&format!(
                        "  partition {}: {}, {} at {}{}\n",
                        p.entry.number,
                        type_name(&p.entry.type_guid),
                        disk_size(p.len),
                        disk_size(p.start),
                        match name_of(&p) {
                            name if slopos || name.is_empty() => String::new(),
                            name => format!(", “{name}”"),
                        }
                    ));
                }
                for (i, r) in self.regions().iter().enumerate() {
                    out.push_str(&format!(
                        "  free space {}: {} at {}\n",
                        i + 1,
                        disk_size(r.blocks() * self.geometry.block()),
                        disk_size(r.first_lba * self.geometry.block())
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
pub fn reread(node: &str) -> Result<(), String> {
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
fn mbr_entries(sector: &[u8], geometry: Geometry) -> Option<Vec<MbrPartition>> {
    (sector[BOOT_SIGNATURE_AT..BOOT_SIGNATURE_AT + 2] == BOOT_SIGNATURE).then(|| {
        (0..4u8)
            .map(|i| {
                let e = &sector[MBR_ENTRIES_AT + usize::from(i) * MBR_ENTRY_BYTES..];
                let sectors = u32::from_le_bytes([e[12], e[13], e[14], e[15]]);
                MbrPartition {
                    index: i,
                    kind: e[4],
                    bytes: u64::from(sectors) * geometry.block(),
                }
            })
            .filter(|part| part.kind != 0)
            .collect()
    })
}

/// An MBR listing partitions in front of a GPT without protecting it, which
/// other systems read rather than the GPT.
fn unprotected_gpt(file: &File, geometry: Geometry) -> Option<Vec<MbrPartition>> {
    let mut sector = vec![0u8; geometry.block() as usize];
    file.read_exact_at(&mut sector, 0).ok()?;
    mbr_entries(&sector, geometry).filter(|entries| {
        !entries.is_empty() && entries.iter().all(|e| e.kind != MBR_PROTECTIVE_TYPE)
    })
}

fn no_table(node: &str, file: &File, geometry: Geometry) -> Result<NoTable, String> {
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
        return Ok(NoTable::UnreadableGpt);
    }
    match mbr_entries(&read(0, block)?, geometry) {
        Some(entries) if entries.iter().any(|e| e.kind == MBR_PROTECTIVE_TYPE) => {
            return Ok(NoTable::UnreadableGpt);
        }
        Some(entries) if !entries.is_empty() => return Ok(NoTable::Mbr(entries)),
        _ => {}
    }
    if holds_ext4(node) {
        return Ok(NoTable::Ext4);
    }
    let probe = BLANK_PROBE_BYTES.min(geometry.capacity() as usize);
    if read(0, probe)?.iter().any(|&b| b != 0) {
        return Ok(NoTable::Data);
    }
    Ok(NoTable::Blank)
}

/// Hands `on_line` each line of `bytes`, keeping a line's start until its end
/// arrives.
fn split_lines(pending: &mut Vec<u8>, bytes: &[u8], on_line: &mut dyn FnMut(&[u8])) {
    for &byte in bytes {
        if byte == b'\n' {
            on_line(pending);
            pending.clear();
        } else {
            pending.push(byte);
        }
    }
}

/// Run `program` holding the raw-device right, which it needs to write a
/// volume beneath every filesystem, and nothing of the installer's
/// environment, which would reach it with that right. What it prints is
/// logged line by line under its name; it reads nothing.
fn e2fsprogs(program: &str, args: &[&str], events: &mut dyn Events) -> Result<(), String> {
    let name = program.rsplit('/').next().unwrap_or(program);
    let what = format!("{name} {}", args.join(" "));
    let argv: Vec<CString> = std::iter::once(program)
        .chain(args.iter().copied())
        .map(|arg| CString::new(arg).map_err(|_| format!("{what}: an argument holds NUL")))
        .collect::<Result<_, _>>()?;
    let env: Vec<CString> = [
        format!("MKE2FS_CONFIG={E2FSPROGS_PROFILE}"),
        format!("E2FSCK_CONFIG={E2FSPROGS_PROFILE}"),
        "E2FSPROGS_UNDO_DIR=none".to_owned(),
    ]
    .into_iter()
    .map(|var| CString::new(var).expect("no NUL in a fixed variable"))
    .collect();
    let argv_ptrs: Vec<*const u8> = argv.iter().map(|a| a.as_ptr().cast()).collect();
    let env_ptrs: Vec<*const u8> = env.iter().map(|v| v.as_ptr().cast()).collect();
    let (output, said) = pipe2(O_CLOEXEC as u32).map_err(|e| format!("{what}: {e}"))?;
    let null = open_cstr(c"/dev/null", O_RDONLY | O_CLOEXEC as u32)
        .map_err(|e| format!("{what}: /dev/null: {e}"))?;
    let stdio = [
        process::clone_fd(null.raw(), 0),
        process::clone_fd(said.raw(), 1),
        process::clone_fd(said.raw(), 2),
    ];
    let pid = process::spawn_endowed(
        program.as_bytes(),
        &argv_ptrs,
        &env_ptrs,
        b"/",
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &stdio,
        0,
        Endowment {
            caps: RAW_BLOCK,
            ..Endowment::NONE
        },
    );
    drop((said, null));
    if pid <= 0 {
        return Err(format!("{what}: {}", SyscallError::from_errno(-pid)));
    }
    let mut log = |line: &[u8]| events.log(format!("{name}: {}", String::from_utf8_lossy(line)));
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match read_slice(output.raw(), &mut chunk) {
            Ok(0) => break,
            Ok(n) => split_lines(&mut pending, &chunk[..n], &mut log),
            Err(SyscallError::EINTR) => {}
            Err(_) => break,
        }
    }
    drop(output);
    if !pending.is_empty() {
        log(&pending);
    }
    match process::waitpid(pid as u32).map(|(_, status)| process::wait_status(status)) {
        Some(WaitStatus::Exited(0)) => Ok(()),
        Some(status) => Err(format!("{what} exited {status:?}")),
        None => Err(format!("{what}: could not be waited for")),
    }
}

/// Format `node` as a root in the profile every SlopOS volume has.
pub fn mke2fs(node: &str, events: &mut dyn Events) -> Result<(), String> {
    use slopos_ext4_core::profile;
    let inode_size = profile::inode_size().to_string();
    let block_size = profile::block_size().to_string();
    let journal = format!("size={ROOT_JOURNAL_MIB}");
    e2fsprogs(
        MKE2FS,
        &[
            "-q",
            "-F",
            "-t",
            "ext4",
            "-O",
            "none",
            "-O",
            profile::features(),
            "-I",
            &inode_size,
            "-b",
            &block_size,
            "-J",
            &journal,
            "-E",
            "lazy_itable_init=1",
            node,
        ],
        events,
    )
}

/// `e2fsck -fn`: a full check that changes nothing.
pub fn e2fsck(node: &str, events: &mut dyn Events) -> Result<(), String> {
    e2fsprogs(E2FSCK, &["-f", "-n", node], events)
}
