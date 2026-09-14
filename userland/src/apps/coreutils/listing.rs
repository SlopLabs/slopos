//! Directory listing and file status: `ls` and `stat`.
//!
//! The listing a pipeline reads is bare names, one per line. Columns and
//! colour appear only when the destination is a terminal, which is what lets
//! `ls | while read name` work at all.

use super::fsutil::{self, Kind};
use super::input::as_str;
use super::opts::{Opt, Opts};
use super::time::{self, MONTHS};
use super::{Ctx, Tool};
use std::fs::{self, Metadata};
use std::time::UNIX_EPOCH;

const LS_USAGE: &str = "ls [-aAlF1dRhrt] [--color[=WHEN]] [file...]";
const STAT_USAGE: &str = "stat [-t] file...";

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "ls",
        desc: "List directory contents",
        usage: LS_USAGE,
        run: ls,
    },
    Tool {
        name: "stat",
        desc: "Report file status",
        usage: STAT_USAGE,
        run: stat,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Never,
    Auto,
    Always,
}

struct LsOpts {
    all: bool,
    almost: bool,
    long: bool,
    classify: bool,
    one: bool,
    dir_itself: bool,
    recurse: bool,
    human: bool,
    reverse: bool,
    by_time: bool,
    color: Color,
}

struct Row {
    name: String,
    path: String,
    kind: Kind,
    size: u64,
    mtime: i64,
}

fn row(name: String, path: String, meta: &Metadata) -> Row {
    Row {
        name,
        path,
        kind: Kind::of(meta),
        size: meta.len(),
        mtime: mtime_secs(meta),
    }
}

fn ls(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut o = LsOpts {
        all: false,
        almost: false,
        long: false,
        classify: false,
        one: false,
        dir_itself: false,
        recurse: false,
        human: false,
        reverse: false,
        by_time: false,
        color: Color::Auto,
    };
    let mut opts = Opts::new(argv, "aAlF1dRhrt");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'a') => o.all = true,
            Opt::Flag(b'A') => o.almost = true,
            Opt::Flag(b'l') => o.long = true,
            Opt::Flag(b'F') => o.classify = true,
            Opt::Flag(b'1') => o.one = true,
            Opt::Flag(b'd') => o.dir_itself = true,
            Opt::Flag(b'R') => o.recurse = true,
            Opt::Flag(b'h') => o.human = true,
            Opt::Flag(b'r') => o.reverse = true,
            Opt::Flag(b't') => o.by_time = true,
            Opt::Long(name, value) => {
                if name != b"color" {
                    ctx.warn_at(name, b"unrecognized option");
                    return ctx.usage(LS_USAGE);
                }
                o.color = match value {
                    None => Color::Always,
                    Some(when) if when == b"always" || when == b"force" || when == b"yes" => {
                        Color::Always
                    }
                    Some(when) if when == b"auto" || when == b"tty" || when == b"if-tty" => {
                        Color::Auto
                    }
                    Some(when) if when == b"never" || when == b"none" || when == b"no" => {
                        Color::Never
                    }
                    Some(when) => {
                        ctx.warn_at(when, b"invalid argument to --color");
                        return ctx.usage(LS_USAGE);
                    }
                };
            }
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(LS_USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(LS_USAGE);
            }
            _ => {}
        }
    }

    const DOT: &[&[u8]] = &[b"."];
    let given = opts.operands();
    let operands = if given.is_empty() { DOT } else { given };

    let mut status = 0;
    let mut files: Vec<Row> = Vec::new();
    let mut dirs: Vec<Row> = Vec::new();
    for &operand in operands {
        let Some(path) = as_str(ctx, operand) else {
            status = 2;
            continue;
        };
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                ctx.warn_io(operand, &error);
                status = 2;
                continue;
            }
        };
        let kind = Kind::of(&meta);
        let listable =
            !o.dir_itself && (kind == Kind::Dir || (kind == Kind::Symlink && fsutil::is_dir(path)));
        let entry = row(path.to_string(), path.to_string(), &meta);
        if listable {
            dirs.push(entry);
        } else {
            files.push(entry);
        }
    }
    sort_rows(&mut files, &o);
    sort_rows(&mut dirs, &o);

    let now = crate::syscall::core::realtime_secs();
    let headers = o.recurse || dirs.len() + usize::from(!files.is_empty()) > 1;
    let mut printed = false;
    if !files.is_empty() {
        emit(ctx, &files, &o, now);
        printed = true;
    }

    for dir in &dirs {
        let mut stack = vec![dir.path.clone()];
        while let Some(path) = stack.pop() {
            if ctx.out.broken() {
                return status;
            }
            let Some(rows) = dir_rows(ctx, &path, &o, &mut status) else {
                continue;
            };
            if headers {
                if printed {
                    ctx.out.nl();
                }
                ctx.out.s(&path);
                ctx.out.s(":");
                ctx.out.nl();
            }
            emit(ctx, &rows, &o, now);
            printed = true;
            if o.recurse {
                let mut subs: Vec<String> = rows
                    .iter()
                    .filter(|r| r.kind == Kind::Dir && r.name != "." && r.name != "..")
                    .map(|r| r.path.clone())
                    .collect();
                subs.reverse();
                stack.extend(subs);
            }
        }
    }
    status
}

