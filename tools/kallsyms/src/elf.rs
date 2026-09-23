//! The ELF64 `.symtab` of an executable, classified the way `llvm-nm` letters
//! it. Reads the section headers, the symbol table and its string table, and
//! nothing else — the kernel ELF is mostly debug info.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SHT_SYMTAB: u32 = 2;
const SHT_SYMTAB_SHNDX: u32 = 18;
const SHF_EXECINSTR: u64 = 0x4;

const SHN_UNDEF: u16 = 0;
const SHN_LORESERVE: u16 = 0xff00;
const SHN_ABS: u16 = 0xfff1;
const SHN_COMMON: u16 = 0xfff2;
const SHN_XINDEX: u16 = 0xffff;

const STB_LOCAL: u8 = 0;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STT_OBJECT: u8 = 1;
const STT_SECTION: u8 = 3;
const STT_FILE: u8 = 4;
const STT_COMMON: u8 = 5;
const STT_GNU_IFUNC: u8 = 10;

const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;

pub struct Symbol {
    pub addr: u64,
    pub size: u64,
    pub name: Vec<u8>,
}

struct Section {
    kind: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn read_at(file: &mut File, offset: u64, len: u64) -> Result<Vec<u8>, String> {
    let len = usize::try_from(len).map_err(|_| "section larger than memory".to_string())?;
    let mut buf = vec![0; len];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut buf))
        .map_err(|e| format!("reading {len} bytes at {offset:#x}: {e}"))?;
    Ok(buf)
}

/// Every defined symbol `llvm-nm` letters `t`, `T` or `W`: a local or global
/// symbol in an executable section, or a weak one that is not a data object.
pub fn text_symbols(path: &Path) -> Result<Vec<Symbol>, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let ehdr = read_at(&mut file, 0, 64)?;
    if &ehdr[..4] != b"\x7fELF" || ehdr[4] != 2 || ehdr[5] != 1 {
        return Err("not a little-endian ELF64 file".into());
    }
    if !matches!(u16_at(&ehdr, 16), 2 | 3) {
        return Err("not an executable or shared object".into());
    }
    let shoff = u64_at(&ehdr, 0x28);
    if shoff == 0 || usize::from(u16_at(&ehdr, 0x3a)) != SHDR_SIZE {
        return Err("no section header table".into());
    }
    let mut shnum = u64::from(u16_at(&ehdr, 0x3c));
    if shnum == 0 {
        shnum = u64_at(&read_at(&mut file, shoff, SHDR_SIZE as u64)?, 32);
    }
    let raw = read_at(&mut file, shoff, shnum * SHDR_SIZE as u64)?;
    let sections: Vec<Section> = raw
        .as_chunks::<SHDR_SIZE>()
        .0
        .iter()
        .map(|h| Section {
            kind: u32_at(h, 4),
            flags: u64_at(h, 8),
            offset: u64_at(h, 24),
            size: u64_at(h, 32),
            link: u32_at(h, 40),
        })
        .collect();

    let symtab = sections
        .iter()
        .find(|s| s.kind == SHT_SYMTAB)
        .ok_or("no .symtab: the kernel was linked stripped")?;
    let strtab = sections
        .get(symtab.link as usize)
        .ok_or(".symtab links to a section that does not exist")?;
    let strings = read_at(&mut file, strtab.offset, strtab.size)?;
    let syms = read_at(&mut file, symtab.offset, symtab.size)?;
    let shndx_table = match sections.iter().find(|s| s.kind == SHT_SYMTAB_SHNDX) {
        Some(s) => Some(read_at(&mut file, s.offset, s.size)?),
        None => None,
    };

    let mut out = Vec::new();
    for (index, sym) in syms.as_chunks::<SYM_SIZE>().0.iter().enumerate().skip(1) {
        let info = sym[4];
        let (bind, kind) = (info >> 4, info & 0xf);
        let shndx = u16_at(sym, 6);
        if kind == STT_SECTION || kind == STT_FILE || shndx == SHN_UNDEF {
            continue;
        }
        let section = if shndx == SHN_ABS || shndx == SHN_COMMON {
            None
        } else {
            let resolved = match shndx {
                SHN_XINDEX => shndx_table
                    .as_deref()
                    .and_then(|t| t.get(index * 4..index * 4 + 4))
                    .map(|b| u32_at(b, 0) as usize),
                n if n >= SHN_LORESERVE => None,
                n => Some(usize::from(n)),
            };
            match resolved.filter(|&i| i != 0).and_then(|i| sections.get(i)) {
                Some(s) => Some(s),
                None => continue,
            }
        };

        // llvm-nm's precedence: `i` for an ifunc, `W`/`V` for any weak symbol,
        // `C` for a common one; an absolute one has no section and is `a`.
        let text = match (kind, bind) {
            (STT_GNU_IFUNC, _) => false,
            (_, STB_WEAK) => kind != STT_OBJECT,
            (STT_COMMON, _) => false,
            (_, STB_LOCAL | STB_GLOBAL) => section.is_some_and(|s| s.flags & SHF_EXECINSTR != 0),
            _ => false,
        };
        if !text {
            continue;
        }

        let start = u32_at(sym, 0) as usize;
        let name = strings
            .get(start..)
            .and_then(|s| s.iter().position(|&b| b == 0).map(|end| &s[..end]))
            .ok_or_else(|| format!("symbol {index} names past the end of its string table"))?;
        out.push(Symbol {
            addr: u64_at(sym, 8),
            size: u64_at(sym, 16),
            name: name.to_vec(),
        });
    }
    Ok(out)
}
