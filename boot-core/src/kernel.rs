//! The base interfaces a kernel file serves, from its note in
//! [`NOTE_SECTION`], found by name through the section headers. Notes follow
//! the ELF gABI: 4-byte aligned unless the section asks for 8, as readelf
//! reads them.

use core::ops::RangeInclusive;

use slopos_abi::base_interface::NT_SLOPOS_BASE_INTERFACE;

pub const NOTE_SECTION: &str = ".note.slopos.base-interface";
const NOTE_OWNER: &[u8] = b"SlopOS\0";

const SHT_NOTE: u32 = 7;
const SHT_NOBITS: u32 = 8;
const SHN_XINDEX: u16 = 0xFFFF;
const SECTION_HEADER: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteError {
    NotElf,
    /// Section headers or a name table that lie outside the file.
    Truncated,
    NoNote,
    /// A note of SlopOS's type whose range is not `min..=max` from 1.
    BadNote,
}

fn le<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    le(bytes, at).map(u16::from_le_bytes)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    le(bytes, at).map(u32::from_le_bytes)
}

fn u64_at(bytes: &[u8], at: usize) -> Option<usize> {
    le(bytes, at)
        .map(u64::from_le_bytes)
        .and_then(|n| usize::try_from(n).ok())
}

struct Section<'a> {
    name: u32,
    kind: u32,
    size: usize,
    data: &'a [u8],
    align: usize,
    link: u32,
}

fn section(elf: &[u8], table: usize, index: usize) -> Option<Section<'_>> {
    let at = table.checked_add(index.checked_mul(SECTION_HEADER)?)?;
    let header = elf.get(at..at.checked_add(SECTION_HEADER)?)?;
    let offset = u64_at(header, 24)?;
    let size = u64_at(header, 32)?;
    let kind = u32_at(header, 4)?;
    let data = match kind {
        SHT_NOBITS => &[][..],
        _ => elf.get(offset..offset.checked_add(size)?)?,
    };
    Some(Section {
        name: u32_at(header, 0)?,
        kind,
        size,
        data,
        align: if u64_at(header, 48)? == 8 { 8 } else { 4 },
        link: u32_at(header, 40)?,
    })
}

fn name_is(names: &[u8], at: u32, want: &str) -> bool {
    let Some(rest) = usize::try_from(at).ok().and_then(|at| names.get(at..)) else {
        return false;
    };
    rest.strip_prefix(want.as_bytes())
        .is_some_and(|tail| tail.first() == Some(&0))
}

/// The interfaces the kernel serves, as its note states them.
pub fn served_interfaces(elf: &[u8]) -> Result<RangeInclusive<u32>, NoteError> {
    if !elf.starts_with(b"\x7fELF\x02\x01") {
        return Err(NoteError::NotElf);
    }
    let table = u64_at(elf, 0x28).ok_or(NoteError::Truncated)?;
    if table == 0 || u16_at(elf, 0x3A) != Some(SECTION_HEADER as u16) {
        return Err(NoteError::Truncated);
    }
    let zeroth = section(elf, table, 0).ok_or(NoteError::Truncated)?;
    let count = match u16_at(elf, 0x3C).ok_or(NoteError::Truncated)? {
        0 => zeroth.size,
        n => usize::from(n),
    };
    let names_at = match u16_at(elf, 0x3E).ok_or(NoteError::Truncated)? {
        SHN_XINDEX => zeroth.link as usize,
        n => usize::from(n),
    };
    let names = section(elf, table, names_at)
        .ok_or(NoteError::Truncated)?
        .data;
    for index in 0..count {
        let section = section(elf, table, index).ok_or(NoteError::Truncated)?;
        if section.kind == SHT_NOTE && name_is(names, section.name, NOTE_SECTION) {
            return find_note(section.data, section.align);
        }
    }
    Err(NoteError::NoNote)
}

fn padded(n: usize, align: usize) -> Option<usize> {
    Some(n.checked_add(align - 1)? & !(align - 1))
}

fn find_note(mut notes: &[u8], align: usize) -> Result<RangeInclusive<u32>, NoteError> {
    while notes.len() >= 12 {
        let (Some(name_size), Some(desc_size), Some(kind)) =
            (u32_at(notes, 0), u32_at(notes, 4), u32_at(notes, 8))
        else {
            break;
        };
        let name_size = name_size as usize;
        let desc_size = desc_size as usize;
        let desc_at = padded(12 + name_size, align).ok_or(NoteError::BadNote)?;
        let next = desc_at
            .checked_add(desc_size)
            .and_then(|end| padded(end, align))
            .ok_or(NoteError::BadNote)?;
        let name = notes.get(12..12 + name_size).ok_or(NoteError::BadNote)?;
        let desc = notes
            .get(desc_at..desc_at + desc_size)
            .ok_or(NoteError::BadNote)?;
        if name == NOTE_OWNER && kind == NT_SLOPOS_BASE_INTERFACE {
            let (Some(min), Some(max), 8) = (u32_at(desc, 0), u32_at(desc, 4), desc_size) else {
                return Err(NoteError::BadNote);
            };
            return if 0 < min && min <= max {
                Ok(min..=max)
            } else {
                Err(NoteError::BadNote)
            };
        }
        notes = notes.get(next..).unwrap_or_default();
    }
    Err(NoteError::NoNote)
}

#[cfg(test)]
mod tests;
