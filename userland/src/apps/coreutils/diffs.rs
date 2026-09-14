//! File comparison: `diff`, whose unified output is exactly what `patch`
//! consumes again, and `cmp` on bytes.

use core::cmp::Ordering;
use std::fs::{self, File};
use std::io::{BufRead, Write};
use std::time::UNIX_EPOCH;

use super::input::{as_str, buffered, open, read_to_end, split_lines};
use super::io::Sink;
use super::opts::{Opt, Opts, parse_u64};
use super::{Ctx, Tool, fsutil, time};

const DIFF_USAGE: &str = "diff [-uqriwN] [-U lines] file1 file2";
const PATCH_USAGE: &str = "patch [-pN] [-R] [-i patchfile] [--dry-run] [file]";
const CMP_USAGE: &str = "cmp [-sl] file1 file2";

pub static TOOLS: &[Tool] = &[
    Tool {
        name: "diff",
        desc: "Compare files line by line",
        usage: DIFF_USAGE,
        run: diff,
    },
    Tool {
        name: "patch",
        desc: "Apply a unified diff",
        usage: PATCH_USAGE,
        run: patch,
    },
    Tool {
        name: "cmp",
        desc: "Compare two files byte by byte",
        usage: CMP_USAGE,
        run: cmp,
    },
];

struct DiffFlags {
    unified: bool,
    context: usize,
    brief: bool,
    recurse: bool,
    ignore_case: bool,
    ignore_space: bool,
    new_file: bool,
}

/// One replaced run: `a_len` lines of the left file become `b_len` lines of
/// the right one. Either length may be zero, which is a deletion or an
/// insertion.
struct Change {
    a: usize,
    a_len: usize,
    b: usize,
    b_len: usize,
}

fn diff(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut f = DiffFlags {
        unified: false,
        context: 3,
        brief: false,
        recurse: false,
        ignore_case: false,
        ignore_space: false,
        new_file: false,
    };
    let mut opts = Opts::new(argv, "uqriwNU:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'u') => f.unified = true,
            Opt::Flag(b'q') => f.brief = true,
            Opt::Flag(b'r') => f.recurse = true,
            Opt::Flag(b'i') => f.ignore_case = true,
            Opt::Flag(b'w') => f.ignore_space = true,
            Opt::Flag(b'N') => f.new_file = true,
            Opt::Value(b'U', value) => match parse_u64(value) {
                Some(lines) => {
                    f.unified = true;
                    f.context = lines as usize;
                }
                None => {
                    ctx.warn_at(value, b"invalid context length");
                    return 2;
                }
            },
            Opt::Long(name, value) => match name {
                b"unified" => {
                    f.unified = true;
                    if let Some(value) = value {
                        match parse_u64(value) {
                            Some(lines) => f.context = lines as usize,
                            None => {
                                ctx.warn_at(value, b"invalid context length");
                                return 2;
                            }
                        }
                    }
                }
                b"brief" => f.brief = true,
                b"recursive" => f.recurse = true,
                b"ignore-case" => f.ignore_case = true,
                b"ignore-all-space" => f.ignore_space = true,
                b"new-file" => f.new_file = true,
                _ => {
                    ctx.warn_at(name, b"unrecognized option");
                    return ctx.usage(DIFF_USAGE);
                }
            },
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(DIFF_USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(DIFF_USAGE);
            }
            _ => {}
        }
    }

    let operands = opts.operands();
    if operands.len() != 2 {
        return ctx.usage(DIFF_USAGE);
    }
    let Some(left) = as_str(ctx, operands[0]) else {
        return 2;
    };
    let Some(right) = as_str(ctx, operands[1]) else {
        return 2;
    };

    match (fsutil::is_dir(left), fsutil::is_dir(right)) {
        (true, true) => diff_dirs(ctx, left, right, &f),
        // POSIX: a directory paired with a file compares the file of that name
        // inside the directory.
        (true, false) => {
            let inside = fsutil::join(left, fsutil::base_name(right));
            diff_files(ctx, &inside, right, &f, false)
        }
        (false, true) => {
            let inside = fsutil::join(right, fsutil::base_name(left));
            diff_files(ctx, left, &inside, &f, false)
        }
        (false, false) => diff_files(ctx, left, right, &f, false),
    }
}

