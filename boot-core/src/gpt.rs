//! The GUID Partition Table, UEFI Specification 2.10 §5.3: a header at LBA 1
//! and its backup at the last logical block, each naming an array of entries,
//! a CRC-32 over the header and another over the array. Everything counts in
//! the device's logical blocks, so a 4K-native disk's header is at byte 4096.
//!
//! This is the format, which entries name a partition and what a table to
//! write holds; moving the blocks is the caller's.

use crate::crc32;
use crate::guid::Guid;

pub const SIGNATURE: &[u8; 8] = b"EFI PART";
pub const PRIMARY_LBA: u64 = 1;
/// The header's defined fields; a larger `HeaderSize` is reserved bytes.
pub const HEADER_MIN: u32 = 92;
pub const MIN_ENTRY_SIZE: u32 = 128;
/// The most entries this reader stages, so the most partitions a disk has.
pub const MAX_ENTRIES: u32 = 128;
/// The largest entry array this reader stages: it CRCs the array whole.
pub const MAX_ARRAY_BYTES: u64 = 32 * 1024;
/// UTF-16 code units in an entry's name.
pub const NAME_UNITS: usize = 36;
/// What a new table holds: 128 entries of 128 bytes, the 16 KiB §5.3.2 asks
/// an array to reserve at least, as partitioning tools lay one out.
pub const NEW_ENTRIES: u32 = 128;
pub const NEW_ENTRY_SIZE: u32 = MIN_ENTRY_SIZE;
const HEADER_REVISION: u32 = 0x0001_0000;
/// The protective MBR's one partition type, §5.2.3.
const PROTECTIVE_TYPE: u8 = 0xEE;

const _: () = assert!(
    MAX_ENTRIES <= u128::BITS,
    "partitions() marks accepted slots in a u128"
);

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(word)
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn guid_at(bytes: &[u8], at: usize) -> Guid {
    let mut guid = [0u8; 16];
    guid.copy_from_slice(&bytes[at..at + 16]);
    Guid(guid)
}

/// A device's size and the logical block its table counts in, both in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    capacity: u64,
    block: u64,
}

impl Geometry {
    /// `None` for a block size a table cannot be counted in: under 512 bytes
    /// or not a power of two.
    pub fn new(capacity: u64, block: u64) -> Option<Self> {
        (block >= 512 && block.is_power_of_two()).then_some(Self { capacity, block })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn block(&self) -> u64 {
        self.block
    }

    pub fn blocks(&self) -> u64 {
        self.capacity / self.block
    }

    pub fn byte_of(&self, lba: u64) -> Option<u64> {
        lba.checked_mul(self.block)
    }

    pub fn backup_lba(&self) -> u64 {
        self.blocks().saturating_sub(1)
    }

    /// The byte window of the inclusive LBA range, `None` if it leaves the
    /// device.
    pub fn window(&self, first_lba: u64, last_lba: u64) -> Option<(u64, u64)> {
        let start = self.byte_of(first_lba)?;
        let blocks = last_lba.checked_sub(first_lba)?.checked_add(1)?;
        let len = self.byte_of(blocks)?;
        (start.checked_add(len)? <= self.capacity).then_some((start, len))
    }
}

/// Why one header copy is not usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// No signature: nothing claims a GPT here.
    Absent,
    /// A GPT is claimed and does not add up.
    Corrupt,
    /// Well-formed but beyond what this reader stages: a major revision other
    /// than 1, more than [`MAX_ENTRIES`] entries, an array over
    /// [`MAX_ARRAY_BYTES`], or an entry size other than 128·2ⁿ (§5.3.2).
    Unsupported,
}

/// A header copy that validated, and nothing else: a value of this type is
/// only ever made by [`Header::parse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    geometry: Geometry,
    my_lba: u64,
    disk_guid: Guid,
    first_usable: u64,
    last_usable: u64,
    entry_lba: u64,
    num_entries: u32,
    entry_size: u32,
    array_crc: u32,
}

