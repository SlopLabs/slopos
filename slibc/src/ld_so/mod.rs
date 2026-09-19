//! The dynamic linker.
//!
//! `libc.so` is both this program interpreter and the C library, the way
//! musl's is: there is one artifact, so there is one allocator, one `errno`
//! and one TLS implementation in a process however many objects it loads.
//! The two-libc allocator mismatch a separate `ld.so` has to solve cannot be
//! expressed here.
//!
//! Binding is eager throughout — see [`reloc`] — so no code in this module
//! runs with a caller's argument registers live, and full RELRO is free.

pub mod api;
pub mod dso;
pub mod elf;
pub mod load;
pub mod reloc;
pub mod start;
pub mod sym;

use core::cell::SyncUnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::pal::{Pal, Sys};

use dso::{
    DL_MAX_OBJECTS, DSO_BOOTSTRAPPED, DSO_DLOPENED, DSO_DYING, DSO_GLOBAL, DSO_INITED,
    DSO_RELOCATED, DSO_USED, Dso, NEEDED_UNRESOLVED,
};
use elf::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DlError {
    NotFound,
    Malformed,
    Relocation,
    NoMemory,
    TooManyObjects,
    InvalidHandle,
}

impl DlError {
    /// A NUL-terminated message for `dlerror`.
    pub fn message(self) -> &'static [u8] {
        match self {
            DlError::NotFound => b"cannot open shared object\0",
            DlError::Malformed => b"not a loadable shared object\0",
            DlError::Relocation => b"unresolved relocation\0",
            DlError::NoMemory => b"out of memory\0",
            DlError::TooManyObjects => b"too many shared objects\0",
            DlError::InvalidHandle => b"invalid handle\0",
        }
    }
}

/// Where a bare `DT_NEEDED`/`dlopen` name is looked for, in order.
const SEARCH_DIRS: [&[u8]; 2] = [b"/lib/", b"/usr/lib/"];

/// Longest path the loader will build. Matches the kernel's `PATH_MAX`.
const DL_PATH_MAX: usize = 4096;

pub struct Loader {
    objects: [Dso; DL_MAX_OBJECTS],
    count: usize,
    /// Search order for an undefined symbol, nearest first.
    global: [u16; DL_MAX_OBJECTS],
    global_count: usize,
}

static LOADER: SyncUnsafeCell<Loader> = SyncUnsafeCell::new(Loader {
    objects: [Dso::empty(); DL_MAX_OBJECTS],
    count: 0,
    global: [0; DL_MAX_OBJECTS],
    global_count: 0,
});

static LOCK: AtomicBool = AtomicBool::new(false);

pub struct LoaderGuard;

impl Drop for LoaderGuard {
    fn drop(&mut self) {
        LOCK.store(false, Ordering::Release);
    }
}

/// Take the loader's lock. Every entry point that reads or writes the object
/// table holds it; no constructor runs under it.
pub fn lock() -> LoaderGuard {
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    LoaderGuard
}

/// # Safety
/// Caller holds [`lock`].
#[allow(clippy::mut_from_ref)]
pub unsafe fn loader() -> &'static mut Loader {
    &mut *LOADER.get()
}

pub fn streq(a: *const u8, b: *const u8) -> bool {
    unsafe {
        let mut i = 0usize;
        loop {
            let ca = *a.add(i);
            if ca != *b.add(i) {
                return false;
            }
            if ca == 0 {
                return true;
            }
            i += 1;
        }
    }
}

pub fn strlen(s: *const u8) -> usize {
    unsafe {
        let mut i = 0usize;
        while *s.add(i) != 0 {
            i += 1;
        }
        i
    }
}

fn basename(path: *const u8) -> *const u8 {
    unsafe {
        let len = strlen(path);
        let mut i = len;
        while i > 0 {
            i -= 1;
            if *path.add(i) == b'/' {
                return path.add(i + 1);
            }
        }
        path
    }
}

/// Copy a NUL-terminated string onto the heap.
unsafe fn dup_cstr(s: *const u8) -> *mut u8 {
    let len = strlen(s);
    let out = crate::mem::malloc::alloc(len + 1) as *mut u8;
    if out.is_null() {
        return out;
    }
    ptr::copy_nonoverlapping(s, out, len + 1);
    out
}