/// `in_tree` marks a pair reached through a directory comparison, where the
/// normal format would otherwise print a bare `2c2` with nothing naming the
/// file it belongs to.
fn diff_files(ctx: &mut Ctx, left: &str, right: &str, f: &DiffFlags, in_tree: bool) -> i32 {
    let Some(lbytes) = load(ctx, left, f) else {
        return 2;
    };
    let Some(rbytes) = load(ctx, right, f) else {
        return 2;
    };
    let a = split_lines(&lbytes);
    let b = split_lines(&rbytes);
    if identical(&a, &b, f) {
        return 0;
    }
    if f.brief {
        ctx.out.s("Files ");
        ctx.out.s(left);
        ctx.out.s(" and ");
        ctx.out.s(right);
        ctx.out.s(" differ");
        ctx.out.nl();
        return 1;
    }
    let script = changes(&a, &b, f);
    if script.is_empty() {
        return 0;
    }
    if f.unified {
        print_unified(&mut ctx.out, left, right, &a, &b, &script, f);
    } else {
        if in_tree {
            ctx.out.s("diff ");
            ctx.out.s(left);
            ctx.out.b(b' ');
            ctx.out.s(right);
            ctx.out.nl();
        }
        print_normal(&mut ctx.out, &a, &b, &script);
    }
    1
}

/// Reads one operand whole: the two files under comparison are the only thing
/// held in memory at a time, even under `-r`.
fn load(ctx: &mut Ctx, path: &str, f: &DiffFlags) -> Option<Vec<u8>> {
    if path == "-" {
        let mut input = open(ctx, b"-")?;
        return match read_to_end(&mut input) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                ctx.warn_io(b"-", &e);
                None
            }
        };
    }
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if f.new_file && e.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(e) => {
            ctx.warn_io(path.as_bytes(), &e);
            None
        }
    }
}

