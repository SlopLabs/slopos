//! Ordering and adjacent-duplicate collapsing: `sort`, `uniq`.
//!
//! `uniq` only ever looks at neighbours, so the pair composes the way POSIX
//! intends: `sort | uniq -c`.

use std::cmp::Ordering;
use std::fs::File;
use std::io::Write;

use super::io::Sink;
use super::opts::{Opt, Opts, parse_u64};
use super::{Ctx, Tool, input};

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "sort",
        desc: "Sort lines of text",
        usage: "sort [-rnufbi] [-k keydef] [-t sep] [-o file] [file...]",
        run: sort,
    },
    Tool {
        name: "uniq",
        desc: "Collapse adjacent repeated lines",
        usage: "uniq [-cdui] [-f fields] [-s chars] [input [output]]",
        run: uniq,
    },
];

const SORT_USAGE: &str = "sort [-rnufbi] [-k keydef] [-t sep] [-o file] [file...]";
const UNIQ_USAGE: &str = "uniq [-cdui] [-f fields] [-s chars] [input [output]]";

#[derive(Clone, Copy, Default, PartialEq)]
struct Flags {
    blanks: bool,
    fold: bool,
    ignore: bool,
    numeric: bool,
    reverse: bool,
}

impl Flags {
    fn merge(self, other: Self) -> Self {
        Self {
            blanks: self.blanks || other.blanks,
            fold: self.fold || other.fold,
            ignore: self.ignore || other.ignore,
            numeric: self.numeric || other.numeric,
            reverse: self.reverse || other.reverse,
        }
    }
}

/// `-k F[.C][mods][,F[.C][mods]]`. `end_field` 0 is "to end of line" and
/// `end_char` 0 is "to end of that field", which is how POSIX spells the
/// defaults.
struct Key {
    start_field: usize,
    start_char: usize,
    end_field: usize,
    end_char: usize,
    flags: Option<Flags>,
}

