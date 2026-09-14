//! `stty`: the terminal's line settings.
//!
//! Every name accepted maps to a bit `UserTermios` really has; one it cannot
//! represent is refused rather than accepted and dropped.

use slopos_abi::syscall::{
    ControlFlags, InputFlags, LocalFlags, NCCS, OutputFlags, POSIX_VDISABLE, UserTermios,
    UserWinsize, VDISCARD, VEOF, VEOL, VEOL2, VERASE, VINTR, VKILL, VLNEXT, VMIN, VQUIT, VREPRINT,
    VSTART, VSTOP, VSUSP, VTIME, VWERASE,
};

use super::io::Sink;
use super::opts::parse_u64;
use super::{Ctx, Tool};
use crate::syscall::fs;

pub static TOOLS: &[Tool] = &[Tool {
    name: "stty",
    desc: "Report or change terminal settings",
    usage: USAGE,
    run: stty,
}];

const USAGE: &str = "stty [-a] [-g] [setting...]";

/// Which `termios` word a flag lives in.
#[derive(Clone, Copy, PartialEq)]
enum Group {
    Control,
    Input,
    Output,
    Local,
}

/// Name, word, and bit mask. `tab3` is a two-bit mask, which is why the test
/// is `bits & mask == mask` rather than a single-bit probe.
static FLAGS: &[(&str, Group, u32)] = &[
    ("parenb", Group::Control, ControlFlags::PARENB.bits()),
    ("parodd", Group::Control, ControlFlags::PARODD.bits()),
    ("hupcl", Group::Control, ControlFlags::HUPCL.bits()),
    ("cstopb", Group::Control, ControlFlags::CSTOPB.bits()),
    ("cread", Group::Control, ControlFlags::CREAD.bits()),
    ("clocal", Group::Control, ControlFlags::CLOCAL.bits()),
    ("crtscts", Group::Control, ControlFlags::CRTSCTS.bits()),
    ("ignbrk", Group::Input, InputFlags::IGNBRK.bits()),
    ("brkint", Group::Input, InputFlags::BRKINT.bits()),
    ("ignpar", Group::Input, InputFlags::IGNPAR.bits()),
    ("parmrk", Group::Input, InputFlags::PARMRK.bits()),
    ("inpck", Group::Input, InputFlags::INPCK.bits()),
    ("istrip", Group::Input, InputFlags::ISTRIP.bits()),
    ("inlcr", Group::Input, InputFlags::INLCR.bits()),
    ("igncr", Group::Input, InputFlags::IGNCR.bits()),
    ("icrnl", Group::Input, InputFlags::ICRNL.bits()),
    ("iuclc", Group::Input, InputFlags::IUCLC.bits()),
    ("ixon", Group::Input, InputFlags::IXON.bits()),
    ("ixany", Group::Input, InputFlags::IXANY.bits()),
    ("ixoff", Group::Input, InputFlags::IXOFF.bits()),
    ("imaxbel", Group::Input, InputFlags::IMAXBEL.bits()),
    ("iutf8", Group::Input, InputFlags::IUTF8.bits()),
    ("opost", Group::Output, OutputFlags::OPOST.bits()),
    ("olcuc", Group::Output, OutputFlags::OLCUC.bits()),
    ("onlcr", Group::Output, OutputFlags::ONLCR.bits()),
    ("ocrnl", Group::Output, OutputFlags::OCRNL.bits()),
    ("onocr", Group::Output, OutputFlags::ONOCR.bits()),
    ("onlret", Group::Output, OutputFlags::ONLRET.bits()),
    ("tab3", Group::Output, OutputFlags::TAB3.bits()),
    ("isig", Group::Local, LocalFlags::ISIG.bits()),
    ("icanon", Group::Local, LocalFlags::ICANON.bits()),
    ("iexten", Group::Local, LocalFlags::IEXTEN.bits()),
    ("echo", Group::Local, LocalFlags::ECHO.bits()),
    ("echoe", Group::Local, LocalFlags::ECHOE.bits()),
    ("echok", Group::Local, LocalFlags::ECHOK.bits()),
    ("echonl", Group::Local, LocalFlags::ECHONL.bits()),
    ("echoctl", Group::Local, LocalFlags::ECHOCTL.bits()),
    ("echoprt", Group::Local, LocalFlags::ECHOPRT.bits()),
    ("echoke", Group::Local, LocalFlags::ECHOKE.bits()),
    ("noflsh", Group::Local, LocalFlags::NOFLSH.bits()),
    ("tostop", Group::Local, LocalFlags::TOSTOP.bits()),
    ("flusho", Group::Local, LocalFlags::FLUSHO.bits()),
    ("pendin", Group::Local, LocalFlags::PENDIN.bits()),
    ("extproc", Group::Local, LocalFlags::EXTPROC.bits()),
];