/// One directory level at a time rather than [`fsutil::Walk`]: the two
/// listings are merged name by name, which a tree-wide walk cannot do without
/// holding both trees.
fn diff_dirs(ctx: &mut Ctx, left: &str, right: &str, f: &DiffFlags) -> i32 {
    let mut status = 0;
    let mut pending = vec![String::new()];
    while let Some(rel) = pending.pop() {
        let ldir = sub(left, &rel);
        let rdir = sub(right, &rel);
        let lnames = match names(&ldir) {
            Ok(names) => names,
            Err(e) => {
                ctx.warn_io(ldir.as_bytes(), &e);
                status = 2;
                continue;
            }
        };
        let rnames = match names(&rdir) {
            Ok(names) => names,
            Err(e) => {
                ctx.warn_io(rdir.as_bytes(), &e);
                status = 2;
                continue;
            }
        };

        let mut subdirs = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < lnames.len() || j < rnames.len() {
            let order = if i >= lnames.len() {
                Ordering::Greater
            } else if j >= rnames.len() {
                Ordering::Less
            } else {
                lnames[i].cmp(&rnames[j])
            };
            match order {
                Ordering::Less => {
                    status = status.max(unpaired(ctx, &ldir, &rdir, &lnames[i], true, f));
                    i += 1;
                }
                Ordering::Greater => {
                    status = status.max(unpaired(ctx, &rdir, &ldir, &rnames[j], false, f));
                    j += 1;
                }
                Ordering::Equal => {
                    let name = &lnames[i];
                    let lpath = fsutil::join(&ldir, name);
                    let rpath = fsutil::join(&rdir, name);
                    let ldirp = fsutil::is_dir(&lpath);
                    let rdirp = fsutil::is_dir(&rpath);
                    if ldirp && rdirp {
                        if f.recurse {
                            subdirs.push(rel_join(&rel, name));
                        } else {
                            ctx.out.s("Common subdirectories: ");
                            ctx.out.s(&lpath);
                            ctx.out.s(" and ");
                            ctx.out.s(&rpath);
                            ctx.out.nl();
                        }
                    } else if ldirp != rdirp {
                        kind_clash(ctx, &lpath, &rpath, ldirp);
                        status = status.max(1);
                    } else {
                        status = status.max(diff_files(ctx, &lpath, &rpath, f, true));
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
        for dir in subdirs.into_iter().rev() {
            pending.push(dir);
        }
    }
    status
}

/// An entry present on one side only. `-N` diffs it against the absent file,
/// which `load` reads as empty; otherwise it is just reported.
fn unpaired(
    ctx: &mut Ctx,
    here: &str,
    there: &str,
    name: &str,
    here_is_left: bool,
    f: &DiffFlags,
) -> i32 {
    let present = fsutil::join(here, name);
    if f.new_file && !fsutil::is_dir(&present) {
        let absent = fsutil::join(there, name);
        return if here_is_left {
            diff_files(ctx, &present, &absent, f, true)
        } else {
            diff_files(ctx, &absent, &present, f, true)
        };
    }
    ctx.out.s("Only in ");
    ctx.out.s(here);
    ctx.out.s(": ");
    ctx.out.s(name);
    ctx.out.nl();
    1
}

fn kind_clash(ctx: &mut Ctx, lpath: &str, rpath: &str, left_is_dir: bool) {
    let (dir, file) = if left_is_dir {
        (lpath, rpath)
    } else {
        (rpath, lpath)
    };
    ctx.out.s("File ");
    ctx.out.s(dir);
    ctx.out.s(" is a directory while file ");
    ctx.out.s(file);
    ctx.out.s(" is a regular file");
    ctx.out.nl();
}

fn names(dir: &str) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        if let Ok(name) = entry?.file_name().into_string() {
            out.push(name);
        }
    }
    out.sort_unstable();
    Ok(out)
}

fn sub(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        base.to_string()
    } else {
        fsutil::join(base, rel)
    }
}

fn rel_join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_string()
    } else {
        fsutil::join(rel, name)
    }
}

fn body(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(&b'\n') => &line[..line.len() - 1],
        _ => line,
    }
}

fn terminated(line: &[u8]) -> bool {
    line.last() == Some(&b'\n')
}

fn byte_eq(a: u8, b: u8, ignore_case: bool) -> bool {
    if ignore_case {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn same(a: &[u8], b: &[u8], f: &DiffFlags) -> bool {
    if f.ignore_space {
        return space_free_eq(a, b, f.ignore_case);
    }
    // An unterminated last line differs from a terminated one — that is what
    // the `\ No newline at end of file` marker records.
    if terminated(a) != terminated(b) {
        return false;
    }
    let (a, b) = (body(a), body(b));
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| byte_eq(x, y, f.ignore_case))
}

fn space_free_eq(a: &[u8], b: &[u8], ignore_case: bool) -> bool {
    let mut i = 0;
    let mut j = 0;
    loop {
        while i < a.len() && a[i].is_ascii_whitespace() {
            i += 1;
        }
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if i == a.len() || j == b.len() {
            return i == a.len() && j == b.len();
        }
        if !byte_eq(a[i], b[j], ignore_case) {
            return false;
        }
        i += 1;
        j += 1;
    }
}

fn identical(a: &[&[u8]], b: &[&[u8]], f: &DiffFlags) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| same(x, y, f))
}

/// The LCS table costs one `u32` per line pair, so it is only built under this
/// many cells; past it the differing middle is reported as a single
/// replacement instead of being refined.
const LCS_CELL_LIMIT: usize = 1 << 20;

