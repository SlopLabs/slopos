//! `times(3)`. The kernel's process CPU clock has no user/system split, so
//! all of it is `tms_utime`. Every wait goes through [`reap`], which adds a
//! terminated child's time to `tms_cutime`/`tms_cstime`.

use core::ffi::{c_int, c_long};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::errno::{Errno, errno_set};
use crate::pal::{Pal, Sys};
use crate::process::wait::{WIFEXITED, WIFSIGNALED};
use crate::types::{rusage, timeval};
use slopos_abi::syscall::{CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, Timespec};

static CHILD_USER_US: AtomicU64 = AtomicU64::new(0);
static CHILD_SYSTEM_US: AtomicU64 = AtomicU64::new(0);

/// `sysconf(_SC_CLK_TCK)`.
const TICKS_PER_SEC: u64 = 1000;

fn micros(tv: &timeval) -> u64 {
    (tv.tv_sec.max(0) as u64)
        .saturating_mul(1_000_000)
        .saturating_add(tv.tv_usec.max(0) as u64)
}

/// `wait4(2)`, adding a terminated child's CPU time to the child totals.
///
/// # Safety
/// `status` and `usage` are null or writable.
pub unsafe fn reap(
    pid: i32,
    status: *mut c_int,
    options: c_int,
    usage: *mut rusage,
) -> Result<i32, Errno> {
    let mut code: c_int = 0;
    let mut own = rusage::default();
    let child = Sys::wait4(pid, &raw mut code, options, (&raw mut own).cast())?;
    if child > 0 {
        if WIFEXITED(code) || WIFSIGNALED(code) {
            CHILD_USER_US.fetch_add(micros(&own.ru_utime), Ordering::Relaxed);
            CHILD_SYSTEM_US.fetch_add(micros(&own.ru_stime), Ordering::Relaxed);
        }
        if !status.is_null() {
            *status = code;
        }
        if !usage.is_null() {
            *usage = own;
        }
    }
    Ok(child)
}

fn clock_ticks(clock: u64) -> Result<u64, Errno> {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    Sys::clock_gettime(clock, (&raw mut ts).cast::<u8>())?;
    Ok((ts.tv_sec.max(0) as u64)
        .saturating_mul(TICKS_PER_SEC)
        .saturating_add(ts.tv_nsec.max(0) as u64 / (1_000_000_000 / TICKS_PER_SEC)))
}

/// `struct tms`, as the target's `libc` declares it.
#[repr(C)]
pub struct Tms {
    pub tms_utime: c_long,
    pub tms_stime: c_long,
    pub tms_cutime: c_long,
    pub tms_cstime: c_long,
}

/// Measures elapsed ticks from boot.
///
/// # Safety
/// `buf` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn times(buf: *mut Tms) -> c_long {
    let cpu = match clock_ticks(CLOCK_PROCESS_CPUTIME_ID) {
        Ok(ticks) => ticks,
        Err(e) => {
            errno_set(e.raw());
            return -1;
        }
    };
    let elapsed = match clock_ticks(CLOCK_MONOTONIC) {
        Ok(ticks) => ticks,
        Err(e) => {
            errno_set(e.raw());
            return -1;
        }
    };
    if buf.is_null() {
        errno_set(crate::errno::EFAULT.raw());
        return -1;
    }
    let to_ticks = |us: u64| (us / (1_000_000 / TICKS_PER_SEC)) as c_long;
    *buf = Tms {
        tms_utime: cpu as c_long,
        tms_stime: 0,
        tms_cutime: to_ticks(CHILD_USER_US.load(Ordering::Relaxed)),
        tms_cstime: to_ticks(CHILD_SYSTEM_US.load(Ordering::Relaxed)),
    };
    elapsed as c_long
}
