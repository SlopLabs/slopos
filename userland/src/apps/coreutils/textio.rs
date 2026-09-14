//! The byte and line filters: `cat`, `head`, `tail`, `wc`, `tee`, `cut`, `tr`
//! and `hexdump`. Everything streams through a fixed buffer, where the shell
//! builtins these replace stopped after one 512-byte read.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use super::input::{self, Input};
use super::io::decimal_width;
use super::opts::{Opt, Opts, parse_u64};
use super::{Ctx, Tool};

const CHUNK: usize = 64 * 1024;

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "cat",
        desc: "Concatenate files to standard output",
        usage: "cat [-nsE] [file...]",
        run: cat,
    },
    Tool {
        name: "head",
        desc: "Print the first lines or bytes of a file",
        usage: "head [-n count] [-c count] [-count] [file...]",
        run: head,
    },
    Tool {
        name: "tail",
        desc: "Print the last lines or bytes of a file",
        usage: "tail [-n count] [-c count] [-count] [file...]",
        run: tail,
    },
    Tool {
        name: "wc",
        desc: "Count lines, words and bytes",
        usage: "wc [-lwcm] [file...]",
        run: wc,
    },
    Tool {
        name: "tee",
        desc: "Copy standard input to files and standard output",
        usage: "tee [-a] [file...]",
        run: tee,
    },
    Tool {
        name: "cut",
        desc: "Select fields or byte positions from each line",
        usage: "cut -b list | -c list | -f list [-d delim] [-s] [file...]",
        run: cut,
    },
    Tool {
        name: "tr",
        desc: "Translate, squeeze or delete bytes",
        usage: "tr [-dsc] set1 [set2]",
        run: tr,
    },
    Tool {
        name: "hexdump",
        desc: "Dump a file as hex and ASCII",
        usage: "hexdump [-C] [-n count] [file...]",
        run: hexdump,
    },
];

/// A line without its `\n`, keeping any `\r`: a filter must not quietly
/// rewrite CRLF data on its way through.
fn chop(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(&b'\n') => &line[..line.len() - 1],
        _ => line,
    }
}

/// Copy a reader to the sink in fixed chunks, stopping when the destination
/// closes.
fn drain(ctx: &mut Ctx, mut input: Input, operand: &[u8]) -> bool {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match input.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                ctx.out.write(&buf[..n]);
                if ctx.out.broken() {
                    return true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                ctx.warn_io(operand, &e);
                return false;
            }
        }
    }
}

fn cat(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "cat [-nsE] [file...]";
    let mut number = false;
    let mut squeeze = false;
    let mut ends = false;
    let mut opts = Opts::new(argv, "nsE");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'n') => number = true,
            Opt::Flag(b's') => squeeze = true,
            Opt::Flag(b'E') => ends = true,
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    let annotate = number || squeeze || ends;
    let mut status = 0;
    let mut line_no = 1u64;
    let mut blanks = 0u32;
    for &src in input::sources(operands) {
        let Some(input) = input::open(ctx, src) else {
            status = 1;
            continue;
        };
        let ok = if annotate {
            cat_lines(
                ctx,
                input,
                src,
                number,
                squeeze,
                ends,
                &mut line_no,
                &mut blanks,
            )
        } else {
            drain(ctx, input, src)
        };
        if !ok {
            status = 1;
        }
        if ctx.out.broken() {
            break;
        }
    }
    status
}

fn cat_lines(
    ctx: &mut Ctx,
    input: Input,
    operand: &[u8],
    number: bool,
    squeeze: bool,
    ends: bool,
    line_no: &mut u64,
    blanks: &mut u32,
) -> bool {
    let mut reader = input::buffered(input);
    let mut line = Vec::new();
    loop {
        match input::read_line(&mut reader, &mut line) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) => {
                ctx.warn_io(operand, &e);
                return false;
            }
        }
        let terminated = line.last() == Some(&b'\n');
        let body = chop(&line);
        if squeeze {
            if body.is_empty() {
                *blanks += 1;
                if *blanks > 1 {
                    continue;
                }
            } else {
                *blanks = 0;
            }
        }
        if number {
            ctx.out.u_right(*line_no, 6);
            ctx.out.b(b'\t');
            *line_no += 1;
        }
        ctx.out.write(body);
        if ends && terminated {
            ctx.out.b(b'$');
        }
        if terminated {
            ctx.out.nl();
        }
        if ctx.out.broken() {
            return true;
        }
    }
}

