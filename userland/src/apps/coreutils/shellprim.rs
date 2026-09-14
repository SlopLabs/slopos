//! Shell primitives: the utilities a script leans on between the commands it
//! actually meant to run — `echo`, `printf`, `test`, `seq`, `sleep`, `env`.

use std::ffi::CString;

use slopos_abi::fs::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFREG, S_IFSOCK};

use crate::syscall::UserFsStat;
use crate::syscall::core as sys_core;
use crate::syscall::fs as sys_fs;
use crate::syscall::process as sys_process;

use super::input::as_str;
use super::io::Sink;
use super::opts::{Opt, Opts, parse_i64};
use super::{Ctx, Tool, fsutil};

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "echo",
        desc: "Write arguments to standard output",
        usage: ECHO_USAGE,
        run: echo,
    },
    Tool {
        name: "printf",
        desc: "Format and print arguments",
        usage: PRINTF_USAGE,
        run: printf,
    },
    Tool {
        name: "test",
        desc: "Evaluate a conditional expression",
        usage: TEST_USAGE,
        run: test,
    },
    Tool {
        name: "[",
        desc: "Evaluate a conditional expression",
        usage: BRACKET_USAGE,
        run: test,
    },
    Tool {
        name: "true",
        desc: "Succeed",
        usage: "true",
        run: true_tool,
    },
    Tool {
        name: "false",
        desc: "Fail",
        usage: "false",
        run: false_tool,
    },
    Tool {
        name: "yes",
        desc: "Repeat a line until the pipe closes",
        usage: "yes [string...]",
        run: yes,
    },
    Tool {
        name: "seq",
        desc: "Print a sequence of numbers",
        usage: SEQ_USAGE,
        run: seq,
    },
    Tool {
        name: "sleep",
        desc: "Suspend execution for an interval",
        usage: SLEEP_USAGE,
        run: sleep,
    },
    Tool {
        name: "env",
        desc: "Print or modify the environment for a command",
        usage: ENV_USAGE,
        run: env,
    },
];

const ECHO_USAGE: &str = "echo [-neE] [string...]";
/// Ceiling on a field width or precision. The parser multiplies into a
/// `usize` with overflow checks off in release, and the width is an
/// allocation size — and the shell runs `printf` in its own process.
const FIELD_MAX: usize = 1 << 16;

const PRINTF_USAGE: &str = "printf format [argument...]";
const TEST_USAGE: &str = "test expression";
const BRACKET_USAGE: &str = "[ expression ]";
const SEQ_USAGE: &str = "seq [-w] [-s separator] [first [increment]] last";
const SLEEP_USAGE: &str = "sleep number[smhd]...";
const ENV_USAGE: &str = "env [-i] [-u name] [name=value...] [command [argument...]]";

fn true_tool(_ctx: &mut Ctx, _argv: &[&[u8]]) -> i32 {
    0
}

fn false_tool(_ctx: &mut Ctx, _argv: &[&[u8]]) -> i32 {
    1
}

fn hex_digit(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'f' => Some((byte - b'a' + 10) as u32),
        b'A'..=b'F' => Some((byte - b'A' + 10) as u32),
        _ => None,
    }
}

/// Expand the escape at `*at` (the backslash) and leave `*at` past it.
///
/// Octal is `\0NNN` in every context. POSIX lets a format spell it `\NNN`, at
/// the price of `printf '\0101'` and `printf '%b' '\0101'` disagreeing.
fn escape_one(bytes: &[u8], at: &mut usize, out: &mut Vec<u8>) {
    let mut i = *at + 1;
    let Some(&kind) = bytes.get(i) else {
        out.push(b'\\');
        *at = i;
        return;
    };
    i += 1;
    match kind {
        b'a' => out.push(0x07),
        b'b' => out.push(0x08),
        b'f' => out.push(0x0c),
        b'n' => out.push(b'\n'),
        b'r' => out.push(b'\r'),
        b't' => out.push(b'\t'),
        b'v' => out.push(0x0b),
        b'\\' => out.push(b'\\'),
        b'0' => {
            let mut value: u32 = 0;
            let mut taken = 0;
            while taken < 3 && matches!(bytes.get(i), Some(b'0'..=b'7')) {
                value = value * 8 + (bytes[i] - b'0') as u32;
                i += 1;
                taken += 1;
            }
            out.push(value as u8);
        }
        b'x' => {
            let mut value: u32 = 0;
            let mut taken = 0;
            while taken < 2 {
                let Some(digit) = bytes.get(i).copied().and_then(hex_digit) else {
                    break;
                };
                value = value * 16 + digit;
                i += 1;
                taken += 1;
            }
            if taken == 0 {
                out.push(b'\\');
                out.push(b'x');
            } else {
                out.push(value as u8);
            }
        }
        other => {
            out.push(b'\\');
            out.push(other);
        }
    }
    *at = i;
}

