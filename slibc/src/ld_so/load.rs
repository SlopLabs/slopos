//! Mapping a shared object out of a file.

use core::ptr;

use slopos_abi::syscall::{
    MAP_ANONYMOUS, MAP_FIXED, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE,
};

use crate::pal::{Pal, Sys};

use super::dso::Dso;
use super::elf::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadError {
    NotFound,
    Io,
    Malformed,
    NoMemory,
}

const O_RDONLY: i32 = 0;
const AT_FDCWD: i32 = -100;

/// The header window a mapping decision is made from: the ELF header plus
/// every program header, which is what `PT_LOAD`, `PT_DYNAMIC`, `PT_TLS` and
/// `PT_GNU_RELRO` live in.
pub const HEADER_WINDOW: usize = size_of::<Ehdr>() + MAX_PHNUM * size_of::<Phdr>();

/// Program headers one object may declare. Every linker in this tree emits
/// fewer than a dozen; the bound is what keeps the window a fixed array.
pub const MAX_PHNUM: usize = 64;

pub struct Mapped {
    pub base: usize,
    pub span_start: usize,
    pub span_len: usize,
    pub phdr: *const Phdr,
    pub phnum: usize,
    pub entry: usize,
}

struct Fd(i32);

impl Drop for Fd {
    fn drop(&mut self) {
        let _ = Sys::close(self.0);
    }
}

/// Map every `PT_LOAD` of `path` at a kernel-chosen base.
///
/// The whole span is reserved `PROT_NONE` first and each segment then mapped
/// `MAP_FIXED` inside it, so no other mapping can land in a hole between two
/// segments. Writable segments stay writable until relocation is done; the
/// caller calls [`protect`] afterwards.
///
/// # Safety
/// Writes into freshly created mappings only.
pub unsafe fn map_object(
    path: *const u8,
    window: &mut [u8; HEADER_WINDOW],
) -> Result<Mapped, LoadError> {
    let fd = Fd(Sys::openat(AT_FDCWD, path, O_RDONLY, 0).map_err(|_| LoadError::NotFound)?);
    read_exact_at(fd.0, 0, &mut window[..size_of::<Ehdr>()])?;

    let ehdr = ptr::read_unaligned(window.as_ptr() as *const Ehdr);
    if ehdr.e_ident[0..4] != ELF_MAGIC
        || ehdr.e_ident[4] != ELFCLASS64
        || ehdr.e_ident[5] != ELFDATA2LSB
        || ehdr.e_ident[6] != EV_CURRENT
        || ehdr.e_machine != EM_X86_64
        || ehdr.e_type != ET_DYN
        || ehdr.e_phentsize as usize != size_of::<Phdr>()
        || ehdr.e_phnum as usize > MAX_PHNUM
        || ehdr.e_phnum == 0
    {
        return Err(LoadError::Malformed);
    }

    let phnum = ehdr.e_phnum as usize;
    let table_len = phnum * size_of::<Phdr>();
    read_exact_at(fd.0, ehdr.e_phoff as usize, &mut window[..table_len])?;
    let phdrs = window.as_ptr() as *const Phdr;

    let (low, high) = span_of(phdrs, phnum)?;
    let span_len = high - low;
    let span = Sys::mmap(
        ptr::null_mut(),
        span_len,
        PROT_NONE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    )
    .map_err(|_| LoadError::NoMemory)?;
    let base = (span as usize).wrapping_sub(low);

    for i in 0..phnum {
        let ph = ptr::read_unaligned(phdrs.add(i));
        if ph.p_type != PT_LOAD {
            continue;
        }
        map_segment(fd.0, base, &ph).inspect_err(|_| {
            let _ = Sys::munmap(span, span_len);
        })?;
    }

    // The program headers the object sees have to be the mapped ones: the
    // window is a stack buffer this call is about to drop, and `dl_iterate_phdr`
    // hands them to an unwinder long afterwards.
    let phdr = mapped_phdrs(base, phdrs, phnum, ehdr.e_phoff as usize)
        .ok_or(LoadError::Malformed)
        .inspect_err(|_| {
            let _ = Sys::munmap(span, span_len);
        })?;

    Ok(Mapped {
        base,
        span_start: span as usize,
        span_len,
        phdr,
        phnum,
        entry: base.wrapping_add(ehdr.e_entry as usize),
    })
}