#[derive(Clone, Copy)]
enum Unit {
    Lines,
    Bytes,
}

/// The `-n`/`-c` pair shared by `head` and `tail`, including the historical
/// `-N` spelling: `getopt` reports each digit of `-25` as its own unknown
/// flag, so they are folded back into one count here.
fn count_opts<'a>(
    ctx: &mut Ctx,
    argv: &'a [&'a [u8]],
    usage: &str,
) -> Result<(Unit, u64, &'a [&'a [u8]]), i32> {
    let mut unit = Unit::Lines;
    let mut count: Option<u64> = None;
    let mut digits: Option<u64> = None;
    let mut opts = Opts::new(argv, "n:c:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Value(flag, value) => {
                let Some(n) = parse_u64(value) else {
                    ctx.warn_at(value, b"invalid number");
                    return Err(1);
                };
                unit = if flag == b'c' {
                    Unit::Bytes
                } else {
                    Unit::Lines
                };
                count = Some(n);
            }
            Opt::Unknown(f) if f.is_ascii_digit() => {
                digits = Some(digits.unwrap_or(0) * 10 + (f - b'0') as u64);
            }
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return Err(ctx.usage(usage));
            }
            Opt::Missing(f) => {
                ctx.warn_at(&[f], b"option requires an argument");
                return Err(ctx.usage(usage));
            }
            _ => {}
        }
    }
    Ok((unit, count.or(digits).unwrap_or(10), opts.operands()))
}

fn banner(ctx: &mut Ctx, operand: &[u8], gap: bool) {
    if gap {
        ctx.out.nl();
    }
    ctx.out.s("==> ");
    ctx.out.write(operand);
    ctx.out.s(" <==");
    ctx.out.nl();
}

fn head(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "head [-n count] [-c count] [-count] [file...]";
    let (unit, count, operands) = match count_opts(ctx, argv, USAGE) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let sources = input::sources(operands);
    let headers = sources.len() > 1;
    let mut status = 0;
    let mut printed = false;
    for &src in sources {
        let Some(input) = input::open(ctx, src) else {
            status = 1;
            continue;
        };
        if headers {
            banner(ctx, src, printed);
        }
        printed = true;
        let ok = match unit {
            Unit::Lines => head_lines(ctx, input, src, count),
            Unit::Bytes => head_bytes(ctx, input, src, count),
        };
        if !ok {
            status = 1;
        }
        if ctx.out.broken() {
            break;
        }
    }
    status
}

fn head_lines(ctx: &mut Ctx, input: Input, operand: &[u8], count: u64) -> bool {
    let mut reader = input::buffered(input);
    let mut line = Vec::new();
    let mut left = count;
    while left > 0 && !ctx.out.broken() {
        match input::read_line(&mut reader, &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                ctx.warn_io(operand, &e);
                return false;
            }
        }
        ctx.out.write(&line);
        left -= 1;
    }
    true
}

fn head_bytes(ctx: &mut Ctx, mut input: Input, operand: &[u8], count: u64) -> bool {
    let mut buf = vec![0u8; CHUNK];
    let mut left = count;
    while left > 0 && !ctx.out.broken() {
        let want = left.min(CHUNK as u64) as usize;
        match input.read(&mut buf[..want]) {
            Ok(0) => break,
            Ok(n) => {
                ctx.out.write(&buf[..n]);
                left -= n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                ctx.warn_io(operand, &e);
                return false;
            }
        }
    }
    true
}

