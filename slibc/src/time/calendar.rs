//! `struct tm` and the calendar half of `<time.h>`.
//!
//! SlopOS keeps no time zone database and reads no `TZ`: the only zone is
//! UTC. `localtime` is therefore `gmtime` and `mktime` is `timegm`, each pair
//! sharing one body rather than pretending to a conversion that would be the
//! identity anyway. Every producer writes `tm_isdst = 0`, `tm_gmtoff = 0` and
//! a `tm_zone` of `"UTC"`; `mktime` ignores `tm_isdst` on input.
//!
//! The arithmetic itself lives in [`slopos_slibc_core::calendar`] and the
//! rendering in [`slopos_slibc_core::strftime`], both host-tested. What is
//! here is marshalling, the shared statics POSIX allows, and the layout pins.

#![allow(non_camel_case_types)]

use core::cell::SyncUnsafeCell;
use core::ffi::{CStr, c_char, c_int, c_long};
use core::ptr;

use slopos_slibc_core::calendar::{self, Tm};
use slopos_slibc_core::strftime;

use crate::errno::{EINVAL, EOVERFLOW, errno_set};
use crate::types::time_t;

/// C's broken-down time in Linux x86-64's layout: the nine standard members,
/// then the two BSD extensions glibc and the target's `libc` both declare.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct tm {
    pub tm_sec: c_int,
    pub tm_min: c_int,
    pub tm_hour: c_int,
    pub tm_mday: c_int,
    pub tm_mon: c_int,
    pub tm_year: c_int,
    pub tm_wday: c_int,
    pub tm_yday: c_int,
    pub tm_isdst: c_int,
    pub tm_gmtoff: c_long,
    pub tm_zone: *const c_char,
}

impl tm {
    const fn empty() -> Self {
        Self {
            tm_sec: 0,
            tm_min: 0,
            tm_hour: 0,
            tm_mday: 0,
            tm_mon: 0,
            tm_year: 0,
            tm_wday: 0,
            tm_yday: 0,
            tm_isdst: 0,
            tm_gmtoff: 0,
            tm_zone: ptr::null(),
        }
    }
}

const _: () = assert!(size_of::<tm>() == 56);
const _: () = assert!(align_of::<tm>() == 8);
const _: () = assert!(core::mem::offset_of!(tm, tm_sec) == 0);
const _: () = assert!(core::mem::offset_of!(tm, tm_isdst) == 32);
const _: () = assert!(core::mem::offset_of!(tm, tm_gmtoff) == 40);
const _: () = assert!(core::mem::offset_of!(tm, tm_zone) == 48);

/// The one zone name this libc has. Exposed to C through `tm_zone` and
/// `tzname`, so it has to outlive every caller.
static UTC: [c_char; 4] = [b'U' as c_char, b'T' as c_char, b'C' as c_char, 0];

/// `asctime(3)`'s width, newline and NUL included.
const ASCTIME_LEN: usize = 26;

#[repr(transparent)]
struct SharedTm(tm);

// `tm_zone` makes `tm` non-`Sync`. The pointer it holds is `UTC`, which is
// static and never written, and the struct is the single-threaded scratch
// POSIX already says `gmtime` may return.
unsafe impl Sync for SharedTm {}

static SHARED_TM: SyncUnsafeCell<SharedTm> = SyncUnsafeCell::new(SharedTm(tm::empty()));
static SHARED_BUF: SyncUnsafeCell<[c_char; ASCTIME_LEN]> = SyncUnsafeCell::new([0; ASCTIME_LEN]);

fn shared_tm() -> *mut tm {
    SHARED_TM.get().cast::<tm>()
}

fn shared_buf() -> *mut c_char {
    SHARED_BUF.get().cast::<c_char>()
}

/// # Safety
/// `src` points at a readable `tm`.
unsafe fn parts_of(src: *const tm) -> Tm {
    Tm {
        sec: (*src).tm_sec,
        min: (*src).tm_min,
        hour: (*src).tm_hour,
        mday: (*src).tm_mday,
        mon: (*src).tm_mon,
        year: (*src).tm_year,
        wday: (*src).tm_wday,
        yday: (*src).tm_yday,
    }
}