impl Loader {
    pub fn count(&self) -> usize {
        self.count
    }

    pub fn get(&self, index: usize) -> &Dso {
        &self.objects[index]
    }

    pub fn global_scope(&self) -> &[u16] {
        &self.global[..self.global_count]
    }

    /// Whether `index` names a live object, which is what makes a `dlopen`
    /// handle checkable rather than trusted.
    pub fn is_live(&self, index: usize) -> bool {
        index < self.count && self.objects[index].flags & (DSO_USED | DSO_DYING) == DSO_USED
    }

    pub fn objects_slice(&self) -> &[Dso] {
        &self.objects[..self.count]
    }

    /// Take a reference to a live object, for a `dlopen` that answers
    /// without loading anything.
    pub fn retain(&mut self, index: usize) {
        self.objects[index].refs += 1;
    }

    /// Take a reference to an already-loaded object of that name, for
    /// `RTLD_NOLOAD`. POSIX balances every successful `dlopen` with one
    /// `dlclose`, this one included.
    pub fn retain_loaded(&mut self, name: *const u8) -> Option<u16> {
        let index = self.find_by_name(basename(name))?;
        self.objects[index as usize].refs += 1;
        Some(index)
    }

    /// `index` and everything its `DT_NEEDED` closure reaches, which is the
    /// scope `dlsym` on that handle searches.
    pub fn dependency_group(&self, index: u16, group: &mut [u16; DL_MAX_OBJECTS]) -> usize {
        group[0] = index;
        let mut count = 1usize;
        let mut cursor = 0usize;
        while cursor < count {
            let at = group[cursor] as usize;
            cursor += 1;
            for n in 0..self.objects[at].needed_count as usize {
                let dep = self.objects[at].needed[n];
                if dep != NEEDED_UNRESOLVED
                    && self.is_live(dep as usize)
                    && !group[..count].contains(&dep)
                {
                    group[count] = dep;
                    count += 1;
                }
            }
        }
        count
    }

    /// The object whose `PT_LOAD` extents contain `addr`.
    pub fn owner_of(&self, addr: usize) -> Option<u16> {
        for i in 0..self.count {
            if self.objects[i].flags & DSO_USED != 0 && self.objects[i].contains(addr) {
                return Some(i as u16);
            }
        }
        None
    }

    fn find_by_name(&self, name: *const u8) -> Option<u16> {
        for i in 0..self.count {
            let dso = &self.objects[i];
            if !self.is_live(i) || dso.name.is_null() {
                continue;
            }
            if streq(name, dso.name) {
                return Some(i as u16);
            }
        }
        None
    }

    /// Slots are taken in order and never reused. A handle is a slot's
    /// address, so a reused slot would make a stale handle name whatever took
    /// its place — and `is_live` would agree, because the new object is. The
    /// table is therefore a high-water mark rather than a population.
    fn alloc_slot(&mut self) -> Option<usize> {
        if self.count == DL_MAX_OBJECTS {
            return None;
        }
        let index = self.count;
        self.objects[index] = Dso::empty();
        self.count += 1;
        Some(index)
    }

    pub fn add_global(&mut self, index: u16) {
        if self.objects[index as usize].flags & DSO_GLOBAL != 0 {
            return;
        }
        self.objects[index as usize].flags |= DSO_GLOBAL;
        self.global[self.global_count] = index;
        self.global_count += 1;
    }

    fn drop_global(&mut self, index: u16) {
        let Some(at) = self.global[..self.global_count]
            .iter()
            .position(|slot| *slot == index)
        else {
            return;
        };
        self.global.copy_within(at + 1..self.global_count, at);
        self.global_count -= 1;
        self.objects[index as usize].flags &= !DSO_GLOBAL;
    }

    /// Register an image the kernel mapped: the executable, or this
    /// interpreter.
    ///
    /// # Safety
    /// `phdr`/`phnum` must describe an image loaded at `base`.
    pub unsafe fn adopt(
        &mut self,
        base: usize,
        phdr: *const Phdr,
        phnum: usize,
        name: *const u8,
        bootstrapped: bool,
    ) -> Option<u16> {
        let index = self.alloc_slot()?;
        let dso = &mut self.objects[index];
        dso.flags = DSO_USED | if bootstrapped { DSO_BOOTSTRAPPED } else { 0 };
        dso.refs = 1;
        dso.base = base;
        dso.scan_phdrs(phdr, phnum);
        dso.parse_dynamic();
        // `DT_SONAME` wins when the object carries one, which is how a
        // `DT_NEEDED` on `libc.so` finds the interpreter already mapped.
        if dso.name.is_null() {
            dso.name = name;
        }
        self.add_global(index as u16);
        Some(index as u16)
    }