impl Header {
    /// Validate the copy `block` holds, read from `lba`.
    pub fn parse(block: &[u8], lba: u64, geometry: Geometry) -> Result<Header, Reject> {
        if block.len() < HEADER_MIN as usize || &block[..8] != SIGNATURE {
            return Err(Reject::Absent);
        }
        if le_u32(block, 8) >> 16 != 1 {
            return Err(Reject::Unsupported);
        }
        let header_size = le_u32(block, 12) as usize;
        if !(HEADER_MIN as usize..=block.len()).contains(&header_size)
            || header_size as u64 > geometry.block
        {
            return Err(Reject::Corrupt);
        }
        // §5.3.2: the CRC covers `HeaderSize` bytes with its own field zero.
        let mut state = crc32::feed(crc32::INIT, &block[..16]);
        state = crc32::feed(state, &[0; 4]);
        state = crc32::feed(state, &block[20..header_size]);
        if crc32::finish(state) != le_u32(block, 16) {
            return Err(Reject::Corrupt);
        }
        // A copy that disagrees about where it lives is the other copy, and
        // its array pointer cannot be trusted either.
        if le_u64(block, 24) != lba {
            return Err(Reject::Corrupt);
        }
        let header = Header {
            geometry,
            my_lba: lba,
            first_usable: le_u64(block, 40),
            last_usable: le_u64(block, 48),
            disk_guid: guid_at(block, 56),
            entry_lba: le_u64(block, 72),
            num_entries: le_u32(block, 80),
            entry_size: le_u32(block, 84),
            array_crc: le_u32(block, 88),
        };
        if header.num_entries == 0 || header.num_entries > MAX_ENTRIES {
            return Err(Reject::Unsupported);
        }
        if header.entry_size < MIN_ENTRY_SIZE || !header.entry_size.is_power_of_two() {
            return Err(Reject::Unsupported);
        }
        let array_bytes = u64::from(header.num_entries) * u64::from(header.entry_size);
        if array_bytes > MAX_ARRAY_BYTES {
            return Err(Reject::Unsupported);
        }
        if header.first_usable > header.last_usable || header.last_usable >= geometry.blocks() {
            return Err(Reject::Corrupt);
        }
        let array_end = geometry
            .byte_of(header.entry_lba)
            .and_then(|b| b.checked_add(array_bytes))
            .ok_or(Reject::Corrupt)?;
        if array_end > geometry.capacity {
            return Err(Reject::Corrupt);
        }
        // §5.3.2: LBA 0, both headers and both arrays lie outside the usable
        // range, or a mount could overwrite the table.
        let array_blocks = array_bytes.div_ceil(geometry.block);
        let array_last_lba = header.entry_lba + array_blocks - 1;
        if header.first_usable < PRIMARY_LBA + 1 + array_blocks
            || header.last_usable + 1 + array_blocks > geometry.backup_lba()
            || (header.entry_lba <= header.last_usable && array_last_lba >= header.first_usable)
        {
            return Err(Reject::Corrupt);
        }
        Ok(header)
    }

    /// The table a device that holds none is given: [`NEW_ENTRIES`] entries
    /// of [`NEW_ENTRY_SIZE`] bytes after each header, every block between
    /// them usable and none used. `None` for a device too small to hold both
    /// copies and a block between them.
    pub fn new(geometry: Geometry, disk_guid: Guid) -> Option<Header> {
        let array_bytes = u64::from(NEW_ENTRIES) * u64::from(NEW_ENTRY_SIZE);
        let array_blocks = array_bytes.div_ceil(geometry.block);
        let first_usable = PRIMARY_LBA + 1 + array_blocks;
        let last_usable = geometry
            .backup_lba()
            .checked_sub(array_blocks + 1)
            .filter(|&last| last >= first_usable)?;
        Some(Header {
            geometry,
            my_lba: PRIMARY_LBA,
            disk_guid,
            first_usable,
            last_usable,
            entry_lba: PRIMARY_LBA + 1,
            num_entries: NEW_ENTRIES,
            entry_size: NEW_ENTRY_SIZE,
            array_crc: crc32::crc32(&[0; (NEW_ENTRIES * NEW_ENTRY_SIZE) as usize]),
        })
    }