/// The edit script: common prefix and suffix are stripped first, then the
/// differing middle is resolved by a longest-common-subsequence table.
fn changes(a: &[&[u8]], b: &[&[u8]], f: &DiffFlags) -> Vec<Change> {
    let mut lo = 0;
    while lo < a.len() && lo < b.len() && same(a[lo], b[lo], f) {
        lo += 1;
    }
    let mut hi = 0;
    while hi < a.len() - lo
        && hi < b.len() - lo
        && same(a[a.len() - 1 - hi], b[b.len() - 1 - hi], f)
    {
        hi += 1;
    }
    let na = a.len() - lo - hi;
    let nb = b.len() - lo - hi;
    if na == 0 && nb == 0 {
        return Vec::new();
    }
    if na == 0 || nb == 0 || (na + 1).saturating_mul(nb + 1) > LCS_CELL_LIMIT {
        return vec![Change {
            a: lo,
            a_len: na,
            b: lo,
            b_len: nb,
        }];
    }

    let mid_a = &a[lo..lo + na];
    let mid_b = &b[lo..lo + nb];
    let width = nb + 1;
    let mut lcs = vec![0u32; (na + 1) * width];
    for i in (0..na).rev() {
        for j in (0..nb).rev() {
            lcs[i * width + j] = if same(mid_a[i], mid_b[j], f) {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }

    let mut script = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < na || j < nb {
        if i < na && j < nb && same(mid_a[i], mid_b[j], f) {
            i += 1;
            j += 1;
            continue;
        }
        let (si, sj) = (i, j);
        while i < na || j < nb {
            if i < na && j < nb && same(mid_a[i], mid_b[j], f) {
                break;
            }
            if i == na || (j < nb && lcs[i * width + j + 1] >= lcs[(i + 1) * width + j]) {
                j += 1;
            } else {
                i += 1;
            }
        }
        script.push(Change {
            a: lo + si,
            a_len: i - si,
            b: lo + sj,
            b_len: j - sj,
        });
    }
    script
}

fn marked(out: &mut Sink, prefix: &[u8], line: &[u8]) {
    out.write(prefix);
    out.write(line);
    if !terminated(line) {
        out.nl();
        out.s("\\ No newline at end of file\n");
    }
}

fn normal_range(out: &mut Sink, start: usize, len: usize) {
    out.u(start as u64 + 1);
    if len > 1 {
        out.b(b',');
        out.u((start + len) as u64);
    }
}

fn print_normal(out: &mut Sink, a: &[&[u8]], b: &[&[u8]], script: &[Change]) {
    for c in script {
        if c.b_len == 0 {
            normal_range(out, c.a, c.a_len);
            out.b(b'd');
            out.u(c.b as u64);
        } else if c.a_len == 0 {
            out.u(c.a as u64);
            out.b(b'a');
            normal_range(out, c.b, c.b_len);
        } else {
            normal_range(out, c.a, c.a_len);
            out.b(b'c');
            normal_range(out, c.b, c.b_len);
        }
        out.nl();
        for line in &a[c.a..c.a + c.a_len] {
            marked(out, b"< ", line);
        }
        if c.a_len > 0 && c.b_len > 0 {
            out.s("---\n");
        }
        for line in &b[c.b..c.b + c.b_len] {
            marked(out, b"> ", line);
        }
    }
}

fn unified_range(out: &mut Sink, lo: usize, count: usize) {
    if count == 0 {
        out.u(lo as u64);
        out.s(",0");
        return;
    }
    out.u(lo as u64 + 1);
    if count != 1 {
        out.b(b',');
        out.u(count as u64);
    }
}

fn print_unified(
    out: &mut Sink,
    left: &str,
    right: &str,
    a: &[&[u8]],
    b: &[&[u8]],
    script: &[Change],
    f: &DiffFlags,
) {
    file_header(out, "--- ", left);
    file_header(out, "+++ ", right);

    let mut k = 0;
    while k < script.len() {
        let mut last = k;
        while last + 1 < script.len() {
            let end = script[last].a + script[last].a_len;
            if script[last + 1].a - end <= 2 * f.context {
                last += 1;
            } else {
                break;
            }
        }
        let first = &script[k];
        let tail = &script[last];
        let tail_end = tail.a + tail.a_len;
        let a_lo = first.a.saturating_sub(f.context);
        let a_hi = (tail_end + f.context).min(a.len());
        let b_lo = first.b - (first.a - a_lo);
        let b_hi = tail.b + tail.b_len + (a_hi - tail_end);

        out.s("@@ -");
        unified_range(out, a_lo, a_hi - a_lo);
        out.s(" +");
        unified_range(out, b_lo, b_hi - b_lo);
        out.s(" @@");
        out.nl();

        let mut pos = a_lo;
        for c in &script[k..=last] {
            while pos < c.a {
                marked(out, b" ", a[pos]);
                pos += 1;
            }
            for line in &a[c.a..c.a + c.a_len] {
                marked(out, b"-", line);
            }
            for line in &b[c.b..c.b + c.b_len] {
                marked(out, b"+", line);
            }
            pos = c.a + c.a_len;
        }
        while pos < a_hi {
            marked(out, b" ", a[pos]);
            pos += 1;
        }
        k = last + 1;
    }
}

fn file_header(out: &mut Sink, prefix: &str, name: &str) {
    out.s(prefix);
    out.s(name);
    out.b(b'\t');
    stamp(out, mtime_secs(name));
    out.nl();
}

fn mtime_secs(path: &str) -> i64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

fn stamp(out: &mut Sink, secs: i64) {
    let t = time::utc_from_epoch(secs);
    out.s(&format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} +0000",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    ));
}

/// A hunk body line keeps its own terminator, so a patch that ends a file
/// without a newline round-trips.
struct Hunk {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    lines: Vec<(u8, Vec<u8>)>,
}

impl Hunk {
    fn consistent(&self) -> bool {
        let old = self.lines.iter().filter(|(mark, _)| *mark != b'+').count();
        let new = self.lines.iter().filter(|(mark, _)| *mark != b'-').count();
        old == self.old_len && new == self.new_len
    }
}

struct Section {
    old: Vec<u8>,
    new: Vec<u8>,
    hunks: Vec<Hunk>,
}

/// Lines searched either side of a hunk's stated position before it is
/// declared unplaceable.
const FUZZ: isize = 64;

fn patch(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut strip = 0usize;
    let mut reversed = false;
    let mut dry = false;
    let mut source: Option<&[u8]> = None;
    let mut opts = Opts::new(argv, "p:Ri:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Value(b'p', value) => match parse_u64(value) {
                Some(count) => strip = count as usize,
                None => {
                    ctx.warn_at(value, b"invalid strip count");
                    return 2;
                }
            },
            Opt::Flag(b'R') => reversed = true,
            Opt::Value(b'i', value) => source = Some(value),
            Opt::Long(name, _) => match name {
                b"dry-run" => dry = true,
                b"reverse" => reversed = true,
                _ => {
                    ctx.warn_at(name, b"unrecognized option");
                    return ctx.usage(PATCH_USAGE);
                }
            },
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(PATCH_USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(PATCH_USAGE);
            }
            _ => {}
        }
    }

    let operands = opts.operands();
    if operands.len() > 1 {
        return ctx.usage(PATCH_USAGE);
    }

    let bytes = match source {
        Some(operand) => {
            let Some(path) = as_str(ctx, operand) else {
                return 2;
            };
            match fs::read(path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    ctx.warn_io(operand, &e);
                    return 2;
                }
            }
        }
        None => {
            let Some(mut input) = open(ctx, b"-") else {
                return 2;
            };
            match read_to_end(&mut input) {
                Ok(bytes) => bytes,
                Err(e) => {
                    ctx.warn_io(b"-", &e);
                    return 2;
                }
            }
        }
    };

    let mut sections = parse_patch(&bytes);
    if sections.is_empty() {
        ctx.warn(b"only garbage was found in the patch input");
        return 2;
    }
    if reversed {
        reverse(&mut sections);
    }

    let mut status = 0;
    for section in &sections {
        let target = match operands.first() {
            Some(name) => name.to_vec(),
            None => pick_target(&section.old, &section.new, strip),
        };
        // The header's name is the patch author's, not the operator's: an
        // absolute or `..`-bearing one would write outside the directory the
        // patch was applied in. Refused rather than trimmed, because trimming
        // turns a hostile name into a silently successful overwrite.
        if operands.first().is_none() && fsutil::escapes(&target) {
            ctx.warn_at(&target, b"patch target escapes the working directory");
            status = status.max(2);
            continue;
        }
        let Some(path) = as_str(ctx, &target) else {
            status = status.max(2);
            continue;
        };
        ctx.out.s("patching file ");
        ctx.out.s(path);
        ctx.out.nl();
        status = status.max(apply_section(ctx, path, section, dry));
    }
    status
}