fn escape_into(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            escape_one(bytes, &mut i, out);
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
}

fn echo(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut args = &argv[1..];
    let mut newline = true;
    let mut escapes = false;
    while let Some(word) = args.first() {
        if word.len() < 2
            || word[0] != b'-'
            || !word[1..]
                .iter()
                .all(|&b| b == b'n' || b == b'e' || b == b'E')
        {
            break;
        }
        for &flag in &word[1..] {
            match flag {
                b'n' => newline = false,
                b'e' => escapes = true,
                _ => escapes = false,
            }
        }
        args = &args[1..];
    }

    let mut line = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            line.push(b' ');
        }
        if escapes {
            escape_into(&mut line, arg);
        } else {
            line.extend_from_slice(arg);
        }
    }
    if newline {
        line.push(b'\n');
    }
    ctx.out.write(&line);
    0
}

#[derive(Default)]
struct Spec {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    width: usize,
    prec: Option<usize>,
}

fn write_padded(out: &mut Sink, body: &[u8], spec: &Spec) {
    let pad = spec.width.saturating_sub(body.len());
    if spec.left {
        out.write(body);
        for _ in 0..pad {
            out.b(b' ');
        }
    } else {
        for _ in 0..pad {
            out.b(b' ');
        }
        out.write(body);
    }
}

fn write_number(out: &mut Sink, sign: &[u8], digits: &[u8], spec: &Spec) {
    let prec_zeros = spec.prec.map_or(0, |p| p.saturating_sub(digits.len()));
    let base = sign.len() + prec_zeros + digits.len();
    // `0` is dropped when a precision is given, as C has it.
    let zero_pad = if !spec.left && spec.zero && spec.prec.is_none() && spec.width > base {
        spec.width - base
    } else {
        0
    };
    let mut body = Vec::with_capacity(base + zero_pad);
    body.extend_from_slice(sign);
    for _ in 0..zero_pad + prec_zeros {
        body.push(b'0');
    }
    body.extend_from_slice(digits);
    write_padded(out, &body, spec);
}

fn radix_digits(mut value: u64, radix: u64, upper: bool) -> Vec<u8> {
    if value == 0 {
        return vec![b'0'];
    }
    let alphabet: &[u8] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(alphabet[(value % radix) as usize]);
        value /= radix;
    }
    digits.reverse();
    digits
}