/// Name, `c_cc` index, and whether the value is a count rather than a key.
static CHARS: &[(&str, usize, bool)] = &[
    ("intr", VINTR, false),
    ("quit", VQUIT, false),
    ("erase", VERASE, false),
    ("kill", VKILL, false),
    ("eof", VEOF, false),
    ("eol", VEOL, false),
    ("eol2", VEOL2, false),
    ("start", VSTART, false),
    ("stop", VSTOP, false),
    ("susp", VSUSP, false),
    ("rprnt", VREPRINT, false),
    ("werase", VWERASE, false),
    ("lnext", VLNEXT, false),
    ("discard", VDISCARD, false),
    ("min", VMIN, true),
    ("time", VTIME, true),
];

/// Settings a POSIX `stty` takes that this termios has no bit for: the delay
/// classes, the case-mapping local flag, the control characters with no `c_cc`
/// slot, and the split input/output speeds this kernel folds into `CBAUD`.
static UNSUPPORTED: &[&str] = &[
    "xcase", "lcase", "parext", "cdtrdsr", "ofill", "ofdel", "cr1", "cr2", "cr3", "nl1", "bs1",
    "ff1", "vt1", "tab1", "tab2", "dsusp", "swtch", "status", "ispeed", "ospeed",
];

/// `-a` and `-g` are matched as whole words rather than parsed as options: a
/// setting like `-echo` is spelt exactly like a clustered flag, so `stty` has
/// never been able to use `getopt` for its operands.
fn stty(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut all = false;
    let mut save = false;
    let mut index = 1;
    while index < argv.len() {
        let word = argv[index];
        if word == b"-a" || word == b"--all" {
            all = true;
        } else if word == b"-g" || word == b"--save" {
            save = true;
        } else {
            break;
        }
        index += 1;
    }
    let settings = &argv[index..];

    let Ok(mut termios) = fs::tcgetattr(0) else {
        ctx.warn_at(b"standard input", b"not a terminal");
        return 1;
    };

    if all || save {
        if (all && save) || !settings.is_empty() {
            return ctx.usage(USAGE);
        }
        if save {
            print_save(ctx, &termios);
        } else {
            print_all(ctx, &termios);
        }
        return 0;
    }
    if settings.is_empty() {
        print_short(ctx, &termios);
        return 0;
    }

    let mut winsize = fs::tiocgwinsz(0).unwrap_or_default();
    let mut touched = false;
    let mut resized = false;
    let mut i = 0;
    while i < settings.len() {
        let word = settings[i];
        i += 1;
        if word == b"size" {
            ctx.out.u(winsize.ws_row as u64);
            ctx.out.b(b' ');
            ctx.out.u(winsize.ws_col as u64);
            ctx.out.nl();
            continue;
        }
        if word == b"rows" || word == b"columns" || word == b"cols" {
            let Some(value) = settings.get(i) else {
                ctx.warn_at(word, b"missing argument");
                return 1;
            };
            i += 1;
            let Some(count) = parse_u64(value).filter(|n| *n <= u16::MAX as u64) else {
                ctx.warn_at(value, b"invalid integer argument");
                return 1;
            };
            if word == b"rows" {
                winsize.ws_row = count as u16;
            } else {
                winsize.ws_col = count as u16;
            }
            resized = true;
            continue;
        }
        if word.contains(&b':') {
            if !load_saved(word, &mut termios) {
                ctx.warn_at(word, b"invalid saved settings");
                return 1;
            }
            touched = true;
            continue;
        }
        if let Some(&(_, slot, numeric)) = CHARS.iter().find(|c| word == c.0.as_bytes()) {
            let Some(value) = settings.get(i) else {
                ctx.warn_at(word, b"missing argument");
                return 1;
            };
            i += 1;
            let parsed = if numeric {
                parse_u64(value).filter(|n| *n <= 0xff).map(|n| n as u8)
            } else {
                parse_cc(value)
            };
            let Some(byte) = parsed else {
                ctx.warn_at(value, b"invalid argument");
                return 1;
            };
            termios.c_cc[slot] = byte;
            touched = true;
            continue;
        }

        let negated = word.len() > 1 && word[0] == b'-';
        let body = if negated { &word[1..] } else { word };
        let on = !negated;
        if body == b"raw" {
            set_raw(&mut termios, on);
        } else if body == b"cooked" {
            set_raw(&mut termios, !on);
        } else if body == b"sane" && on {
            set_sane(&mut termios);
        } else if let Some(&(_, group, bit)) = FLAGS.iter().find(|f| body == f.0.as_bytes()) {
            flag_set(&mut termios, group, bit, on);
        } else if UNSUPPORTED.iter().any(|name| body == name.as_bytes()) {
            ctx.warn_at(word, b"unsupported setting");
            return 1;
        } else {
            ctx.warn_at(word, b"invalid argument");
            return 1;
        }
        touched = true;
    }

    if touched && fs::tcsetattr(0, &termios).is_err() {
        ctx.warn(b"unable to perform all requested operations");
        return 1;
    }
    if resized && fs::tiocswinsz(0, &winsize).is_err() {
        ctx.warn(b"unable to set window size");
        return 1;
    }
    0
}