fn parse_patch(bytes: &[u8]) -> Vec<Section> {
    let lines = split_lines(bytes);
    let mut sections = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let head = body(lines[i]);
        if !head.starts_with(b"--- ") || i + 1 >= lines.len() {
            i += 1;
            continue;
        }
        let next = body(lines[i + 1]);
        if !next.starts_with(b"+++ ") {
            i += 1;
            continue;
        }
        let old = header_name(&head[4..]);
        let new = header_name(&next[4..]);
        i += 2;

        let mut hunks = Vec::new();
        while i < lines.len() {
            let Some((old_start, old_len, new_start, new_len)) = parse_hunk_header(body(lines[i]))
            else {
                break;
            };
            i += 1;
            let mut hunk_lines: Vec<(u8, Vec<u8>)> = Vec::new();
            let mut old_seen = 0;
            let mut new_seen = 0;
            while i < lines.len() && (old_seen < old_len || new_seen < new_len) {
                let raw = lines[i];
                let (mark, text): (u8, &[u8]) = if raw.is_empty() || raw == b"\n" {
                    (b' ', b"\n")
                } else {
                    (raw[0], &raw[1..])
                };
                match mark {
                    b' ' => {
                        old_seen += 1;
                        new_seen += 1;
                    }
                    b'-' => old_seen += 1,
                    b'+' => new_seen += 1,
                    b'\\' => {
                        retract_newline(&mut hunk_lines);
                        i += 1;
                        continue;
                    }
                    _ => break,
                }
                hunk_lines.push((mark, text.to_vec()));
                i += 1;
            }
            // The marker for the hunk's last line follows the counted body,
            // so it is consumed here rather than by the loop above.
            while i < lines.len() && lines[i].first() == Some(&b'\\') {
                retract_newline(&mut hunk_lines);
                i += 1;
            }
            hunks.push(Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
                lines: hunk_lines,
            });
        }

        if !hunks.is_empty() {
            sections.push(Section { old, new, hunks });
        }
    }
    sections
}