fn dir_rows(ctx: &mut Ctx, dir: &str, o: &LsOpts, status: &mut i32) -> Option<Vec<Row>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            ctx.warn_io(dir.as_bytes(), &error);
            *status = 2;
            return None;
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    if o.all {
        for name in [".", ".."] {
            let path = if name == "." {
                dir.to_string()
            } else {
                fsutil::join(dir, "..")
            };
            if let Ok(meta) = fs::symlink_metadata(&path) {
                rows.push(row(name.to_string(), path, &meta));
            }
        }
    }
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                ctx.warn_io(dir.as_bytes(), &error);
                *status = 2;
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') && !o.all && !o.almost {
            continue;
        }
        let path = fsutil::join(dir, &name);
        match fs::symlink_metadata(&path) {
            Ok(meta) => rows.push(row(name, path, &meta)),
            Err(error) => {
                ctx.warn_io(path.as_bytes(), &error);
                *status = 2;
            }
        }
    }
    sort_rows(&mut rows, o);
    Some(rows)
}

fn sort_rows(rows: &mut [Row], o: &LsOpts) {
    if o.by_time {
        rows.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.name.cmp(&b.name)));
    } else {
        rows.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if o.reverse {
        rows.reverse();
    }
}

fn emit(ctx: &mut Ctx, rows: &[Row], o: &LsOpts, now: Option<i64>) {
    if o.long {
        emit_long(ctx, rows, o, now);
    } else if o.one || !ctx.out.is_tty() {
        for entry in rows {
            if ctx.out.broken() {
                return;
            }
            write_name(ctx, entry, o);
            ctx.out.nl();
        }
    } else {
        emit_columns(ctx, rows, o);
    }
}

fn emit_columns(ctx: &mut Ctx, rows: &[Row], o: &LsOpts) {
    let widest = rows.iter().map(|r| display_width(r, o)).max().unwrap_or(0);
    let cell = widest + 2;
    let columns = (term_width(ctx.out.fd()) / cell).max(1);
    let lines = rows.len().div_ceil(columns);
    for line in 0..lines {
        if ctx.out.broken() {
            return;
        }
        for column in 0..columns {
            let index = column * lines + line;
            let Some(entry) = rows.get(index) else {
                break;
            };
            write_name(ctx, entry, o);
            if index + lines < rows.len() {
                for _ in display_width(entry, o)..cell {
                    ctx.out.b(b' ');
                }
            }
        }
        ctx.out.nl();
    }
}

fn emit_long(ctx: &mut Ctx, rows: &[Row], o: &LsOpts, now: Option<i64>) {
    let sizes: Vec<String> = rows
        .iter()
        .map(|r| {
            if o.human {
                human_size(r.size)
            } else {
                r.size.to_string()
            }
        })
        .collect();
    let width = sizes.iter().map(|s| s.len()).max().unwrap_or(1);
    for (entry, size) in rows.iter().zip(sizes.iter()) {
        if ctx.out.broken() {
            return;
        }
        ctx.out.s(mode_string(entry.kind));
        // `Metadata` carries no link count on this target, so the field is 1.
        ctx.out.s(" 1 ");
        for _ in size.len()..width {
            ctx.out.b(b' ');
        }
        ctx.out.s(size);
        ctx.out.b(b' ');
        ctx.out.s(&format_time(entry.mtime, now));
        ctx.out.b(b' ');
        write_name(ctx, entry, o);
        if entry.kind == Kind::Symlink {
            if let Ok(target) = fsutil::read_link(&entry.path) {
                ctx.out.s(" -> ");
                ctx.out.write(&target);
            }
        }
        ctx.out.nl();
    }
}

fn write_name(ctx: &mut Ctx, entry: &Row, o: &LsOpts) {
    let params = match entry.kind {
        Kind::Dir => Some("1;34"),
        Kind::Symlink => Some("1;36"),
        _ => None,
    };
    if let Some(params) = params {
        paint(ctx, o.color, params);
    }
    ctx.out.s(&entry.name);
    if params.is_some() {
        paint(ctx, o.color, "0");
    }
    if o.classify {
        if let Some(suffix) = classify(entry.kind) {
            ctx.out.b(suffix);
        }
    }
}