fn flag_get(t: &UserTermios, group: Group, bit: u32) -> bool {
    let bits = match group {
        Group::Control => t.c_cflag.bits(),
        Group::Input => t.c_iflag.bits(),
        Group::Output => t.c_oflag.bits(),
        Group::Local => t.c_lflag.bits(),
    };
    bits & bit == bit
}

fn flag_set(t: &mut UserTermios, group: Group, bit: u32, on: bool) {
    let merge = |bits: u32| if on { bits | bit } else { bits & !bit };
    match group {
        Group::Control => t.c_cflag = ControlFlags::from_bits_retain(merge(t.c_cflag.bits())),
        Group::Input => t.c_iflag = InputFlags::from_bits_retain(merge(t.c_iflag.bits())),
        Group::Output => t.c_oflag = OutputFlags::from_bits_retain(merge(t.c_oflag.bits())),
        Group::Local => t.c_lflag = LocalFlags::from_bits_retain(merge(t.c_lflag.bits())),
    }
}

/// GNU's `raw` / `cooked` pair, minus `istrip`: stripping the eighth bit would
/// mangle the UTF-8 this console speaks. `IUTF8` survives `raw` for the same
/// reason.
fn set_raw(t: &mut UserTermios, on: bool) {
    if on {
        t.c_iflag &= InputFlags::IUTF8;
        t.c_oflag.remove(OutputFlags::OPOST);
        t.c_lflag
            .remove(LocalFlags::ISIG | LocalFlags::ICANON | LocalFlags::IEXTEN);
        t.c_cc[VMIN] = 1;
        t.c_cc[VTIME] = 0;
    } else {
        t.c_iflag
            .insert(InputFlags::BRKINT | InputFlags::IGNPAR | InputFlags::ICRNL | InputFlags::IXON);
        t.c_oflag.insert(OutputFlags::OPOST | OutputFlags::ONLCR);
        t.c_lflag
            .insert(LocalFlags::ISIG | LocalFlags::ICANON | LocalFlags::IEXTEN);
    }
}

