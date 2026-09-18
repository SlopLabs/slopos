//! The ELF auxiliary vector, and `getauxval`.
//!
//! The kernel hands the auxv to the process on its entry stack and nothing
//! else ever sees it, so it has to be captured once during startup. The single
//! place that is handed the initial stack pointer is
//! [`crate::thread::tls::capture_tls_template_from_stack`], which already
//! walks past `argv` and `envp` to read `AT_PHDR`; [`capture`] is called from
//! inside that walk so there is exactly one walk and the CRT needs no change.

use core::cell::SyncUnsafeCell;
use core::ffi::c_ulong;

/// Start of the `[a_type, a_val]` pair array, or null before startup has run.
static AUXV: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);

/// Record where the auxv begins. Idempotent, and a no-op for a null pointer.
///
/// # Safety
/// `auxv` must address the `AT_*`/value pair array on the entry stack,
/// terminated by an `AT_NULL` entry.
pub(crate) unsafe fn capture(auxv: *const usize) {
    if !auxv.is_null() {
        *AUXV.get() = auxv as usize;
    }
}

/// `getauxval(3)`.
///
/// Answers 0 for a tag the kernel did not supply, which is what glibc does and
/// what every caller already has to handle: the auxv is a list of what the
/// kernel chose to say, not a fixed table. `errno` is left alone, because 0 is
/// a legitimate value for several tags and a caller cannot distinguish the two
/// cases anyway.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getauxval(type_: c_ulong) -> c_ulong {
    tag(type_ as u64).unwrap_or(0) as c_ulong
}

/// Read a tag without the C conversion, for slibc's own use.
pub(crate) fn tag(type_: u64) -> Option<u64> {
    // SAFETY: `AUXV` is either 0 or the entry-stack array `capture` recorded,
    // which is `AT_NULL`-terminated and live for the whole process.
    unsafe {
        let base = *AUXV.get();
        if base == 0 {
            return None;
        }
        let mut p = base as *const usize;
        loop {
            let a_type = *p as u64;
            if a_type == slopos_abi::auxv::AT_NULL {
                return None;
            }
            if a_type == type_ {
                return Some(*p.add(1) as u64);
            }
            p = p.add(2);
        }
    }
}
