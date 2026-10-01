//! Partition-table parsing and the [`BlockDevice`] adaptors that turn a table
//! entry into a device.
//!
//! GPT is read through `slopos_boot_core::gpt`, which `bootctl` reads the
//! same tables with, so both name the same partitions. MBR is the
//! conventional boot sector, `0xAA55` at offset 510, the disk signature at 440
//! and four 16-byte entries from 446. Only an MBR entry's LBA fields are read:
//! its CHS fields cannot address a modern disk and disagree with the LBA
//! fields often enough to be a trap. They count in the device's logical
//! blocks.

use slopos_boot_core::Guid;
use slopos_boot_core::gpt::{self, Geometry, Header, Reject, Skip, Skipped};
use slopos_ostd::klog_info;
use slopos_ostd::{KArc, KVec};

use crate::blockdev::{BlockDevice, BlockDeviceError, WriteTicket, total_seg_len};

/// The boot sector's size, which is also the smallest logical block.
const MBR_BYTES: usize = 512;

const _: () = assert!(
    gpt::MAX_ENTRIES <= u8::MAX as u32,
    "a GPT entry's number is a u8 here"
);

const MBR_DISK_SIGNATURE_AT: usize = 440;
const MBR_SIGNATURE_AT: usize = 510;
const MBR_SIGNATURE: u16 = 0xAA55;
const MBR_ENTRY_AT: usize = 446;
const MBR_ENTRY_SIZE: usize = 16;
const MBR_ENTRY_COUNT: usize = 4;
const MBR_TYPE_UNUSED: u8 = 0x00;
const MBR_TYPE_PROTECTIVE: u8 = 0xEE;
const MBR_TYPE_EXTENDED_CHS: u8 = 0x05;
const MBR_TYPE_EXTENDED_LBA: u8 = 0x0F;
const MBR_TYPE_EXTENDED_LINUX: u8 = 0x85;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartitionError {
    Io,
    NoMemory,
    /// Well-formed but beyond what the GPT reader stages: see
    /// [`Reject::Unsupported`].
    Unsupported,
    /// A protective MBR (type `0xEE`) with no usable GPT behind it: the real
    /// table is unreadable, so reporting the disk partitionless would hand the
    /// mount a stale whole-device filesystem.
    ProtectiveMbrWithoutGpt,
    /// Both GPT copies failed validation and there is no MBR to fall back to.
    CorruptGpt,
    /// A [`PartitionDevice`] window that does not start on a logical-block
    /// boundary, or a device whose logical block size is not a power of two
    /// of at least 512.
    Misaligned,
    /// A [`PartitionDevice`] window that is empty or leaves the parent.
    OutOfRange,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartitionKind {
    Gpt { type_guid: Guid },
    Mbr { type_byte: u8 },
}

/// One usable partition: a byte window into the device it was parsed from.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PartitionEntry {
    /// 1-based, as `/dev/vda1` spells it.
    pub number: u8,
    pub start: u64,
    pub len: u64,
    pub kind: PartitionKind,
    pub uuid: PartUuid,
}

/// What `PARTUUID=` names a partition by: a GPT entry's unique partition
/// GUID, or an MBR disk's signature with the entry's number.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartUuid {
    Gpt(Guid),
    Mbr { signature: u32, number: u8 },
}

/// Room for the longest [`PartUuid`] spelling, a GUID's 36 characters.
pub const PARTUUID_TEXT_MAX: usize = 36;

const HEX: &[u8; 16] = b"0123456789abcdef";

fn put_hex(out: &mut [u8], bytes: impl Iterator<Item = u8>) -> usize {
    let mut at = 0;
    for byte in bytes {
        out[at] = HEX[usize::from(byte >> 4)];
        out[at + 1] = HEX[usize::from(byte & 0xF)];
        at += 2;
    }
    at
}

impl PartUuid {
    /// The lowercase spelling Linux gives it: a GUID, or `ssssssss-nn` in
    /// hex for MBR.
    pub fn format(&self, out: &mut [u8; PARTUUID_TEXT_MAX]) -> usize {
        match self {
            PartUuid::Gpt(guid) => {
                *out = guid.spelling();
                PARTUUID_TEXT_MAX
            }
            PartUuid::Mbr { signature, number } => {
                let at = put_hex(out, signature.to_be_bytes().into_iter());
                out[at] = b'-';
                at + 1 + put_hex(&mut out[at + 1..], core::iter::once(*number))
            }
        }
    }