    /// Load `name` and everything its `DT_NEEDED` closure reaches.
    ///
    /// `group` receives every object the call touched, root first, in
    /// breadth-first order — which is the order `dlsym` on the returned
    /// handle searches and the reverse of the order constructors run in.
    ///
    /// # Safety
    /// Caller holds [`lock`].
    pub unsafe fn load_closure(
        &mut self,
        name: *const u8,
        dlopened: bool,
        group: &mut [u16; DL_MAX_OBJECTS],
    ) -> Result<usize, DlError> {
        // A reference is what an object's *parents* and its open handles hold.
        // An already-loaded root therefore takes one reference for this call
        // and its closure is left alone: its dependencies' references belong
        // to it, were taken when it was loaded, and are released with it.
        let (root, fresh) = self.load_one(name, dlopened)?;
        group[0] = root;
        let mut count = 1usize;
        if !fresh {
            return Ok(count);
        }

        match self.close_over(group, &mut count, dlopened) {
            Ok(()) => Ok(count),
            Err(err) => {
                // Nothing can reach what was mapped: no handle exists, and a
                // slot is never reused, so leaving it would retire the slot
                // and hold every reference the walk took.
                let mut doomed = [0u16; DL_MAX_OBJECTS];
                let n = self.plan_release(root as usize, &mut doomed);
                self.finalize(&doomed[..n]);
                Err(err)
            }
        }
    }

    unsafe fn close_over(
        &mut self,
        group: &mut [u16; DL_MAX_OBJECTS],
        count: &mut usize,
        dlopened: bool,
    ) -> Result<(), DlError> {
        let mut cursor = 0usize;
        while cursor < *count {
            let index = group[cursor] as usize;
            cursor += 1;
            for n in 0..self.objects[index].needed_count as usize {
                let dep_name = self.objects[index].needed_name(n);
                if dep_name.is_null() {
                    continue;
                }
                let (dep, dep_fresh) = self.load_one(dep_name, dlopened)?;
                self.objects[index].needed[n] = dep;
                if dep_fresh && !group[..*count].contains(&dep) {
                    group[*count] = dep;
                    *count += 1;
                }
            }
        }
        Ok(())
    }

    /// Map one object, or answer the already-loaded one of that name.
    unsafe fn load_one(&mut self, name: *const u8, dlopened: bool) -> Result<(u16, bool), DlError> {
        if let Some(index) = self.find_by_name(basename(name)) {
            self.objects[index as usize].refs += 1;
            return Ok((index, false));
        }

        let mut path = [0u8; DL_PATH_MAX];
        let resolved = resolve_path(name, &mut path)?;
        let mut window = [0u8; load::HEADER_WINDOW];
        let mapped = load::map_object(resolved, &mut window).map_err(|e| match e {
            load::LoadError::NotFound => DlError::NotFound,
            load::LoadError::NoMemory => DlError::NoMemory,
            _ => DlError::Malformed,
        })?;

        let owned = dup_cstr(resolved);
        let slot = self.alloc_slot();
        if owned.is_null() || slot.is_none() {
            let _ = Sys::munmap(mapped.span_start as *mut u8, mapped.span_len);
            if !owned.is_null() {
                crate::mem::malloc::dealloc(owned.cast());
            }
            return Err(if slot.is_none() {
                DlError::TooManyObjects
            } else {
                DlError::NoMemory
            });
        }
        let index = slot.unwrap();

        let dso = &mut self.objects[index];
        dso.flags = DSO_USED | if dlopened { DSO_DLOPENED } else { 0 };
        dso.refs = 1;
        dso.base = mapped.base;
        dso.map_start = mapped.span_start;
        dso.map_len = mapped.span_len;
        dso.path = owned;
        dso.scan_phdrs(mapped.phdr, mapped.phnum);
        dso.parse_dynamic();
        if dso.name.is_null() {
            dso.name = basename(owned);
        }
        Ok((index as u16, true))
    }