/// `Sink::sgr` is already a no-op off a terminal, so `auto` needs no test of
/// its own; `always` has to write the escape past that guard.
fn paint(ctx: &mut Ctx, color: Color, params: &str) {
    match color {
        Color::Never => {}
        Color::Auto => ctx.out.sgr(params),
        Color::Always => {
            ctx.out.s("\x1b[");
            ctx.out.s(params);
            ctx.out.s("m");
        }
    }
}

fn classify(kind: Kind) -> Option<u8> {
    match kind {
        Kind::Dir => Some(b'/'),
        Kind::Symlink => Some(b'@'),
        _ => None,
    }
}

fn display_width(entry: &Row, o: &LsOpts) -> usize {
    let suffix = usize::from(o.classify && classify(entry.kind).is_some());
    entry.name.chars().count() + suffix
}

fn term_width(fd: i32) -> usize {
    match crate::syscall::fs::tiocgwinsz(fd) {
        Ok(ws) if ws.ws_col != 0 => ws.ws_col as usize,
        _ => 80,
    }
}

/// Only the type bits are real: SlopOS is single-user at uid 0 and `Metadata`
/// exposes no mode, so the permission triples are the fixed answer rather than
/// an invented one.
fn mode_string(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir => "drwxr-xr-x",
        Kind::Symlink => "lrwxrwxrwx",
        _ => "-rwxr-xr-x",
    }
}

fn type_char(kind: Kind) -> u8 {
    match kind {
        Kind::Dir => b'd',
        Kind::Symlink => b'l',
        _ => b'-',
    }
}

fn type_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir => "directory",
        Kind::Symlink => "symbolic link",
        Kind::File => "regular file",
        Kind::Other => "special file",
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[u8] = b"KMGTPE";
    if bytes < 1024 {
        return bytes.to_string();
    }
    let mut value = bytes;
    let mut rest = 0;
    let mut unit = 0;
    while value >= 1024 && unit < UNITS.len() {
        rest = value % 1024;
        value /= 1024;
        unit += 1;
    }
    let suffix = UNITS[unit - 1] as char;
    if value >= 10 {
        let value = value + u64::from(rest >= 512);
        return format!("{value}{suffix}");
    }
    let tenths = (rest * 10 + 512) / 1024;
    let (value, tenths) = if tenths >= 10 {
        (value + 1, 0)
    } else {
        (value, tenths)
    };
    format!("{value}.{tenths}{suffix}")
}

fn mtime_secs(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Roughly six months: past that, `ls -l` shows the year instead of the clock,
/// because the year is what disambiguates an old file.
const RECENT_SECS: i64 = 15_778_476;

fn format_time(secs: i64, now: Option<i64>) -> String {
    let t = time::utc_from_epoch(secs);
    let name = MONTHS[t.month as usize - 1];
    if now.is_none_or(|now| (now - secs).abs() <= RECENT_SECS) {
        format!("{name} {:>2} {:02}:{:02}", t.day, t.hour, t.minute)
    } else {
        format!("{name} {:>2}  {}", t.day, t.year)
    }
}

fn format_stamp(secs: i64) -> String {
    let t = time::utc_from_epoch(secs);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

fn stat(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut terse = false;
    let mut opts = Opts::new(argv, "t");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b't') => terse = true,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(STAT_USAGE);
            }
            _ => return ctx.usage(STAT_USAGE),
        }
    }
    let operands = opts.operands();
    if operands.is_empty() {
        return ctx.usage(STAT_USAGE);
    }

    let mut status = 0;
    for &operand in operands {
        let Some(path) = as_str(ctx, operand) else {
            status = 1;
            continue;
        };
        // No `-L`: a symlink operand is reported as the link, never as what it
        // points at.
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                ctx.warn_io(operand, &error);
                status = 1;
                continue;
            }
        };
        let kind = Kind::of(&meta);
        let secs = mtime_secs(&meta);
        if terse {
            ctx.out.s(path);
            ctx.out.b(b' ');
            ctx.out.u(meta.len());
            ctx.out.b(b' ');
            ctx.out.b(type_char(kind));
            ctx.out.b(b' ');
            ctx.out.i(secs);
            ctx.out.nl();
            continue;
        }
        ctx.out.s("  File: ");
        ctx.out.s(path);
        if kind == Kind::Symlink {
            if let Ok(target) = fsutil::read_link(path) {
                ctx.out.s(" -> ");
                ctx.out.write(&target);
            }
        }
        ctx.out.nl();
        ctx.out.s("  Size: ");
        ctx.out.u(meta.len());
        ctx.out.s("\tType: ");
        ctx.out.s(type_name(kind));
        ctx.out.nl();
        ctx.out.s("Modify: ");
        ctx.out.s(&format_stamp(secs));
        ctx.out.nl();
    }
    status
}