    /// This table with every block up to its backup copy usable, so a device
    /// grown since the table was written offers what it gained. The backup
    /// it was read beside is then only data; the next write puts the backup
    /// at the device's end.
    pub fn grown(self) -> Header {
        let last_usable = self.geometry.backup_lba() - self.array_blocks() - 1;
        Header {
            last_usable: self.last_usable.max(last_usable),
            ..self
        }
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn disk_guid(&self) -> Guid {
        self.disk_guid
    }

    pub fn first_usable(&self) -> u64 {
        self.first_usable
    }

    pub fn last_usable(&self) -> u64 {
        self.last_usable
    }

    fn array_blocks(&self) -> u64 {
        (self.array_bytes() as u64).div_ceil(self.geometry.block)
    }

    /// Where `copy`'s array is written: the primary's where its header put it,
    /// or just after the primary header when the table was read from its
    /// backup; the backup's just below the device's last block, where §5.3.2
    /// puts the backup header. [`Header::parse`] holds both clear of the usable
    /// range.
    pub fn array_lba(&self, copy: Location) -> u64 {
        match copy {
            Location::Primary if self.my_lba == PRIMARY_LBA => self.entry_lba,
            Location::Primary => PRIMARY_LBA + 1,
            Location::Backup => self.geometry.backup_lba() - self.array_blocks(),
        }
    }

    /// The order this table is written in, each piece behind the last by a
    /// flush: the copy it was not read from first, each header after its
    /// array, so a reader that takes the primary and falls back to the backup
    /// finds the old table or the new one whole at every step.
    pub fn write_order(&self) -> [Piece; 4] {
        let (first, last) = if self.my_lba == PRIMARY_LBA {
            (Location::Backup, Location::Primary)
        } else {
            (Location::Primary, Location::Backup)
        };
        [
            Piece::Array(first),
            Piece::Header(first),
            Piece::Array(last),
            Piece::Header(last),
        ]
    }

    /// This table holding `array`, which must be [`Header::array_bytes`] long.
    pub fn holding(self, array: &[u8]) -> Header {
        debug_assert_eq!(array.len(), self.array_bytes());
        Header {
            array_crc: crc32::crc32(array),
            ..self
        }
    }

    /// The header block of `copy` into `block`, one logical block long, the
    /// bytes past the header zeroed.
    pub fn encode(&self, copy: Location, block: &mut [u8]) {
        let (my, alternate) = match copy {
            Location::Primary => (PRIMARY_LBA, self.geometry.backup_lba()),
            Location::Backup => (self.geometry.backup_lba(), PRIMARY_LBA),
        };
        block.fill(0);
        block[..8].copy_from_slice(SIGNATURE);
        put_u32(block, 8, HEADER_REVISION);
        put_u32(block, 12, HEADER_MIN);
        put_u64(block, 24, my);
        put_u64(block, 32, alternate);
        put_u64(block, 40, self.first_usable);
        put_u64(block, 48, self.last_usable);
        block[56..72].copy_from_slice(&self.disk_guid.0);
        put_u64(block, 72, self.array_lba(copy));
        put_u32(block, 80, self.num_entries);
        put_u32(block, 84, self.entry_size);
        put_u32(block, 88, self.array_crc);
        let crc = crc32::crc32(&block[..HEADER_MIN as usize]);
        put_u32(block, 16, crc);
    }

    pub fn entries(&self) -> u32 {
        self.num_entries
    }

    fn slot_range(&self, number: u32) -> Option<core::ops::Range<usize>> {
        let index = (number as usize).checked_sub(1)?;
        let stride = self.entry_size as usize;
        (index < self.num_entries as usize).then(|| index * stride..(index + 1) * stride)
    }

    /// The slot numbered `number` (1-based) of `array`, as stored.
    pub fn slot<'a>(&self, array: &'a [u8], number: u32) -> Option<&'a [u8]> {
        array.get(self.slot_range(number)?)
    }

    pub fn slot_mut<'a>(&self, array: &'a mut [u8], number: u32) -> Option<&'a mut [u8]> {
        array.get_mut(self.slot_range(number)?)
    }

    /// The runs of usable blocks that no used entry of `array` reaches, lowest
    /// first, as inclusive LBA ranges. An entry [`Header::partitions`] skips
    /// still holds its blocks: something on the disk may read them as its own.
    pub fn free(&self, array: &[u8]) -> Free {
        let mut used = [(0u64, 0u64); MAX_ENTRIES as usize];
        let mut count = 0;
        let stride = self.entry_size as usize;
        for index in 0..(self.num_entries as usize).min(array.len() / stride) {
            if let Some(entry) = self.entry(array, index) {
                let (first, last) = (entry.first_lba, entry.last_lba);
                used[count] = (first.min(last), first.max(last));
                count += 1;
            }
        }
        let used = &mut used[..count];
        used.sort_unstable();
        let mut free = Free {
            runs: [(0, 0); MAX_ENTRIES as usize + 1],
            len: 0,
        };
        let mut next = self.first_usable;
        for &(first, last) in used.iter() {
            if first > next && next <= self.last_usable {
                free.push(next, first.min(self.last_usable + 1) - 1);
            }
            next = next.max(last.saturating_add(1));
        }
        if next <= self.last_usable {
            free.push(next, self.last_usable);
        }
        free
    }

    pub fn entry_lba(&self) -> u64 {
        self.entry_lba
    }

    pub fn array_bytes(&self) -> usize {
        self.num_entries as usize * self.entry_size as usize
    }

    /// Whether `array`, read from [`Header::entry_lba`], is the array this
    /// copy describes.
    pub fn array_matches(&self, array: &[u8]) -> bool {
        array.len() == self.array_bytes() && crc32::crc32(array) == self.array_crc
    }

    fn entry(&self, array: &[u8], index: usize) -> Option<Entry> {
        let stride = self.entry_size as usize;
        let raw = array.get(index * stride..(index + 1) * stride)?;
        Entry::decode(index as u32 + 1, raw)
    }

    /// The partitions `array` names, in slot order, and the used entries that
    /// name none. An entry must lie inside the usable range, and must share no
    /// block with an earlier partition: a disk on which two windows overlap
    /// would let a mount of one write the other.
    pub fn partitions<'a>(
        &'a self,
        array: &'a [u8],
    ) -> impl Iterator<Item = Result<Partition, Skipped>> + 'a {
        let count = (self.num_entries as usize).min(array.len() / self.entry_size as usize);
        let mut accepted: u128 = 0;
        (0..count).filter_map(move |index| {
            let entry = self.entry(array, index)?;
            let skip = |why| {
                Some(Err(Skipped {
                    number: entry.number,
                    why,
                }))
            };
            if entry.first_lba > entry.last_lba
                || entry.first_lba < self.first_usable
                || entry.last_lba > self.last_usable
            {
                return skip(Skip::OutsideUsable);
            }
            let overlaps = (0..index)
                .filter(|&earlier| accepted & (1 << earlier) != 0)
                .filter_map(|earlier| self.entry(array, earlier))
                .any(|e| entry.first_lba <= e.last_lba && e.first_lba <= entry.last_lba);
            if overlaps {
                return skip(Skip::Overlaps);
            }
            accepted |= 1 << index;
            let block = self.geometry.block;
            Some(Ok(Partition {
                entry,
                start: entry.first_lba * block,
                len: (entry.last_lba - entry.first_lba + 1) * block,
            }))
        })
    }
}