    /// Load the `slot`th `DT_NEEDED` of `parent` and record where it landed.
    ///
    /// # Safety
    /// Caller holds [`lock`].
    pub unsafe fn load_needed(
        &mut self,
        parent: usize,
        slot: usize,
        name: *const u8,
    ) -> Result<u16, DlError> {
        let (dep, _) = self.load_one(name, false)?;
        self.objects[parent].needed[slot] = dep;
        Ok(dep)
    }

    /// Relocate `group` deepest dependency first, against the global scope
    /// widened by the group itself, then give each object its final page
    /// protections.
    ///
    /// # Safety
    /// Caller holds [`lock`]; every object in `group` is mapped and parsed.
    pub unsafe fn relocate_group(&mut self, group: &[u16]) -> Result<(), DlError> {
        let mut scope = [0u16; DL_MAX_OBJECTS];
        let mut scope_len = 0usize;
        for slot in self.global[..self.global_count].iter() {
            scope[scope_len] = *slot;
            scope_len += 1;
        }
        for slot in group.iter() {
            if !scope[..scope_len].contains(slot) {
                scope[scope_len] = *slot;
                scope_len += 1;
            }
        }

        for slot in group.iter().rev() {
            let index = *slot as usize;
            if self.objects[index].flags & DSO_RELOCATED != 0 {
                continue;
            }
            reloc::relocate(&self.objects, index, &scope[..scope_len])
                .map_err(|_| DlError::Relocation)?;
            self.objects[index].flags |= DSO_RELOCATED;
            if self.objects[index].map_len != 0 {
                load::protect_segments(&self.objects[index]).map_err(|_| DlError::NoMemory)?;
            }
            load::protect_relro(&self.objects[index]).map_err(|_| DlError::NoMemory)?;
        }
        Ok(())
    }

    /// Give every TLS-carrying object in `group` a module id.
    ///
    /// `static_block` distinguishes the startup set, whose modules get a place
    /// below the thread pointer, from a `dlopen`ed one, whose block every
    /// thread allocates on first access.
    ///
    /// # Safety
    /// Caller holds [`lock`], and for `static_block` no thread exists yet.
    pub unsafe fn assign_tls(&mut self, group: &[u16], static_block: bool) -> Result<(), DlError> {
        for slot in group.iter() {
            let dso = &mut self.objects[*slot as usize];
            if dso.tls_memsz == 0 || dso.tls_modid != 0 {
                continue;
            }
            dso.tls_modid = if static_block {
                crate::thread::tls::register_static_module(
                    dso.tls_image,
                    dso.tls_filesz,
                    dso.tls_memsz,
                    dso.tls_align,
                )
            } else {
                crate::thread::tls::register_dynamic_module(
                    dso.tls_image,
                    dso.tls_filesz,
                    dso.tls_memsz,
                    dso.tls_align,
                )
            };
            if dso.tls_modid == 0 {
                // Module 0 is "no module": a `DTPMOD64` carrying it resolves
                // to a null pointer at the first thread-local access, which
                // is a crash inside the loaded object rather than a failed
                // `dlopen`.
                return Err(DlError::TooManyObjects);
            }
            dso.tls_offset = crate::thread::tls::static_offset(dso.tls_modid);
        }
        Ok(())
    }

    /// Drop one reference to `index` and answer everything that reached zero
    /// with it, nearest first.
    ///
    /// Nothing is unmapped here: the objects are still readable so their
    /// destructors can run, which is what [`finalize`] then makes final.
    ///
    /// # Safety
    /// Caller holds [`lock`].
    pub unsafe fn plan_release(
        &mut self,
        index: usize,
        doomed: &mut [u16; DL_MAX_OBJECTS],
    ) -> usize {
        let mut count = 0usize;
        self.plan_into(index, doomed, &mut count);
        count
    }