/// A `%d` argument POSIX calls a C constant: decimal, or hexadecimal with an
/// `0x` prefix.
fn numeric_arg(arg: &[u8]) -> Option<i64> {
    if arg.is_empty() {
        return Some(0);
    }
    let (negative, body) = match arg[0] {
        b'-' => (true, &arg[1..]),
        b'+' => (false, &arg[1..]),
        _ => (false, arg),
    };
    let (radix, digits) = if body.len() > 2 && body[0] == b'0' && (body[1] | 0x20) == b'x' {
        (16u64, &body[2..])
    } else {
        (10u64, body)
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for &b in digits {
        let digit = hex_digit(b)?;
        if u64::from(digit) >= radix {
            return None;
        }
        value = value.checked_mul(radix)?.checked_add(digit as u64)?;
    }
    if negative {
        i64::try_from(value).ok().map(|v| -v)
    } else {
        i64::try_from(value).ok()
    }
}

fn printf(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let operands = &argv[1..];
    let Some(format) = operands.first().copied() else {
        return ctx.usage(PRINTF_USAGE);
    };
    let args = &operands[1..];

    let mut idx = 0usize;
    let mut status = 0;
    loop {
        let before = idx;
        let consuming = printf_pass(ctx, format, args, &mut idx, &mut status);
        if !consuming || idx == before || idx >= args.len() || ctx.out.broken() {
            break;
        }
    }
    status
}

/// One walk of the format. Returns whether it contained a conversion that
/// consumes an argument, which is what makes recycling terminate.
fn printf_pass(
    ctx: &mut Ctx,
    fmt: &[u8],
    args: &[&[u8]],
    idx: &mut usize,
    status: &mut i32,
) -> bool {
    let mut consuming = false;
    let mut scratch = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] == b'\\' {
            scratch.clear();
            escape_one(fmt, &mut i, &mut scratch);
            ctx.out.write(&scratch);
            continue;
        }
        if fmt[i] != b'%' {
            let start = i;
            while i < fmt.len() && fmt[i] != b'%' && fmt[i] != b'\\' {
                i += 1;
            }
            ctx.out.write(&fmt[start..i]);
            continue;
        }

        i += 1;
        let mut spec = Spec::default();
        while let Some(&flag) = fmt.get(i) {
            match flag {
                b'-' => spec.left = true,
                b'0' => spec.zero = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                _ => break,
            }
            i += 1;
        }
        while matches!(fmt.get(i), Some(b'0'..=b'9')) {
            spec.width = (spec.width * 10 + (fmt[i] - b'0') as usize).min(FIELD_MAX);
            i += 1;
        }
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            let mut prec = 0usize;
            while matches!(fmt.get(i), Some(b'0'..=b'9')) {
                prec = (prec * 10 + (fmt[i] - b'0') as usize).min(FIELD_MAX);
                i += 1;
            }
            spec.prec = Some(prec);
        }
        let Some(&conv) = fmt.get(i) else {
            ctx.warn(b"missing conversion specifier");
            *status = 1;
            break;
        };
        i += 1;
        if conv == b'%' {
            ctx.out.b(b'%');
            continue;
        }

        consuming = true;
        let arg: &[u8] = args.get(*idx).copied().unwrap_or(&[]);
        if *idx < args.len() {
            *idx += 1;
        }

        match conv {
            b's' | b'b' | b'c' => {
                let mut text: &[u8] = arg;
                if conv == b'b' {
                    scratch.clear();
                    escape_into(&mut scratch, arg);
                    text = &scratch;
                }
                if conv == b'c' {
                    text = &text[..text.len().min(1)];
                } else if let Some(prec) = spec.prec {
                    text = &text[..text.len().min(prec)];
                }
                write_padded(&mut ctx.out, text, &spec);
            }
            b'd' | b'i' | b'u' | b'x' | b'X' | b'o' => {
                let value = match numeric_arg(arg) {
                    Some(value) => value,
                    None => {
                        ctx.warn_at(arg, b"expected a numeric value");
                        *status = 1;
                        0
                    }
                };
                let (sign, magnitude): (&[u8], u64) = match conv {
                    b'd' | b'i' if value < 0 => (b"-", value.unsigned_abs()),
                    b'd' | b'i' if spec.plus => (b"+", value as u64),
                    b'd' | b'i' if spec.space => (b" ", value as u64),
                    _ => (b"", value as u64),
                };
                let digits = match conv {
                    b'x' => radix_digits(magnitude, 16, false),
                    b'X' => radix_digits(magnitude, 16, true),
                    b'o' => radix_digits(magnitude, 8, false),
                    _ => radix_digits(magnitude, 10, false),
                };
                write_number(&mut ctx.out, sign, &digits, &spec);
            }
            other => {
                ctx.warn_at(&[other], b"invalid conversion specifier");
                *status = 1;
            }
        }
    }
    consuming
}

fn stat_of(path: &str) -> Option<UserFsStat> {
    let c_path = CString::new(path).ok()?;
    let mut stat = UserFsStat::default();
    sys_fs::stat_path(c_path.as_ptr(), &mut stat).ok()?;
    Some(stat)
}

fn is_unary(op: &[u8]) -> bool {
    matches!(
        op,
        b"-e"
            | b"-f"
            | b"-d"
            | b"-s"
            | b"-r"
            | b"-w"
            | b"-x"
            | b"-h"
            | b"-L"
            | b"-b"
            | b"-c"
            | b"-p"
            | b"-S"
            | b"-n"
            | b"-z"
            | b"-t"
    )
}

fn is_binary(op: &[u8]) -> bool {
    matches!(
        op,
        b"=" | b"!="
            | b"<"
            | b">"
            | b"-eq"
            | b"-ne"
            | b"-lt"
            | b"-le"
            | b"-gt"
            | b"-ge"
            | b"-nt"
            | b"-ot"
            | b"-ef"
    )
}