fn tail(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "tail [-n count] [-c count] [-count] [file...]";
    let (unit, count, operands) = match count_opts(ctx, argv, USAGE) {
        Ok(parsed) => parsed,
        Err(status) => return status,
    };
    let sources = input::sources(operands);
    let headers = sources.len() > 1;
    let mut status = 0;
    let mut printed = false;
    for &src in sources {
        let Some(input) = input::open(ctx, src) else {
            status = 1;
            continue;
        };
        if headers {
            banner(ctx, src, printed);
        }
        printed = true;
        let ok = match input {
            Input::File(file) => tail_file(ctx, file, src, unit, count),
            Input::Stdin(stdin) => {
                let mut stream = Input::Stdin(stdin);
                tail_stream(ctx, &mut stream, src, unit, count)
            }
        };
        if !ok {
            status = 1;
        }
        if ctx.out.broken() {
            break;
        }
    }
    status
}

/// A seekable file is read from its tail, so `tail` of a huge file costs one
/// window rather than the whole file; anything that will not seek falls back
/// to the bounded ring buffer a pipe needs.
fn tail_file(ctx: &mut Ctx, mut file: File, operand: &[u8], unit: Unit, count: u64) -> bool {
    match tail_start(&mut file, unit, count) {
        Ok(start) => match file.seek(SeekFrom::Start(start)) {
            Ok(_) => drain(ctx, Input::File(file), operand),
            Err(e) => {
                ctx.warn_io(operand, &e);
                false
            }
        },
        Err(_) => tail_stream(ctx, &mut Input::File(file), operand, unit, count),
    }
}

fn tail_start(file: &mut File, unit: Unit, count: u64) -> std::io::Result<u64> {
    let len = file.seek(SeekFrom::End(0))?;
    if let Unit::Bytes = unit {
        return Ok(len.saturating_sub(count));
    }
    if len == 0 || count == 0 {
        return Ok(len);
    }
    let mut buf = vec![0u8; 8192];
    let mut pos = len;
    let mut found = 0u64;
    while pos > 0 {
        let span = pos.min(buf.len() as u64) as usize;
        pos -= span as u64;
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut buf[..span])?;
        for i in (0..span).rev() {
            if buf[i] != b'\n' {
                continue;
            }
            let at = pos + i as u64;
            // The newline ending the file terminates the last line; it does
            // not start another one.
            if at + 1 == len {
                continue;
            }
            found += 1;
            if found == count {
                return Ok(at + 1);
            }
        }
    }
    Ok(0)
}

fn tail_stream(ctx: &mut Ctx, input: &mut Input, operand: &[u8], unit: Unit, count: u64) -> bool {
    match unit {
        Unit::Bytes => {
            let keep = count as usize;
            let mut tailed: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; CHUNK];
            loop {
                match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        tailed.extend_from_slice(&buf[..n]);
                        if tailed.len() > keep {
                            let excess = tailed.len() - keep;
                            tailed.drain(..excess);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        ctx.warn_io(operand, &e);
                        return false;
                    }
                }
            }
            ctx.out.write(&tailed);
        }
        Unit::Lines => {
            let keep = count as usize;
            let mut ring: VecDeque<Vec<u8>> = VecDeque::new();
            let mut reader = std::io::BufReader::with_capacity(8192, input);
            let mut line = Vec::new();
            loop {
                match input::read_line(&mut reader, &mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        ctx.warn_io(operand, &e);
                        return false;
                    }
                }
                if keep == 0 {
                    continue;
                }
                if ring.len() == keep {
                    ring.pop_front();
                }
                ring.push_back(line.clone());
            }
            for held in &ring {
                ctx.out.write(held);
                if ctx.out.broken() {
                    break;
                }
            }
        }
    }
    true
}

#[derive(Clone, Copy, Default)]
struct Counts {
    lines: u64,
    words: u64,
    bytes: u64,
}