/// The baud code, `hupcl`, `clocal` and `crtscts` are line properties, not
/// discipline state, so `sane` leaves them where it found them.
fn set_sane(t: &mut UserTermios) {
    let keep = t.c_cflag.bits()
        & !(ControlFlags::PARENB
            | ControlFlags::PARODD
            | ControlFlags::CSTOPB
            | ControlFlags::CSIZE)
            .bits();
    t.c_iflag = InputFlags::BRKINT
        | InputFlags::ICRNL
        | InputFlags::IXON
        | InputFlags::IMAXBEL
        | InputFlags::IUTF8;
    t.c_oflag = OutputFlags::OPOST | OutputFlags::ONLCR;
    t.c_cflag =
        ControlFlags::from_bits_retain(keep | (ControlFlags::CREAD | ControlFlags::CS8).bits());
    t.c_lflag = LocalFlags::ISIG
        | LocalFlags::ICANON
        | LocalFlags::IEXTEN
        | LocalFlags::ECHO
        | LocalFlags::ECHOE
        | LocalFlags::ECHOK
        | LocalFlags::ECHOCTL
        | LocalFlags::ECHOKE;
    t.c_cc = UserTermios::default().c_cc;
}

fn sane_of(t: &UserTermios) -> UserTermios {
    let mut copy = *t;
    set_sane(&mut copy);
    copy
}

fn csize_name(t: &UserTermios) -> &'static str {
    let size = t.c_cflag.bits() & ControlFlags::CSIZE.bits();
    if size == ControlFlags::CS8.bits() {
        "cs8"
    } else if size == ControlFlags::CS7.bits() {
        "cs7"
    } else if size == ControlFlags::CS6.bits() {
        "cs6"
    } else {
        "cs5"
    }
}

fn print_all(ctx: &mut Ctx, t: &UserTermios) {
    let ws = fs::tiocgwinsz(0).unwrap_or_default();
    print_header(ctx, t, Some(&ws));
    print_chars(ctx, t, None);
    for group in [Group::Control, Group::Input, Group::Output, Group::Local] {
        print_group(ctx, t, group, None);
    }
}

/// The short form: the speed, then only what `sane` would have set otherwise.
fn print_short(ctx: &mut Ctx, t: &UserTermios) {
    print_header(ctx, t, None);
    let reference = sane_of(t);
    print_chars(ctx, t, Some(&reference));
    for group in [Group::Control, Group::Input, Group::Output, Group::Local] {
        print_group(ctx, t, group, Some(&reference));
    }
}

fn print_header(ctx: &mut Ctx, t: &UserTermios, window: Option<&UserWinsize>) {
    ctx.out.s("speed ");
    ctx.out.u(t.c_ospeed as u64);
    ctx.out.s(" baud;");
    if let Some(ws) = window {
        ctx.out.s(" rows ");
        ctx.out.u(ws.ws_row as u64);
        ctx.out.s("; columns ");
        ctx.out.u(ws.ws_col as u64);
        ctx.out.b(b';');
    }
    ctx.out.s(" line = ");
    ctx.out.u(t.c_line as u64);
    ctx.out.b(b';');
    ctx.out.nl();
}

fn print_chars(ctx: &mut Ctx, t: &UserTermios, reference: Option<&UserTermios>) {
    let mut count = 0;
    for &(name, slot, numeric) in CHARS {
        if let Some(sane) = reference {
            if sane.c_cc[slot] == t.c_cc[slot] {
                continue;
            }
        }
        if count > 0 {
            ctx.out.b(b' ');
        }
        ctx.out.s(name);
        ctx.out.s(" = ");
        if numeric {
            ctx.out.u(t.c_cc[slot] as u64);
        } else {
            write_cc(&mut ctx.out, t.c_cc[slot]);
        }
        ctx.out.b(b';');
        count += 1;
    }
    if count > 0 {
        ctx.out.nl();
    }
}