/// Give each `PT_LOAD` its declared protection.
///
/// # Safety
/// `dso` must name an object this module mapped.
pub unsafe fn protect_segments(dso: &Dso) -> Result<(), LoadError> {
    for i in 0..dso.phnum {
        let ph = ptr::read_unaligned(dso.phdr.add(i));
        if ph.p_type != PT_LOAD {
            continue;
        }
        let start = page_down(dso.base + ph.p_vaddr as usize);
        let end = page_up(dso.base + ph.p_vaddr as usize + ph.p_memsz as usize);
        Sys::mprotect(start as *mut u8, end - start, prot_of(ph.p_flags))
            .map_err(|_| LoadError::NoMemory)?;
    }
    Ok(())
}

/// Seal `PT_GNU_RELRO`. Eager binding is what makes this total: nothing
/// writes a GOT slot after relocation, so the whole segment can go read-only
/// rather than stopping short of `.got.plt`.
///
/// # Safety
/// `dso` must be relocated.
pub unsafe fn protect_relro(dso: &Dso) -> Result<(), LoadError> {
    if dso.relro_end <= dso.relro_start {
        return Ok(());
    }
    Sys::mprotect(
        dso.relro_start as *mut u8,
        dso.relro_end - dso.relro_start,
        PROT_READ,
    )
    .map_err(|_| LoadError::NoMemory)
}

fn span_of(phdrs: *const Phdr, phnum: usize) -> Result<(usize, usize), LoadError> {
    let mut low = usize::MAX;
    let mut high = 0usize;
    for i in 0..phnum {
        let ph = unsafe { ptr::read_unaligned(phdrs.add(i)) };
        if ph.p_type != PT_LOAD {
            continue;
        }
        let start = page_down(ph.p_vaddr as usize);
        let end = page_up(ph.p_vaddr as usize + ph.p_memsz as usize);
        if start < low {
            low = start;
        }
        if end > high {
            high = end;
        }
    }
    if low == usize::MAX || high <= low {
        return Err(LoadError::Malformed);
    }
    Ok((low, high))
}

/// Where the program-header table ended up, given the segment whose file
/// extent covers it.
unsafe fn mapped_phdrs(
    base: usize,
    phdrs: *const Phdr,
    phnum: usize,
    phoff: usize,
) -> Option<*const Phdr> {
    let table_end = phoff + phnum * size_of::<Phdr>();
    for i in 0..phnum {
        let ph = ptr::read_unaligned(phdrs.add(i));
        if ph.p_type != PT_LOAD {
            continue;
        }
        let file_start = ph.p_offset as usize;
        let file_end = file_start + ph.p_filesz as usize;
        if phoff >= file_start && table_end <= file_end {
            return Some((base + ph.p_vaddr as usize + (phoff - file_start)) as *const Phdr);
        }
    }
    None
}

/// Map one `PT_LOAD`, writable regardless of its declared protection:
/// relocations land in it, and `.bss`'s partial page has to be zeroed.
unsafe fn map_segment(fd: i32, base: usize, ph: &Phdr) -> Result<(), LoadError> {
    let vaddr = base + ph.p_vaddr as usize;
    let page_off = vaddr - page_down(vaddr);
    let start = page_down(vaddr);
    let file_end = vaddr + ph.p_filesz as usize;
    let mem_end = vaddr + ph.p_memsz as usize;
    let prot = prot_of(ph.p_flags) | PROT_READ | PROT_WRITE;

    if ph.p_filesz > 0 {
        let len = page_up(page_off + ph.p_filesz as usize);
        Sys::mmap(
            start as *mut u8,
            len,
            prot,
            MAP_PRIVATE | MAP_FIXED,
            fd,
            (ph.p_offset as usize - page_off) as u64,
        )
        .map_err(|_| LoadError::NoMemory)?;
    }

    // The tail of the last file page belongs to `.bss`, and a file mapping
    // brought the next file bytes along with it.
    if mem_end > file_end {
        let tail = page_up(file_end);
        if ph.p_filesz > 0 && tail > file_end {
            ptr::write_bytes(file_end as *mut u8, 0, (tail.min(mem_end)) - file_end);
        }
        let anon_start = if ph.p_filesz > 0 { tail } else { start };
        let anon_end = page_up(mem_end);
        if anon_end > anon_start {
            Sys::mmap(
                anon_start as *mut u8,
                anon_end - anon_start,
                prot,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                -1,
                0,
            )
            .map_err(|_| LoadError::NoMemory)?;
        }
    }
    Ok(())
}

fn read_exact_at(fd: i32, offset: usize, buf: &mut [u8]) -> Result<(), LoadError> {
    let mut done = 0usize;
    while done < buf.len() {
        let got = Sys::pread64(
            fd,
            unsafe { buf.as_mut_ptr().add(done) },
            buf.len() - done,
            (offset + done) as i64,
        )
        .map_err(|_| LoadError::Io)?;
        if got == 0 {
            return Err(LoadError::Malformed);
        }
        done += got;
    }
    Ok(())
}
