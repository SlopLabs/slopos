//! `getopt`, and the GNU `getopt_long` and `getopt_long_only`.
//!
//! `getopt` stops at the first operand, as POSIX has it. The long forms take
//! options wherever they stand and, as glibc's do, move the operands they
//! step over after them at the start of the next call, not before an option
//! is returned, so its handler still finds the words that followed it at
//! `argv[optind]`; `optind` ends at the first operand. A leading `+` in
//! `optstring`, or `POSIXLY_CORRECT` in the environment, keeps POSIX's
//! order, and a leading `-` hands each operand back as option 1's argument.
//! After either, a leading `:` reports a missing argument as `:` and prints
//! nothing.

#![allow(non_upper_case_globals)]

use core::ffi::{c_char, c_int};

use crate::pal::{Pal, Sys};
use crate::string::u_strlen;

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct option {
    pub name: *const c_char,
    pub has_arg: c_int,
    pub flag: *mut c_int,
    pub val: c_int,
}

pub const no_argument: c_int = 0;
pub const required_argument: c_int = 1;
pub const optional_argument: c_int = 2;

#[unsafe(no_mangle)]
pub static mut optarg: *mut c_char = core::ptr::null_mut();
#[unsafe(no_mangle)]
pub static mut optind: c_int = 1;
#[unsafe(no_mangle)]
pub static mut opterr: c_int = 1;
#[unsafe(no_mangle)]
pub static mut optopt: c_int = '?' as c_int;

struct Scan {
    /// The unread rest of the short-option cluster in `argv[cluster_word]`.
    next: *const u8,
    cluster_word: c_int,
    /// `first_operand..operands_end` are operands stepped over, and
    /// `operands_end..optind` the options read since, which [`settle`] moves
    /// ahead of them.
    first_operand: c_int,
    operands_end: c_int,
}

static mut SCAN: Scan = Scan {
    next: core::ptr::null(),
    cluster_word: 0,
    first_operand: 1,
    operands_end: 1,
};

#[derive(Clone, Copy, PartialEq)]
enum Order {
    StopAtOperand,
    OptionsFirst,
    OperandsAsOption1,
}

#[derive(Clone, Copy, PartialEq)]
enum Longs {
    None,
    DoubleDash,
    SingleDashToo,
}

#[derive(Clone, Copy, PartialEq)]
enum LongMatch {
    One(usize),
    Ambiguous,
    Unknown,
}

unsafe fn bytes<'a>(s: *const u8) -> &'a [u8] {
    core::slice::from_raw_parts(s, u_strlen(s))
}

unsafe fn word<'a>(argv: *mut *mut c_char, at: c_int) -> &'a [u8] {
    bytes((*argv.add(at as usize)).cast())
}

fn is_option(arg: &[u8]) -> bool {
    arg.len() > 1 && arg[0] == b'-'
}

fn is_short(spec: &[u8], c: u8) -> bool {
    c != b':' && spec.contains(&c)
}

/// Move the option words read since the operands were stepped over to before
/// them, both in order, leaving the operands at `first_operand..optind`.
unsafe fn settle(argv: *mut *mut c_char) {
    let (first, end) = (SCAN.first_operand, SCAN.operands_end);
    if first < end && end < optind {
        core::slice::from_raw_parts_mut(argv.add(first as usize), (optind - first) as usize)
            .rotate_left((end - first) as usize);
        SCAN.first_operand = first + (optind - end);
    } else if first == end {
        SCAN.first_operand = optind;
    }
    SCAN.operands_end = optind;
}

unsafe fn complain(argv0: *const c_char, what: &[u8], detail: &[u8]) {
    let mut line = [0u8; 256];
    let mut len = 0;
    let program = if argv0.is_null() {
        b"" as &[u8]
    } else {
        bytes(argv0.cast())
    };
    for part in [program, b": ", what, detail, b"\n"] {
        let take = part.len().min(line.len() - len);
        line[len..len + take].copy_from_slice(&part[..take]);
        len += take;
    }
    let mut sent = 0;
    while sent < len {
        match Sys::write(
            crate::io::STDERR_FILENO,
            line[sent..len].as_ptr(),
            len - sent,
        ) {
            Ok(n) if n > 0 => sent += n,
            _ => break,
        }
    }
}

