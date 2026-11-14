//! What a kernel file carries for whoever installs it, found by name through
//! the section headers: the base interfaces it serves, from its note in
//! [`NOTE_SECTION`], and the words it appends to its loader's command line,
//! in [`CMDLINE_SECTION`]. Notes follow the ELF gABI: 4-byte aligned unless
//! the section asks for 8, as readelf reads them.

use core::ops::RangeInclusive;

use slopos_abi::base_interface::NT_SLOPOS_BASE_INTERFACE;

pub const NOTE_SECTION: &str = ".note.slopos.base-interface";
const NOTE_OWNER: &[u8] = b"SlopOS\0";

/// The section holding the words a kernel appends to its loader's command
/// line: [`CMDLINE_BYTES`] bytes, the words then NULs to its end, at least
/// one NUL. Rewritten in the linked file, so a slot's kernel carries its
/// machine's options whatever its loader entry says.
pub const CMDLINE_SECTION: &str = ".slopos.cmdline";
pub const CMDLINE_BYTES: usize = 512;

const SHT_PROGBITS: u32 = 1;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdlineError {
    NotElf,
    /// Section headers or a name table that lie outside the file.
    Truncated,
    NoSection,
    /// A section of that name that is not PROGBITS, not [`CMDLINE_BYTES`]
    /// long, or not words then NULs.
    BadSection,
    /// Words longer than `CMDLINE_BYTES - 1` bytes once joined.
    TooLong,
    /// A byte that is neither ASCII whitespace nor ASCII graphic.
    BadWord,
}

enum Malformed {
    NotElf,
    Truncated,
}

impl From<Malformed> for NoteError {
    fn from(error: Malformed) -> Self {
        match error {
            Malformed::NotElf => Self::NotElf,
            Malformed::Truncated => Self::Truncated,
        }
    }
}

impl From<Malformed> for CmdlineError {
    fn from(error: Malformed) -> Self {
        match error {
            Malformed::NotElf => Self::NotElf,
            Malformed::Truncated => Self::Truncated,
        }
    }
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
    offset: usize,
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
        offset,
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

/// The first section named `want`, of type `kind` when one is given.
fn find_section<'a>(
    elf: &'a [u8],
    want: &str,
    kind: Option<u32>,
) -> Result<Option<Section<'a>>, Malformed> {
    if !elf.starts_with(b"\x7fELF\x02\x01") {
        return Err(Malformed::NotElf);
    }
    let table = u64_at(elf, 0x28).ok_or(Malformed::Truncated)?;
    if table == 0 || u16_at(elf, 0x3A) != Some(SECTION_HEADER as u16) {
        return Err(Malformed::Truncated);
    }
    let zeroth = section(elf, table, 0).ok_or(Malformed::Truncated)?;
    let count = match u16_at(elf, 0x3C).ok_or(Malformed::Truncated)? {
        0 => zeroth.size,
        n => usize::from(n),
    };
    let names_at = match u16_at(elf, 0x3E).ok_or(Malformed::Truncated)? {
        SHN_XINDEX => zeroth.link as usize,
        n => usize::from(n),
    };
    let names = section(elf, table, names_at)
        .ok_or(Malformed::Truncated)?
        .data;
    for index in 0..count {
        let section = section(elf, table, index).ok_or(Malformed::Truncated)?;
        if kind.is_none_or(|kind| section.kind == kind) && name_is(names, section.name, want) {
            return Ok(Some(section));
        }
    }
    Ok(None)
}

/// The interfaces the kernel serves, as its note states them.
pub fn served_interfaces(elf: &[u8]) -> Result<RangeInclusive<u32>, NoteError> {
    let section = find_section(elf, NOTE_SECTION, Some(SHT_NOTE))?.ok_or(NoteError::NoNote)?;
    find_note(section.data, section.align)
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

/// The words the kernel's [`CMDLINE_SECTION`] holds (may be empty).
pub fn cmdline(elf: &[u8]) -> Result<&str, CmdlineError> {
    let section = find_section(elf, CMDLINE_SECTION, None)?.ok_or(CmdlineError::NoSection)?;
    if section.kind != SHT_PROGBITS {
        return Err(CmdlineError::BadSection);
    }
    cmdline_words(section.data)
}

/// Replace the words the kernel's [`CMDLINE_SECTION`] holds with `words`,
/// as [`cmdline_bytes`] spells them; nothing outside the section changes,
/// and nothing at all on an error.
pub fn stamp_cmdline(elf: &mut [u8], words: &str) -> Result<(), CmdlineError> {
    cmdline(elf)?;
    let offset = find_section(elf, CMDLINE_SECTION, None)?
        .ok_or(CmdlineError::NoSection)?
        .offset;
    let bytes = cmdline_bytes(words)?;
    elf[offset..offset + CMDLINE_BYTES].copy_from_slice(&bytes);
    Ok(())
}

/// Whether `words` would stamp, by [`cmdline_bytes`]'s rule.
pub fn cmdline_fits(words: &str) -> Result<(), CmdlineError> {
    cmdline_bytes(words).map(|_| ())
}

/// The section's bytes holding `words`: split on ASCII whitespace, each word
/// ASCII graphic (0x21..=0x7E), joined by single spaces, NUL-padded, at most
/// `CMDLINE_BYTES - 1` bytes of words. Const, so a kernel built with words
/// that break the rule does not compile.
pub const fn cmdline_bytes(words: &str) -> Result<[u8; CMDLINE_BYTES], CmdlineError> {
    let words = words.as_bytes();
    let mut out = [0u8; CMDLINE_BYTES];
    let mut len = 0;
    let mut gap = false;
    let mut at = 0;
    while at < words.len() {
        let byte = words[at];
        at += 1;
        if byte.is_ascii_whitespace() {
            gap = len != 0;
            continue;
        }
        if !byte.is_ascii_graphic() {
            return Err(CmdlineError::BadWord);
        }
        if len + if gap { 2 } else { 1 } > CMDLINE_BYTES - 1 {
            return Err(CmdlineError::TooLong);
        }
        if gap {
            out[len] = b' ';
            len += 1;
            gap = false;
        }
        out[len] = byte;
        len += 1;
    }
    Ok(out)
}

/// The words a [`CMDLINE_SECTION`]'s bytes hold, refused unless they are
/// exactly what [`cmdline_bytes`] writes.
pub fn cmdline_words(section: &[u8]) -> Result<&str, CmdlineError> {
    if section.len() != CMDLINE_BYTES {
        return Err(CmdlineError::BadSection);
    }
    let end = section
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(CmdlineError::BadSection)?;
    let (words, tail) = section.split_at(end);
    let normal = words.is_empty()
        || words
            .split(|&byte| byte == b' ')
            .all(|word| !word.is_empty() && word.iter().all(u8::is_ascii_graphic));
    if !normal || tail.iter().any(|&byte| byte != 0) {
        return Err(CmdlineError::BadSection);
    }
    core::str::from_utf8(words).map_err(|_| CmdlineError::BadSection)
}

#[cfg(test)]
mod tests;