/// `\ No newline at end of file` retracts the terminator the body line before
/// it was printed with.
fn retract_newline(lines: &mut [(u8, Vec<u8>)]) {
    if let Some((_, prev)) = lines.last_mut() {
        if prev.last() == Some(&b'\n') {
            prev.pop();
        }
    }
}

/// A header name runs to the first tab, which is where the timestamp starts.
fn header_name(rest: &[u8]) -> Vec<u8> {
    let end = rest
        .iter()
        .position(|&byte| byte == b'\t')
        .unwrap_or(rest.len());
    let mut name = &rest[..end];
    while let Some(&last) = name.last() {
        if last == b' ' || last == b'\r' {
            name = &name[..name.len() - 1];
        } else {
            break;
        }
    }
    name.to_vec()
}

fn parse_hunk_header(line: &[u8]) -> Option<(usize, usize, usize, usize)> {
    let rest = line.strip_prefix(b"@@ ")?;
    let rest = rest.strip_prefix(b"-")?;
    let ((old_start, old_len), rest) = parse_range(rest)?;
    let rest = skip_spaces(rest).strip_prefix(b"+")?;
    let ((new_start, new_len), _) = parse_range(rest)?;
    Some((old_start, old_len, new_start, new_len))
}

fn parse_range(bytes: &[u8]) -> Option<((usize, usize), &[u8])> {
    let (start, rest) = parse_number(bytes)?;
    match rest.strip_prefix(b",") {
        Some(rest) => {
            let (len, rest) = parse_number(rest)?;
            Some(((start, len), rest))
        }
        None => Some(((start, 1), rest)),
    }
}

