//! `less`: a pager an editor session can live inside.
//!
//! With a terminal on both ends it takes over fd 0 in raw mode and paints
//! screenfuls; with either end redirected it is a `cat`, which is what keeps
//! `less` safe in the middle of a pipeline.

use std::io::Read;

use slopos_abi::syscall::{LocalFlags, UserTermios, VMIN, VTIME};

use super::input::{open, sources, split_lines, trim_newline};
use super::io::{Sink, decimal_width};
use super::opts::{Opt, Opts};
use super::{Ctx, Tool};
use crate::syscall::fs;

pub static TOOLS: &[Tool] = &[Tool {
    name: "less",
    desc: "Browse a file a screenful at a time",
    usage: USAGE,
    run: less,
}];

const USAGE: &str = "less [-NS] [file...]";
const STDIN: i32 = 0;
const FALLBACK_ROWS: usize = 24;
const FALLBACK_COLS: usize = 80;
const TAB_WIDTH: usize = 8;

static HELP: &[&str] = &[
    "less -- commands",
    "",
    "  SPACE, f, PageDown   forward one screen",
    "  b, PageUp            back one screen",
    "  j, Down, Enter       forward one line",
    "  k, Up                back one line",
    "  g, Home              first line",
    "  G, End               last line",
    "  /pattern             search forward (plain substring)",
    "  n                    repeat the last search",
    "  h                    this list",
    "  q                    quit",
];

fn less(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut numbers = false;
    let mut chop = false;
    let mut opts = Opts::new(argv, "NS");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'N') => numbers = true,
            Opt::Flag(b'S') => chop = true,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();

    if !ctx.out.is_tty() || !fs::isatty(STDIN) {
        return concatenate(ctx, operands);
    }

    if operands.is_empty() {
        return ctx.usage(USAGE);
    }

    // The content has to be off fd 0 before raw mode: once the pager owns the
    // descriptor it is the key stream, not a file to read.
    let mut data = Vec::new();
    let mut status = 0;
    for operand in operands {
        let Some(mut input) = open(ctx, operand) else {
            status = 1;
            continue;
        };
        if let Err(error) = input.read_to_end(&mut data) {
            ctx.warn_io(operand, &error);
            status = 1;
        }
    }
    let lines: Vec<&[u8]> = split_lines(&data).into_iter().map(trim_newline).collect();

    let Some(_raw) = RawMode::enter() else {
        ctx.warn_at(b"standard input", b"cannot enter raw mode");
        return 1;
    };
    let view = View {
        lines: &lines,
        numbers,
        chop,
        width: decimal_width(lines.len() as u64),
    };
    browse(ctx, &view);
    status
}

/// The degraded path: byte-for-byte `cat`, including its exit status.
fn concatenate(ctx: &mut Ctx, operands: &[&[u8]]) -> i32 {
    let mut status = 0;
    let mut buf = [0u8; 8192];
    for operand in sources(operands) {
        let Some(mut input) = open(ctx, operand) else {
            status = 1;
            continue;
        };
        loop {
            match input.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => {
                    ctx.out.write(&buf[..count]);
                    if ctx.out.broken() {
                        return status;
                    }
                }
                Err(error) => {
                    ctx.warn_io(operand, &error);
                    status = 1;
                    break;
                }
            }
        }
    }
    status
}

/// Raw mode with its restore in `Drop`, so no exit path leaves the terminal
/// unusable. `ISIG` goes too: a signal death is the one path `Drop` misses.
struct RawMode {
    saved: UserTermios,
}

impl RawMode {
    fn enter() -> Option<Self> {
        let saved = fs::tcgetattr(STDIN).ok()?;
        let mut raw = saved;
        raw.c_lflag
            .remove(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG);
        raw.c_cc[VMIN] = 1;
        raw.c_cc[VTIME] = 0;
        fs::tcsetattr(STDIN, &raw).ok()?;
        Some(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = fs::tcsetattr(STDIN, &self.saved);
    }
}

struct View<'a> {
    lines: &'a [&'a [u8]],
    numbers: bool,
    chop: bool,
    width: usize,
}

