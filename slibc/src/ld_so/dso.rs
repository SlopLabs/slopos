//! One loaded object: what its `PT_DYNAMIC` says, and where it landed.

use core::ptr;

use super::elf::*;

/// Objects one process may hold. A miss is `ENOMEM` out of `dlopen`, not a
/// reallocation: the table is what `dl_iterate_phdr` walks and what a handle
/// indexes, so its entries have to keep their addresses.
pub const DL_MAX_OBJECTS: usize = 128;

/// `DT_NEEDED` entries recorded per object, which is what `dlclose` walks to
/// release a subtree.
pub const DL_MAX_NEEDED: usize = 32;

/// A `DT_NEEDED` slot no dependency has landed in yet, and the `loader` of an
/// object nothing loaded (the executable, the interpreter).
pub const NEEDED_UNRESOLVED: u16 = u16::MAX;

pub const DSO_USED: u32 = 1 << 0;
pub const DSO_GLOBAL: u32 = 1 << 1;
pub const DSO_RELOCATED: u32 = 1 << 2;
pub const DSO_INITED: u32 = 1 << 3;
pub const DSO_DLOPENED: u32 = 1 << 4;
pub const DSO_SYMBOLIC: u32 = 1 << 5;
/// Condemned by `plan_release` and not yet unmapped: its destructors are
/// running, so it must stop answering lookups and stop taking references.
pub const DSO_DYING: u32 = 1 << 7;
/// The interpreter, whose `R_X86_64_RELATIVE` and `DT_RELR` were applied by
/// its own bootstrap. `DT_RELR` is a read-modify-write, so applying it twice
/// adds the load bias twice.
pub const DSO_BOOTSTRAPPED: u32 = 1 << 6;

#[derive(Clone, Copy)]
pub struct Dso {
    pub flags: u32,
    pub refs: u32,

    /// Load bias: 0 for an `ET_EXEC` image, the mapping address otherwise.
    pub base: usize,
    /// Whole reserved span, for `munmap` on unload. Zero for an image the
    /// kernel mapped (the executable and the interpreter).
    pub map_start: usize,
    pub map_len: usize,

    /// Owned NUL-terminated path the object was opened by. For the executable
    /// it is the kernel's `AT_EXECFN` string, borrowed; null if there was none.
    pub path: *mut u8,
    /// `DT_SONAME`, else the basename of `path`. Borrowed.
    pub name: *const u8,

    pub phdr: *const Phdr,
    pub phnum: usize,
    pub dynamic: *const Dyn,

    pub strtab: *const u8,
    pub symtab: *const Sym,
    pub nsyms: usize,
    pub gnu_hash: *const u32,
    pub sysv_hash: *const u32,

    pub rela: *const Rela,
    pub rela_count: usize,
    pub jmprel: *const Rela,
    pub jmprel_count: usize,
    pub relr: *const usize,
    pub relr_count: usize,

    pub init: usize,
    pub init_array: *const usize,
    pub init_count: usize,
    pub fini: usize,
    pub fini_array: *const usize,
    pub fini_count: usize,
    pub preinit_array: *const usize,
    pub preinit_count: usize,

    pub tls_modid: usize,
    pub tls_image: usize,
    pub tls_filesz: usize,
    pub tls_memsz: usize,
    pub tls_align: usize,
    /// Distance below the thread pointer, for a module in the static block.
    pub tls_offset: usize,

    pub relro_start: usize,
    pub relro_end: usize,

    /// Table indices of the objects this one's `DT_NEEDED` entries resolved
    /// to, in declaration order. [`NEEDED_UNRESOLVED`] until each is loaded:
    /// zero is a real index — the executable's.
    pub needed: [u16; DL_MAX_NEEDED],
    pub needed_count: u16,

    /// `DT_RPATH` and `DT_RUNPATH` out of the string table, or null.
    pub rpath: *const u8,
    pub runpath: *const u8,
    /// The object whose `DT_NEEDED` or `dlopen` call loaded this one: a search
    /// from this one falls back to its `DT_RPATH`.
    pub loader: u16,
}

unsafe impl Send for Dso {}
unsafe impl Sync for Dso {}