/// `-r -w -x` answer from existence: single-user at uid 0, and nothing
/// consults the permission bits. `-b -c -p -S` answer from `st_mode`'s type.
fn unary(ctx: &mut Ctx, op: &[u8], arg: &[u8]) -> bool {
    match op {
        b"-n" => return !arg.is_empty(),
        b"-z" => return arg.is_empty(),
        b"-t" => {
            return parse_i64(arg)
                .is_some_and(|fd| i32::try_from(fd).map(sys_fs::isatty).unwrap_or(false));
        }
        _ => {}
    }
    let Some(path) = as_str(ctx, arg) else {
        return false;
    };
    if op == b"-h" || op == b"-L" {
        return std::fs::symlink_metadata(path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false);
    }
    let Some(stat) = stat_of(path) else {
        return false;
    };
    match op {
        b"-e" | b"-r" | b"-w" | b"-x" => true,
        b"-f" => stat.file_kind() == S_IFREG,
        b"-d" => stat.file_kind() == S_IFDIR,
        b"-s" => stat.st_size > 0,
        b"-b" => stat.file_kind() == S_IFBLK,
        b"-c" => stat.file_kind() == S_IFCHR,
        b"-p" => stat.file_kind() == S_IFIFO,
        b"-S" => stat.file_kind() == S_IFSOCK,
        _ => false,
    }
}

fn mtime_of(ctx: &mut Ctx, arg: &[u8]) -> Option<(i64, i64)> {
    let path = as_str(ctx, arg)?;
    let stat = stat_of(path)?;
    Some((stat.st_mtim.tv_sec, stat.st_mtim.tv_nsec))
}

fn binary(ctx: &mut Ctx, left: &[u8], op: &[u8], right: &[u8]) -> Option<bool> {
    match op {
        b"=" => return Some(left == right),
        b"!=" => return Some(left != right),
        b"<" => return Some(left < right),
        b">" => return Some(left > right),
        _ => {}
    }
    if matches!(op, b"-eq" | b"-ne" | b"-lt" | b"-le" | b"-gt" | b"-ge") {
        let (Some(a), Some(b)) = (parse_i64(left), parse_i64(right)) else {
            ctx.warn(b"integer expression expected");
            return None;
        };
        return Some(match op {
            b"-eq" => a == b,
            b"-ne" => a != b,
            b"-lt" => a < b,
            b"-le" => a <= b,
            b"-gt" => a > b,
            _ => a >= b,
        });
    }
    if op == b"-ef" {
        let (Some(a), Some(b)) = (
            as_str(ctx, left).and_then(stat_of),
            as_str(ctx, right).and_then(stat_of),
        ) else {
            return Some(false);
        };
        return Some(a.st_dev == b.st_dev && a.st_ino == b.st_ino);
    }
    let a = mtime_of(ctx, left);
    let b = mtime_of(ctx, right);
    Some(match (op, a, b) {
        (b"-nt", Some(a), Some(b)) => a > b,
        (b"-nt", Some(_), None) => true,
        (b"-ot", Some(a), Some(b)) => a < b,
        (b"-ot", None, Some(_)) => true,
        _ => false,
    })
}

struct Parser<'a, 'c> {
    args: &'a [&'a [u8]],
    pos: usize,
    ctx: &'c mut Ctx,
}

