//! `<dlfcn.h>` and `<link.h>`: the C surface over the loader.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use super::dso::{DL_MAX_OBJECTS, Dso};
use super::elf::*;
use super::{DlError, loader, lock};

pub const RTLD_LAZY: c_int = 1;
pub const RTLD_NOW: c_int = 2;
pub const RTLD_LOCAL: c_int = 0;
pub const RTLD_GLOBAL: c_int = 0x100;
pub const RTLD_NOLOAD: c_int = 4;

/// `dlsym`'s "search the global scope" pseudo-handle. `RTLD_NEXT` has no
/// spelling here: it means "after the calling object", which needs the return
/// address to identify the caller, and answering it as `RTLD_DEFAULT` would
/// silently return the same definition the caller is trying to skip.
const RTLD_DEFAULT: usize = 0;

#[repr(C)]
pub struct DlInfo {
    pub dli_fname: *const c_char,
    pub dli_fbase: *mut c_void,
    pub dli_sname: *const c_char,
    pub dli_saddr: *mut c_void,
}

/// `dl_iterate_phdr`'s callback argument. Field-for-field the glibc/musl
/// layout, because an unwinder compiled against either reads it.
#[repr(C)]
pub struct DlPhdrInfo {
    pub dlpi_addr: usize,
    pub dlpi_name: *const c_char,
    pub dlpi_phdr: *const Phdr,
    pub dlpi_phnum: u16,
    pub dlpi_adds: u64,
    pub dlpi_subs: u64,
    pub dlpi_tls_modid: usize,
    pub dlpi_tls_data: *mut c_void,
}

/// How many objects have ever been added and removed, which is what an
/// unwinder caches its `dl_iterate_phdr` walk against.
static ADDS: AtomicU32 = AtomicU32::new(0);
static SUBS: AtomicU32 = AtomicU32::new(0);

/// The pending `dlerror` message, as a [`DlError`] discriminant plus one for
/// "no error". Global rather than per-thread: the loader deliberately touches
/// no thread-local, and POSIX permits a non-thread-safe `dlerror`.
static LAST_ERROR: AtomicU32 = AtomicU32::new(0);

fn record(err: DlError) {
    LAST_ERROR.store(err as u32 + 1, Ordering::Relaxed);
}

fn clear() {
    LAST_ERROR.store(0, Ordering::Relaxed);
}

const ERRORS: [DlError; 6] = [
    DlError::NotFound,
    DlError::Malformed,
    DlError::Relocation,
    DlError::NoMemory,
    DlError::TooManyObjects,
    DlError::InvalidHandle,
];

/// A handle is the address of the loader's table entry. A caller cannot forge
/// one out of an integer, and because slots are never reused a closed slot's
/// address fails the liveness check rather than naming what took its place.
unsafe fn handle_index(handle: *mut c_void) -> Option<usize> {
    let dl = loader();
    for i in 0..dl.count() {
        if dl.get(i) as *const Dso as *mut c_void == handle {
            return dl.is_live(i).then_some(i);
        }
    }
    None
}