unsafe fn scan(
    argc: c_int,
    argv: *mut *mut c_char,
    optstring: *const c_char,
    longopts: *const option,
    longindex: *mut c_int,
    longs: Longs,
) -> c_int {
    optarg = core::ptr::null_mut();
    if optind == 0 {
        optind = 1;
        SCAN.next = core::ptr::null();
    }
    // A caller that moved `optind` back has begun another parse.
    SCAN.operands_end = SCAN.operands_end.min(optind);
    SCAN.first_operand = SCAN.first_operand.min(SCAN.operands_end);
    let mut spec = if optstring.is_null() {
        b"" as &[u8]
    } else {
        bytes(optstring.cast())
    };
    let order = match spec.first() {
        Some(b'+') => {
            spec = &spec[1..];
            Order::StopAtOperand
        }
        Some(b'-') => {
            spec = &spec[1..];
            Order::OperandsAsOption1
        }
        _ if longs == Longs::None
            || !crate::env::getenv(c"POSIXLY_CORRECT".as_ptr().cast()).is_null() =>
        {
            Order::StopAtOperand
        }
        _ => Order::OptionsFirst,
    };
    let quiet = spec.first() == Some(&b':');
    if quiet {
        spec = &spec[1..];
    }
    let loud = opterr != 0 && !quiet;
    let argv0 = *argv;

    let in_cluster = !SCAN.next.is_null() && SCAN.cluster_word == optind && *SCAN.next != 0;
    if !in_cluster {
        SCAN.next = core::ptr::null();
        let permute = order == Order::OptionsFirst;
        if permute {
            settle(argv);
            while optind < argc && !is_option(word(argv, optind)) {
                optind += 1;
            }
            SCAN.operands_end = optind;
        }
        if optind >= argc {
            if permute {
                optind = SCAN.first_operand;
            }
            return -1;
        }
        let arg = word(argv, optind);
        if arg == b"--" {
            optind += 1;
            if permute {
                settle(argv);
                optind = SCAN.first_operand;
            }
            return -1;
        }
        if !is_option(arg) {
            if order == Order::OperandsAsOption1 {
                optarg = *argv.add(optind as usize);
                optind += 1;
                return 1;
            }
            return -1;
        }
        let double = arg[1] == b'-';
        if !longopts.is_null() && (double || longs == Longs::SingleDashToo) {
            let body = &arg[if double { 2 } else { 1 }..];
            let name = body.split(|&b| b == b'=').next().unwrap_or(body);
            let single_short = !double && body.len() == 1 && is_short(spec, body[0]);
            let found = find_long(name, longopts);
            let as_short =
                !double && (single_short || found == LongMatch::Unknown && is_short(spec, body[0]));
            if !as_short {
                optind += 1;
                let dashes = &arg[..arg.len() - body.len()];
                return match found {
                    LongMatch::One(index) => long_option(
                        argc, argv, body, index, longopts, longindex, quiet, loud, argv0,
                    ),
                    LongMatch::Ambiguous | LongMatch::Unknown => {
                        optopt = 0;
                        if loud {
                            let what: &[u8] = if found == LongMatch::Ambiguous {
                                b"ambiguous option: "
                            } else {
                                b"unrecognized option: "
                            };
                            complain(argv0, what, &arg[..dashes.len() + name.len()]);
                        }
                        '?' as c_int
                    }
                };
            }
        }
        SCAN.next = (*argv.add(optind as usize)).cast::<u8>().add(1);
        SCAN.cluster_word = optind;
    }

    let c = *SCAN.next;
    SCAN.next = SCAN.next.add(1);
    let at_end = *SCAN.next == 0;
    if !is_short(spec, c) {
        optopt = c_int::from(c);
        if at_end {
            optind += 1;
        }
        if loud {
            complain(argv0, b"invalid option -- ", &[c]);
        }
        return '?' as c_int;
    }
    let pos = spec.iter().position(|&s| s == c).unwrap_or(0);
    let takes = spec.get(pos + 1) == Some(&b':');
    let optional = takes && spec.get(pos + 2) == Some(&b':');
    if !takes {
        if at_end {
            optind += 1;
        }
        return c_int::from(c);
    }
    let rest = SCAN.next;
    SCAN.next = core::ptr::null();
    if !at_end {
        optarg = rest as *mut c_char;
        optind += 1;
    } else if optional {
        optind += 1;
    } else if optind + 1 < argc {
        optarg = *argv.add(optind as usize + 1);
        optind += 2;
    } else {
        optopt = c_int::from(c);
        optind += 1;
        if loud {
            complain(argv0, b"option requires an argument -- ", &[c]);
        }
        return if quiet { ':' as c_int } else { '?' as c_int };
    }
    c_int::from(c)
}