impl<'a> Parser<'a, '_> {
    fn fail(&mut self, message: &str) -> Option<bool> {
        self.ctx.warn(message.as_bytes());
        None
    }

    fn at(&self, offset: usize) -> Option<&'a [u8]> {
        self.args.get(self.pos + offset).copied()
    }

    fn is(&self, offset: usize, token: &[u8]) -> bool {
        self.at(offset) == Some(token)
    }

    fn expr(&mut self) -> Option<bool> {
        let mut value = self.term()?;
        while self.is(0, b"-o") {
            self.pos += 1;
            value = self.term()? || value;
        }
        Some(value)
    }

    fn term(&mut self) -> Option<bool> {
        let mut value = self.factor()?;
        while self.is(0, b"-a") {
            self.pos += 1;
            value = self.factor()? && value;
        }
        Some(value)
    }

    fn factor(&mut self) -> Option<bool> {
        match self.at(0) {
            Some(b"!") => {
                self.pos += 1;
                self.factor().map(|value| !value)
            }
            Some(b"(") => {
                self.pos += 1;
                let value = self.expr()?;
                if !self.is(0, b")") {
                    return self.fail("missing )");
                }
                self.pos += 1;
                Some(value)
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Option<bool> {
        if let (Some(left), Some(op), Some(right)) = (self.at(0), self.at(1), self.at(2))
            && is_binary(op)
        {
            self.pos += 3;
            return binary(self.ctx, left, op, right);
        }
        if let (Some(op), Some(arg)) = (self.at(0), self.at(1))
            && is_unary(op)
        {
            self.pos += 2;
            return Some(unary(self.ctx, op, arg));
        }
        match self.at(0) {
            Some(token) => {
                self.pos += 1;
                Some(!token.is_empty())
            }
            None => self.fail("missing operand"),
        }
    }
}

/// POSIX's two-argument form: `! string`, or a unary primary.
fn test_two(ctx: &mut Ctx, args: &[&[u8]]) -> Option<bool> {
    if args[0] == b"!" {
        return Some(args[1].is_empty());
    }
    if is_unary(args[0]) {
        return Some(unary(ctx, args[0], args[1]));
    }
    ctx.warn_at(args[0], b"unary operator expected");
    None
}

/// POSIX's three-argument form: a binary primary wins over `!` and over `( )`.
/// What POSIX leaves unspecified — `x -a y` — every shell reads as the
/// operator, so the tail goes to the general parser rather than erroring.
fn test_three(ctx: &mut Ctx, args: &[&[u8]]) -> Option<bool> {
    if is_binary(args[1]) {
        return binary(ctx, args[0], args[1], args[2]);
    }
    if args[0] == b"!" {
        return test_two(ctx, &args[1..]).map(|value| !value);
    }
    if args[0] == b"(" && args[2] == b")" {
        return Some(!args[1].is_empty());
    }
    test_general(ctx, args)
}

fn test_general(ctx: &mut Ctx, args: &[&[u8]]) -> Option<bool> {
    let mut parser = Parser {
        args,
        pos: 0,
        ctx: &mut *ctx,
    };
    let value = parser.expr();
    let complete = parser.pos == args.len();
    match value {
        Some(value) if complete => Some(value),
        Some(_) => {
            ctx.warn(b"unexpected operand");
            None
        }
        None => None,
    }
}

fn test(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let bracket = argv[0].ends_with(b"[");
    let mut args = &argv[1..];
    if bracket {
        match args.last() {
            Some(&b"]") => args = &args[..args.len() - 1],
            _ => {
                ctx.warn(b"missing ]");
                return 2;
            }
        }
    }

    let value = match args.len() {
        0 => Some(false),
        1 => Some(!args[0].is_empty()),
        2 => test_two(ctx, args),
        3 => test_three(ctx, args),
        4 if args[0] == b"!" => test_three(ctx, &args[1..]).map(|value| !value),
        4 if args[0] == b"(" && args[3] == b")" => test_two(ctx, &args[1..3]),
        _ => test_general(ctx, args),
    };
    match value {
        Some(true) => 0,
        Some(false) => 1,
        None => 2,
    }
}

fn yes(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut line = Vec::new();
    if argv.len() == 1 {
        line.push(b'y');
    }
    for (i, arg) in argv[1..].iter().enumerate() {
        if i > 0 {
            line.push(b' ');
        }
        line.extend_from_slice(arg);
    }
    line.push(b'\n');
    while !ctx.out.broken() {
        ctx.out.write(&line);
    }
    0
}

/// A fixed-point operand: the value scaled by `10^places`. Decimals work by
/// scaling every operand to the widest one's fraction; exponent notation and
/// anything else a float would accept are refused.
fn decimal(bytes: &[u8]) -> Option<(i128, u32)> {
    let (negative, body) = match bytes.first()? {
        b'-' => (true, &bytes[1..]),
        b'+' => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut value: i128 = 0;
    let mut places = 0u32;
    let mut digits = 0usize;
    let mut dotted = false;
    for &b in body {
        if b == b'.' {
            if dotted {
                return None;
            }
            dotted = true;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add((b - b'0') as i128)?;
        digits += 1;
        if dotted {
            places += 1;
        }
    }
    if digits == 0 || places > 18 {
        return None;
    }
    Some((if negative { -value } else { value }, places))
}

fn push_decimal(out: &mut Vec<u8>, value: u128) {
    if value >= 10 {
        push_decimal(out, value / 10);
    }
    out.push(b'0' + (value % 10) as u8);
}

fn render(value: i128, places: u32) -> Vec<u8> {
    let mut out = Vec::new();
    if value < 0 {
        out.push(b'-');
    }
    let scale = 10u128.pow(places);
    let magnitude = value.unsigned_abs();
    push_decimal(&mut out, magnitude / scale);
    if places > 0 {
        out.push(b'.');
        let mut fraction = Vec::new();
        push_decimal(&mut fraction, magnitude % scale);
        for _ in fraction.len()..places as usize {
            out.push(b'0');
        }
        out.extend_from_slice(&fraction);
    }
    out
}

fn pad_to(text: &mut Vec<u8>, width: usize) {
    let signed = usize::from(text.first() == Some(&b'-'));
    while text.len() < width {
        text.insert(signed, b'0');
    }
}

fn seq(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut equal_width = false;
    let mut separator: &[u8] = b"\n";
    let mut i = 1;
    while i < argv.len() {
        let word = argv[i];
        if word == b"--" {
            i += 1;
            break;
        }
        // A leading `-` in front of a digit is a negative operand, not options.
        if word.len() < 2 || word[0] != b'-' || word[1].is_ascii_digit() || word[1] == b'.' {
            break;
        }
        let mut j = 1;
        while j < word.len() {
            match word[j] {
                b'w' => {
                    equal_width = true;
                    j += 1;
                }
                b's' => {
                    if j + 1 < word.len() {
                        separator = &word[j + 1..];
                    } else if i + 1 < argv.len() {
                        i += 1;
                        separator = argv[i];
                    } else {
                        ctx.warn_at(b"s", b"option requires an argument");
                        return ctx.usage(SEQ_USAGE);
                    }
                    j = word.len();
                }
                flag => {
                    ctx.warn_at(&[flag], b"invalid option");
                    return ctx.usage(SEQ_USAGE);
                }
            }
        }
        i += 1;
    }

    let operands = &argv[i..];
    let (first_arg, incr_arg, last_arg): (&[u8], &[u8], &[u8]) = match operands.len() {
        1 => (b"1", b"1", operands[0]),
        2 => (operands[0], b"1", operands[1]),
        3 => (operands[0], operands[1], operands[2]),
        _ => return ctx.usage(SEQ_USAGE),
    };

    let mut parsed = [(0i128, 0u32); 3];
    for (slot, arg) in parsed.iter_mut().zip([first_arg, incr_arg, last_arg]) {
        match decimal(arg) {
            Some(value) => *slot = value,
            None => {
                ctx.warn_at(arg, b"invalid floating point argument");
                return 1;
            }
        }
    }
    let places = parsed.iter().map(|&(_, p)| p).max().unwrap_or(0);
    let scaled = |(value, from): (i128, u32)| value * 10i128.pow(places - from);
    let first = scaled(parsed[0]);
    let increment = scaled(parsed[1]);
    let last = scaled(parsed[2]);

    if increment == 0 {
        ctx.warn(b"increment must not be zero");
        return 1;
    }

    let width = if equal_width {
        render(first, places).len().max(render(last, places).len())
    } else {
        0
    };

    let mut value = first;
    let mut emitted = false;
    while (increment > 0 && value <= last) || (increment < 0 && value >= last) {
        if ctx.out.broken() {
            return 0;
        }
        if emitted {
            ctx.out.write(separator);
        }
        let mut text = render(value, places);
        pad_to(&mut text, width);
        ctx.out.write(&text);
        emitted = true;
        value += increment;
    }
    if emitted {
        ctx.out.nl();
    }
    0
}

fn duration_ms(bytes: &[u8]) -> Option<u64> {
    let (body, unit_ms) = match bytes.last()? {
        b's' => (&bytes[..bytes.len() - 1], 1_000u128),
        b'm' => (&bytes[..bytes.len() - 1], 60_000),
        b'h' => (&bytes[..bytes.len() - 1], 3_600_000),
        b'd' => (&bytes[..bytes.len() - 1], 86_400_000),
        _ => (bytes, 1_000),
    };
    let (value, places) = decimal(body)?;
    if value < 0 {
        return None;
    }
    let ms = (value as u128).checked_mul(unit_ms)? / 10u128.pow(places);
    u64::try_from(ms).ok()
}

fn sleep(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let operands = &argv[1..];
    if operands.is_empty() {
        return ctx.usage(SLEEP_USAGE);
    }
    let mut total_ms: u64 = 0;
    for operand in operands {
        match duration_ms(operand) {
            Some(ms) => total_ms = total_ms.saturating_add(ms),
            None => {
                ctx.warn_at(operand, b"invalid time interval");
                return 1;
            }
        }
    }
    // Short slices rather than one long call, so a signal lands within a tick
    // instead of at the end of the interval.
    let mut remaining = total_ms;
    while remaining > 0 {
        let slice = remaining.min(100);
        sys_core::sleep_ms(slice as u32);
        remaining -= slice;
    }
    0
}

fn env_set(vars: &mut Vec<(String, String)>, name: &str, value: &str) {
    match vars.iter_mut().find(|(key, _)| key == name) {
        Some(slot) => slot.1 = value.to_string(),
        None => vars.push((name.to_string(), value.to_string())),
    }
}

fn locate(program: &str, path_var: &str) -> Option<String> {
    if program.contains('/') {
        return Some(program.to_string());
    }
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| fsutil::join(dir, program))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|meta| meta.is_file())
                .unwrap_or(false)
        })
}