/// `dlopen(3)`.
///
/// `RTLD_LAZY` and `RTLD_NOW` name the same thing here: binding is eager, so
/// a missing symbol is reported by `dlopen` whichever was asked for.
///
/// # Safety
/// `path` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void {
    clear();
    if path.is_null() {
        // The main program's handle: a lookup against the global scope. It
        // takes a reference like any other successful `dlopen`, so the
        // `dlclose` POSIX pairs with it has one to give back.
        let _guard = lock();
        let dl = loader();
        dl.retain(0);
        return dl.get(0) as *const Dso as *mut c_void;
    }

    let mut group = [0u16; DL_MAX_OBJECTS];
    let count = {
        let _guard = lock();
        let dl = loader();
        if flags & RTLD_NOLOAD != 0 {
            let Some(index) = dl.retain_loaded(path.cast::<u8>()) else {
                record(DlError::NotFound);
                return ptr::null_mut();
            };
            return dl.get(index as usize) as *const Dso as *mut c_void;
        }
        let count = match dl.load_closure(path.cast::<u8>(), true, &mut group) {
            Ok(count) => count,
            Err(err) => {
                record(err);
                return ptr::null_mut();
            }
        };
        // A `dlopen`ed object's TLS has no place in the static block, which
        // was sized before any thread existed.
        let linked = dl
            .assign_tls(&group[..count], false)
            .and_then(|()| dl.relocate_group(&group[..count]));
        if let Err(err) = linked {
            // Nothing can reach these objects: no handle was returned, so
            // leaving them mapped would consume table slots for good.
            let mut doomed = [0u16; DL_MAX_OBJECTS];
            let n = dl.plan_release(group[0] as usize, &mut doomed);
            dl.finalize(&doomed[..n]);
            record(err);
            return ptr::null_mut();
        }
        if flags & RTLD_GLOBAL != 0 {
            // From the root's whole dependency group, not from `group`: a
            // root that was already loaded reports a group of one.
            let mut scope = [0u16; DL_MAX_OBJECTS];
            let n = dl.dependency_group(group[0], &mut scope);
            for slot in scope[..n].iter() {
                dl.add_global(*slot);
            }
        }
        count
    };

    ADDS.fetch_add(count as u32, Ordering::Relaxed);
    super::run_init(&group[..count]);

    let _guard = lock();
    loader().get(group[0] as usize) as *const Dso as *mut c_void
}

/// `dlsym(3)`.
///
/// # Safety
/// `handle` is null, `RTLD_DEFAULT`/`RTLD_NEXT`, or a live `dlopen` handle;
/// `name` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void {
    clear();
    if name.is_null() {
        record(DlError::InvalidHandle);
        return ptr::null_mut();
    }
    let _guard = lock();
    let dl = loader();
    let name = name.cast::<u8>();

    let found = if handle as usize == RTLD_DEFAULT {
        super::sym::resolve(dl.objects_slice(), dl.global_scope(), name, false, None)
    } else {
        let Some(index) = handle_index(handle) else {
            record(DlError::InvalidHandle);
            return ptr::null_mut();
        };
        let mut group = [0u16; DL_MAX_OBJECTS];
        let count = dl.dependency_group(index as u16, &mut group);
        super::sym::resolve(dl.objects_slice(), &group[..count], name, false, None)
    };

    let Some(def) = found else {
        record(DlError::NotFound);
        return ptr::null_mut();
    };
    // A thread-local resolves to *this* thread's copy, which is the only
    // address a caller can do anything with.
    if def.sym.sym_type() == STT_TLS {
        let index = TlsIndex {
            ti_module: dl.get(def.dso).tls_modid,
            ti_offset: def.sym.st_value as usize,
        };
        drop(_guard);
        return crate::thread::tls::__tls_get_addr(&index).cast();
    }
    (dl.get(def.dso).base + def.sym.st_value as usize) as *mut c_void
}

/// `dlclose(3)`.
///
/// # Safety
/// `handle` is a live `dlopen` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlclose(handle: *mut c_void) -> c_int {
    clear();
    let mut doomed = [0u16; DL_MAX_OBJECTS];
    let count = {
        let _guard = lock();
        let dl = loader();
        let Some(index) = handle_index(handle) else {
            record(DlError::InvalidHandle);
            return -1;
        };
        dl.plan_release(index, &mut doomed)
    };
    if count == 0 {
        return 0;
    }

    // Destructors run outside the lock, root first, for every object this
    // close unloads and not only for the one named: a dependency the close
    // drops to zero is unmapped either way.
    for slot in doomed[..count].iter() {
        let dso = {
            let _guard = lock();
            *loader().get(*slot as usize)
        };
        super::run_fini(&dso);
    }

    let _guard = lock();
    loader().finalize(&doomed[..count]);
    SUBS.fetch_add(count as u32, Ordering::Relaxed);
    0
}