/// # Safety
/// `dst` points at a writable `tm`.
unsafe fn store_parts(dst: *mut tm, parts: &Tm) {
    (*dst).tm_sec = parts.sec;
    (*dst).tm_min = parts.min;
    (*dst).tm_hour = parts.hour;
    (*dst).tm_mday = parts.mday;
    (*dst).tm_mon = parts.mon;
    (*dst).tm_year = parts.year;
    (*dst).tm_wday = parts.wday;
    (*dst).tm_yday = parts.yday;
    (*dst).tm_isdst = 0;
    (*dst).tm_gmtoff = 0;
    (*dst).tm_zone = UTC.as_ptr();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gmtime_r(timep: *const time_t, result: *mut tm) -> *mut tm {
    if timep.is_null() || result.is_null() {
        errno_set(EINVAL.raw());
        return ptr::null_mut();
    }

    let Some(parts) = calendar::utc_from_epoch(*timep) else {
        errno_set(EOVERFLOW.raw());
        return ptr::null_mut();
    };

    store_parts(result, &parts);
    result
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn localtime_r(timep: *const time_t, result: *mut tm) -> *mut tm {
    gmtime_r(timep, result)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn gmtime(timep: *const time_t) -> *mut tm {
    gmtime_r(timep, shared_tm())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn localtime(timep: *const time_t) -> *mut tm {
    gmtime_r(timep, shared_tm())
}

/// `timegm(3)`: the epoch second `tmp` names, with every field normalised in
/// place. `tm_isdst` is neither read nor honoured — there is no DST here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn timegm(tmp: *mut tm) -> time_t {
    if tmp.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }

    let mut parts = parts_of(tmp);
    let Some(secs) = calendar::epoch_from_tm(&mut parts) else {
        errno_set(EOVERFLOW.raw());
        return -1;
    };

    store_parts(tmp, &parts);
    secs
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mktime(tmp: *mut tm) -> time_t {
    timegm(tmp)
}

#[unsafe(no_mangle)]
pub extern "C" fn difftime(time1: time_t, time0: time_t) -> f64 {
    time1 as f64 - time0 as f64
}

/// `asctime_r(3)`. `buf` must have room for 26 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asctime_r(tmp: *const tm, buf: *mut c_char) -> *mut c_char {
    if tmp.is_null() || buf.is_null() {
        errno_set(EINVAL.raw());
        return ptr::null_mut();
    }

    let parts = parts_of(tmp);
    let mut rendered = [0u8; ASCTIME_LEN];
    if !strftime::asctime(&parts, &mut rendered) {
        errno_set(EOVERFLOW.raw());
        return ptr::null_mut();
    }

    ptr::copy_nonoverlapping(rendered.as_ptr(), buf.cast::<u8>(), ASCTIME_LEN);
    buf
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn asctime(tmp: *const tm) -> *mut c_char {
    asctime_r(tmp, shared_buf())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctime_r(timep: *const time_t, buf: *mut c_char) -> *mut c_char {
    let mut broken = tm::empty();
    if gmtime_r(timep, &mut broken).is_null() {
        return ptr::null_mut();
    }
    asctime_r(&broken, buf)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ctime(timep: *const time_t) -> *mut c_char {
    ctime_r(timep, shared_buf())
}

/// `%Z`'s zone name. `tm_zone` is a BSD extension, not one of the nine
/// members C17 7.27.1 gives `struct tm`, so a conforming caller can leave it
/// null, which reads as `"UTC"`. The `tm` pointer is [`strftime`]'s own,
/// live for the call.
struct TmZone(*const tm);

impl strftime::Zone for TmZone {
    fn name(&self) -> &[u8] {
        let zone = unsafe { (*self.0).tm_zone };
        if zone.is_null() {
            return b"UTC";
        }
        unsafe { CStr::from_ptr(zone) }.to_bytes()
    }
}

/// `strftime(3)`. Answers the length written, excluding the NUL, and 0 when
/// the result does not fit — in which case `s` holds unspecified bytes, as C
/// says.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strftime(
    s: *mut c_char,
    max: usize,
    format: *const c_char,
    tmp: *const tm,
) -> usize {
    if s.is_null() || format.is_null() || tmp.is_null() || max == 0 {
        return 0;
    }

    let parts = parts_of(tmp);
    let out = core::slice::from_raw_parts_mut(s.cast::<u8>(), max.min(isize::MAX as usize));
    let fmt = CStr::from_ptr(format).to_bytes();
    strftime::format(out, fmt, &parts, (*tmp).tm_gmtoff, &TmZone(tmp)).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub static mut timezone: c_long = 0;

#[unsafe(no_mangle)]
pub static mut daylight: c_int = 0;

#[unsafe(no_mangle)]
pub static mut tzname: [*mut c_char; 2] = [UTC.as_ptr().cast_mut(); 2];

/// `tzset(3)`. There is one zone and it never changes, so this only restates
/// the globals a program is allowed to have overwritten.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tzset() {
    timezone = 0;
    daylight = 0;
    tzname[0] = UTC.as_ptr().cast_mut();
    tzname[1] = UTC.as_ptr().cast_mut();
}
