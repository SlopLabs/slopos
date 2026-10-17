//! What an x86-64 ELF file says about how it links: its type, its
//! interpreter, the libraries it needs, its soname, the names it binds and
//! those it exports, read from its program and section headers.

use alloc::vec::Vec;

const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const SHT_DYNAMIC: u32 = 6;
const SHT_DYNSYM: u32 = 11;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_SONAME: u64 = 14;
const SHN_UNDEF: u16 = 0;
const SHN_XINDEX: u16 = 0xffff;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STB_GNU_UNIQUE: u8 = 10;
const STT_TLS: u8 = 6;
/// What the loader binds a name to: no type, an object, a function, a
/// common block or thread-local storage.
const BINDABLE_TYPES: [u8; 5] = [0, 1, 2, 5, STT_TLS];
const STV_DEFAULT: u8 = 0;
const STV_PROTECTED: u8 = 3;
const EM_X86_64: u16 = 62;
const PHDR_LEN: usize = 56;
const SHDR_LEN: usize = 64;
const SYM_LEN: usize = 24;
const DYN_LEN: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Executable,
    Shared,
    /// A relocatable object or a core file: nothing the loader maps.
    Other,
}

#[derive(Clone, Debug)]
pub struct Linking<'a> {
    pub kind: Kind,
    pub interpreter: Option<&'a [u8]>,
    /// Whether it has a dynamic segment at all.
    pub dynamic: bool,
    pub needed: Vec<&'a [u8]>,
    pub soname: Option<&'a [u8]>,
    /// Undefined names it binds; weak ones may stay unbound.
    pub imports: Vec<&'a [u8]>,
    pub exports: Vec<&'a [u8]>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElfError {
    /// Not a 64-bit little-endian x86-64 ELF file.
    Foreign,
    /// A header or table that points past the file.
    Truncated,
    /// Dynamic, but with no section headers to read its symbols from.
    NoSections,
}

pub fn is_elf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, ElfError> {
    let bytes = b.get(at..at + 2).ok_or(ElfError::Truncated)?;
    Ok(u16::from_le_bytes(bytes.try_into().expect("2 bytes")))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, ElfError> {
    let bytes = b.get(at..at + 4).ok_or(ElfError::Truncated)?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("4 bytes")))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, ElfError> {
    let bytes = b.get(at..at + 8).ok_or(ElfError::Truncated)?;
    Ok(u64::from_le_bytes(bytes.try_into().expect("8 bytes")))
}

fn usize_at(b: &[u8], at: usize) -> Result<usize, ElfError> {
    usize::try_from(u64_at(b, at)?).map_err(|_| ElfError::Truncated)
}

/// `len` bytes from `offset`, wholly inside `b`.
fn span(b: &[u8], offset: usize, len: usize) -> Result<&[u8], ElfError> {
    b.get(offset..offset.checked_add(len).ok_or(ElfError::Truncated)?)
        .ok_or(ElfError::Truncated)
}

/// The NUL-terminated name at `at` in a string table.
fn name_in(table: &[u8], at: usize) -> Result<&[u8], ElfError> {
    let rest = table.get(at..).ok_or(ElfError::Truncated)?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(ElfError::Truncated)?;
    Ok(&rest[..end])
}

struct Section<'a> {
    kind: u32,
    data: &'a [u8],
    link: usize,
    entsize: usize,
}

struct Sections<'a> {
    elf: &'a [u8],
    offset: usize,
    count: usize,
}

impl<'a> Sections<'a> {
    fn of(elf: &'a [u8]) -> Result<Sections<'a>, ElfError> {
        let offset = usize_at(elf, 40)?;
        if offset == 0 {
            return Ok(Sections {
                elf,
                offset,
                count: 0,
            });
        }
        if usize::from(u16_at(elf, 58)?) != SHDR_LEN {
            return Err(ElfError::Truncated);
        }
        let count = match u16_at(elf, 60)? {
            0 => usize_at(span(elf, offset, SHDR_LEN)?, 32)?,
            n => usize::from(n),
        };
        span(
            elf,
            offset,
            count.checked_mul(SHDR_LEN).ok_or(ElfError::Truncated)?,
        )?;
        Ok(Sections { elf, offset, count })
    }

    fn get(&self, index: usize) -> Result<Section<'a>, ElfError> {
        if index >= self.count {
            return Err(ElfError::Truncated);
        }
        let header = span(self.elf, self.offset + index * SHDR_LEN, SHDR_LEN)?;
        let kind = u32_at(header, 4)?;
        let size = usize_at(header, 32)?;
        // A NOBITS section takes no room in the file.
        let data = if kind == 8 {
            &[][..]
        } else {
            span(self.elf, usize_at(header, 24)?, size)?
        };
        Ok(Section {
            kind,
            data,
            link: u32_at(header, 40)? as usize,
            entsize: usize_at(header, 56)?,
        })
    }

