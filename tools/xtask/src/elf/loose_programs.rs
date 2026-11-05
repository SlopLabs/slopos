//! Programs SlopOS ships outside its base, held to libc.so as their one interface.
//!
//! `libc.so`'s exports only ever grow. A program outside the base changes on
//! its own schedule, the base and the kernel on theirs, so it may not depend
//! on what the base interface number guards: a system call made in place
//! reaches the private call surface, or a Linux one, without the library
//! between. Each program must be dynamic (carry a `PT_INTERP`; a static one
//! has slibc's own copy linked in), name `/lib/ld-slopos.so.1` as that
//! interpreter, list `libc.so` in `DT_NEEDED`, and hold no `syscall`
//! instruction in its own code.

use std::path::Path;

use iced_x86::{Decoder, DecoderOptions, Mnemonic};
use object::Endianness;
use object::elf::{DT_NEEDED, DT_STRSZ, DT_STRTAB, PF_X, PT_LOAD, SHF_EXECINSTR, SHT_NOBITS};
use object::read::elf::{Dyn as _, ElfFile64, ProgramHeader as _, SectionHeader as _};

use crate::Result;
use crate::report::{self, Report};

const INTERP: &[u8] = b"/lib/ld-slopos.so.1";

/// `xtask programs PROGRAM...`.
pub fn main(args: &[String]) -> Result<bool> {
    if args.is_empty() {
        return Err("usage: xtask programs PROGRAM...".into());
    }
    let mut report = Report::new();
    for program in args {
        let path = Path::new(program);
        if !path.is_file() {
            return Err(format!("{program}: no such file").into());
        }
        let messages = match std::fs::read(path) {
            Ok(data) => findings(&data),
            Err(e) => vec![format!("unreadable: {e}")],
        };
        for message in messages {
            report.finding(program.as_str(), message);
        }
    }
    report.summary(format!(
        "{} program(s) reach the system through libc.so alone",
        args.len()
    ));
    Ok(report::print_all(&[("loose-programs", report)]))
}

/// What one program breaks, one message each; nothing when it holds.
fn findings(data: &[u8]) -> Vec<String> {
    let file = match ElfFile64::<Endianness>::parse(data) {
        Ok(file) => file,
        Err(e) => return vec![format!("not a 64-bit ELF file this gate reads: {e}")],
    };
    match check(&file, data) {
        Ok(found) => found,
        Err(e) => vec![format!("malformed ELF: {e}")],
    }
}

fn check(file: &ElfFile64<Endianness>, data: &[u8]) -> Result<Vec<String>> {
    let endian = file.endian();
    let headers = file.elf_program_headers();
    let mut found = Vec::new();

    let mut interp = None;
    for ph in headers {
        if let Some(i) = ph.interpreter(endian, data)? {
            interp = Some(i);
        }
    }
    match interp {
        None => found.push(
            "static, with no PT_INTERP: it carries its own C library — link it dynamically against libc.so".to_string(),
        ),
        Some(i) if i != INTERP => found.push(format!(
            "its interpreter is {}, not {} — link with --dynamic-linker={}",
            String::from_utf8_lossy(i),
            String::from_utf8_lossy(INTERP),
            String::from_utf8_lossy(INTERP)
        )),
        Some(_) => {}
    }

    if !needed(file, data)?.iter().any(|&n| n == b"libc.so") {
        found.push(
            "libc.so is not among its DT_NEEDED — link it against libc.so, the one interface a program outside the base may use".to_string(),
        );
    }

    let calls = syscalls(file, data)?;
    if calls > 0 {
        found.push(format!(
            "{calls} syscall instruction(s) of its own — reach the system through libc.so instead"
        ));
    }
    Ok(found)
}

/// The `DT_NEEDED` names, read through `PT_DYNAMIC` the way the loader does.
fn needed<'d>(file: &ElfFile64<'d, Endianness>, data: &'d [u8]) -> Result<Vec<&'d [u8]>> {
    let endian = file.endian();
    let headers = file.elf_program_headers();
    let mut names = Vec::new();
    for ph in headers {
        let Some(dynamic) = ph.dynamic(endian, data)? else {
            continue;
        };
        let (mut strtab, mut strsz, mut offsets) = (None, None, Vec::new());
        for d in dynamic {
            match d.tag(endian) {
                DT_NEEDED => offsets.push(d.val(endian)),
                DT_STRTAB => strtab = Some(d.val(endian)),
                DT_STRSZ => strsz = Some(d.val(endian)),
                _ => {}
            }
        }
        if offsets.is_empty() {
            continue;
        }
        let (Some(strtab), Some(strsz)) = (strtab, strsz) else {
            return Err("DT_NEEDED without DT_STRTAB and DT_STRSZ".into());
        };
        let strings = headers
            .iter()
            .filter(|p| p.p_type(endian) == PT_LOAD)
            .find_map(|p| p.data_range(endian, data, strtab, strsz).ok().flatten())
            .ok_or("DT_STRTAB lies outside every PT_LOAD")?;
        for offset in offsets {
            let tail = usize::try_from(offset)
                .ok()
                .and_then(|o| strings.get(o..))
                .ok_or("DT_NEEDED points past DT_STRTAB")?;
            let len = tail
                .iter()
                .position(|&b| b == 0)
                .ok_or("DT_NEEDED name is unterminated")?;
            names.push(&tail[..len]);
        }
    }
    Ok(names)
}

