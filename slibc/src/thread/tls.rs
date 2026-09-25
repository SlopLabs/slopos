//! TLS: the static block, the DTV, and `__tls_get_addr`.
//!
//! x86_64 variant-II layout: the thread pointer (`fs_base`) points at the
//! [`Tcb`], every module's TLS image sits *below* it, and module `m`'s block
//! begins at `tp - offset[m]`. A `#[thread_local]` in the executable is
//! reached at a negative offset from `fs_base` without a lookup; one in a
//! shared object goes through `__tls_get_addr` and the DTV.
//!
//! libc owns all TLS. A static program registers its own `PT_TLS` from
//! `AT_PHDR` at startup; under an interpreter the loader registers every
//! module it maps, which is the same table.

use core::cell::SyncUnsafeCell;
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::mem::malloc;
use crate::pal::{Pal, Sys};

use super::tcb::Tcb;

/// TLS modules one process may hold: the startup set plus whatever `dlopen`
/// adds. A miss makes the `dlopen` fail rather than the access.
pub const MAX_TLS_MODULES: usize = 64;

/// The whole block is aligned to at least this, so an SSE spill inside a
/// thread-local lands aligned whatever the modules ask for.
const MIN_TLS_ALIGN: usize = 16;

static mut TLS_READY: bool = false;

/// Guards [`LAYOUT`] and a thread's DTV growth.
///
/// Its own lock rather than the loader's, and ordered *below* it: `dlsym` and
/// `dl_iterate_phdr` resolve a thread-local while holding the loader's, and a
/// second acquire of a non-reentrant flag is a hang.
static LAYOUT_LOCK: AtomicBool = AtomicBool::new(false);

pub(crate) struct LayoutGuard;

impl Drop for LayoutGuard {
    fn drop(&mut self) {
        LAYOUT_LOCK.store(false, Ordering::Release);
    }
}

pub(crate) fn lock_layout() -> LayoutGuard {
    while LAYOUT_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    LayoutGuard
}

/// One module's TLS template.
///
/// `offset` is the distance below the thread pointer at which the module's
/// block sits, and zero means the module is not in the static block — a
/// `dlopen`ed object, whose block `__tls_get_addr` allocates per thread on
/// first use.
#[derive(Clone, Copy)]
struct TlsModule {
    image: usize,
    filesz: usize,
    memsz: usize,
    align: usize,
    offset: usize,
}

const EMPTY_MODULE: TlsModule = TlsModule {
    image: 0,
    filesz: 0,
    memsz: 0,
    align: MIN_TLS_ALIGN,
    offset: 0,
};

struct TlsLayout {
    modules: [TlsModule; MAX_TLS_MODULES],
    count: usize,
    /// Bytes below the thread pointer the static modules occupy.
    static_size: usize,
    max_align: usize,
}

static LAYOUT: SyncUnsafeCell<TlsLayout> = SyncUnsafeCell::new(TlsLayout {
    modules: [EMPTY_MODULE; MAX_TLS_MODULES],
    count: 0,
    static_size: 0,
    max_align: MIN_TLS_ALIGN,
});

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    let a = if align == 0 { 1 } else { align };
    (value + a - 1) & !(a - 1)
}

/// Give a module a place in the static block and answer its module id.
///
/// Ids start at 1, which is what a `DTPMOD64` relocation stores and what
/// indexes the DTV. Zero means the layout is full.
///
/// # Safety
/// Must run before any thread exists, from the loader or from startup.
pub unsafe fn register_static_module(
    image: usize,
    filesz: usize,
    memsz: usize,
    align: usize,
) -> usize {
    let _guard = lock_layout();
    let layout = &mut *LAYOUT.get();
    if layout.count >= MAX_TLS_MODULES {
        return 0;
    }
    let align = align.max(1);
    // Variant II grows downward from the thread pointer, so a module's offset
    // is the running total rounded up to its own alignment: the block then
    // starts at `tp - offset`, which is `align`-aligned because `tp` is.
    let offset = align_up(layout.static_size + memsz, align);
    layout.modules[layout.count] = TlsModule {
        image,
        filesz,
        memsz,
        align,
        offset,
    };
    layout.count += 1;
    layout.static_size = offset;
    if align > layout.max_align {
        layout.max_align = align;
    }
    layout.count
}

