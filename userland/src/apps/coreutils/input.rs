//! Reading operands: a named file, or standard input for `-` and for no
//! operand at all.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};

use super::io::Ctx;

/// Standard input read through a descriptor rather than `std::io::stdin`, so a
/// tool that also writes to fd 0's peer cannot deadlock on std's own lock.
pub struct Stdin;

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match crate::syscall::fs::read_slice(0, buf) {
            Ok(n) => Ok(n),
            Err(_) => Err(std::io::Error::other("read failed")),
        }
    }
}

pub enum Input {
    Stdin(Stdin),
    File(File),
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Input::Stdin(s) => s.read(buf),
            Input::File(f) => f.read(buf),
        }
    }
}

/// `std::path::Path` is UTF-8-backed on this target — `OsStrExt` is not wired
/// into the patched std — so a non-UTF-8 operand is refused here rather than
/// silently mangled into the name of a different file.
pub fn as_str<'a>(ctx: &mut Ctx, operand: &'a [u8]) -> Option<&'a str> {
    match core::str::from_utf8(operand) {
        Ok(text) => Some(text),
        Err(_) => {
            ctx.warn_at(operand, b"invalid byte sequence in file name");
            None
        }
    }
}

/// Open an operand for reading. `-` is standard input. Diagnoses its own
/// failure, so a caller only has to count one.
pub fn open(ctx: &mut Ctx, operand: &[u8]) -> Option<Input> {
    if operand == b"-" {
        return Some(Input::Stdin(Stdin));
    }
    let path = as_str(ctx, operand)?;
    match File::open(path) {
        Ok(file) => Some(Input::File(file)),
        Err(e) => {
            ctx.warn_io(operand, &e);
            None
        }
    }
}

/// The operand list a filter runs over: its own operands, or a single `-` when
/// it was given none.
pub fn sources<'a>(operands: &'a [&'a [u8]]) -> &'a [&'a [u8]] {
    if operands.is_empty() {
        const STDIN: &[&[u8]] = &[b"-"];
        STDIN
    } else {
        operands
    }
}

pub fn read_to_end(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a line including its `\n`, so a file whose last line is unterminated
/// round-trips through a filter unchanged. `Ok(0)` is end of input.
pub fn read_line(reader: &mut impl BufRead, out: &mut Vec<u8>) -> std::io::Result<usize> {
    out.clear();
    reader.read_until(b'\n', out)
}

/// Split a byte blob into lines, keeping each terminator.
pub fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

/// A line with its trailing `\n` (and `\r\n`) removed.
pub fn trim_newline(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    if end > 0 && line[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && line[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &line[..end]
}

/// Buffer a reader for line work.
pub fn buffered(input: Input) -> BufReader<Input> {
    BufReader::with_capacity(8192, input)
}

/// Copy a reader into a writer in fixed chunks, so a copy's memory cost is
/// independent of the file's size.
pub fn copy_through(reader: &mut impl Read, writer: &mut impl Write) -> std::io::Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..n])?;
        total += n as u64;
    }
}
