//! The Level-1 Itanium unwinder, and how it finds an object's frame tables.
//!
//! `libc.so` exports the seventeen `_Unwind_*` entry points because it is
//! already the one artifact a process has to agree with about allocation,
//! `errno` and TLS. The C++ runtime's `__gxx_personality_v0` is the Level-2
//! half and lives in `libc++`.
//!
//! **Finding an FDE is `fde-custom`, and the two published roads are both
//! closed.** `fde-phdr-dl` reaches `dl_iterate_phdr` through the `libc`
//! *crate*, which slibc cannot depend on — slibc is what that crate declares;
//! `fde-registry` needs a `crtbegin` calling `__register_frame_info` per
//! object, which this target has not got. So the finder answers out of the
//! loader's own object table, the same walk with one fewer indirection.
//!
//! A static program has no loader table, and its one object's program headers
//! are in the auxv instead. Both cases are below, in that order.

#[cfg(feature = "unwinder")]
use unwinding::custom_eh_frame_finder::{
    EhFrameFinder, FrameInfo, FrameInfoKind, set_custom_eh_frame_finder,
};

#[cfg(feature = "unwinder")]
use crate::ld_so::elf::{PT_GNU_EH_FRAME, PT_LOAD, Phdr};

#[cfg(feature = "unwinder")]
struct LoadedObjects;

/// The `PT_GNU_EH_FRAME` of the object whose `PT_LOAD` covers `pc`, with that
/// segment's start as the text base.
///
/// # Safety
/// `phdr` addresses `phnum` program headers of an image loaded at `base`.
#[cfg(feature = "unwinder")]
unsafe fn frame_info(phdr: *const Phdr, phnum: usize, base: usize, pc: usize) -> Option<FrameInfo> {
    let mut text_base = None;
    let mut eh_frame_hdr = None;
    for i in 0..phnum {
        let ph = core::ptr::read_unaligned(phdr.add(i));
        let start = base + ph.p_vaddr as usize;
        match ph.p_type {
            PT_LOAD if (start..start + ph.p_memsz as usize).contains(&pc) => {
                text_base = Some(start);
            }
            PT_GNU_EH_FRAME => eh_frame_hdr = Some(start),
            _ => {}
        }
    }
    Some(FrameInfo {
        text_base: Some(text_base?),
        kind: FrameInfoKind::EhFrameHdr(eh_frame_hdr?),
    })
}

// SAFETY: every `FrameInfo` returned names a `PT_GNU_EH_FRAME` of an object
// that was mapped while the loader's lock was held. The lock is dropped before
// the unwinder parses those headers, so a `dlclose` on another thread can
// still unmap them mid-unwind — the same window glibc's `dl_iterate_phdr` road
// has, and not one this finder can close without holding the lock across a
// caller's parse.
#[cfg(feature = "unwinder")]
unsafe impl EhFrameFinder for LoadedObjects {
    fn find(&self, pc: usize) -> Option<FrameInfo> {
        // SAFETY: the table is walked under the loader's lock, so no entry
        // moves or is reclaimed mid-walk.
        unsafe {
            let _guard = crate::ld_so::lock();
            let loader = crate::ld_so::loader();
            for index in 0..loader.count() {
                // Mapped, not live: `dlclose` marks an object dying before it
                // runs its destructors, and one of those may throw.
                if !loader.is_mapped(index) {
                    continue;
                }
                let dso = loader.get(index);
                if !dso.contains(pc) {
                    continue;
                }
                return frame_info(dso.phdr, dso.phnum, dso.base, pc);
            }
            if loader.count() != 0 {
                return None;
            }
            static_program_frame_info(pc)
        }
    }
}

/// A statically linked program registers no object, so its headers come from
/// `AT_PHDR` — where they *are* — against `PT_PHDR`, where the image intended
/// them to be. The difference is the load bias, zero for the `ET_EXEC` images
/// this target links.
///
/// # Safety
/// Reads the program headers the kernel mapped, which outlive the process.
#[cfg(feature = "unwinder")]
unsafe fn static_program_frame_info(pc: usize) -> Option<FrameInfo> {
    let phdr = crate::auxv::tag(slopos_abi::auxv::AT_PHDR)? as *const Phdr;
    let phnum = crate::auxv::tag(slopos_abi::auxv::AT_PHNUM)? as usize;
    if phdr.is_null() || phnum == 0 {
        return None;
    }
    let mut base = 0usize;
    for i in 0..phnum {
        let ph = core::ptr::read_unaligned(phdr.add(i));
        if ph.p_type == crate::ld_so::elf::PT_PHDR {
            base = (phdr as usize).wrapping_sub(ph.p_vaddr as usize);
            break;
        }
    }
    frame_info(phdr, phnum, base, pc)
}

/// Install the finder, once, before anything can throw. A second call is
/// discarded: the finder already installed is this one.
pub(crate) fn init() {
    #[cfg(feature = "unwinder")]
    let _ = set_custom_eh_frame_finder(&LoadedObjects);
}