/// Where one of a table's two copies lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    Primary,
    Backup,
}

/// What a table write puts down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece {
    Array(Location),
    Header(Location),
}

/// The free runs [`Header::free`] found.
pub struct Free {
    runs: [(u64, u64); MAX_ENTRIES as usize + 1],
    len: usize,
}

impl Free {
    fn push(&mut self, first: u64, last: u64) {
        self.runs[self.len] = (first, last);
        self.len += 1;
    }

    pub fn runs(&self) -> &[(u64, u64)] {
        &self.runs[..self.len]
    }
}

/// The protective MBR §5.2.3 puts in LBA 0 before a GPT: one partition of
/// type `0xEE` from LBA 1 over the rest of the device, as far as 32 bits
/// reach. `sector` is the first 512 bytes of LBA 0.
pub fn protective_mbr(geometry: Geometry, sector: &mut [u8; 512]) {
    sector.fill(0);
    let entry = &mut sector[446..462];
    entry[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    entry[4] = PROTECTIVE_TYPE;
    entry[5..8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    put_u32(entry, 8, PRIMARY_LBA as u32);
    let blocks = geometry.blocks().saturating_sub(1).min(u64::from(u32::MAX));
    put_u32(entry, 12, blocks as u32);
    sector[510] = 0x55;
    sector[511] = 0xAA;
}

/// Why a used entry names no partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Ends before it starts, or reaches outside the usable range, which
    /// `Header::parse` holds inside the device.
    OutsideUsable,
    /// Shares a block with a partition an earlier entry names.
    Overlaps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub number: u32,
    pub why: Skip,
}

/// One slot of the entry array, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 1-based slot, as `/dev/vda1` and a device path's `HD()` node number it.
    pub number: u32,
    pub type_guid: Guid,
    pub unique: Guid,
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: [u16; NAME_UNITS],
}