    fn find(&self, kind: u32) -> Result<Option<Section<'a>>, ElfError> {
        for index in 1..self.count {
            let section = self.get(index)?;
            if section.kind == kind {
                return Ok(Some(section));
            }
        }
        Ok(None)
    }
}

pub fn read(elf: &[u8]) -> Result<Linking<'_>, ElfError> {
    if elf.get(..6) != Some(b"\x7fELF\x02\x01") || u16_at(elf, 18)? != EM_X86_64 {
        return Err(ElfError::Foreign);
    }
    let kind = match u16_at(elf, 16)? {
        2 => Kind::Executable,
        3 => Kind::Shared,
        _ => Kind::Other,
    };
    let mut linking = Linking {
        kind,
        interpreter: None,
        dynamic: false,
        needed: Vec::new(),
        soname: None,
        imports: Vec::new(),
        exports: Vec::new(),
    };
    let phoff = usize_at(elf, 32)?;
    let phnum = usize::from(u16_at(elf, 56)?);
    if phnum != 0 && usize::from(u16_at(elf, 54)?) != PHDR_LEN {
        return Err(ElfError::Truncated);
    }
    for i in 0..phnum {
        let header = span(elf, phoff + i * PHDR_LEN, PHDR_LEN)?;
        match u32_at(header, 0)? {
            PT_INTERP => {
                let path = span(elf, usize_at(header, 8)?, usize_at(header, 32)?)?;
                linking.interpreter = Some(path.split(|&b| b == 0).next().unwrap_or_default());
            }
            PT_DYNAMIC => linking.dynamic = true,
            _ => {}
        }
    }
    if !linking.dynamic {
        return Ok(linking);
    }
    let sections = Sections::of(elf)?;
    if sections.count == 0 {
        return Err(ElfError::NoSections);
    }
    if let Some(dynamic) = sections.find(SHT_DYNAMIC)? {
        let strings = sections.get(dynamic.link)?.data;
        for entry in dynamic.data.chunks_exact(DYN_LEN) {
            let (tag, value) = (u64_at(entry, 0)?, usize_at(entry, 8)?);
            match tag {
                DT_NULL => break,
                DT_NEEDED => linking.needed.push(name_in(strings, value)?),
                DT_SONAME => linking.soname = Some(name_in(strings, value)?),
                _ => {}
            }
        }
    }
    if let Some(symbols) = sections.find(SHT_DYNSYM)? {
        if symbols.entsize != SYM_LEN {
            return Err(ElfError::Truncated);
        }
        let strings = sections.get(symbols.link)?.data;
        for symbol in symbols.data.chunks_exact(SYM_LEN).skip(1) {
            let name = name_in(strings, u32_at(symbol, 0)? as usize)?;
            let (bind, kind) = (symbol[4] >> 4, symbol[4] & 0xf);
            let visibility = symbol[5] & 3;
            let index = u16_at(symbol, 6)?;
            if name.is_empty() || ![STB_GLOBAL, STB_WEAK, STB_GNU_UNIQUE].contains(&bind) {
                continue;
            }
            if index == SHN_UNDEF {
                if bind != STB_WEAK {
                    linking.imports.push(name);
                }
            } else if index != SHN_XINDEX
                && BINDABLE_TYPES.contains(&kind)
                && (u64_at(symbol, 8)? != 0 || kind == STT_TLS)
                && [STV_DEFAULT, STV_PROTECTED].contains(&visibility)
            {
                linking.exports.push(name);
            }
        }
    }
    Ok(linking)
}