fn sort(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut global = Flags::default();
    let mut unique = false;
    let mut sep: Option<u8> = None;
    let mut output: Option<&[u8]> = None;
    let mut keys: Vec<Key> = Vec::new();

    let mut opts = Opts::new(argv, "bfinrut:k:o:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'b') => global.blanks = true,
            Opt::Flag(b'f') => global.fold = true,
            Opt::Flag(b'i') => global.ignore = true,
            Opt::Flag(b'n') => global.numeric = true,
            Opt::Flag(b'r') => global.reverse = true,
            Opt::Flag(b'u') => unique = true,
            Opt::Value(b't', value) => {
                if value.len() != 1 {
                    ctx.warn(b"field separator must be a single byte");
                    return ctx.usage(SORT_USAGE);
                }
                sep = Some(value[0]);
            }
            Opt::Value(b'k', value) => match parse_key(value) {
                Some(key) => keys.push(key),
                None => {
                    ctx.warn_at(value, b"invalid key definition");
                    return ctx.usage(SORT_USAGE);
                }
            },
            Opt::Value(b'o', value) => output = Some(value),
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(SORT_USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(SORT_USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();

    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut status = 0;
    for &operand in input::sources(operands) {
        if !read_lines(ctx, operand, &mut lines) {
            status = 1;
        }
    }

    let mut spans: Vec<(u32, u32)> = Vec::new();
    if !keys.is_empty() {
        spans.reserve(lines.len() * keys.len());
        let mut fields: Vec<(usize, usize)> = Vec::new();
        for line in &lines {
            split_fields(line, sep, &mut fields);
            for key in &keys {
                let blanks = key.flags.unwrap_or(global).blanks;
                let (start, end) = key_span(line, &fields, key, blanks);
                spans.push((start as u32, end as u32));
            }
        }
    }

    let sorter = Sorter {
        lines: &lines,
        spans: &spans,
        keys: &keys,
        global,
    };
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by(|&a, &b| sorter.cmp(a, b));

    let mut out = Vec::new();
    let mut previous: Option<usize> = None;
    for &index in &order {
        if unique {
            if let Some(prior) = previous {
                if sorter.cmp(prior, index) == Ordering::Equal {
                    continue;
                }
            }
            previous = Some(index);
        }
        out.extend_from_slice(&lines[index]);
        // Sorting reorders lines, so an unterminated last input line cannot
        // stay unterminated without gluing itself to a neighbour.
        out.push(b'\n');
    }

    match write_out(ctx, output, &out) {
        0 => status,
        code => code,
    }
}

fn read_lines(ctx: &mut Ctx, operand: &[u8], lines: &mut Vec<Vec<u8>>) -> bool {
    let Some(source) = input::open(ctx, operand) else {
        return false;
    };
    let mut reader = input::buffered(source);
    let mut buf = Vec::new();
    loop {
        match input::read_line(&mut reader, &mut buf) {
            Ok(0) => return true,
            Ok(_) => lines.push(input::trim_newline(&buf).to_vec()),
            Err(error) => {
                ctx.warn_io(operand, &error);
                return false;
            }
        }
    }
}

struct Sorter<'a> {
    lines: &'a [Vec<u8>],
    spans: &'a [(u32, u32)],
    keys: &'a [Key],
    global: Flags,
}

impl Sorter<'_> {
    fn cmp(&self, a: usize, b: usize) -> Ordering {
        if self.keys.is_empty() {
            return compare(&self.lines[a], &self.lines[b], self.global);
        }
        let stride = self.keys.len();
        for (slot, key) in self.keys.iter().enumerate() {
            let (a_start, a_end) = self.spans[a * stride + slot];
            let (b_start, b_end) = self.spans[b * stride + slot];
            let left = &self.lines[a][a_start as usize..a_end as usize];
            let right = &self.lines[b][b_start as usize..b_end as usize];
            let order = compare(left, right, key.flags.unwrap_or(self.global));
            if order != Ordering::Equal {
                return order;
            }
        }
        Ordering::Equal
    }
}

fn compare(left: &[u8], right: &[u8], flags: Flags) -> Ordering {
    let order = if flags.numeric {
        compare_numeric(left, right)
    } else {
        compare_text(left, right, flags)
    };
    if flags.reverse {
        order.reverse()
    } else {
        order
    }
}

fn compare_text(left: &[u8], right: &[u8], flags: Flags) -> Ordering {
    let left = if flags.blanks {
        trim_blanks(left)
    } else {
        left
    };
    let right = if flags.blanks {
        trim_blanks(right)
    } else {
        right
    };
    let sift = move |&byte: &u8| !flags.ignore || byte == b' ' || byte.is_ascii_graphic();
    let key = move |byte: u8| {
        if flags.fold {
            byte.to_ascii_uppercase()
        } else {
            byte
        }
    };
    left.iter()
        .copied()
        .filter(sift)
        .map(key)
        .cmp(right.iter().copied().filter(sift).map(key))
}

/// A signed decimal split into its parts, so ordering never routes through a
/// float and never loses a digit to rounding.
struct Decimal<'a> {
    negative: bool,
    integer: &'a [u8],
    fraction: &'a [u8],
}

fn decimal(text: &[u8]) -> Decimal<'_> {
    let mut i = 0;
    while i < text.len() && is_blank(text[i]) {
        i += 1;
    }
    let mut negative = false;
    if let Some(&sign) = text.get(i) {
        if sign == b'-' || sign == b'+' {
            negative = sign == b'-';
            i += 1;
        }
    }
    let start = i;
    while i < text.len() && text[i].is_ascii_digit() {
        i += 1;
    }
    let mut integer = &text[start..i];
    while integer.first() == Some(&b'0') {
        integer = &integer[1..];
    }
    let mut fraction: &[u8] = b"";
    if text.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < text.len() && text[i].is_ascii_digit() {
            i += 1;
        }
        fraction = &text[start..i];
        while fraction.last() == Some(&b'0') {
            fraction = &fraction[..fraction.len() - 1];
        }
    }
    if integer.is_empty() && fraction.is_empty() {
        negative = false;
    }
    Decimal {
        negative,
        integer,
        fraction,
    }
}

