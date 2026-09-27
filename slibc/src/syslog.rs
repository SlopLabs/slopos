//! `<syslog.h>`. The logging facility is the kernel log: each message is one
//! `klog_write`, formatted `<priority>ident[pid]: message` as RFC 3164 lays a
//! record out. `LOG_CONS` falls back to the console when the kernel refuses
//! the write, and `LOG_PERROR` copies the message to standard error.

use core::ffi::{VaList, c_char, c_int};
use core::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

use crate::errno::errno_get;
use crate::pal::{Pal, Sys};
use crate::stdio::printf::vsnprintf_impl;

pub const LOG_EMERG: c_int = 0;
pub const LOG_ALERT: c_int = 1;
pub const LOG_CRIT: c_int = 2;
pub const LOG_ERR: c_int = 3;
pub const LOG_WARNING: c_int = 4;
pub const LOG_NOTICE: c_int = 5;
pub const LOG_INFO: c_int = 6;
pub const LOG_DEBUG: c_int = 7;

pub const LOG_KERN: c_int = 0;
pub const LOG_USER: c_int = 8;
pub const LOG_MAIL: c_int = 16;
pub const LOG_DAEMON: c_int = 24;
pub const LOG_AUTH: c_int = 32;
pub const LOG_SYSLOG: c_int = 40;
pub const LOG_LPR: c_int = 48;
pub const LOG_NEWS: c_int = 56;
pub const LOG_UUCP: c_int = 64;
pub const LOG_CRON: c_int = 72;
pub const LOG_AUTHPRIV: c_int = 80;
pub const LOG_FTP: c_int = 88;
pub const LOG_LOCAL0: c_int = 128;
pub const LOG_LOCAL1: c_int = 136;
pub const LOG_LOCAL2: c_int = 144;
pub const LOG_LOCAL3: c_int = 152;
pub const LOG_LOCAL4: c_int = 160;
pub const LOG_LOCAL5: c_int = 168;
pub const LOG_LOCAL6: c_int = 176;
pub const LOG_LOCAL7: c_int = 184;

pub const LOG_PID: c_int = 0x01;
pub const LOG_CONS: c_int = 0x02;
pub const LOG_ODELAY: c_int = 0x04;
pub const LOG_NDELAY: c_int = 0x08;
pub const LOG_NOWAIT: c_int = 0x10;
pub const LOG_PERROR: c_int = 0x20;

const PRIORITY_MASK: c_int = 0x07;
const FACILITY_MASK: c_int = 0x03f8;
const LINE_MAX: usize = 1024;

static IDENT: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static OPTIONS: AtomicI32 = AtomicI32::new(0);
static FACILITY: AtomicI32 = AtomicI32::new(LOG_USER);
static MASK: AtomicI32 = AtomicI32::new(0xff);

/// `ident` is kept by pointer, as POSIX has `openlog` do: the caller's
/// string must outlive the log.
///
/// # Safety
/// `ident` is null or a NUL-terminated string that stays valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn openlog(ident: *const c_char, option: c_int, facility: c_int) {
    IDENT.store(ident.cast_mut().cast(), Ordering::Relaxed);
    OPTIONS.store(option, Ordering::Relaxed);
    if facility & !FACILITY_MASK == 0 && facility != 0 {
        FACILITY.store(facility, Ordering::Relaxed);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn closelog() {
    IDENT.store(core::ptr::null_mut(), Ordering::Relaxed);
    OPTIONS.store(0, Ordering::Relaxed);
    FACILITY.store(LOG_USER, Ordering::Relaxed);
}

/// Answers the previous mask; a zero `mask` changes nothing.
#[unsafe(no_mangle)]
pub extern "C" fn setlogmask(mask: c_int) -> c_int {
    if mask == 0 {
        MASK.load(Ordering::Relaxed)
    } else {
        MASK.swap(mask, Ordering::Relaxed)
    }
}

/// # Safety
/// `format`'s conversions match the arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syslog(priority: c_int, format: *const c_char, mut args: ...) {
    log(priority, format.cast(), &mut args);
}