#[derive(Clone, Copy)]
struct Show {
    lines: bool,
    words: bool,
    bytes: bool,
}

fn wc(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "wc [-lwcm] [file...]";
    let mut show = Show {
        lines: false,
        words: false,
        bytes: false,
    };
    let mut opts = Opts::new(argv, "lwcm");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'l') => show.lines = true,
            Opt::Flag(b'w') => show.words = true,
            // `-m` counts characters, which here is the byte count: there is
            // no locale, so a character is a byte.
            Opt::Flag(b'c') | Opt::Flag(b'm') => show.bytes = true,
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    if !(show.lines || show.words || show.bytes) {
        show = Show {
            lines: true,
            words: true,
            bytes: true,
        };
    }
    let operands = opts.operands();
    let named = !operands.is_empty();
    let mut status = 0;
    let mut rows: Vec<(&[u8], Counts)> = Vec::new();
    let mut total = Counts::default();
    for &src in input::sources(operands) {
        let Some(input) = input::open(ctx, src) else {
            status = 1;
            continue;
        };
        match wc_count(ctx, input, src) {
            Some(counts) => {
                total.lines += counts.lines;
                total.words += counts.words;
                total.bytes += counts.bytes;
                rows.push((src, counts));
            }
            None => status = 1,
        }
    }
    let summary = rows.len() > 1;
    let mut width = 1usize;
    for (_, counts) in &rows {
        width = width.max(wc_width(counts, show));
    }
    if summary {
        width = width.max(wc_width(&total, show));
    }
    for (name, counts) in &rows {
        wc_row(
            ctx,
            counts,
            show,
            width,
            if named { Some(*name) } else { None },
        );
    }
    if summary {
        wc_row(ctx, &total, show, width, Some(&b"total"[..]));
    }
    status
}

fn wc_width(counts: &Counts, show: Show) -> usize {
    let mut width = 1;
    for (enabled, value) in [
        (show.lines, counts.lines),
        (show.words, counts.words),
        (show.bytes, counts.bytes),
    ] {
        if enabled {
            width = width.max(decimal_width(value));
        }
    }
    width
}

fn wc_row(ctx: &mut Ctx, counts: &Counts, show: Show, width: usize, name: Option<&[u8]>) {
    let mut first = true;
    for (enabled, value) in [
        (show.lines, counts.lines),
        (show.words, counts.words),
        (show.bytes, counts.bytes),
    ] {
        if !enabled {
            continue;
        }
        if !first {
            ctx.out.b(b' ');
        }
        first = false;
        ctx.out.u_right(value, width);
    }
    if let Some(name) = name {
        ctx.out.b(b' ');
        ctx.out.write(name);
    }
    ctx.out.nl();
}

fn wc_count(ctx: &mut Ctx, mut input: Input, operand: &[u8]) -> Option<Counts> {
    let mut counts = Counts::default();
    let mut buf = vec![0u8; CHUNK];
    let mut in_word = false;
    loop {
        match input.read(&mut buf) {
            Ok(0) => return Some(counts),
            Ok(n) => {
                for &b in &buf[..n] {
                    counts.bytes += 1;
                    if b == b'\n' {
                        counts.lines += 1;
                    }
                    if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                        in_word = false;
                    } else if !in_word {
                        in_word = true;
                        counts.words += 1;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                ctx.warn_io(operand, &e);
                return None;
            }
        }
    }
}

fn tee(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "tee [-a] [file...]";
    let mut append = false;
    let mut opts = Opts::new(argv, "a");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'a') => append = true,
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let mut status = 0;
    let mut files: Vec<(&[u8], File)> = Vec::new();
    for &operand in opts.operands() {
        let Some(path) = input::as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        let opened = if append {
            OpenOptions::new().create(true).append(true).open(path)
        } else {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)
        };
        match opened {
            Ok(file) => files.push((operand, file)),
            Err(e) => {
                ctx.warn_io(operand, &e);
                status = 1;
            }
        }
    }
    let mut stdin = input::Stdin;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                ctx.warn_io(b"stdin", &e);
                return 1;
            }
        };
        ctx.out.write(&buf[..n]);
        let mut i = 0;
        while i < files.len() {
            match files[i].1.write_all(&buf[..n]) {
                Ok(()) => i += 1,
                Err(e) => {
                    let name = files[i].0;
                    ctx.warn_io(name, &e);
                    files.remove(i);
                    status = 1;
                }
            }
        }
    }
    status
}