/// Give a `dlopen`ed module an id with no place in the static block.
///
/// # Safety
/// Caller holds the loader's lock; the image must outlive every thread.
pub unsafe fn register_dynamic_module(
    image: usize,
    filesz: usize,
    memsz: usize,
    align: usize,
) -> usize {
    let _guard = lock_layout();
    let layout = &mut *LAYOUT.get();
    if layout.count >= MAX_TLS_MODULES {
        return 0;
    }
    layout.modules[layout.count] = TlsModule {
        image,
        filesz,
        memsz,
        align: align.max(1),
        offset: 0,
    };
    layout.count += 1;
    layout.count
}

/// A registered module's distance below the thread pointer, which is what a
/// `TPOFF64` relocation subtracts. Zero for a `dlopen`ed module, which has no
/// static place and therefore cannot satisfy initial-exec.
pub fn static_offset(modid: usize) -> usize {
    unsafe {
        let layout = &*LAYOUT.get();
        if modid == 0 || modid > layout.count {
            return 0;
        }
        layout.modules[modid - 1].offset
    }
}

/// Capture the program's own `PT_TLS` by walking the auxv to `AT_PHDR` and
/// scanning the program headers. Idempotent, and a no-op once the loader has
/// registered a module — under an interpreter the loader owns the layout.
///
/// # Safety
/// `stack_base` must point at the kernel-prepared entry stack (`argc` at
/// `[stack_base]`, then `argv`, `envp`, and the auxv).
pub unsafe extern "C" fn capture_tls_template_from_stack(stack_base: *const usize) {
    if stack_base.is_null() {
        return;
    }
    let argc = *stack_base;
    if argc > 4096 {
        return; // implausible argc → bail rather than walk garbage
    }
    // envp begins after argc + argv[0..argc] + the argv NULL terminator.
    let mut p = stack_base.add(1 + argc + 1);
    while *p != 0 {
        p = p.add(1);
    }
    p = p.add(1); // step past the envp NULL → first auxv entry

    // The only walk of the entry stack in the whole library, so `getauxval`
    // records its base here rather than the CRT growing a second one.
    crate::auxv::capture(p);

    if (*LAYOUT.get()).count != 0 {
        return;
    }

    let (mut phdr, mut phnum, mut phent) = (0usize, 0usize, 0usize);
    loop {
        let a_type = *p as u64;
        let a_val = *p.add(1);
        if a_type == slopos_abi::auxv::AT_NULL {
            break;
        }
        match a_type {
            slopos_abi::auxv::AT_PHDR => phdr = a_val,
            slopos_abi::auxv::AT_PHNUM => phnum = a_val,
            slopos_abi::auxv::AT_PHENT => phent = a_val,
            _ => {}
        }
        p = p.add(2);
    }
    if phdr == 0 || phnum == 0 || phent == 0 {
        return;
    }

    // Load bias is `AT_PHDR - PT_PHDR.p_vaddr`, so 0 for a non-relocated
    // executable. The `.tdata` image at `bias + p_vaddr` lies in the mapped
    // data segment and stays pristine for every thread to copy from.
    // Elf64_Phdr: p_type@0 (u32), p_vaddr@16, p_filesz@32, p_memsz@40, p_align@48.
    const PT_PHDR: u32 = 6;
    const PT_TLS: u32 = 7;
    let mut bias: usize = 0;
    let mut tls: Option<(usize, usize, usize, usize)> = None;
    for i in 0..phnum {
        let ph = (phdr + i * phent) as *const u8;
        let p_type = ptr::read_unaligned(ph as *const u32);
        let p_vaddr = ptr::read_unaligned(ph.add(16) as *const u64) as usize;
        if p_type == PT_PHDR {
            bias = phdr.wrapping_sub(p_vaddr);
        } else if p_type == PT_TLS {
            let p_filesz = ptr::read_unaligned(ph.add(32) as *const u64) as usize;
            let p_memsz = ptr::read_unaligned(ph.add(40) as *const u64) as usize;
            let p_align = ptr::read_unaligned(ph.add(48) as *const u64) as usize;
            tls = Some((p_vaddr, p_filesz, p_memsz, p_align));
        }
    }
    if let Some((p_vaddr, filesz, memsz, align)) = tls {
        register_static_module(bias.wrapping_add(p_vaddr), filesz, memsz, align);
    }
}