    pub fn matches(&self, text: &[u8]) -> bool {
        let mut buf = [0u8; PARTUUID_TEXT_MAX];
        let len = self.format(&mut buf);
        buf[..len].eq_ignore_ascii_case(text)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PartitionScheme {
    None,
    Gpt { disk: Guid },
    Mbr,
}

pub struct PartitionTable {
    pub scheme: PartitionScheme,
    pub entries: KVec<PartitionEntry>,
}

/// The device's size and logical block, if it is one a table can be counted
/// in.
fn geometry_of(device: &dyn BlockDevice) -> Result<Geometry, PartitionError> {
    Geometry::new(device.capacity(), u64::from(device.logical_block_size()))
        .ok_or(PartitionError::Misaligned)
}

impl PartitionTable {
    pub const fn unpartitioned() -> Self {
        Self {
            scheme: PartitionScheme::None,
            entries: KVec::new(),
        }
    }

    pub fn find(&self, number: u8) -> Option<&PartitionEntry> {
        self.entries.iter().find(|e| e.number == number)
    }
}

#[inline]
fn le_u16(bytes: &[u8], at: usize) -> u16 {
    let Some(s) = bytes.get(at..at + 2) else {
        return 0;
    };
    (s[0] as u16) | ((s[1] as u16) << 8)
}

#[inline]
fn le_u32(bytes: &[u8], at: usize) -> u32 {
    let Some(s) = bytes.get(at..at + 4) else {
        return 0;
    };
    (s[0] as u32) | ((s[1] as u32) << 8) | ((s[2] as u32) << 16) | ((s[3] as u32) << 24)
}

/// Zeroed heap buffer: a sector or an entry array must not sit on the kernel
/// stack.
fn staged(len: usize) -> Result<KVec<u8>, PartitionError> {
    let mut buf = KVec::with_capacity(len).map_err(|_| PartitionError::NoMemory)?;
    for _ in 0..len {
        buf.push(0).map_err(|_| PartitionError::NoMemory)?;
    }
    Ok(buf)
}

/// Why one GPT copy was not usable.
///
/// Only a copy that *claimed* a GPT — one whose signature matched — may
/// suppress the MBR fallback, so a sector that could not be read or staged
/// before any signature matched is [`Reject::Absent`]: a plain MBR disk whose
/// last logical block is unreadable must still parse its MBR.
#[derive(Copy, Clone, PartialEq, Eq)]
enum GptReject {
    Rejected(Reject),
    /// A GPT was claimed, but its entry array could not be read or staged.
    Indeterminate(PartitionError),
}

const ABSENT: GptReject = GptReject::Rejected(Reject::Absent);

impl From<Reject> for GptReject {
    fn from(reject: Reject) -> Self {
        GptReject::Rejected(reject)
    }
}

/// Parse the partition table of `device`. Neither signature present is
/// [`PartitionScheme::None`], the whole-device case.
///
/// GPT is tried before MBR because a GPT disk carries a protective MBR that
/// would otherwise parse as one partition spanning the disk.
pub fn probe(device: &dyn BlockDevice) -> Result<PartitionTable, PartitionError> {
    let geometry = geometry_of(device)?;
    if geometry.blocks() < 2 {
        return Ok(PartitionTable::unpartitioned());
    }

    let gpt_claimed = match probe_gpt(device, geometry) {
        Ok(Some(table)) => return Ok(table),
        Ok(None) => false,
        Err(PartitionError::CorruptGpt) => true,
        Err(e) => return Err(e),
    };

    match probe_mbr(device, geometry) {
        Ok(table) if gpt_claimed && table.scheme == PartitionScheme::None => {
            Err(PartitionError::CorruptGpt)
        }
        other => other,
    }
}

/// `Ok(None)`: no GPT here, try MBR. `Err(CorruptGpt)`: a GPT was claimed by
/// at least one copy and neither validated, so an MBR fallback must not report
/// the disk as partitionless.
#[inline(never)]
fn probe_gpt(
    device: &dyn BlockDevice,
    geometry: Geometry,
) -> Result<Option<PartitionTable>, PartitionError> {
    let primary = match read_gpt_copy(device, geometry, gpt::PRIMARY_LBA) {
        Ok(table) => return Ok(Some(table)),
        Err(e) => e,
    };

    let backup_lba = geometry.backup_lba();
    let backup = if backup_lba == gpt::PRIMARY_LBA {
        ABSENT
    } else {
        match read_gpt_copy(device, geometry, backup_lba) {
            Ok(table) => {
                klog_info!(
                    "PART: primary GPT header unusable, parsed the backup at LBA {backup_lba}"
                );
                return Ok(Some(table));
            }
            Err(e) => e,
        }
    };

    let verdict = if primary == ABSENT { backup } else { primary };
    match verdict {
        GptReject::Rejected(Reject::Absent) => Ok(None),
        GptReject::Rejected(Reject::Corrupt) => Err(PartitionError::CorruptGpt),
        GptReject::Rejected(Reject::Unsupported) => Err(PartitionError::Unsupported),
        GptReject::Indeterminate(e) => Err(e),
    }
}

#[inline(never)]
fn read_gpt_copy(
    device: &dyn BlockDevice,
    geometry: Geometry,
    lba: u64,
) -> Result<PartitionTable, GptReject> {
    let header = read_gpt_header(device, geometry, lba)?;

    let mut array = staged(header.array_bytes())
        .map_err(|_| GptReject::Indeterminate(PartitionError::NoMemory))?;
    let at = geometry
        .byte_of(header.entry_lba())
        .ok_or(GptReject::Rejected(Reject::Corrupt))?;
    device
        .read_at(at, array.as_mut_slice())
        .map_err(|_| GptReject::Indeterminate(PartitionError::Io))?;
    if !header.array_matches(&array) {
        return Err(Reject::Corrupt.into());
    }

    let mut entries = KVec::new();
    for partition in header.partitions(&array) {
        let partition = match partition {
            Ok(partition) => partition,
            Err(Skipped { number, why }) => {
                let why = match why {
                    Skip::OutsideUsable => "lies outside the usable range",
                    Skip::Overlaps => "overlaps an earlier one",
                };
                klog_info!("PART: GPT entry {number} {why} — skipped");
                continue;
            }
        };
        entries
            .push(PartitionEntry {
                number: partition.entry.number as u8,
                start: partition.start,
                len: partition.len,
                kind: PartitionKind::Gpt {
                    type_guid: partition.entry.type_guid,
                },
                uuid: PartUuid::Gpt(partition.entry.unique),
            })
            .map_err(|_| GptReject::Indeterminate(PartitionError::NoMemory))?;
    }

    Ok(PartitionTable {
        scheme: PartitionScheme::Gpt {
            disk: header.disk_guid(),
        },
        entries,
    })
}

fn read_gpt_header(
    device: &dyn BlockDevice,
    geometry: Geometry,
    lba: u64,
) -> Result<Header, GptReject> {
    // Nothing has claimed a GPT here yet, so a failure to stage or read the
    // block leaves the disk a candidate for MBR.
    let mut block = staged(geometry.block() as usize).map_err(|_| ABSENT)?;
    let at = geometry.byte_of(lba).ok_or(ABSENT)?;
    device
        .read_at(at, block.as_mut_slice())
        .map_err(|_| ABSENT)?;
    Ok(Header::parse(&block, lba, geometry)?)
}

/// Whether `[start, start + len)` shares a byte with a window in `entries`: two
/// claims on one disk exclude each other only when they name the same window.
fn overlaps_any(entries: &[PartitionEntry], start: u64, len: u64) -> bool {
    entries
        .iter()
        .any(|e| start < e.start + e.len && e.start < start + len)
}

#[inline(never)]
fn probe_mbr(
    device: &dyn BlockDevice,
    geometry: Geometry,
) -> Result<PartitionTable, PartitionError> {
    let mut sector = staged(MBR_BYTES)?;
    device
        .read_at(0, sector.as_mut_slice())
        .map_err(|_| PartitionError::Io)?;
    if le_u16(&sector, MBR_SIGNATURE_AT) != MBR_SIGNATURE {
        return Ok(PartitionTable::unpartitioned());
    }
    let signature = le_u32(&sector, MBR_DISK_SIGNATURE_AT);

    let mut entries = KVec::new();
    let mut protective = false;
    for index in 0..MBR_ENTRY_COUNT {
        let base = MBR_ENTRY_AT + index * MBR_ENTRY_SIZE;
        let Some(raw) = sector.get(base..base + MBR_ENTRY_SIZE) else {
            break;
        };
        let type_byte = raw[4];
        let number = (index + 1) as u8;
        match type_byte {
            MBR_TYPE_UNUSED => continue,
            MBR_TYPE_PROTECTIVE => {
                protective = true;
                continue;
            }
            MBR_TYPE_EXTENDED_CHS | MBR_TYPE_EXTENDED_LBA | MBR_TYPE_EXTENDED_LINUX => {
                klog_info!(
                    "PART: MBR entry {number} is an extended container (type {type_byte:#04x}) — \
                     logical partitions inside it are not enumerated"
                );
                continue;
            }
            _ => {}
        }
        let first = le_u32(raw, 8) as u64;
        let sectors = le_u32(raw, 12) as u64;
        if sectors == 0 {
            continue;
        }
        if first == 0 {
            klog_info!("PART: MBR entry {number} covers the table itself — skipped");
            continue;
        }
        let Some((start, len)) = geometry.window(first, first + sectors - 1) else {
            klog_info!("PART: MBR entry {number} leaves the device — skipped");
            continue;
        };
        if overlaps_any(&entries, start, len) {
            klog_info!("PART: MBR entry {number} overlaps an earlier one — skipped");
            continue;
        }
        entries
            .push(PartitionEntry {
                number,
                start,
                len,
                kind: PartitionKind::Mbr { type_byte },
                uuid: PartUuid::Mbr { signature, number },
            })
            .map_err(|_| PartitionError::NoMemory)?;
    }

    if entries.is_empty() {
        if protective {
            return Err(PartitionError::ProtectiveMbrWithoutGpt);
        }
        return Ok(PartitionTable::unpartitioned());
    }
    Ok(PartitionTable {
        scheme: PartitionScheme::Mbr,
        entries,
    })
}

/// Whole-device delegate over a shared device, so the same claim can back both
/// the mount and a `/dev` node.
pub struct SharedBlockDevice(pub KArc<dyn BlockDevice + Send + Sync>);

impl BlockDevice for SharedBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.0.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.0.write_at(offset, buffer)
    }

    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.0.write_vectored(offset, segs)
    }

    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        self.0.submit_write(offset, segs)
    }

    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        self.0.complete_write(ticket)
    }

    fn write_depth(&self) -> usize {
        self.0.write_depth()
    }

    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.0.logical_block_size()
    }

    fn write_protected(&self) -> bool {
        self.0.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.0.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.0.checkpoint()
    }
}