/// An inclusive position range, `u64::MAX` standing for an open end.
type Range = (u64, u64);

fn parse_list(spec: &[u8]) -> Option<Vec<Range>> {
    let mut ranges = Vec::new();
    for part in spec.split(|&b| b == b',') {
        if part.is_empty() {
            return None;
        }
        match part.iter().position(|&b| b == b'-') {
            None => {
                let at = parse_u64(part)?;
                if at == 0 {
                    return None;
                }
                ranges.push((at, at));
            }
            Some(dash) => {
                let low = &part[..dash];
                let high = &part[dash + 1..];
                let start = if low.is_empty() { 1 } else { parse_u64(low)? };
                let end = if high.is_empty() {
                    u64::MAX
                } else {
                    parse_u64(high)?
                };
                if start == 0 || end < start {
                    return None;
                }
                ranges.push((start, end));
            }
        }
    }
    if ranges.is_empty() {
        None
    } else {
        Some(ranges)
    }
}

fn selected(ranges: &[Range], at: u64) -> bool {
    ranges.iter().any(|&(start, end)| at >= start && at <= end)
}

fn cut(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "cut -b list | -c list | -f list [-d delim] [-s] [file...]";
    let mut ranges: Option<Vec<Range>> = None;
    let mut fields = false;
    let mut delim = b'\t';
    let mut suppress = false;
    let mut opts = Opts::new(argv, "b:c:f:d:s");
    for opt in opts.by_ref() {
        match opt {
            // `-b` and `-c` are the same list here: a character is a byte
            // without a locale.
            Opt::Value(flag @ (b'b' | b'c' | b'f'), value) => {
                let Some(list) = parse_list(value) else {
                    ctx.warn_at(value, b"invalid list");
                    return 1;
                };
                fields = flag == b'f';
                ranges = Some(list);
            }
            Opt::Value(b'd', value) => {
                if value.len() != 1 {
                    ctx.warn(b"the delimiter must be a single byte");
                    return 1;
                }
                delim = value[0];
            }
            Opt::Flag(b's') => suppress = true,
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Missing(f) => {
                ctx.warn_at(&[f], b"option requires an argument");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let Some(ranges) = ranges else {
        return ctx.usage(USAGE);
    };
    let mut status = 0;
    for &src in input::sources(opts.operands()) {
        let Some(input) = input::open(ctx, src) else {
            status = 1;
            continue;
        };
        if !cut_stream(ctx, input, src, &ranges, fields, delim, suppress) {
            status = 1;
        }
        if ctx.out.broken() {
            break;
        }
    }
    status
}

fn cut_stream(
    ctx: &mut Ctx,
    input: Input,
    operand: &[u8],
    ranges: &[Range],
    fields: bool,
    delim: u8,
    suppress: bool,
) -> bool {
    let mut reader = input::buffered(input);
    let mut line = Vec::new();
    loop {
        match input::read_line(&mut reader, &mut line) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) => {
                ctx.warn_io(operand, &e);
                return false;
            }
        }
        let body = chop(&line);
        if fields {
            if !body.contains(&delim) {
                if !suppress {
                    ctx.out.write(body);
                    ctx.out.nl();
                }
            } else {
                let mut first = true;
                for (i, part) in body.split(|&b| b == delim).enumerate() {
                    if !selected(ranges, i as u64 + 1) {
                        continue;
                    }
                    if !first {
                        ctx.out.b(delim);
                    }
                    first = false;
                    ctx.out.write(part);
                }
                ctx.out.nl();
            }
        } else {
            for (i, &b) in body.iter().enumerate() {
                if selected(ranges, i as u64 + 1) {
                    ctx.out.b(b);
                }
            }
            ctx.out.nl();
        }
        if ctx.out.broken() {
            return true;
        }
    }
}

/// One byte of a set specification, resolving a backslash escape. An unknown
/// escape is the escaped byte itself, as POSIX leaves it.
fn set_byte(spec: &[u8], at: usize) -> (u8, usize) {
    if spec[at] != b'\\' {
        return (spec[at], at + 1);
    }
    match spec.get(at + 1) {
        Some(b'n') => (b'\n', at + 2),
        Some(b't') => (b'\t', at + 2),
        Some(b'r') => (b'\r', at + 2),
        Some(b'0') => (0, at + 2),
        Some(b'\\') => (b'\\', at + 2),
        Some(&byte) => (byte, at + 2),
        None => (b'\\', at + 1),
    }
}

fn set_class(spec: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    if !spec.starts_with(b"[:") {
        return None;
    }
    let end = spec.windows(2).position(|pair| pair == b":]")?;
    match &spec[2..end] {
        b"alpha" => out.extend((0u8..=127).filter(|b| b.is_ascii_alphabetic())),
        b"digit" => out.extend(b'0'..=b'9'),
        b"alnum" => out.extend((0u8..=127).filter(|b| b.is_ascii_alphanumeric())),
        b"upper" => out.extend(b'A'..=b'Z'),
        b"lower" => out.extend(b'a'..=b'z'),
        b"space" => out.extend_from_slice(b" \t\n\x0b\x0c\r"),
        b"punct" => out.extend((0u8..=127).filter(|b| b.is_ascii_punctuation())),
        _ => return None,
    }
    Some(end + 2)
}

fn expand_set(spec: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < spec.len() {
        if spec[at] == b'[' {
            if let Some(taken) = set_class(&spec[at..], &mut out) {
                at += taken;
                continue;
            }
        }
        let (first, next) = set_byte(spec, at);
        at = next;
        if at + 1 < spec.len() && spec[at] == b'-' {
            let (last, after) = set_byte(spec, at + 1);
            if last < first {
                return None;
            }
            out.extend(first..=last);
            at = after;
            continue;
        }
        out.push(first);
    }
    Some(out)
}

fn tr(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "tr [-dsc] set1 [set2]";
    let mut delete = false;
    let mut squeeze = false;
    let mut complement = false;
    let mut opts = Opts::new(argv, "dsc");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'd') => delete = true,
            Opt::Flag(b's') => squeeze = true,
            Opt::Flag(b'c') => complement = true,
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    let valid = match operands.len() {
        1 => delete || squeeze,
        2 => !delete || squeeze,
        _ => false,
    };
    if !valid {
        return ctx.usage(USAGE);
    }
    let Some(spelled) = expand_set(operands[0]) else {
        ctx.warn_at(operands[0], b"invalid set");
        return 1;
    };
    let second = match operands.get(1) {
        Some(spec) => match expand_set(spec) {
            Some(set) => Some(set),
            None => {
                ctx.warn_at(spec, b"invalid set");
                return 1;
            }
        },
        None => None,
    };

    let members: Vec<u8> = if complement {
        (0u8..=255).filter(|b| !spelled.contains(b)).collect()
    } else {
        spelled
    };
    let mut in_set1 = [false; 256];
    for &b in &members {
        in_set1[b as usize] = true;
    }

    let mut table = [0u8; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        *slot = i as u8;
    }
    let translate = if delete { None } else { second.as_ref() };
    if let Some(to) = translate {
        if to.is_empty() {
            ctx.warn(b"set2 must not be empty");
            return 1;
        }
        for (i, &b) in members.iter().enumerate() {
            table[b as usize] = to[i.min(to.len() - 1)];
        }
    }

    // Squeezing applies to the last set named, which is set2 whenever there
    // is one — the translated bytes, not the bytes that were translated.
    let mut repeated = [false; 256];
    if squeeze {
        let set = second.as_ref().unwrap_or(&members);
        for &b in set {
            repeated[b as usize] = true;
        }
    }

    let mut stdin = input::Stdin;
    let mut buf = vec![0u8; CHUNK];
    let mut out = Vec::with_capacity(CHUNK);
    let mut last: Option<u8> = None;
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                ctx.warn_io(b"stdin", &e);
                return 1;
            }
        };
        out.clear();
        for &b in &buf[..n] {
            if delete && in_set1[b as usize] {
                continue;
            }
            let byte = table[b as usize];
            if squeeze && repeated[byte as usize] && last == Some(byte) {
                continue;
            }
            last = Some(byte);
            out.push(byte);
        }
        ctx.out.write(&out);
        if ctx.out.broken() {
            break;
        }
    }
    0
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_offset(ctx: &mut Ctx, offset: u64) {
    let mut digits = [0u8; 16];
    for (i, slot) in digits.iter_mut().enumerate() {
        *slot = HEX[((offset >> (60 - 4 * i)) & 0xf) as usize];
    }
    let mut start = 0usize;
    while start < 8 && digits[start] == b'0' {
        start += 1;
    }
    ctx.out.write(&digits[start..]);
}

