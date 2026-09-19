//! ELF64 wire layout, and the `DT_*`/`R_X86_64_*` subset a loader reads.
//!
//! Values follow the gABI and its x86-64 supplement.

pub const EI_NIDENT: usize = 16;

pub const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
pub const ELFCLASS64: u8 = 2;
pub const ELFDATA2LSB: u8 = 1;
pub const EV_CURRENT: u8 = 1;

pub const ET_EXEC: u16 = 2;
pub const ET_DYN: u16 = 3;
pub const EM_X86_64: u16 = 0x3e;

pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PT_INTERP: u32 = 3;
pub const PT_PHDR: u32 = 6;
pub const PT_TLS: u32 = 7;
pub const PT_GNU_RELRO: u32 = 0x6474_e552;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

pub const DT_NULL: u64 = 0;
pub const DT_NEEDED: u64 = 1;
pub const DT_PLTRELSZ: u64 = 2;
pub const DT_PLTGOT: u64 = 3;
pub const DT_HASH: u64 = 4;
pub const DT_STRTAB: u64 = 5;
pub const DT_SYMTAB: u64 = 6;
pub const DT_RELA: u64 = 7;
pub const DT_RELASZ: u64 = 8;
pub const DT_RELAENT: u64 = 9;
pub const DT_STRSZ: u64 = 10;
pub const DT_SYMENT: u64 = 11;
pub const DT_INIT: u64 = 12;
pub const DT_FINI: u64 = 13;
pub const DT_SONAME: u64 = 14;
pub const DT_RPATH: u64 = 15;
pub const DT_SYMBOLIC: u64 = 16;
pub const DT_REL: u64 = 17;
pub const DT_PLTREL: u64 = 20;
pub const DT_DEBUG: u64 = 21;
pub const DT_TEXTREL: u64 = 22;
pub const DT_JMPREL: u64 = 23;
pub const DT_BIND_NOW: u64 = 24;
pub const DT_INIT_ARRAY: u64 = 25;
pub const DT_FINI_ARRAY: u64 = 26;
pub const DT_INIT_ARRAYSZ: u64 = 27;
pub const DT_FINI_ARRAYSZ: u64 = 28;
pub const DT_RUNPATH: u64 = 29;
pub const DT_FLAGS: u64 = 30;
pub const DT_PREINIT_ARRAY: u64 = 32;
pub const DT_PREINIT_ARRAYSZ: u64 = 33;
pub const DT_RELRSZ: u64 = 35;
pub const DT_RELR: u64 = 36;
pub const DT_RELRENT: u64 = 37;
pub const DT_GNU_HASH: u64 = 0x6fff_fef5;
pub const DT_FLAGS_1: u64 = 0x6fff_fffb;

pub const DF_SYMBOLIC: u64 = 0x02;
pub const DF_TEXTREL: u64 = 0x04;
pub const DF_BIND_NOW: u64 = 0x08;
pub const DF_1_NOW: u64 = 0x01;

pub const R_X86_64_NONE: u32 = 0;
pub const R_X86_64_64: u32 = 1;
pub const R_X86_64_PC32: u32 = 2;
pub const R_X86_64_COPY: u32 = 5;
pub const R_X86_64_GLOB_DAT: u32 = 6;
pub const R_X86_64_JUMP_SLOT: u32 = 7;
pub const R_X86_64_RELATIVE: u32 = 8;
pub const R_X86_64_32: u32 = 10;
pub const R_X86_64_32S: u32 = 11;
pub const R_X86_64_DTPMOD64: u32 = 16;
pub const R_X86_64_DTPOFF64: u32 = 17;
pub const R_X86_64_TPOFF64: u32 = 18;
pub const R_X86_64_IRELATIVE: u32 = 37;

pub const STB_GLOBAL: u8 = 1;
pub const STB_WEAK: u8 = 2;
pub const STB_GNU_UNIQUE: u8 = 10;

pub const STT_NOTYPE: u8 = 0;
pub const STT_OBJECT: u8 = 1;
pub const STT_FUNC: u8 = 2;
pub const STT_COMMON: u8 = 5;
pub const STT_TLS: u8 = 6;
pub const STT_GNU_IFUNC: u8 = 10;

pub const SHN_UNDEF: u16 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ehdr {
    pub e_ident: [u8; EI_NIDENT],
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
    pub e_shentsize: u16,
    pub e_shnum: u16,
    pub e_shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Dyn {
    pub d_tag: u64,
    pub d_val: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Sym {
    pub st_name: u32,
    pub st_info: u8,
    pub st_other: u8,
    pub st_shndx: u16,
    pub st_value: u64,
    pub st_size: u64,
}

impl Sym {
    #[inline]
    pub fn bind(&self) -> u8 {
        self.st_info >> 4
    }

    #[inline]
    pub fn sym_type(&self) -> u8 {
        self.st_info & 0xf
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Rela {
    pub r_offset: u64,
    pub r_info: u64,
    pub r_addend: i64,
}

impl Rela {
    #[inline]
    pub fn sym(&self) -> u32 {
        (self.r_info >> 32) as u32
    }

    #[inline]
    pub fn reloc_type(&self) -> u32 {
        self.r_info as u32
    }
}

/// `__tls_get_addr`'s argument, the pair a general-dynamic call site builds.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TlsIndex {
    pub ti_module: usize,
    pub ti_offset: usize,
}

pub const PAGE_SIZE: usize = 4096;

#[inline]
pub fn page_down(v: usize) -> usize {
    v & !(PAGE_SIZE - 1)
}

#[inline]
pub fn page_up(v: usize) -> usize {
    (v + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

#[inline]
pub fn align_up(v: usize, align: usize) -> usize {
    let a = if align == 0 { 1 } else { align };
    (v + a - 1) & !(a - 1)
}

/// `PF_*` as the `PROT_*` a mapping of the segment needs.
pub fn prot_of(flags: u32) -> u64 {
    use slopos_abi::syscall::{PROT_EXEC, PROT_READ, PROT_WRITE};
    let mut prot = 0;
    if flags & PF_R != 0 {
        prot |= PROT_READ;
    }
    if flags & PF_W != 0 {
        prot |= PROT_WRITE;
    }
    if flags & PF_X != 0 {
        prot |= PROT_EXEC;
    }
    prot
}