fn parse_number(bytes: &[u8]) -> Option<(usize, &[u8])> {
    let end = bytes
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    Some((parse_u64(&bytes[..end])? as usize, &bytes[end..]))
}

fn skip_spaces(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&byte| byte != b' ')
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn reverse(sections: &mut [Section]) {
    for section in sections.iter_mut() {
        core::mem::swap(&mut section.old, &mut section.new);
        for hunk in &mut section.hunks {
            core::mem::swap(&mut hunk.old_start, &mut hunk.new_start);
            core::mem::swap(&mut hunk.old_len, &mut hunk.new_len);
            for (mark, _) in &mut hunk.lines {
                *mark = match *mark {
                    b'-' => b'+',
                    b'+' => b'-',
                    other => other,
                };
            }
        }
    }
}

/// The file being patched is the old side; the new name only serves when the
/// old one is absent, as in a patch that creates a file from `/dev/null`.
fn pick_target(old: &[u8], new: &[u8], strip: usize) -> Vec<u8> {
    let old = strip_components(old, strip);
    let new = strip_components(new, strip);
    let null = &b"/dev/null"[..];
    for candidate in [old, new] {
        if candidate == null {
            continue;
        }
        if let Ok(path) = core::str::from_utf8(candidate) {
            if fs::metadata(path).is_ok() {
                return candidate.to_vec();
            }
        }
    }
    if old != null {
        old.to_vec()
    } else {
        new.to_vec()
    }
}

fn strip_components(name: &[u8], strip: usize) -> &[u8] {
    let mut rest = name;
    for _ in 0..strip {
        match rest.iter().position(|&byte| byte == b'/') {
            Some(slash) => rest = &rest[slash + 1..],
            None => break,
        }
    }
    rest
}

/// A hunk that will not place is reported and the run fails, but the hunks
/// that did place are still written out — losing them would make a partly
/// applied patch unrecoverable. Nothing is written when no hunk placed.
fn apply_section(ctx: &mut Ctx, target: &str, section: &Section, dry: bool) -> i32 {
    let source = match fs::read(target) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && creates(section) => Vec::new(),
        Err(e) => {
            ctx.warn_io(target.as_bytes(), &e);
            return 2;
        }
    };
    let src = split_lines(&source);

    let mut out: Vec<&[u8]> = Vec::new();
    let mut cur = 0usize;
    let mut offset = 0isize;
    let mut applied = 0usize;
    let mut failed = 0usize;

    for (index, hunk) in section.hunks.iter().enumerate() {
        if !hunk.consistent() {
            ctx.warn(b"malformed patch");
            return 2;
        }
        // An empty old range names the line the insertion follows, not a line
        // of its own, so it does not get the 1-based adjustment.
        let want = if hunk.old_len == 0 {
            hunk.old_start
        } else {
            hunk.old_start.saturating_sub(1)
        };
        match locate(&src, hunk, want, offset, cur) {
            Some(at) => {
                out.extend_from_slice(&src[cur..at]);
                let mut pos = at;
                for (mark, text) in &hunk.lines {
                    match *mark {
                        // Context comes from the file, not the patch, so a
                        // file's own line endings survive.
                        b' ' => {
                            out.push(src[pos]);
                            pos += 1;
                        }
                        b'-' => pos += 1,
                        _ => out.push(text.as_slice()),
                    }
                }
                cur = pos;
                offset = at as isize - want as isize;
                applied += 1;
            }
            None => {
                failed += 1;
                ctx.warn(format!("Hunk #{} FAILED at {}", index + 1, hunk.old_start).as_bytes());
            }
        }
    }
    out.extend_from_slice(&src[cur.min(src.len())..]);

    if dry || (failed > 0 && applied == 0) {
        return if failed > 0 { 1 } else { 0 };
    }
    match write_atomic(target, &out) {
        Ok(()) => {
            if failed > 0 {
                1
            } else {
                0
            }
        }
        Err(e) => {
            ctx.warn_io(target.as_bytes(), &e);
            2
        }
    }
}