unsafe fn find_long(name: &[u8], longopts: *const option) -> LongMatch {
    let mut prefix = None;
    let mut ambiguous = false;
    let mut i = 0;
    loop {
        let entry = &*longopts.add(i);
        if entry.name.is_null() {
            break;
        }
        let candidate = bytes(entry.name.cast());
        if candidate == name {
            return LongMatch::One(i);
        }
        if candidate.starts_with(name) {
            match prefix {
                None => prefix = Some(i),
                Some(j) => {
                    let other = &*longopts.add(j);
                    ambiguous |= other.has_arg != entry.has_arg
                        || other.flag != entry.flag
                        || other.val != entry.val;
                }
            }
        }
        i += 1;
    }
    match (prefix, ambiguous) {
        (Some(i), false) => LongMatch::One(i),
        (Some(_), true) => LongMatch::Ambiguous,
        (None, _) => LongMatch::Unknown,
    }
}

/// `body` is the word past its dashes: `name` or `name=value`.
#[allow(clippy::too_many_arguments)]
unsafe fn long_option(
    argc: c_int,
    argv: *mut *mut c_char,
    body: &[u8],
    index: usize,
    longopts: *const option,
    longindex: *mut c_int,
    quiet: bool,
    loud: bool,
    argv0: *const c_char,
) -> c_int {
    let entry = &*longopts.add(index);
    let name = bytes(entry.name.cast());
    let value = body.iter().position(|&b| b == b'=').map(|eq| eq + 1);
    match (entry.has_arg, value) {
        (no_argument, Some(_)) => {
            optopt = entry.val;
            if loud {
                complain(argv0, b"option doesn't allow an argument: ", name);
            }
            return '?' as c_int;
        }
        (required_argument | optional_argument, Some(at)) => {
            optarg = body.as_ptr().add(at) as *mut c_char;
        }
        (required_argument, None) => {
            if optind < argc {
                optarg = *argv.add(optind as usize);
                optind += 1;
            } else {
                optopt = entry.val;
                if loud {
                    complain(argv0, b"option requires an argument: ", name);
                }
                return if quiet { ':' as c_int } else { '?' as c_int };
            }
        }
        _ => {}
    }
    if !longindex.is_null() {
        *longindex = index as c_int;
    }
    if entry.flag.is_null() {
        entry.val
    } else {
        *entry.flag = entry.val;
        0
    }
}

/// `getopt(3)`.
///
/// # Safety
/// `argv` holds `argc` NUL-terminated strings; `optstring` is one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getopt(
    argc: c_int,
    argv: *const *mut c_char,
    optstring: *const c_char,
) -> c_int {
    scan(
        argc,
        argv as *mut *mut c_char,
        optstring,
        core::ptr::null(),
        core::ptr::null_mut(),
        Longs::None,
    )
}

/// `getopt_long(3)`.
///
/// # Safety
/// As [`getopt`]; `longopts` ends in an all-zero entry; `longindex` is null
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getopt_long(
    argc: c_int,
    argv: *const *mut c_char,
    optstring: *const c_char,
    longopts: *const option,
    longindex: *mut c_int,
) -> c_int {
    scan(
        argc,
        argv as *mut *mut c_char,
        optstring,
        longopts,
        longindex,
        Longs::DoubleDash,
    )
}

/// `getopt_long_only(3)`: as [`getopt_long`], with `-name` a long option too.
///
/// # Safety
/// As [`getopt_long`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getopt_long_only(
    argc: c_int,
    argv: *const *mut c_char,
    optstring: *const c_char,
    longopts: *const option,
    longindex: *mut c_int,
) -> c_int {
    scan(
        argc,
        argv as *mut *mut c_char,
        optstring,
        longopts,
        longindex,
        Longs::SingleDashToo,
    )
}