impl Dso {
    pub const fn empty() -> Self {
        Self {
            flags: 0,
            refs: 0,
            base: 0,
            map_start: 0,
            map_len: 0,
            path: ptr::null_mut(),
            name: ptr::null(),
            phdr: ptr::null(),
            phnum: 0,
            dynamic: ptr::null(),
            strtab: ptr::null(),
            symtab: ptr::null(),
            nsyms: 0,
            gnu_hash: ptr::null(),
            sysv_hash: ptr::null(),
            rela: ptr::null(),
            rela_count: 0,
            jmprel: ptr::null(),
            jmprel_count: 0,
            relr: ptr::null(),
            relr_count: 0,
            init: 0,
            init_array: ptr::null(),
            init_count: 0,
            fini: 0,
            fini_array: ptr::null(),
            fini_count: 0,
            preinit_array: ptr::null(),
            preinit_count: 0,
            tls_modid: 0,
            tls_image: 0,
            tls_filesz: 0,
            tls_memsz: 0,
            tls_align: 0,
            tls_offset: 0,
            relro_start: 0,
            relro_end: 0,
            needed: [NEEDED_UNRESOLVED; DL_MAX_NEEDED],
            needed_count: 0,
            rpath: ptr::null(),
            runpath: ptr::null(),
            loader: NEEDED_UNRESOLVED,
        }
    }

    #[inline]
    pub fn str_at(&self, off: u32) -> *const u8 {
        unsafe { self.strtab.add(off as usize) }
    }

    /// Whether `addr` falls inside one of this object's `PT_LOAD` extents.
    pub fn contains(&self, addr: usize) -> bool {
        for i in 0..self.phnum {
            let ph = unsafe { ptr::read_unaligned(self.phdr.add(i)) };
            if ph.p_type != PT_LOAD {
                continue;
            }
            let start = self.base + ph.p_vaddr as usize;
            if addr >= start && addr < start + ph.p_memsz as usize {
                return true;
            }
        }
        false
    }

    /// Record the program headers and derive `PT_DYNAMIC`, `PT_TLS` and
    /// `PT_GNU_RELRO` from them.
    ///
    /// # Safety
    /// `phdr` must address `phnum` program headers of an image loaded at
    /// `self.base`.
    pub unsafe fn scan_phdrs(&mut self, phdr: *const Phdr, phnum: usize) {
        self.phdr = phdr;
        self.phnum = phnum;
        for i in 0..phnum {
            let ph = ptr::read_unaligned(phdr.add(i));
            match ph.p_type {
                PT_DYNAMIC => self.dynamic = (self.base + ph.p_vaddr as usize) as *const Dyn,
                PT_TLS => {
                    self.tls_image = self.base + ph.p_vaddr as usize;
                    self.tls_filesz = ph.p_filesz as usize;
                    self.tls_memsz = ph.p_memsz as usize;
                    self.tls_align = if ph.p_align == 0 {
                        1
                    } else {
                        ph.p_align as usize
                    };
                }
                PT_GNU_RELRO => {
                    // Both ends round down: the partial trailing page can
                    // share with `.data`, which stays writable.
                    let start = self.base + ph.p_vaddr as usize;
                    self.relro_start = page_down(start);
                    self.relro_end = page_down(start + ph.p_memsz as usize);
                }
                _ => {}
            }
        }
    }