fn creates(section: &Section) -> bool {
    section
        .hunks
        .iter()
        .all(|hunk| hunk.lines.iter().all(|(mark, _)| *mark == b'+'))
}

fn locate(src: &[&[u8]], hunk: &Hunk, want: usize, offset: isize, floor: usize) -> Option<usize> {
    let guess = want as isize + offset;
    for delta in 0..=FUZZ {
        for candidate in [guess + delta, guess - delta] {
            // A hunk of nothing but `+` lines matches anywhere, so the file's
            // length is the only thing bounding the declared start.
            if candidate >= floor as isize
                && candidate <= src.len() as isize
                && matches_at(src, hunk, candidate as usize)
            {
                return Some(candidate as usize);
            }
            if delta == 0 {
                break;
            }
        }
    }
    None
}

fn matches_at(src: &[&[u8]], hunk: &Hunk, at: usize) -> bool {
    let mut pos = at;
    for (mark, text) in &hunk.lines {
        if *mark == b'+' {
            continue;
        }
        if pos >= src.len() || body(src[pos]) != body(text) {
            return false;
        }
        pos += 1;
    }
    true
}

/// Through a temporary and a rename, so an interrupted write cannot leave a
/// half-patched file.
fn write_atomic(target: &str, lines: &[&[u8]]) -> std::io::Result<()> {
    let temp = format!("{target}.patch.tmp");
    {
        let mut file = File::create(&temp)?;
        for line in lines {
            file.write_all(line)?;
        }
        file.flush()?;
    }
    match fs::rename(&temp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(e)
        }
    }
}

fn cmp(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut silent = false;
    let mut list = false;
    let mut opts = Opts::new(argv, "sl");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b's') => silent = true,
            Opt::Flag(b'l') => list = true,
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(CMP_USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    if operands.len() != 2 {
        return ctx.usage(CMP_USAGE);
    }
    let Some(left) = open(ctx, operands[0]) else {
        return 2;
    };
    let Some(right) = open(ctx, operands[1]) else {
        return 2;
    };
    let mut left = buffered(left);
    let mut right = buffered(right);

    let mut offset = 0u64;
    let mut line = 1u64;
    let mut status = 0;
    loop {
        let take;
        {
            let a = match left.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) => {
                    ctx.warn_io(operands[0], &e);
                    return 2;
                }
            };
            let b = match right.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) => {
                    ctx.warn_io(operands[1], &e);
                    return 2;
                }
            };
            if a.is_empty() || b.is_empty() {
                if a.is_empty() && b.is_empty() {
                    return status;
                }
                let short = if a.is_empty() {
                    operands[0]
                } else {
                    operands[1]
                };
                if !silent {
                    let mut message = Vec::with_capacity(7 + short.len());
                    message.extend_from_slice(b"EOF on ");
                    message.extend_from_slice(short);
                    ctx.warn(&message);
                }
                return 1;
            }

            let n = a.len().min(b.len());
            for k in 0..n {
                let (x, y) = (a[k], b[k]);
                if x != y {
                    if silent {
                        return 1;
                    }
                    if !list {
                        ctx.out.write(operands[0]);
                        ctx.out.b(b' ');
                        ctx.out.write(operands[1]);
                        ctx.out.s(" differ: char ");
                        ctx.out.u(offset + k as u64 + 1);
                        ctx.out.s(", line ");
                        ctx.out.u(line);
                        ctx.out.nl();
                        return 1;
                    }
                    ctx.out.u(offset + k as u64 + 1);
                    ctx.out.s(&format!(" {x:3o} {y:3o}"));
                    ctx.out.nl();
                    status = 1;
                }
                if x == b'\n' {
                    line += 1;
                }
            }
            offset += n as u64;
            take = n;
        }
        left.consume(take);
        right.consume(take);
    }
}
