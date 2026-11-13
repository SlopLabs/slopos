//! `libc.so`'s dynamic symbol table in its canonical form.

use object::Endianness;
use object::elf::{
    SHN_UNDEF, STB_LOCAL, STT_COMMON, STT_FILE, STT_FUNC, STT_GNU_IFUNC, STT_NOTYPE, STT_OBJECT,
    STT_SECTION, STT_TLS,
};
use object::read::elf::{ElfFile64, Sym as _};

use crate::Result;

/// Every defined, non-local dynamic symbol, one canonical line each, sorted
/// bytewise.
pub(super) fn extract(data: &[u8]) -> Result<String> {
    let file =
        ElfFile64::<Endianness>::parse(data).map_err(|e| format!("not a 64-bit ELF file: {e}"))?;
    let endian = file.endian();
    let table = file.elf_dynamic_symbol_table();
    if table.is_empty() {
        return Err("no .dynsym".into());
    }
    let mut lines = Vec::new();
    for sym in table.symbols() {
        if sym.st_shndx(endian) == SHN_UNDEF || sym.st_bind() == STB_LOCAL {
            continue;
        }
        let name = table.symbol_name(endian, sym)?;
        if name.is_empty() {
            continue;
        }
        let name = String::from_utf8_lossy(name);
        let size = sym.st_size(endian);
        lines.push(match sym.st_type() {
            STT_FUNC | STT_GNU_IFUNC => format!("{name} func"),
            STT_OBJECT | STT_COMMON => format!("{name} object {size}"),
            STT_TLS => format!("{name} tls {size}"),
            other => format!("{name} {}", type_name(other)),
        });
    }
    lines.sort();
    Ok(lines.iter().map(|l| format!("{l}\n")).collect())
}

/// The type as readelf spells it, lowercased.
fn type_name(st_type: u8) -> String {
    match st_type {
        STT_NOTYPE => "notype".into(),
        STT_SECTION => "section".into(),
        STT_FILE => "file".into(),
        other => format!("{other:x}"),
    }
}

#[cfg(test)]
mod tests {
    use object::elf::{EM_X86_64, ET_DYN, SHF_ALLOC, SHF_EXECINSTR, SHF_TLS, SHF_WRITE};
    use object::elf::{SHT_PROGBITS, STB_GLOBAL};
    use object::write::elf::{FileHeader, SectionHeader, Sym, Writer};

    use super::*;

    /// A shared object's section headers and `.dynsym`: a local function, an
    /// undefined one, and a defined function, data object and TLS object.
    fn shared_object() -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = Writer::new(Endianness::Little, true, &mut out);
        w.reserve_file_header();

        let names = [
            "local_fn",
            "undefined_fn",
            "exported_fn",
            "exported_data",
            "exported_tls",
        ]
        .map(|n| w.add_dynamic_string(n.as_bytes()));
        for _ in names {
            w.reserve_dynamic_symbol_index();
        }
        w.reserve_dynsym();
        w.reserve_dynstr();

        w.reserve_null_section_index();
        let sections = [".text", ".data", ".tdata"].map(|n| {
            let name = w.add_section_name(n.as_bytes());
            (name, w.reserve_section_index())
        });
        w.reserve_dynsym_section_index();
        w.reserve_dynstr_section_index();
        w.reserve_shstrtab_section_index();
        w.reserve_shstrtab();
        w.reserve_section_headers();

        w.write_file_header(&FileHeader {
            os_abi: 0,
            abi_version: 0,
            e_type: ET_DYN,
            e_machine: EM_X86_64,
            e_entry: 0,
            e_flags: 0,
        })
        .unwrap();
        w.write_null_dynamic_symbol();
        let [text, data, tdata] = sections.map(|(_, index)| Some(index));
        let symbols = [
            (STB_LOCAL, STT_FUNC, text, 4),
            (STB_GLOBAL, STT_FUNC, None, 0),
            (STB_GLOBAL, STT_FUNC, text, 6),
            (STB_GLOBAL, STT_OBJECT, data, 24),
            (STB_GLOBAL, STT_TLS, tdata, 8),
        ];
        for (name, (bind, kind, section, size)) in names.into_iter().zip(symbols) {
            w.write_dynamic_symbol(&Sym {
                name: Some(name),
                section,
                st_info: (bind << 4) | kind,
                st_other: 0,
                st_shndx: SHN_UNDEF,
                st_value: 0,
                st_size: size,
            });
        }
        w.write_dynstr();
        w.write_shstrtab();
        w.write_null_section_header();
        let flags = [
            SHF_ALLOC | SHF_EXECINSTR,
            SHF_ALLOC | SHF_WRITE,
            SHF_ALLOC | SHF_WRITE | SHF_TLS,
        ];
        for ((name, _), flags) in sections.into_iter().zip(flags) {
            w.write_section_header(&SectionHeader {
                name: Some(name),
                sh_type: SHT_PROGBITS,
                sh_flags: u64::from(flags),
                sh_addr: 0,
                sh_offset: 0,
                sh_size: 0,
                sh_link: 0,
                sh_info: 0,
                sh_addralign: 1,
                sh_entsize: 0,
            });
        }
        w.write_dynsym_section_header(0, 2);
        w.write_dynstr_section_header(0);
        w.write_shstrtab_section_header();
        out
    }

    #[test]
    fn a_shared_object_yields_its_defined_exports_alone() {
        assert_eq!(
            extract(&shared_object()).unwrap(),
            "exported_data object 24\nexported_fn func\nexported_tls tls 8\n"
        );
    }

    #[test]
    fn a_file_that_is_not_elf_is_an_error() {
        assert!(extract(b"not an ELF file").is_err());
    }
}