impl View<'_> {
    /// Columns the line-number gutter takes before the text starts.
    fn prefix(&self) -> usize {
        if self.numbers { self.width + 1 } else { 0 }
    }

    fn rows_for(&self, index: usize, cols: usize) -> usize {
        if self.chop {
            return 1;
        }
        let mut buf = Vec::new();
        let cells = self.prefix() + render(self.lines[index], self.prefix(), usize::MAX, &mut buf);
        cells.div_ceil(cols.max(1)).max(1)
    }

    fn emit(&self, out: &mut Sink, index: usize, cols: usize) {
        if self.numbers {
            out.u_right(index as u64 + 1, self.width);
            out.b(b' ');
        }
        let limit = if self.chop {
            cols.saturating_sub(self.prefix())
        } else {
            usize::MAX
        };
        let mut buf = Vec::new();
        render(self.lines[index], self.prefix(), limit, &mut buf);
        out.write(&buf);
    }

    /// The first line of the screenful that ends just before `from`, which is
    /// both "one screen back" and, from the end, the furthest `top` may go.
    fn back_from(&self, from: usize, body: usize, cols: usize) -> usize {
        let mut used = 0;
        let mut index = from;
        while index > 0 {
            let need = self.rows_for(index - 1, cols);
            if used + need > body {
                break;
            }
            used += need;
            index -= 1;
        }
        index
    }

    /// Plain substring search: a pager wants the literal the reader typed, and
    /// a second pattern engine here would earn nothing.
    fn search(&self, from: usize, pattern: &[u8]) -> Option<usize> {
        if pattern.is_empty() {
            return None;
        }
        self.lines
            .iter()
            .enumerate()
            .skip(from + 1)
            .find(|(_, line)| line.windows(pattern.len()).any(|window| window == pattern))
            .map(|(index, _)| index)
    }
}

fn browse(ctx: &mut Ctx, view: &View) {
    let mut top = 0;
    let mut pattern: Vec<u8> = Vec::new();
    loop {
        let (rows, cols) = window();
        let body = rows.saturating_sub(1).max(1);
        let shown = draw(ctx, view, top, rows, cols);
        if ctx.out.broken() {
            return;
        }
        let last = view.back_from(view.lines.len(), body, cols);
        let Some(key) = read_key() else { break };
        match key {
            Key::Char(b'q') => break,
            Key::Char(b' ') | Key::Char(b'f') | Key::PageDown => {
                top = (top + shown.max(1)).min(last);
            }
            Key::Char(b'b') | Key::PageUp => top = view.back_from(top, body, cols),
            Key::Char(b'j') | Key::Char(b'\r') | Key::Char(b'\n') | Key::Down => {
                top = (top + 1).min(last);
            }
            Key::Char(b'k') | Key::Up => top = top.saturating_sub(1),
            Key::Char(b'g') | Key::Home => top = 0,
            Key::Char(b'G') | Key::End => top = last,
            Key::Char(b'h') => show_help(ctx, rows),
            Key::Char(b'/') => {
                if let Some(typed) = read_pattern(ctx, rows) {
                    pattern = typed;
                    if let Some(hit) = view.search(top, &pattern) {
                        top = hit.min(last);
                    }
                }
            }
            Key::Char(b'n') => {
                if let Some(hit) = view.search(top, &pattern) {
                    top = hit.min(last);
                }
            }
            _ => {}
        }
    }
    ctx.out.s("\r\n");
    present(ctx);
}

/// Returns how many source lines the screenful held, which is what "forward
/// one screen" advances by.
fn draw(ctx: &mut Ctx, view: &View, top: usize, rows: usize, cols: usize) -> usize {
    let body = rows.saturating_sub(1).max(1);
    ctx.out.s("\x1b[H\x1b[2J");
    let mut used = 0;
    let mut index = top;
    while index < view.lines.len() {
        let need = view.rows_for(index, cols);
        if used > 0 && used + need > body {
            break;
        }
        if index > top {
            ctx.out.s("\r\n");
        }
        view.emit(&mut ctx.out, index, cols);
        used += need;
        index += 1;
        if used >= body {
            break;
        }
    }
    ctx.out.s("\x1b[");
    ctx.out.u(rows as u64);
    ctx.out.s(";1H\x1b[K");
    if index >= view.lines.len() {
        ctx.out.s("(END)");
    } else {
        ctx.out.b(b':');
    }
    present(ctx);
    index - top
}