fn env(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut ignore = false;
    let mut unset: Vec<&[u8]> = Vec::new();
    let mut opts = Opts::new(argv, "iu:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'i') => ignore = true,
            Opt::Value(b'u', name) => unset.push(name),
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(ENV_USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(ENV_USAGE);
            }
            _ => {}
        }
    }

    let mut vars: Vec<(String, String)> = if ignore {
        Vec::new()
    } else {
        std::env::vars().collect()
    };
    for name in unset {
        let Some(name) = as_str(ctx, name) else {
            return 1;
        };
        vars.retain(|(key, _)| key != name);
    }

    let operands = opts.operands();
    let mut split = 0;
    while split < operands.len() {
        let Some(text) = as_str(ctx, operands[split]) else {
            return 1;
        };
        let Some(eq) = text.find('=') else {
            break;
        };
        env_set(&mut vars, &text[..eq], &text[eq + 1..]);
        split += 1;
    }

    let command = &operands[split..];
    if command.is_empty() {
        for (key, value) in &vars {
            ctx.out.s(key);
            ctx.out.b(b'=');
            ctx.out.s(value);
            ctx.out.nl();
        }
        return 0;
    }

    let Some(program) = as_str(ctx, command[0]) else {
        return 1;
    };
    let path_var = vars
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or("/bin:/sbin");
    let Some(resolved) = locate(program, path_var) else {
        ctx.warn_at(command[0], b"No such file or directory");
        return super::STATUS_NOT_FOUND;
    };

    let Ok(image) = CString::new(resolved) else {
        ctx.warn_at(command[0], b"Invalid argument");
        return 1;
    };
    let argv_owned: Vec<Vec<u8>> = command
        .iter()
        .map(|arg| nul_terminated(arg, None))
        .collect();
    let envp_owned: Vec<Vec<u8>> = vars
        .iter()
        .map(|(key, value)| nul_terminated(key.as_bytes(), Some(value.as_bytes())))
        .collect();
    let mut argv_ptrs: Vec<*const u8> = argv_owned.iter().map(|owned| owned.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const u8> = envp_owned.iter().map(|owned| owned.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    let rc = sys_process::execve(
        image.as_bytes_with_nul().as_ptr(),
        argv_ptrs.as_ptr(),
        envp_ptrs.as_ptr(),
    );
    // The kernel reads through these pointers during the syscall, so the owned
    // bytes outlive it; a return at all means the exec failed.
    drop(argv_owned);
    drop(envp_owned);
    let failure = std::io::Error::from_raw_os_error((-rc) as i32);
    ctx.warn_io(command[0], &failure);
    if failure.kind() == std::io::ErrorKind::NotFound {
        super::STATUS_NOT_FOUND
    } else {
        126
    }
}

/// One NUL-terminated element of the exec ABI's vectors: an argument, or a
/// `key=value` pair when a value is given.
fn nul_terminated(name: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let extra = value.map_or(0, |value| value.len() + 1);
    let mut out = Vec::with_capacity(name.len() + extra + 1);
    out.extend_from_slice(name);
    if let Some(value) = value {
        out.push(b'=');
        out.extend_from_slice(value);
    }
    out.push(0);
    out
}