/// Allocate and initialise a per-thread TLS block in variant-II layout.
///
/// Returns `(alloc_base, tp)`: the raw allocation to free later, and the
/// thread pointer (the [`Tcb`] address) to load into `fs_base`. Null on OOM.
///
/// # Safety
/// Reads each registered module's image, which must stay mapped.
pub unsafe fn alloc_thread_tls() -> (*mut u8, *mut Tcb) {
    // Held across the allocations: a `dlopen` raising `count` between sizing
    // the DTV and filling it would write past the vector and leave `dtv[0]`
    // claiming slots the allocation does not contain.
    let _guard = lock_layout();
    let layout = &*LAYOUT.get();
    let align = layout.max_align.max(MIN_TLS_ALIGN);
    let tls_size = align_up(layout.static_size, align);
    let block_size = tls_size + mem::size_of::<Tcb>();

    let base = malloc::memalign(align, block_size);
    if base.is_null() {
        return (ptr::null_mut(), ptr::null_mut());
    }
    // Zeroing the whole image region is what initialises every `.tbss`.
    ptr::write_bytes(base, 0, tls_size);
    let tp = base.add(tls_size) as *mut Tcb;
    ptr::write_bytes(tp as *mut u8, 0, mem::size_of::<Tcb>());

    let dtv = malloc::alloc((layout.count + 1) * mem::size_of::<usize>()) as *mut usize;
    if dtv.is_null() {
        malloc::dealloc(base.cast());
        return (ptr::null_mut(), ptr::null_mut());
    }
    *dtv = layout.count;

    for (index, module) in layout.modules[..layout.count].iter().enumerate() {
        if module.offset == 0 {
            // A `dlopen`ed module: no static place, allocated on first access.
            *dtv.add(index + 1) = 0;
            continue;
        }
        let block = (tp as *mut u8).sub(module.offset);
        if module.filesz > 0 && module.image != 0 {
            ptr::copy_nonoverlapping(module.image as *const u8, block, module.filesz);
        }
        *dtv.add(index + 1) = block as usize;
    }
    (*tp).dtv = dtv;
    (*tp).tls_block = base;
    (base, tp)
}

#[inline]
pub fn tls_is_initialized() -> bool {
    unsafe { TLS_READY }
}

/// Set up the main thread's TLS. Must run after
/// [`capture_tls_template_from_stack`]. Until `TLS_READY` flips, `errno` uses
/// its static fallback, so the work below is safe with `fs_base == 0`.
///
/// # Safety
/// Must be called exactly once from the main thread during CRT startup.
pub unsafe fn tls_init_main_thread() {
    // Adopt an already-installed valid TCB rather than building a second one:
    // under an interpreter the loader has already built the block, DTV and
    // all, before the program's own `_start` runs.
    if let Ok(fs_base) = Sys::arch_prctl_get_fs() {
        if fs_base != 0 {
            let tcb_ptr = fs_base as *mut Tcb;
            if !tcb_ptr.is_null() && (*tcb_ptr).self_ptr == tcb_ptr {
                (*tcb_ptr).tid = Sys::getpid();
                TLS_READY = true;
                return;
            }
        }
    }

    let (_base, tcb_ptr) = alloc_thread_tls();
    if tcb_ptr.is_null() {
        return;
    }
    (*tcb_ptr).self_ptr = tcb_ptr;
    (*tcb_ptr).tid = Sys::getpid();

    if Sys::arch_prctl_set_fs(tcb_ptr as u64).is_err() {
        return;
    }

    TLS_READY = true;
}

/// Install the block this thread was handed, and declare TLS live.
///
/// # Safety
/// `tcb` must be the thread pointer the caller loaded into `fs_base`.
pub unsafe fn adopt_thread_tls(tcb: *mut Tcb) {
    (*tcb).self_ptr = tcb;
    TLS_READY = true;
}

/// # Safety
/// `tcb` must be a valid TCB pointer passed as TLS arg to `clone()`.
pub unsafe fn tls_init_new_thread(tcb: *mut Tcb) {
    debug_assert_eq!((*tcb).self_ptr, tcb);
}