/// `syscall` instructions in the executable sections, or in the executable
/// segments of an image that carries no section headers.
fn syscalls(file: &ElfFile64<Endianness>, data: &[u8]) -> Result<usize> {
    let endian = file.endian();
    let mut code: Vec<(u64, &[u8])> = Vec::new();
    let sections = file.elf_section_table();
    if sections.is_empty() {
        for ph in file.elf_program_headers() {
            if ph.p_type(endian) == PT_LOAD && ph.p_flags(endian) & PF_X != 0 {
                let bytes = ph
                    .data(endian, data)
                    .map_err(|()| "executable segment lies outside the file")?;
                code.push((ph.p_vaddr(endian), bytes));
            }
        }
    } else {
        for sh in sections.iter() {
            if sh.sh_flags(endian) & u64::from(SHF_EXECINSTR) != 0
                && sh.sh_type(endian) != SHT_NOBITS
            {
                code.push((sh.sh_addr(endian), sh.data(endian, data)?));
            }
        }
    }
    Ok(code
        .into_iter()
        .map(|(ip, bytes)| {
            Decoder::with_ip(64, bytes, ip, DecoderOptions::NONE)
                .iter()
                .filter(|i| i.mnemonic() == Mnemonic::Syscall)
                .count()
        })
        .sum())
}

#[cfg(test)]
mod tests {
    use object::elf::{
        DT_NULL, EM_X86_64, ET_EXEC, PF_R, PT_DYNAMIC, PT_INTERP, SHF_ALLOC, SHT_PROGBITS,
    };
    use object::write::elf::{FileHeader, ProgramHeader, SectionHeader, Writer};

    use super::*;
    use crate::fixture::Tree;

    const BASE: u64 = 0x40_0000;
    /// `call rel32; ret`: what `_start` calling `f` through the PLT reduces to.
    const CALLS: &[u8] = &[0xe8, 0, 0, 0, 0, 0xc3];
    /// The same with an inline `syscall` before the `ret`.
    const RAW: &[u8] = &[0xe8, 0, 0, 0, 0, 0x0f, 0x05, 0xc3];