/// `dlerror(3)`. Reading clears, as POSIX requires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dlerror() -> *const c_char {
    let code = LAST_ERROR.swap(0, Ordering::Relaxed);
    if code == 0 || code as usize > ERRORS.len() {
        return ptr::null();
    }
    ERRORS[code as usize - 1].message().as_ptr().cast()
}

/// `dladdr(3)`. Answers non-zero on a hit, as the GNU extension does.
///
/// # Safety
/// `info` is a writable `Dl_info`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int {
    if info.is_null() {
        return 0;
    }
    let _guard = lock();
    let dl = loader();
    let Some(index) = dl.owner_of(addr as usize) else {
        return 0;
    };
    let dso = dl.get(index as usize);
    (*info).dli_fname = dso.path.cast();
    (*info).dli_fbase = dso.base as *mut c_void;
    (*info).dli_sname = ptr::null();
    (*info).dli_saddr = ptr::null_mut();

    // The nearest defined symbol at or below `addr`, which is what a
    // backtrace symbolizer wants and what no hash table can answer.
    let mut best: Option<(usize, Sym)> = None;
    for i in 0..dso.nsyms {
        let sym = ptr::read_unaligned(dso.symtab.add(i));
        if sym.st_shndx == SHN_UNDEF || sym.st_value == 0 || sym.sym_type() == STT_TLS {
            continue;
        }
        let at = dso.base + sym.st_value as usize;
        if at > addr as usize {
            continue;
        }
        if sym.st_size != 0 && (addr as usize) >= at + sym.st_size as usize {
            continue;
        }
        if best.is_none_or(|(prev, _)| at > prev) {
            best = Some((at, sym));
        }
    }
    if let Some((at, sym)) = best {
        (*info).dli_sname = dso.str_at(sym.st_name).cast();
        (*info).dli_saddr = at as *mut c_void;
    }
    1
}

/// `dl_iterate_phdr(3)`, which is how an unwinder finds `PT_GNU_EH_FRAME`.
///
/// # Safety
/// `callback` must be a C function of the declared signature.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dl_iterate_phdr(
    callback: Option<
        unsafe extern "C" fn(info: *mut DlPhdrInfo, size: usize, data: *mut c_void) -> c_int,
    >,
    data: *mut c_void,
) -> c_int {
    let Some(callback) = callback else {
        return 0;
    };
    // Walked under the lock, not over a snapshot: `dlpi_name` and
    // `dlpi_phdr` point into an object's own heap allocation and mapping, so
    // a copy of the table would still hand the callback pointers a
    // concurrent `dlclose` had freed. The cost is that a callback must not
    // call back into the loader, which is what an unwinder — the only caller
    // this exists for — does not do.
    let _guard = lock();
    let dl = loader();

    const EMPTY_NAME: &[u8] = b"\0";
    for i in 0..dl.count() {
        if !dl.is_live(i) {
            continue;
        }
        let dso = dl.get(i);
        let mut info = DlPhdrInfo {
            dlpi_addr: dso.base,
            dlpi_name: if dso.path.is_null() {
                EMPTY_NAME.as_ptr().cast()
            } else {
                dso.path.cast()
            },
            dlpi_phdr: dso.phdr,
            dlpi_phnum: dso.phnum as u16,
            dlpi_adds: ADDS.load(Ordering::Relaxed) as u64,
            dlpi_subs: SUBS.load(Ordering::Relaxed) as u64,
            dlpi_tls_modid: dso.tls_modid,
            // The block, not the module: an unwinder reads it, and a
            // callback that needs one can ask `__tls_get_addr` itself. The
            // loader's lock is held here and resolving would take the TLS
            // layout's, which is the one order that must not be inverted.
            dlpi_tls_data: ptr::null_mut(),
        };
        let rc = callback(&mut info, size_of::<DlPhdrInfo>(), data);
        if rc != 0 {
            return rc;
        }
    }
    0
}
