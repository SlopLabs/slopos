//! Relocatable x86_64 objects assembled from raw instruction bytes, for the
//! code gates' tests.

use std::path::Path;

use object::write::{Object as Builder, SectionId, StandardSection, Symbol, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
};

use super::{Ctx, Image};
use crate::repo::Repo;
/// A relocatable x86_64 object assembled from raw instruction bytes. Each
/// function sits at a 16-byte-aligned `.text` offset, padded with `int3`.
pub struct Elf {
    obj: Builder<'static>,
    text: SectionId,
    data: Option<SectionId>,
}

impl Elf {
    pub fn new() -> Self {
        let mut obj = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        Self {
            obj,
            text,
            data: None,
        }
    }

    fn symbol(&mut self, name: &str, value: u64, size: u64, kind: SymbolKind, section: SectionId) {
        self.obj.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value,
            size,
            kind,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
    }

    /// The `.text` offset the next function lands at.
    pub fn next(&self) -> u64 {
        self.obj.section(self.text).data().len() as u64
    }

    /// Raw code with no symbol, as hand-written asm can leave it.
    pub fn code(&mut self, bytes: &[u8]) -> u64 {
        let mut padded = bytes.to_vec();
        padded.resize(bytes.len().div_ceil(16).max(1) * 16, 0xcc);
        self.obj.append_section_data(self.text, &padded, 16)
    }

    pub fn function(&mut self, name: &str, bytes: &[u8]) -> u64 {
        let at = self.code(bytes);
        self.symbol(name, at, bytes.len() as u64, SymbolKind::Text, self.text);
        at
    }

    /// A size-0 label at `at`, as `global_asm!` and the linker script
    /// define them.
    pub fn label(&mut self, name: &str, at: u64) {
        self.symbol(name, at, 0, SymbolKind::Label, self.text);
    }

    /// A data object at `.data` offset `at`.
    pub fn object(&mut self, name: &str, at: u64, size: u64) {
        let data = *self
            .data
            .get_or_insert_with(|| self.obj.section_id(StandardSection::Data));
        let len = self.obj.section(data).data().len() as u64;
        if len < at + size {
            self.obj
                .append_section_data(data, &vec![0; (at + size - len) as usize], 1);
        }
        self.symbol(name, at, size, SymbolKind::Data, data);
    }

    pub fn section(&mut self, name: &str, len: usize) {
        let s = self.obj.add_section(
            Vec::new(),
            name.as_bytes().to_vec(),
            SectionKind::ReadOnlyData,
        );
        self.obj.append_section_data(s, &vec![0; len], 8);
    }

    pub fn finish(self) -> Vec<u8> {
        self.obj.write().unwrap()
    }
}

/// `e8 rel32` from `.text` offset `from` to `to`.
pub fn call(from: u64, to: u64) -> Vec<u8> {
    branch(0xe8, from, to)
}

/// `e9 rel32`: a tail call.
pub fn jmp(from: u64, to: u64) -> Vec<u8> {
    branch(0xe9, from, to)
}

fn branch(op: u8, from: u64, to: u64) -> Vec<u8> {
    let rel = (to as i64 - (from as i64 + 5)) as i32;
    let mut v = vec![op];
    v.extend_from_slice(&rel.to_le_bytes());
    v
}

pub fn with_image<T>(elf: &[u8], f: impl FnOnce(&Image) -> T) -> T {
    f(&Image::parse(elf).unwrap())
}

pub fn ctx<'a>(repo: &'a Repo, variant: &'a str, image: &'a Image<'a>) -> Ctx<'a> {
    Ctx {
        repo,
        path: Path::new("kernel.elf"),
        variant,
        image,
    }
}