    unsafe fn plan_into(
        &mut self,
        index: usize,
        doomed: &mut [u16; DL_MAX_OBJECTS],
        count: &mut usize,
    ) {
        if !self.is_live(index) {
            return;
        }
        // An image the kernel mapped — the executable and the interpreter —
        // is never unloaded, so a reference against it is given back and
        // nothing is condemned however far the count falls.
        if self.objects[index].map_len == 0 {
            self.objects[index].refs = self.objects[index].refs.saturating_sub(1);
            return;
        }
        if self.objects[index].refs > 1 {
            self.objects[index].refs -= 1;
            return;
        }
        // Marked before the recursion: a cycle would otherwise re-enter, and
        // `DSO_DYING` is also what stops a lookup handing out an object
        // whose destructors are about to run.
        self.objects[index].refs = 0;
        self.objects[index].flags |= DSO_DYING;
        doomed[*count] = index as u16;
        *count += 1;
        let dso = self.objects[index];
        for n in 0..dso.needed_count as usize {
            let dep = dso.needed[n];
            if dep != NEEDED_UNRESOLVED {
                self.plan_into(dep as usize, doomed, count);
            }
        }
    }

    /// Unmap what [`Self::plan_release`] condemned. Destructors have run.
    ///
    /// # Safety
    /// Caller holds [`lock`].
    pub unsafe fn finalize(&mut self, doomed: &[u16]) {
        for slot in doomed.iter() {
            let index = *slot as usize;
            let dso = self.objects[index];
            self.drop_global(*slot);
            self.objects[index] = Dso::empty();
            if dso.map_len != 0 {
                let _ = Sys::munmap(dso.map_start as *mut u8, dso.map_len);
            }
            if !dso.path.is_null() {
                crate::mem::malloc::dealloc(dso.path.cast());
            }
        }
    }
}

/// Run `DT_PREINIT_ARRAY`, `DT_INIT` and `DT_INIT_ARRAY` for `group`,
/// dependencies first.
///
/// The lock is taken per object and released around the call, because a
/// constructor may `dlopen`; claiming the object by setting `DSO_INITED`
/// under the lock is what keeps two callers from running one twice.
///
/// # Safety
/// The caller must not hold [`lock`].
pub unsafe fn run_init(group: &[u16]) {
    for slot in group.iter().rev() {
        let index = *slot as usize;
        let dso = {
            let _guard = lock();
            let dl = loader();
            if !dl.is_live(index) || dl.objects[index].flags & DSO_INITED != 0 {
                continue;
            }
            dl.objects[index].flags |= DSO_INITED;
            dl.objects[index]
        };
        for i in 0..dso.preinit_count {
            call_hook(ptr::read(dso.preinit_array.add(i)));
        }
        call_hook(dso.init);
        for i in 0..dso.init_count {
            call_hook(ptr::read(dso.init_array.add(i)));
        }
    }
}

/// Run `DT_FINI_ARRAY` in reverse, then `DT_FINI`.
///
/// # Safety
/// As [`run_init`].
pub unsafe fn run_fini(dso: &Dso) {
    for i in (0..dso.fini_count).rev() {
        call_hook(ptr::read(dso.fini_array.add(i)));
    }
    call_hook(dso.fini);
}

unsafe fn call_hook(addr: usize) {
    // A `DT_INIT` an object does not carry reads 0 here; `-1` is what a
    // stripped `DT_*_ARRAY` slot is conventionally filled with.
    if addr == 0 || addr == usize::MAX {
        return;
    }
    let hook: extern "C" fn() = core::mem::transmute(addr);
    hook();
}

/// Expand a `DT_NEEDED`/`dlopen` name into a path that exists.
///
/// A name with a slash in it is used as written, as every loader does; a bare
/// one is searched for in [`SEARCH_DIRS`].
fn resolve_path(name: *const u8, out: &mut [u8; DL_PATH_MAX]) -> Result<*const u8, DlError> {
    let len = strlen(name);
    if len == 0 || len >= DL_PATH_MAX {
        return Err(DlError::NotFound);
    }
    if (0..len).any(|i| unsafe { *name.add(i) } == b'/') {
        return Ok(name);
    }
    for dir in SEARCH_DIRS.iter() {
        if dir.len() + len >= DL_PATH_MAX {
            continue;
        }
        out[..dir.len()].copy_from_slice(dir);
        unsafe { ptr::copy_nonoverlapping(name, out.as_mut_ptr().add(dir.len()), len) };
        out[dir.len() + len] = 0;
        if Sys::access(out.as_ptr(), 0).is_ok() {
            return Ok(out.as_ptr());
        }
    }
    Err(DlError::NotFound)
}