fn hex_line(ctx: &mut Ctx, offset: u64, bytes: &[u8]) {
    hex_offset(ctx, offset);
    ctx.out.s("  ");
    for i in 0..16 {
        if i == 8 {
            ctx.out.b(b' ');
        }
        match bytes.get(i) {
            Some(&b) => {
                ctx.out
                    .write(&[HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize], b' ']);
            }
            None => ctx.out.s("   "),
        }
    }
    ctx.out.s(" |");
    for &b in bytes {
        ctx.out.b(if b.is_ascii_graphic() || b == b' ' {
            b
        } else {
            b'.'
        });
    }
    ctx.out.s("|");
    ctx.out.nl();
}

fn hexdump(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    const USAGE: &str = "hexdump [-C] [-n count] [file...]";
    let mut left = u64::MAX;
    // `-C` is accepted and ignored: canonical is the only form emitted, since
    // it is the only one anybody reads a dump in.
    let mut opts = Opts::new(argv, "Cn:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'C') => {}
            Opt::Value(b'n', value) => {
                let Some(n) = parse_u64(value) else {
                    ctx.warn_at(value, b"invalid number");
                    return 1;
                };
                left = n;
            }
            Opt::Unknown(f) => {
                ctx.warn_at(&[f], b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Missing(f) => {
                ctx.warn_at(&[f], b"option requires an argument");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let mut status = 0;
    let mut offset = 0u64;
    let mut row = [0u8; 16];
    let mut fill = 0usize;
    let mut buf = vec![0u8; CHUNK];
    'files: for &src in input::sources(opts.operands()) {
        if left == 0 {
            break;
        }
        let Some(mut input) = input::open(ctx, src) else {
            status = 1;
            continue;
        };
        loop {
            let want = left.min(CHUNK as u64) as usize;
            if want == 0 {
                break;
            }
            match input.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    left -= n as u64;
                    for &b in &buf[..n] {
                        row[fill] = b;
                        fill += 1;
                        if fill == 16 {
                            hex_line(ctx, offset, &row);
                            offset += 16;
                            fill = 0;
                        }
                    }
                    if ctx.out.broken() {
                        break 'files;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    ctx.warn_io(src, &e);
                    status = 1;
                    break;
                }
            }
        }
    }
    if fill > 0 {
        hex_line(ctx, offset, &row[..fill]);
        offset += fill as u64;
    }
    // The closing offset line is the length of what was dumped; an empty
    // input dumped nothing, so it has no length to report.
    if offset > 0 {
        hex_offset(ctx, offset);
        ctx.out.nl();
    }
    status
}