/// An ELF file for tests, with the interpreter, libraries, imports and
/// exports it is given and nothing a loader could map.
#[cfg(any(test, feature = "test-elf"))]
pub fn build(
    kind: Kind,
    interpreter: Option<&str>,
    needed: &[&str],
    imports: &[&str],
    exports: &[&str],
) -> Vec<u8> {
    let mut strings = alloc::vec![0u8];
    let mut name = |s: &str| {
        let at = strings.len() as u32;
        strings.extend_from_slice(s.as_bytes());
        strings.push(0);
        at
    };
    let needed: Vec<u32> = needed.iter().map(|n| name(n)).collect();
    let mut symbols = alloc::vec![0u8; SYM_LEN];
    for (list, defined) in [(imports, false), (exports, true)] {
        for symbol in list {
            let mut entry = [0u8; SYM_LEN];
            entry[..4].copy_from_slice(&name(symbol).to_le_bytes());
            entry[4] = (STB_GLOBAL << 4) | 2;
            if defined {
                entry[6..8].copy_from_slice(&1u16.to_le_bytes());
                entry[8..16].copy_from_slice(&0x1000u64.to_le_bytes());
            }
            symbols.extend_from_slice(&entry);
        }
    }
    let mut dynamic = Vec::new();
    for at in needed {
        dynamic.extend_from_slice(&DT_NEEDED.to_le_bytes());
        dynamic.extend_from_slice(&u64::from(at).to_le_bytes());
    }
    dynamic.extend_from_slice(&[0; DYN_LEN]);
    let interp = interpreter.map(|path| {
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        bytes
    });
    let phnum = 1 + usize::from(interp.is_some());
    let mut out = alloc::vec![0u8; 64 + phnum * PHDR_LEN];
    let place = |out: &mut Vec<u8>, data: &[u8]| {
        let at = out.len();
        out.extend_from_slice(data);
        (at, data.len())
    };
    let interp_at = interp.as_ref().map(|i| place(&mut out, i));
    let strings_at = place(&mut out, &strings);
    let symbols_at = place(&mut out, &symbols);
    let dynamic_at = place(&mut out, &dynamic);
    let shoff = out.len();
    let section =
        |out: &mut Vec<u8>, kind: u32, (at, len): (usize, usize), link: u32, entsize: u64| {
            let mut header = [0u8; SHDR_LEN];
            header[4..8].copy_from_slice(&kind.to_le_bytes());
            header[24..32].copy_from_slice(&(at as u64).to_le_bytes());
            header[32..40].copy_from_slice(&(len as u64).to_le_bytes());
            header[40..44].copy_from_slice(&link.to_le_bytes());
            header[56..64].copy_from_slice(&entsize.to_le_bytes());
            out.extend_from_slice(&header);
        };
    out.extend_from_slice(&[0; SHDR_LEN]);
    section(&mut out, 3, strings_at, 0, 0);
    section(&mut out, SHT_DYNSYM, symbols_at, 1, SYM_LEN as u64);
    section(&mut out, SHT_DYNAMIC, dynamic_at, 1, DYN_LEN as u64);
    out[..6].copy_from_slice(b"\x7fELF\x02\x01");
    let e_type: u16 = match kind {
        Kind::Executable => 2,
        Kind::Shared => 3,
        Kind::Other => 1,
    };
    out[16..18].copy_from_slice(&e_type.to_le_bytes());
    out[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
    out[32..40].copy_from_slice(&64u64.to_le_bytes());
    out[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
    out[54..56].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
    out[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
    out[58..60].copy_from_slice(&(SHDR_LEN as u16).to_le_bytes());
    out[60..62].copy_from_slice(&4u16.to_le_bytes());
    let mut phdr = |i: usize, kind: u32, (at, len): (usize, usize)| {
        let header = &mut out[64 + i * PHDR_LEN..64 + (i + 1) * PHDR_LEN];
        header[..4].copy_from_slice(&kind.to_le_bytes());
        header[8..16].copy_from_slice(&(at as u64).to_le_bytes());
        header[32..40].copy_from_slice(&(len as u64).to_le_bytes());
    };
    phdr(0, PT_DYNAMIC, dynamic_at);
    if let Some(at) = interp_at {
        phdr(1, PT_INTERP, at);
    }
    out
}

#[cfg(test)]
mod tests;