/// A byte window into a parent device: the filesystem inside a partition sees
/// offset 0 as the partition's first byte.
pub struct PartitionDevice {
    parent: KArc<dyn BlockDevice + Send + Sync>,
    start: u64,
    len: u64,
}

impl PartitionDevice {
    /// `start` must be logical-block aligned: the block layer turns a
    /// partial-block write into a read-modify-write, so a misaligned window
    /// would put every filesystem metadata write over bytes outside the
    /// partition.
    pub fn try_new(
        parent: KArc<dyn BlockDevice + Send + Sync>,
        start: u64,
        len: u64,
    ) -> Result<Self, PartitionError> {
        if start % geometry_of(parent.as_ref())?.block() != 0 {
            return Err(PartitionError::Misaligned);
        }
        let end = start.checked_add(len).ok_or(PartitionError::OutOfRange)?;
        if len == 0 || end > parent.capacity() {
            return Err(PartitionError::OutOfRange);
        }
        Ok(Self { parent, start, len })
    }

    fn parent_offset(&self, offset: u64, len: usize) -> Result<u64, BlockDeviceError> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(BlockDeviceError::OutOfBounds)?;
        if end > self.len {
            return Err(BlockDeviceError::OutOfBounds);
        }
        offset
            .checked_add(self.start)
            .ok_or(BlockDeviceError::OutOfBounds)
    }
}

impl BlockDevice for PartitionDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        let at = self.parent_offset(offset, buffer.len())?;
        self.parent.read_at(at, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        let at = self.parent_offset(offset, buffer.len())?;
        self.parent.write_at(at, buffer)
    }

    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        let at = self.parent_offset(offset, total_seg_len(segs)?)?;
        self.parent.write_vectored(at, segs)
    }

    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        let at = self.parent_offset(offset, total_seg_len(segs)?)?;
        self.parent.submit_write(at, segs)
    }

    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        self.parent.complete_write(ticket)
    }

    fn write_depth(&self) -> usize {
        self.parent.write_depth()
    }

    /// The window length, cached: the parent's `capacity()` takes its state
    /// lock and this is on every bounds check.
    fn capacity(&self) -> u64 {
        self.len
    }

    fn logical_block_size(&self) -> u32 {
        self.parent.logical_block_size()
    }

    fn write_protected(&self) -> bool {
        self.parent.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.parent.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.parent.checkpoint()
    }
}