fn show_help(ctx: &mut Ctx, rows: usize) {
    ctx.out.s("\x1b[H\x1b[2J");
    for (index, line) in HELP.iter().enumerate() {
        if index > 0 {
            ctx.out.s("\r\n");
        }
        ctx.out.s(line);
    }
    ctx.out.s("\x1b[");
    ctx.out.u(rows as u64);
    ctx.out.s(";1H\x1b[K:");
    present(ctx);
    let _ = read_key();
}

/// Collect a search pattern on the prompt row. ESC abandons it.
fn read_pattern(ctx: &mut Ctx, rows: usize) -> Option<Vec<u8>> {
    let mut typed = Vec::new();
    loop {
        ctx.out.s("\x1b[");
        ctx.out.u(rows as u64);
        ctx.out.s(";1H\x1b[K/");
        ctx.out.write(&typed);
        present(ctx);
        match read_byte()? {
            b'\r' | b'\n' => return Some(typed),
            0x1b => return None,
            0x7f | 0x08 => {
                typed.pop();
            }
            byte if byte >= 0x20 => typed.push(byte),
            _ => {}
        }
    }
}

/// The pager is the one utility that must flush mid-run: it blocks on a
/// keypress, so the screen has to be on the wire before the read.
fn present(ctx: &mut Ctx) {
    ctx.out.flush();
}

fn window() -> (usize, usize) {
    match fs::tiocgwinsz(1) {
        Ok(ws) if ws.ws_row > 0 && ws.ws_col > 0 => (ws.ws_row as usize, ws.ws_col as usize),
        _ => (FALLBACK_ROWS, FALLBACK_COLS),
    }
}

/// Expand `line` into display cells starting at column `start`, stopping once
/// `limit` cells have been produced, and report the cells added.
fn render(line: &[u8], start: usize, limit: usize, out: &mut Vec<u8>) -> usize {
    let mut cells = 0;
    for &byte in line {
        if cells >= limit {
            break;
        }
        if byte == b'\t' {
            let column = start + cells;
            let stop = (column / TAB_WIDTH + 1) * TAB_WIDTH - start;
            while cells < stop.min(limit) {
                out.push(b' ');
                cells += 1;
            }
        } else {
            out.push(byte);
            // A UTF-8 continuation byte is part of the cell before it.
            if byte & 0xC0 != 0x80 {
                cells += 1;
            }
        }
    }
    cells
}

enum Key {
    Char(u8),
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Other,
}

fn read_byte() -> Option<u8> {
    let mut byte = [0u8; 1];
    match fs::read_slice(STDIN, &mut byte) {
        Ok(1) => Some(byte[0]),
        _ => None,
    }
}

/// Only the sequences `terminal-core`'s `encode_key` emits are decoded: CSI
/// `A`/`B`/`C`/`D`, `H`, `F`, and the `~`-terminated `3`, `5` and `6`.
fn read_key() -> Option<Key> {
    let byte = read_byte()?;
    if byte != 0x1b {
        return Some(Key::Char(byte));
    }
    if read_byte()? != b'[' {
        return Some(Key::Other);
    }
    match read_byte()? {
        b'A' => Some(Key::Up),
        b'B' => Some(Key::Down),
        b'H' => Some(Key::Home),
        b'F' => Some(Key::End),
        b'5' => {
            read_byte()?;
            Some(Key::PageUp)
        }
        b'6' => {
            read_byte()?;
            Some(Key::PageDown)
        }
        b'3' => {
            read_byte()?;
            Some(Key::Other)
        }
        _ => Some(Key::Other),
    }
}
