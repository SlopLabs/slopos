//! The GUID Partition Table, UEFI Specification 2.10 §5.3: a header at LBA 1
//! and its backup at the last logical block, each naming an array of entries,
//! a CRC-32 over the header and another over the array. Everything counts in
//! the device's logical blocks, so a 4K-native disk's header is at byte 4096.
//!
//! This is the format and which entries name a partition; reading the blocks
//! is the caller's.

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

    pub fn disk_guid(&self) -> Guid {
        self.disk_guid
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