    struct Program<'a> {
        interp: Option<&'a str>,
        needed: &'a [&'a str],
        text: &'a [u8],
        sections: bool,
    }

    /// A linked x86_64 executable: one R+X `PT_LOAD` over the whole file,
    /// and `PT_INTERP`/`PT_DYNAMIC` when asked for.
    fn link(p: &Program) -> Vec<u8> {
        let mut out = Vec::new();
        let mut w = Writer::new(Endianness::Little, true, &mut out);
        let dynamic = !p.needed.is_empty();
        let interp = p.interp.map(|i| [i.as_bytes(), b"\0"].concat());

        w.reserve_file_header();
        w.reserve_program_headers(1 + u32::from(interp.is_some()) + u32::from(dynamic));
        let interp_off = interp.as_ref().map_or(0, |b| w.reserve(b.len(), 1));
        let text_off = w.reserve(p.text.len(), 16);
        let ids: Vec<_> = p
            .needed
            .iter()
            .map(|n| w.add_dynamic_string(n.as_bytes()))
            .collect();
        let ndyn = p.needed.len() + 3;
        let (mut dynstr_off, mut dyn_off, mut strsz) = (0, 0, 0);
        if dynamic {
            dynstr_off = w.reserve_dynstr();
            strsz = w.dynstr_len();
            dyn_off = w.reserve_dynamic(ndyn);
        }
        let mut text_name = None;
        if p.sections {
            w.reserve_null_section_index();
            text_name = Some(w.add_section_name(b".text"));
            w.reserve_section_index();
            if dynamic {
                w.reserve_dynstr_section_index();
                w.reserve_dynamic_section_index();
            }
            w.reserve_shstrtab_section_index();
            w.reserve_shstrtab();
            w.reserve_section_headers();
        }
        let end = w.reserved_len() as u64;

        w.write_file_header(&FileHeader {
            os_abi: 0,
            abi_version: 0,
            e_type: ET_EXEC,
            e_machine: EM_X86_64,
            e_entry: BASE + text_off as u64,
            e_flags: 0,
        })
        .unwrap();
        w.write_align_program_headers();
        let segment = |p_type, off: usize, size: u64| ProgramHeader {
            p_type,
            p_flags: PF_R,
            p_offset: off as u64,
            p_vaddr: BASE + off as u64,
            p_paddr: BASE + off as u64,
            p_filesz: size,
            p_memsz: size,
            p_align: 1,
        };
        w.write_program_header(&ProgramHeader {
            p_flags: PF_R | PF_X,
            p_align: 0x1000,
            ..segment(PT_LOAD, 0, end)
        });
        if let Some(b) = &interp {
            w.write_program_header(&segment(PT_INTERP, interp_off, b.len() as u64));
        }
        if dynamic {
            w.write_program_header(&segment(PT_DYNAMIC, dyn_off, ndyn as u64 * 16));
        }
        if let Some(b) = &interp {
            w.write(b);
        }
        w.write_align(16);
        w.write(p.text);
        if dynamic {
            w.write_dynstr();
            w.write_align_dynamic();
            for id in ids {
                w.write_dynamic_string(DT_NEEDED, id).unwrap();
            }
            w.write_dynamic(DT_STRTAB, BASE + dynstr_off as u64)
                .unwrap();
            w.write_dynamic(DT_STRSZ, strsz as u64).unwrap();
            w.write_dynamic(DT_NULL, 0).unwrap();
        }
        if p.sections {
            w.write_shstrtab();
            w.write_null_section_header();
            w.write_section_header(&SectionHeader {
                name: text_name,
                sh_type: SHT_PROGBITS,
                sh_flags: u64::from(SHF_ALLOC | SHF_EXECINSTR),
                sh_addr: BASE + text_off as u64,
                sh_offset: text_off as u64,
                sh_size: p.text.len() as u64,
                sh_link: 0,
                sh_info: 0,
                sh_addralign: 16,
                sh_entsize: 0,
            });
            if dynamic {
                w.write_dynstr_section_header(BASE + dynstr_off as u64);
                w.write_dynamic_section_header(BASE + dyn_off as u64);
            }
            w.write_shstrtab_section_header();
        }
        out
    }

    fn good() -> Program<'static> {
        Program {
            interp: Some("/lib/ld-slopos.so.1"),
            needed: &["libc.so"],
            text: CALLS,
            sections: true,
        }
    }

    #[test]
    fn a_program_on_libc_so_alone_holds() {
        assert_eq!(findings(&link(&good())), Vec::<String>::new());
    }

    #[test]
    fn a_static_program_is_rejected() {
        let found = findings(&link(&Program {
            interp: None,
            needed: &[],
            ..good()
        }));
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].starts_with("static, with no PT_INTERP"));
        assert!(found[1].starts_with("libc.so is not among its DT_NEEDED"));
    }

    #[test]
    fn another_interpreter_is_rejected() {
        let found = findings(&link(&Program {
            interp: Some("/lib64/ld-linux-x86-64.so.2"),
            ..good()
        }));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with(
            "its interpreter is /lib64/ld-linux-x86-64.so.2, not /lib/ld-slopos.so.1"
        ));
    }

    #[test]
    fn a_program_without_libc_so_is_rejected() {
        let found = findings(&link(&Program {
            needed: &["libother.so"],
            ..good()
        }));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("libc.so is not among its DT_NEEDED"));
    }

    #[test]
    fn libc_so_among_others_holds() {
        let found = findings(&link(&Program {
            needed: &["libother.so", "libc.so"],
            ..good()
        }));
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn a_syscall_instruction_is_rejected() {
        let found = findings(&link(&Program {
            text: RAW,
            ..good()
        }));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("1 syscall instruction(s) of its own"));
    }

    #[test]
    fn a_syscall_in_an_image_without_section_headers_is_rejected() {
        let found = findings(&link(&Program {
            text: RAW,
            sections: false,
            ..good()
        }));
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("1 syscall instruction(s)"));
    }

    #[test]
    fn syscall_bytes_inside_another_instruction_do_not_count() {
        let found = findings(&link(&Program {
            text: &[0xb8, 0x0f, 0x05, 0, 0, 0xc3],
            ..good()
        }));
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn a_non_elf_input_is_rejected() {
        let found = findings(b"#!/bin/sh\necho hi\n");
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("not a 64-bit ELF file"));
    }

    fn write(tree: &Tree, name: &str, bytes: &[u8]) -> String {
        let path = tree.repo.path(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn the_command_passes_good_programs_and_fails_one_bad_among_them() {
        let tree = Tree::new(&[]);
        let good = write(&tree, "good", &link(&good()));
        let raw = write(
            &tree,
            "raw",
            &link(&Program {
                text: RAW,
                ..super::tests::good()
            }),
        );
        assert!(main(std::slice::from_ref(&good)).unwrap());
        assert!(!main(&[good, raw]).unwrap());
    }

    #[test]
    fn the_command_refuses_no_program_or_a_missing_one() {
        assert!(main(&[]).unwrap_err().to_string().contains("usage"));
        let tree = Tree::new(&[]);
        let missing = tree.repo.path("nope").to_string_lossy().into_owned();
        assert!(
            main(&[missing])
                .unwrap_err()
                .to_string()
                .contains("no such file")
        );
    }
}
