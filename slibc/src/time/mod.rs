//! Clock and sleep functions.

#[allow(dead_code)]
pub(crate) mod shim;
pub mod tests;

use crate::errno::errno_set;
use crate::pal::{Pal, Sys};

/// Linux's clock ids, taken from the ABI rather than restated here.
pub const CLOCK_REALTIME: i32 = slopos_abi::syscall::CLOCK_REALTIME as i32;
pub const CLOCK_MONOTONIC: i32 = slopos_abi::syscall::CLOCK_MONOTONIC as i32;

pub use slopos_abi::syscall::Timespec;

pub const fn timespec_from_nanos(nanos: u64) -> Timespec {
    Timespec {
        tv_sec: (nanos / 1_000_000_000) as i64,
        tv_nsec: (nanos % 1_000_000_000) as i64,
    }
}

/// POSIX timeval.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32 {
    if tp.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }

    let mut raw = [0u8; 16];
    match Sys::clock_gettime(clk_id as u64, raw.as_mut_ptr()) {
        Ok(()) => {
            (*tp).tv_sec = i64::from_le_bytes([
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            ]);
            (*tp).tv_nsec = i64::from_le_bytes([
                raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
            ]);
            0
        }
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gettimeofday(tv: *mut Timeval, _tz: *mut u8) -> i32 {
    if tv.is_null() {
        return 0; // POSIX permits null tv
    }

    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = clock_gettime(CLOCK_REALTIME, &mut ts);
    if ret != 0 {
        return ret;
    }

    (*tv).tv_sec = ts.tv_sec;
    (*tv).tv_usec = ts.tv_nsec / 1000;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn time(tloc: *mut i64) -> i64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let ret = clock_gettime(CLOCK_REALTIME, &mut ts);
    if ret != 0 {
        return -1;
    }

    if !tloc.is_null() {
        *tloc = ts.tv_sec;
    }
    ts.tv_sec
}

/// `nanosleep(2)`. Interruptible: a delivered signal ends the sleep with
/// `EINTR` and, for a non-null `rem`, the time that was left.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> i32 {
    if req.is_null() {
        errno_set(crate::errno::EINVAL.raw());
        return -1;
    }
    match Sys::nanosleep(req, rem) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `clock_getres(2)`.
///
/// The kernel has no `clock_getres` syscall, so the clock id is validated by
/// reading the clock itself rather than against a list here that could drift
/// from the kernel's. The resolution reported is one nanosecond because that
/// is the denomination `clock_gettime` answers in; the underlying counter is
/// coarser, exactly as it is on any host whose libc reports the same.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_getres(clk_id: i32, tp: *mut Timespec) -> i32 {
    let mut probe = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if let Err(e) = Sys::clock_gettime(clk_id as u64, &raw mut probe as *mut u8) {
        errno_set(e.raw());
        return -1;
    }
    if !tp.is_null() {
        (*tp).tv_sec = 0;
        (*tp).tv_nsec = 1;
    }
    0
}

/// `TIMER_ABSTIME`: `rqtp` is a deadline on `clk_id` rather than an interval.
pub const TIMER_ABSTIME: i32 = 1;

/// `clock_nanosleep(2)`.
///
/// Answers the errno directly rather than setting it, which is the convention
/// this one call uses. An absolute deadline is converted against the named
/// clock's current reading, because the kernel's sleep takes an interval.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_nanosleep(
    clk_id: i32,
    flags: i32,
    rqtp: *const Timespec,
    rmtp: *mut Timespec,
) -> i32 {
    if rqtp.is_null() {
        return crate::errno::EFAULT.raw();
    }
    if flags != 0 && flags != TIMER_ABSTIME {
        return crate::errno::EINVAL.raw();
    }
    // A CPU-time clock is an accounting total, not something to sleep on.
    if clk_id != CLOCK_REALTIME && clk_id != CLOCK_MONOTONIC {
        return crate::errno::EINVAL.raw();
    }
    let want = *rqtp;
    if want.tv_sec < 0 || !(0..1_000_000_000).contains(&want.tv_nsec) {
        return crate::errno::EINVAL.raw();
    }

    let interval = if flags == TIMER_ABSTIME {
        let mut now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if let Err(e) = Sys::clock_gettime(clk_id as u64, &raw mut now as *mut u8) {
            return e.raw();
        }
        let mut sec = want.tv_sec - now.tv_sec;
        let mut nsec = want.tv_nsec - now.tv_nsec;
        if nsec < 0 {
            nsec += 1_000_000_000;
            sec -= 1;
        }
        if sec < 0 {
            // The deadline has passed; there is nothing to wait for.
            return 0;
        }
        Timespec {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    } else {
        want
    };

    // An absolute sleep has no remainder to report: the deadline is the
    // caller's own and re-deriving it is a second call to this function.
    let rem = if flags == TIMER_ABSTIME {
        core::ptr::null_mut()
    } else {
        rmtp
    };
    match Sys::nanosleep(&raw const interval, rem) {
        Ok(()) => 0,
        Err(e) => e.raw(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn usleep(usec: u32) -> i32 {
    let ts = Timespec {
        tv_sec: (usec / 1_000_000) as i64,
        tv_nsec: ((usec % 1_000_000) as i64) * 1000,
    };
    nanosleep(&ts, core::ptr::null_mut())
}

/// Answers the seconds left when a signal cut the sleep short, as POSIX
/// requires — the sleep is interruptible now, so 0 would be a wrong answer
/// rather than a simplification.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sleep(seconds: u32) -> u32 {
    let ts = Timespec {
        tv_sec: seconds as i64,
        tv_nsec: 0,
    };
    let mut rem = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if nanosleep(&ts, &mut rem) == 0 {
        return 0;
    }
    // Round up: POSIX wants the count of seconds still unslept, and reporting
    // a truncated 0 would look like the whole interval elapsed.
    let left = rem.tv_sec + i64::from(rem.tv_nsec > 0);
    left.clamp(0, u32::MAX as i64) as u32
}