fn compare_numeric(left: &[u8], right: &[u8]) -> Ordering {
    let left = decimal(left);
    let right = decimal(right);
    match (left.negative, right.negative) {
        (false, true) => return Ordering::Greater,
        (true, false) => return Ordering::Less,
        _ => {}
    }
    let magnitude = left
        .integer
        .len()
        .cmp(&right.integer.len())
        .then_with(|| left.integer.cmp(right.integer))
        .then_with(|| left.fraction.cmp(right.fraction));
    if left.negative {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// Without `-t` a field is a run of blanks followed by a run of non-blanks, so
/// the separator belongs to the field it precedes and `-b` is what drops it.
fn split_fields(line: &[u8], sep: Option<u8>, out: &mut Vec<(usize, usize)>) {
    out.clear();
    match sep {
        Some(byte) => {
            let mut start = 0;
            for (i, &current) in line.iter().enumerate() {
                if current == byte {
                    out.push((start, i));
                    start = i + 1;
                }
            }
            out.push((start, line.len()));
        }
        None => {
            let mut i = 0;
            while i < line.len() {
                let start = i;
                while i < line.len() && is_blank(line[i]) {
                    i += 1;
                }
                while i < line.len() && !is_blank(line[i]) {
                    i += 1;
                }
                out.push((start, i));
            }
        }
    }
}

fn key_span(line: &[u8], fields: &[(usize, usize)], key: &Key, blanks: bool) -> (usize, usize) {
    let start = match fields.get(key.start_field - 1) {
        Some(&(field_start, field_end)) => {
            let mut at = field_start;
            if blanks {
                while at < field_end && is_blank(line[at]) {
                    at += 1;
                }
            }
            (at + key.start_char - 1).min(field_end)
        }
        None => line.len(),
    };
    let end = if key.end_field == 0 {
        line.len()
    } else {
        match fields.get(key.end_field - 1) {
            Some(&(field_start, field_end)) => {
                if key.end_char == 0 {
                    field_end
                } else {
                    let mut at = field_start;
                    if blanks {
                        while at < field_end && is_blank(line[at]) {
                            at += 1;
                        }
                    }
                    (at + key.end_char).min(field_end)
                }
            }
            None => line.len(),
        }
    };
    (start, end.max(start))
}

fn parse_key(spec: &[u8]) -> Option<Key> {
    let comma = spec.iter().position(|&byte| byte == b',');
    let (head, tail) = match comma {
        Some(at) => (&spec[..at], Some(&spec[at + 1..])),
        None => (spec, None),
    };
    let (start_field, start_char, start_flags) = parse_key_part(head, 1)?;
    let (end_field, end_char, end_flags) = match tail {
        Some(part) => parse_key_part(part, 0)?,
        None => (0, 0, Flags::default()),
    };
    let flags = start_flags.merge(end_flags);
    Some(Key {
        start_field,
        start_char: start_char.max(1),
        end_field,
        end_char,
        flags: if flags == Flags::default() {
            None
        } else {
            Some(flags)
        },
    })
}

fn parse_key_part(part: &[u8], default_char: usize) -> Option<(usize, usize, Flags)> {
    let mut i = 0;
    let field = read_number(part, &mut i)?;
    if field == 0 {
        return None;
    }
    let mut chars = default_char;
    if part.get(i) == Some(&b'.') {
        i += 1;
        chars = read_number(part, &mut i)?;
    }
    let mut flags = Flags::default();
    while i < part.len() {
        match part[i] {
            b'b' => flags.blanks = true,
            b'f' => flags.fold = true,
            b'i' => flags.ignore = true,
            b'n' => flags.numeric = true,
            b'r' => flags.reverse = true,
            _ => return None,
        }
        i += 1;
    }
    Some((field, chars, flags))
}

fn read_number(bytes: &[u8], i: &mut usize) -> Option<usize> {
    let start = *i;
    let mut value: usize = 0;
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
        value = value
            .checked_mul(10)?
            .checked_add((bytes[*i] - b'0') as usize)?;
        *i += 1;
    }
    if *i == start { None } else { Some(value) }
}

struct Mode {
    counts: bool,
    duplicated: bool,
    singles: bool,
    fold: bool,
    skip_fields: usize,
    skip_chars: usize,
}

fn uniq(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut mode = Mode {
        counts: false,
        duplicated: false,
        singles: false,
        fold: false,
        skip_fields: 0,
        skip_chars: 0,
    };

    let mut opts = Opts::new(argv, "cduif:s:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'c') => mode.counts = true,
            Opt::Flag(b'd') => mode.duplicated = true,
            Opt::Flag(b'u') => mode.singles = true,
            Opt::Flag(b'i') => mode.fold = true,
            Opt::Value(b'f', value) => match parse_u64(value) {
                Some(count) => mode.skip_fields = count as usize,
                None => {
                    ctx.warn_at(value, b"invalid field count");
                    return ctx.usage(UNIQ_USAGE);
                }
            },
            Opt::Value(b's', value) => match parse_u64(value) {
                Some(count) => mode.skip_chars = count as usize,
                None => {
                    ctx.warn_at(value, b"invalid character count");
                    return ctx.usage(UNIQ_USAGE);
                }
            },
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(UNIQ_USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(UNIQ_USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    if operands.len() > 2 {
        return ctx.usage(UNIQ_USAGE);
    }
    let source = operands.first().copied().unwrap_or(b"-");
    let target = operands.get(1).copied();

    let Some(opened) = input::open(ctx, source) else {
        return 1;
    };
    let mut reader = input::buffered(opened);
    let mut buf = Vec::new();
    // The output operand may name the input file, so that form holds the
    // result until the input has been read; the stdout form streams.
    let mut buffered = target.map(|_| Vec::new());
    let mut group: Option<(Vec<u8>, u64, bool)> = None;
    loop {
        match input::read_line(&mut reader, &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                ctx.warn_io(source, &error);
                return 1;
            }
        }
        let terminated = buf.last() == Some(&b'\n');
        let line = input::trim_newline(&buf);
        let extends = match &group {
            Some((first, _, _)) => same(first, line, &mode),
            None => false,
        };
        if extends {
            let held = group.as_mut().unwrap();
            held.1 += 1;
            held.2 = terminated;
        } else {
            if let Some((first, count, term)) = group.take() {
                emit(ctx, buffered.as_mut(), &first, count, term, &mode);
            }
            group = Some((line.to_vec(), 1, terminated));
        }
    }
    if let Some((first, count, term)) = group.take() {
        emit(ctx, buffered.as_mut(), &first, count, term, &mode);
    }

    match &buffered {
        Some(bytes) => write_out(ctx, target, bytes),
        None => 0,
    }
}

enum Out<'a> {
    Stream(&'a mut Sink),
    Buffer(&'a mut Vec<u8>),
}

impl Out<'_> {
    fn write(&mut self, bytes: &[u8]) {
        match self {
            Out::Stream(sink) => sink.write(bytes),
            Out::Buffer(buffer) => buffer.extend_from_slice(bytes),
        }
    }
}

fn emit(
    ctx: &mut Ctx,
    buffer: Option<&mut Vec<u8>>,
    line: &[u8],
    count: u64,
    terminated: bool,
    mode: &Mode,
) {
    let show = match (mode.duplicated, mode.singles) {
        (false, false) => true,
        (true, false) => count > 1,
        (false, true) => count == 1,
        (true, true) => false,
    };
    if !show {
        return;
    }
    let mut out = match buffer {
        Some(bytes) => Out::Buffer(bytes),
        None => Out::Stream(&mut ctx.out),
    };
    if mode.counts {
        push_count(&mut out, count);
    }
    out.write(line);
    if terminated {
        out.write(b"\n");
    }
}

fn push_count(out: &mut Out, count: u64) {
    const PAD: &[u8] = b"       ";
    let mut digits = [0u8; 20];
    let mut value = count;
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let text = &digits[at..];
    out.write(&PAD[text.len().min(PAD.len())..]);
    out.write(text);
    out.write(b" ");
}

fn same(left: &[u8], right: &[u8], mode: &Mode) -> bool {
    let left = uniq_key(left, mode);
    let right = uniq_key(right, mode);
    if mode.fold {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    } else {
        left == right
    }
}

fn uniq_key<'a>(line: &'a [u8], mode: &Mode) -> &'a [u8] {
    let mut i = 0;
    for _ in 0..mode.skip_fields {
        while i < line.len() && is_blank(line[i]) {
            i += 1;
        }
        while i < line.len() && !is_blank(line[i]) {
            i += 1;
        }
    }
    &line[(i + mode.skip_chars).min(line.len())..]
}

fn write_out(ctx: &mut Ctx, target: Option<&[u8]>, bytes: &[u8]) -> i32 {
    let Some(operand) = target else {
        ctx.out.write(bytes);
        return 0;
    };
    let Some(path) = input::as_str(ctx, operand) else {
        return 1;
    };
    match File::create(path).and_then(|mut file| file.write_all(bytes)) {
        Ok(()) => 0,
        Err(error) => {
            ctx.warn_io(operand, &error);
            1
        }
    }
}

fn is_blank(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn trim_blanks(text: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < text.len() && is_blank(text[i]) {
        i += 1;
    }
    &text[i..]
}