impl Entry {
    /// The entry into the first 128 bytes of `raw`, a slot as stored; the
    /// bytes past them, which a larger entry size reserves, are left as they
    /// are.
    pub fn encode(&self, raw: &mut [u8]) {
        raw[..16].copy_from_slice(&self.type_guid.0);
        raw[16..32].copy_from_slice(&self.unique.0);
        put_u64(raw, 32, self.first_lba);
        put_u64(raw, 40, self.last_lba);
        put_u64(raw, 48, self.attributes);
        for (pair, unit) in raw[56..128].chunks_exact_mut(2).zip(self.name) {
            pair.copy_from_slice(&unit.to_le_bytes());
        }
    }

    /// `text` as an entry name, cut at [`NAME_UNITS`] code units and never
    /// inside a character.
    pub fn name_of(text: &str) -> [u16; NAME_UNITS] {
        let mut name = [0u16; NAME_UNITS];
        let mut len = 0;
        for c in text.chars() {
            let mut units = [0u16; 2];
            let units = c.encode_utf16(&mut units);
            let Some(slots) = name.get_mut(len..len + units.len()) else {
                break;
            };
            slots.copy_from_slice(units);
            len += units.len();
        }
        name
    }

    /// `None` for an unused slot, whose type GUID is zero.
    fn decode(number: u32, raw: &[u8]) -> Option<Entry> {
        let type_guid = guid_at(raw, 0);
        if type_guid.is_zero() {
            return None;
        }
        let mut name = [0u16; NAME_UNITS];
        for (unit, pair) in name.iter_mut().zip(raw[56..128].chunks_exact(2)) {
            *unit = u16::from_le_bytes([pair[0], pair[1]]);
        }
        Some(Entry {
            number,
            type_guid,
            unique: guid_at(raw, 16),
            first_lba: le_u64(raw, 32),
            last_lba: le_u64(raw, 40),
            attributes: le_u64(raw, 48),
            name,
        })
    }
}

/// An entry that names a partition, and the byte window it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub entry: Entry,
    pub start: u64,
    pub len: u64,
}

impl Partition {
    pub fn blocks(&self) -> u64 {
        self.entry.last_lba - self.entry.first_lba + 1
    }
}

#[cfg(test)]
mod tests;
