//! `<sys/sysinfo.h>` and `getloadavg`: what the kernel's `sysinfo(2)` reports.

use core::ffi::{c_int, c_long};

use crate::conf::{_SC_NPROCESSORS_CONF, _SC_NPROCESSORS_ONLN, sysconf};
use crate::errno::{EINVAL, errno_set};
use crate::pal::{Pal, Sys};
use crate::types::sysinfo as Sysinfo;

/// `loads[]`'s fixed point: one runnable task reads `1 << SI_LOAD_SHIFT`.
pub const SI_LOAD_SHIFT: c_int = 16;
const _: () = assert!(SI_LOAD_SHIFT as u32 == slopos_abi::syscall::SI_LOAD_SHIFT);

/// `sysinfo(2)`.
///
/// # Safety
/// `info` addresses a writable `struct sysinfo`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sysinfo(info: *mut Sysinfo) -> c_int {
    match Sys::sysinfo(info) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

fn read() -> Option<Sysinfo> {
    let mut info = Sysinfo::default();
    // SAFETY: `info` is a live `struct sysinfo`.
    (unsafe { sysinfo(&raw mut info) } == 0).then_some(info)
}

/// `getloadavg(3)`: up to three of the one-, five- and fifteen-minute load
/// averages. The number stored, or -1.
///
/// # Safety
/// `loadavg` addresses `nelem` doubles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getloadavg(loadavg: *mut f64, nelem: c_int) -> c_int {
    if nelem < 0 || (nelem > 0 && loadavg.is_null()) {
        errno_set(EINVAL.raw());
        return -1;
    }
    let Some(info) = read() else {
        return -1;
    };
    let count = (nelem as usize).min(info.loads.len());
    for (i, load) in info.loads.iter().take(count).enumerate() {
        *loadavg.add(i) = *load as f64 / f64::from(1u32 << SI_LOAD_SHIFT);
    }
    count as c_int
}

/// `get_nprocs(3)`: the processors online.
#[unsafe(no_mangle)]
pub extern "C" fn get_nprocs() -> c_int {
    // SAFETY: `sysconf` reads no caller memory.
    unsafe { sysconf(_SC_NPROCESSORS_ONLN) as c_int }
}

/// `get_nprocs_conf(3)`: the processors configured.
#[unsafe(no_mangle)]
pub extern "C" fn get_nprocs_conf() -> c_int {
    // SAFETY: as `get_nprocs`.
    unsafe { sysconf(_SC_NPROCESSORS_CONF) as c_int }
}

pub(crate) fn phys_pages() -> c_long {
    read().map_or(-1, |info| pages(info.totalram, info.mem_unit))
}

pub(crate) fn avphys_pages() -> c_long {
    read().map_or(-1, |info| pages(info.freeram, info.mem_unit))
}

fn pages(units: u64, unit: u32) -> c_long {
    let bytes = units.saturating_mul(u64::from(unit.max(1)));
    (bytes / slopos_abi::PAGE_SIZE).min(c_long::MAX as u64) as c_long
}

/// `get_phys_pages(3)`.
#[unsafe(no_mangle)]
pub extern "C" fn get_phys_pages() -> c_long {
    phys_pages()
}

/// `get_avphys_pages(3)`.
#[unsafe(no_mangle)]
pub extern "C" fn get_avphys_pages() -> c_long {
    avphys_pages()
}