    /// Read `PT_DYNAMIC` into the fields above.
    ///
    /// # Safety
    /// `self.dynamic`, when non-null, must address a `DT_NULL`-terminated
    /// array belonging to an image loaded at `self.base`.
    pub unsafe fn parse_dynamic(&mut self) {
        if self.dynamic.is_null() {
            return;
        }
        let mut relasz = 0usize;
        let mut pltrelsz = 0usize;
        let mut relrsz = 0usize;
        let mut soname = u32::MAX;
        let mut rpath = u32::MAX;
        let mut runpath = u32::MAX;
        let mut flags = 0u64;

        let mut p = self.dynamic;
        loop {
            let d = ptr::read_unaligned(p);
            if d.d_tag == DT_NULL {
                break;
            }
            let addr = self.base.wrapping_add(d.d_val as usize);
            match d.d_tag {
                DT_STRTAB => self.strtab = addr as *const u8,
                DT_SYMTAB => self.symtab = addr as *const Sym,
                DT_GNU_HASH => self.gnu_hash = addr as *const u32,
                DT_HASH => self.sysv_hash = addr as *const u32,
                DT_RELA => self.rela = addr as *const Rela,
                DT_RELASZ => relasz = d.d_val as usize,
                DT_JMPREL => self.jmprel = addr as *const Rela,
                DT_PLTRELSZ => pltrelsz = d.d_val as usize,
                DT_RELR => self.relr = addr as *const usize,
                DT_RELRSZ => relrsz = d.d_val as usize,
                DT_INIT => self.init = addr,
                DT_FINI => self.fini = addr,
                DT_INIT_ARRAY => self.init_array = addr as *const usize,
                DT_INIT_ARRAYSZ => self.init_count = d.d_val as usize / 8,
                DT_FINI_ARRAY => self.fini_array = addr as *const usize,
                DT_FINI_ARRAYSZ => self.fini_count = d.d_val as usize / 8,
                DT_PREINIT_ARRAY => self.preinit_array = addr as *const usize,
                DT_PREINIT_ARRAYSZ => self.preinit_count = d.d_val as usize / 8,
                DT_SONAME => soname = d.d_val as u32,
                DT_RPATH => rpath = d.d_val as u32,
                DT_RUNPATH => runpath = d.d_val as u32,
                DT_FLAGS => flags = d.d_val,
                DT_SYMBOLIC => self.flags |= DSO_SYMBOLIC,
                _ => {}
            }
            p = p.add(1);
        }

        if flags & DF_SYMBOLIC != 0 {
            self.flags |= DSO_SYMBOLIC;
        }
        self.rela_count = relasz / size_of::<Rela>();
        self.jmprel_count = pltrelsz / size_of::<Rela>();
        self.relr_count = relrsz / size_of::<usize>();
        if !self.strtab.is_null() && soname != u32::MAX {
            self.name = self.str_at(soname);
        }
        if !self.strtab.is_null() && rpath != u32::MAX {
            self.rpath = self.str_at(rpath);
        }
        if !self.strtab.is_null() && runpath != u32::MAX {
            self.runpath = self.str_at(runpath);
        }
        self.nsyms = self.count_syms();
        self.needed_count = self.count_needed();
    }

    /// `.dynsym`'s length, which no `DT_*` tag states.
    ///
    /// `DT_HASH`'s `nchain` is it by definition; with GNU hash alone the last
    /// index has to be recovered from the chain array, whose low bit marks the
    /// end of each bucket's chain.
    unsafe fn count_syms(&self) -> usize {
        if !self.sysv_hash.is_null() {
            return ptr::read_unaligned(self.sysv_hash.add(1)) as usize;
        }
        if self.gnu_hash.is_null() {
            return 0;
        }
        let h = self.gnu_hash;
        let nbuckets = ptr::read_unaligned(h) as usize;
        let symoffset = ptr::read_unaligned(h.add(1)) as usize;
        let bloom_size = ptr::read_unaligned(h.add(2)) as usize;
        let buckets = h.add(4).cast::<u64>().add(bloom_size).cast::<u32>();
        let mut last = 0usize;
        for i in 0..nbuckets {
            let b = ptr::read_unaligned(buckets.add(i)) as usize;
            if b > last {
                last = b;
            }
        }
        if last < symoffset {
            return symoffset;
        }
        let chain = buckets.add(nbuckets);
        let mut i = last - symoffset;
        while ptr::read_unaligned(chain.add(i)) & 1 == 0 {
            i += 1;
        }
        symoffset + i + 1
    }

    unsafe fn count_needed(&self) -> u16 {
        let mut p = self.dynamic;
        let mut count = 0u16;
        loop {
            let d = ptr::read_unaligned(p);
            if d.d_tag == DT_NULL {
                return count;
            }
            if d.d_tag == DT_NEEDED && (count as usize) < DL_MAX_NEEDED {
                count += 1;
            }
            p = p.add(1);
        }
    }

    /// The `index`th `DT_NEEDED` name.
    ///
    /// # Safety
    /// `self.dynamic` and `self.strtab` must be the ones `parse_dynamic` read.
    pub unsafe fn needed_name(&self, index: usize) -> *const u8 {
        let mut p = self.dynamic;
        let mut seen = 0usize;
        loop {
            let d = ptr::read_unaligned(p);
            if d.d_tag == DT_NULL {
                return ptr::null();
            }
            if d.d_tag == DT_NEEDED {
                if seen == index {
                    return self.str_at(d.d_val as u32);
                }
                seen += 1;
            }
            p = p.add(1);
        }
    }
}