fn print_group(ctx: &mut Ctx, t: &UserTermios, group: Group, reference: Option<&UserTermios>) {
    let mut count = 0;
    if group == Group::Control {
        let differs = reference.is_none_or(|sane| csize_name(sane) != csize_name(t));
        if differs {
            ctx.out.s(csize_name(t));
            count += 1;
        }
    }
    for &(name, word, bit) in FLAGS {
        if word != group {
            continue;
        }
        let on = flag_get(t, group, bit);
        if let Some(sane) = reference {
            if flag_get(sane, group, bit) == on {
                continue;
            }
        }
        if count > 0 {
            ctx.out.b(b' ');
        }
        if !on {
            ctx.out.b(b'-');
        }
        ctx.out.s(name);
        count += 1;
    }
    if count > 0 {
        ctx.out.nl();
    }
}

/// The four flag words, the line discipline and every `c_cc` byte, in hex —
/// the shape `stty` reads back. The baud lives in `c_cflag`, so a restored
/// string carries the speed with it.
fn print_save(ctx: &mut Ctx, t: &UserTermios) {
    let words = [
        t.c_iflag.bits(),
        t.c_oflag.bits(),
        t.c_cflag.bits(),
        t.c_lflag.bits(),
        t.c_line as u32,
    ];
    for (index, word) in words.iter().enumerate() {
        if index > 0 {
            ctx.out.b(b':');
        }
        write_hex(&mut ctx.out, *word);
    }
    for byte in t.c_cc {
        ctx.out.b(b':');
        write_hex(&mut ctx.out, byte as u32);
    }
    ctx.out.nl();
}

fn load_saved(spec: &[u8], t: &mut UserTermios) -> bool {
    let mut values = [0u32; 5 + NCCS];
    let mut count = 0;
    for field in spec.split(|&b| b == b':') {
        if count == values.len() {
            return false;
        }
        match parse_hex(field) {
            Some(value) => values[count] = value,
            None => return false,
        }
        count += 1;
    }
    if count != values.len() {
        return false;
    }
    t.c_iflag = InputFlags::from_bits_retain(values[0]);
    t.c_oflag = OutputFlags::from_bits_retain(values[1]);
    t.c_cflag = ControlFlags::from_bits_retain(values[2]);
    t.c_lflag = LocalFlags::from_bits_retain(values[3]);
    t.c_line = values[4] as u8;
    for (slot, value) in t.c_cc.iter_mut().zip(&values[5..]) {
        *slot = *value as u8;
    }
    true
}

fn write_cc(out: &mut Sink, value: u8) {
    match value {
        POSIX_VDISABLE => out.s("<undef>"),
        0x7f => out.s("^?"),
        c if c < 0x20 => {
            out.b(b'^');
            out.b(c + 0x40);
        }
        c => out.b(c),
    }
}

fn parse_cc(value: &[u8]) -> Option<u8> {
    if value == b"undef" || value == b"^-" {
        return Some(POSIX_VDISABLE);
    }
    if value.len() == 2 && value[0] == b'^' {
        let c = value[1].to_ascii_uppercase();
        return match c {
            b'?' => Some(0x7f),
            b'@'..=b'_' => Some(c - 0x40),
            _ => None,
        };
    }
    if value.len() == 2 && value[0] == b'\\' {
        return match value[1] {
            b'n' => Some(b'\n'),
            b'r' => Some(b'\r'),
            b't' => Some(b'\t'),
            b'0' => Some(0),
            _ => None,
        };
    }
    if value.len() == 1 {
        return Some(value[0]);
    }
    None
}

fn write_hex(out: &mut Sink, value: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 8];
    let mut pos = buf.len();
    let mut rest = value;
    loop {
        pos -= 1;
        buf[pos] = DIGITS[(rest & 0xf) as usize];
        rest >>= 4;
        if rest == 0 {
            break;
        }
    }
    out.write(&buf[pos..]);
}

fn parse_hex(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || bytes.len() > 8 {
        return None;
    }
    let mut value = 0u32;
    for &byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | digit as u32;
    }
    Some(value)
}