/// # Safety
/// `format`'s conversions match `ap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsyslog(priority: c_int, format: *const c_char, mut ap: VaList<'_>) {
    log(priority, format.cast(), &mut ap);
}

struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Line {
    fn push(&mut self, bytes: &[u8]) {
        let room = LINE_MAX - 1 - self.len;
        let take = bytes.len().min(room);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
    }

    fn number(&mut self, mut value: u32) {
        let mut digits = [0u8; 10];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push(&digits[at..]);
    }
}

/// `format` with every `%m` replaced by the text of `errno` as it was on
/// entry, which is the one conversion `syslog` adds to `printf`'s.
unsafe fn expand_m(format: *const u8, errno: c_int, out: &mut Line) {
    let mut p = format;
    while *p != 0 {
        if *p == b'%' && *p.add(1) == b'm' {
            let text = crate::error::SyscallError::from_errno(errno).as_str();
            // A `%` in the substituted text would be read as a conversion.
            for &b in text.as_bytes() {
                out.push(if b == b'%' {
                    b"%%"
                } else {
                    core::slice::from_ref(&b)
                });
            }
            p = p.add(2);
        } else if *p == b'%' && *p.add(1) == b'%' {
            out.push(b"%%");
            p = p.add(2);
        } else {
            out.push(core::slice::from_ref(&*p));
            p = p.add(1);
        }
    }
}

unsafe fn log(priority: c_int, format: *const u8, ap: &mut VaList<'_>) {
    let errno = errno_get();
    if priority & !(PRIORITY_MASK | FACILITY_MASK) != 0 || format.is_null() {
        return;
    }
    if MASK.load(Ordering::Relaxed) & (1 << (priority & PRIORITY_MASK)) == 0 {
        return;
    }
    let priority = if priority & FACILITY_MASK == 0 {
        priority | FACILITY.load(Ordering::Relaxed)
    } else {
        priority
    };

    let mut fmt = Line {
        buf: [0; LINE_MAX],
        len: 0,
    };
    expand_m(format, errno, &mut fmt);

    let mut line = Line {
        buf: [0; LINE_MAX],
        len: 0,
    };
    line.push(b"<");
    line.number(priority as u32);
    line.push(b">");
    let body = line.len;
    let ident = IDENT.load(Ordering::Relaxed).cast_const();
    if !ident.is_null() {
        line.push(core::slice::from_raw_parts(
            ident,
            crate::string::u_strlen(ident),
        ));
    }
    let options = OPTIONS.load(Ordering::Relaxed);
    if options & LOG_PID != 0 {
        line.push(b"[");
        line.number(Sys::getpid() as u32);
        line.push(b"]");
    }
    if !ident.is_null() || options & LOG_PID != 0 {
        line.push(b": ");
    }
    let room = LINE_MAX - 1 - line.len;
    let wrote = vsnprintf_impl(
        line.buf.as_mut_ptr().add(line.len),
        room + 1,
        fmt.buf.as_ptr(),
        ap,
    );
    if wrote > 0 {
        line.len += (wrote as usize).min(room);
    }
    if line.buf[line.len - 1] != b'\n' {
        line.push(b"\n");
    }

    let logged = Sys::klog_write(&line.buf[..line.len]).is_ok();
    if !logged && options & LOG_CONS != 0 {
        if let Ok(fd) = Sys::open(b"/dev/console\0".as_ptr(), crate::ffi::O_WRONLY, 0) {
            let _ = Sys::write(fd, line.buf[body..].as_ptr(), line.len - body);
            let _ = Sys::close(fd);
        }
    }
    if options & LOG_PERROR != 0 {
        let _ = Sys::write(2, line.buf[body..].as_ptr(), line.len - body);
    }
    crate::errno::errno_set(errno);
}