/// Release a finished thread's TLS: its DTV, the blocks `__tls_get_addr`
/// allocated for `dlopen`ed modules, and the block the TCB sits at the top
/// of. Frees the allocation's base, not the thread pointer.
///
/// # Safety
/// `tcb` must be a thread pointer [`alloc_thread_tls`] produced, and no
/// thread may still be running on it.
pub unsafe fn free_thread_tls(tcb: *mut Tcb) {
    let dtv = (*tcb).dtv;
    if !dtv.is_null() {
        let layout = &*LAYOUT.get();
        for modid in 1..=*dtv {
            let block = *dtv.add(modid);
            // A static module's block is inside the TLS allocation below;
            // only a dynamic one was allocated on its own.
            if block != 0 && modid <= layout.count && layout.modules[modid - 1].offset == 0 {
                malloc::dealloc(block as *mut core::ffi::c_void);
            }
        }
        malloc::dealloc(dtv.cast());
        (*tcb).dtv = ptr::null_mut();
    }
    let base = (*tcb).tls_block;
    if base.is_null() {
        malloc::dealloc(tcb.cast());
    } else {
        malloc::dealloc(base.cast());
    }
}

/// The general-dynamic TLS accessor every `dlopen`able object's thread-local
/// reference goes through.
///
/// # Safety
/// `ti` must address a `TlsIndex` a `TLSGD` call site built, and the calling
/// thread must have a live TCB.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __tls_get_addr(ti: *const crate::ld_so::elf::TlsIndex) -> *mut u8 {
    let index = ptr::read(ti);
    let tcb = Tcb::current();
    let dtv = (*tcb).dtv;
    if dtv.is_null() || index.ti_module == 0 {
        return ptr::null_mut();
    }
    let block = if index.ti_module <= *dtv {
        let slot = dtv.add(index.ti_module);
        if *slot == 0 {
            let fresh = {
                let _guard = lock_layout();
                alloc_dynamic_block(index.ti_module)
            };
            if fresh == 0 {
                return ptr::null_mut();
            }
            *slot = fresh;
        }
        *slot
    } else {
        let grown = grow_dtv(tcb, index.ti_module);
        if grown == 0 {
            return ptr::null_mut();
        }
        grown
    };
    (block + index.ti_offset) as *mut u8
}

/// This thread's block for a module `dlopen` added after the DTV was built.
///
/// Takes [`LAYOUT_LOCK`]: reading a module count that a concurrent `dlopen`
/// is in the middle of raising would size the new vector short.
unsafe fn grow_dtv(tcb: *mut Tcb, modid: usize) -> usize {
    let _guard = lock_layout();
    let layout = &*LAYOUT.get();
    if modid > layout.count {
        return 0;
    }
    let fresh = malloc::alloc((layout.count + 1) * mem::size_of::<usize>()) as *mut usize;
    if fresh.is_null() {
        return 0;
    }
    let old = (*tcb).dtv;
    let old_len = *old;
    ptr::write_bytes(
        fresh as *mut u8,
        0,
        (layout.count + 1) * mem::size_of::<usize>(),
    );
    ptr::copy_nonoverlapping(old.add(1), fresh.add(1), old_len);
    *fresh = layout.count;
    (*tcb).dtv = fresh;
    malloc::dealloc(old.cast());

    let block = alloc_dynamic_block(modid);
    if block != 0 {
        *fresh.add(modid) = block;
    }
    block
}

/// # Safety
/// Caller holds [`LAYOUT_LOCK`].
unsafe fn alloc_dynamic_block(modid: usize) -> usize {
    let layout = &*LAYOUT.get();
    if modid == 0 || modid > layout.count {
        return 0;
    }
    let module = layout.modules[modid - 1];
    let block = malloc::memalign(module.align.max(MIN_TLS_ALIGN), module.memsz.max(1));
    if block.is_null() {
        return 0;
    }
    ptr::write_bytes(block, 0, module.memsz);
    if module.filesz > 0 && module.image != 0 {
        ptr::copy_nonoverlapping(module.image as *const u8, block, module.filesz);
    }
    block as usize
}
