//! `grep`: select lines by pattern.
//!
//! The exit status is the interface — 0 selected, 1 none, 2 trouble — because
//! that is what a shell script tests, not the output.

use super::input::{as_str, buffered, open, read_line, read_to_end, split_lines, trim_newline};
use super::opts::{Opt, Opts};
use super::regex::{self, Regex};
use super::{Ctx, Tool, fsutil};

pub static TOOLS: &[Tool] = &[Tool {
    name: "grep",
    desc: "Search files for a pattern",
    usage: USAGE,
    run: grep,
}];

const USAGE: &str =
    "grep [-EFinvclLoqsrRwxhH] [-e pattern]... [-f file] [--color[=when]] [pattern] [file...]";

const STDIN_NAME: &[u8] = b"(standard input)";

struct Config {
    ere: bool,
    fixed: bool,
    icase: bool,
    invert: bool,
    number: bool,
    count: bool,
    with_match: bool,
    without_match: bool,
    quiet: bool,
    silent: bool,
    word: bool,
    line: bool,
    only: bool,
    recurse: bool,
    follow: bool,
    color: bool,
    names: Option<bool>,
}

enum Matcher {
    Lit(Vec<u8>),
    Re(Regex),
}

struct Run {
    cfg: Config,
    matchers: Vec<Matcher>,
    show_name: bool,
    matched: bool,
    error: bool,
    done: bool,
}

fn grep(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut cfg = Config {
        ere: false,
        fixed: false,
        icase: false,
        invert: false,
        number: false,
        count: false,
        with_match: false,
        without_match: false,
        quiet: false,
        silent: false,
        word: false,
        line: false,
        only: false,
        recurse: false,
        follow: false,
        color: false,
        names: None,
    };
    let mut patterns: Vec<Vec<u8>> = Vec::new();
    let mut pattern_files: Vec<&[u8]> = Vec::new();
    let mut given = false;

    let mut opts = Opts::new(argv, "EFinvclLoqsrRwxhHe:f:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'E') => {
                cfg.ere = true;
                cfg.fixed = false;
            }
            Opt::Flag(b'F') => {
                cfg.fixed = true;
                cfg.ere = false;
            }
            Opt::Flag(b'i') => cfg.icase = true,
            Opt::Flag(b'n') => cfg.number = true,
            Opt::Flag(b'v') => cfg.invert = true,
            Opt::Flag(b'c') => cfg.count = true,
            Opt::Flag(b'l') => cfg.with_match = true,
            Opt::Flag(b'L') => cfg.without_match = true,
            Opt::Flag(b'o') => cfg.only = true,
            Opt::Flag(b'q') => cfg.quiet = true,
            Opt::Flag(b's') => cfg.silent = true,
            Opt::Flag(b'r') => cfg.recurse = true,
            Opt::Flag(b'R') => {
                cfg.recurse = true;
                cfg.follow = true;
            }
            Opt::Flag(b'w') => cfg.word = true,
            Opt::Flag(b'x') => cfg.line = true,
            Opt::Flag(b'h') => cfg.names = Some(false),
            Opt::Flag(b'H') => cfg.names = Some(true),
            Opt::Value(b'e', value) => {
                patterns.push(value.to_vec());
                given = true;
            }
            Opt::Value(b'f', value) => {
                pattern_files.push(value);
                given = true;
            }
            Opt::Long(name, value) => {
                if name != b"color" && name != b"colour" {
                    ctx.warn_at(name, b"unrecognized option");
                    return ctx.usage(USAGE);
                }
                // `always` still only paints a terminal: the escape goes out
                // through `Sink::sgr`, which a pipe never sees.
                let when = value.unwrap_or(b"auto");
                cfg.color = if when == b"auto" || when == b"always" {
                    true
                } else if when == b"never" {
                    false
                } else {
                    ctx.warn_at(when, b"invalid argument for --color");
                    return ctx.usage(USAGE);
                };
            }
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Missing(flag) => {
                ctx.warn_at(&[flag], b"option requires an argument");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let mut operands = opts.operands();

    for file in &pattern_files {
        let Some(mut input) = open(ctx, file) else {
            return 2;
        };
        let bytes = match read_to_end(&mut input) {
            Ok(bytes) => bytes,
            Err(e) => {
                ctx.warn_io(file, &e);
                return 2;
            }
        };
        for line in split_lines(&bytes) {
            patterns.push(trim_newline(line).to_vec());
        }
    }

    if !given {
        let Some((first, rest)) = operands.split_first() else {
            return ctx.usage(USAGE);
        };
        patterns.push(first.to_vec());
        operands = rest;
    }

    let mut matchers = Vec::with_capacity(patterns.len());
    for pattern in &patterns {
        match compile(&cfg, pattern) {
            Ok(matcher) => matchers.push(matcher),
            Err(e) => {
                ctx.warn(e.message().as_bytes());
                return 2;
            }
        }
    }

    let recurse_here: &[&[u8]] = &[b"."];
    let stdin_only: &[&[u8]] = &[b"-"];
    let inputs = if !operands.is_empty() {
        operands
    } else if cfg.recurse {
        recurse_here
    } else {
        stdin_only
    };

    let show_name = cfg.names.unwrap_or(inputs.len() > 1 || cfg.recurse);
    let mut run = Run {
        cfg,
        matchers,
        show_name,
        matched: false,
        error: false,
        done: false,
    };

    for input in inputs {
        if run.cfg.recurse && *input != b"-" {
            run.walk(ctx, input);
        } else {
            run.file(ctx, input);
        }
        if run.done || ctx.out.broken() {
            break;
        }
    }

    if run.cfg.quiet && run.matched {
        return 0;
    }
    if run.error {
        return 2;
    }
    i32::from(!run.matched)
}

fn compile(cfg: &Config, pattern: &[u8]) -> Result<Matcher, regex::Error> {
    if cfg.fixed {
        return Ok(Matcher::Lit(pattern.to_vec()));
    }
    if cfg.line {
        Regex::compile_anchored(pattern, cfg.ere, cfg.icase).map(Matcher::Re)
    } else {
        Regex::compile(pattern, cfg.ere, cfg.icase).map(Matcher::Re)
    }
}

impl Run {
    fn walk(&mut self, ctx: &mut Ctx, operand: &[u8]) {
        let Some(root) = as_str(ctx, operand) else {
            self.error = true;
            return;
        };
        let mut walk = fsutil::Walk::new(root);
        if self.cfg.follow {
            walk = walk.follow();
        }
        for item in walk {
            match item {
                Ok(visit) => {
                    let entry = visit.entry();
                    if entry.kind == fsutil::Kind::File {
                        self.file(ctx, entry.path.as_bytes());
                    }
                }
                Err(failed) => {
                    if !self.cfg.silent {
                        ctx.warn_io(failed.path.as_bytes(), &failed.error);
                    }
                    self.error = true;
                }
            }
            if self.done || ctx.out.broken() {
                break;
            }
        }
    }

    /// `input::open` diagnoses its own failure, which `-s` exists to prevent,
    /// so a named file is opened here and only stdin goes through it.
    fn file(&mut self, ctx: &mut Ctx, operand: &[u8]) {
        let input = if operand == b"-" {
            open(ctx, operand)
        } else {
            match as_str(ctx, operand) {
                Some(path) => match std::fs::File::open(path) {
                    Ok(file) => Some(super::input::Input::File(file)),
                    Err(e) => {
                        if !self.cfg.silent {
                            ctx.warn_io(operand, &e);
                        }
                        None
                    }
                },
                None => None,
            }
        };
        let Some(input) = input else {
            self.error = true;
            return;
        };

        let mut reader = buffered(input);
        let mut buf = Vec::new();
        let mut number = 0u64;
        let mut count = 0u64;
        let mut hit = false;
        loop {
            match read_line(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    if !self.cfg.silent {
                        ctx.warn_io(operand, &e);
                    }
                    self.error = true;
                    break;
                }
            }
            number += 1;
            let line = trim_newline(&buf);
            let selected = self.selects(line);
            if !selected {
                continue;
            }
            hit = true;
            self.matched = true;
            count += 1;
            if self.cfg.quiet {
                self.done = true;
                return;
            }
            if self.cfg.with_match || self.cfg.without_match {
                break;
            }
            if self.cfg.count {
                continue;
            }
            self.emit(ctx, operand, number, line);
            if ctx.out.broken() {
                break;
            }
        }

        if self.cfg.quiet {
            return;
        }
        if self.cfg.with_match || self.cfg.without_match {
            if (self.cfg.with_match && hit) || (self.cfg.without_match && !hit) {
                self.name(ctx, operand);
                ctx.out.nl();
            }
            return;
        }
        if self.cfg.count {
            self.prefix(ctx, operand, 0, false);
            ctx.out.u(count);
            ctx.out.nl();
        }
    }

    fn selects(&self, line: &[u8]) -> bool {
        self.first(line, 0).is_some() != self.cfg.invert
    }

    /// The leftmost match of any pattern at or after `from`; ties go to the
    /// longer span, which is what `--color` and `-o` should highlight.
    fn first(&self, line: &[u8], from: usize) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize)> = None;
        for matcher in &self.matchers {
            let Some((start, end)) = find_span(&self.cfg, matcher, line, from) else {
                continue;
            };
            let better = match best {
                None => true,
                Some((bs, be)) => start < bs || (start == bs && end > be),
            };
            if better {
                best = Some((start, end));
            }
        }
        best
    }

    fn emit(&mut self, ctx: &mut Ctx, operand: &[u8], number: u64, line: &[u8]) {
        if self.cfg.only {
            if self.cfg.invert {
                return;
            }
            let mut from = 0usize;
            while let Some((start, end)) = self.first(line, from) {
                self.prefix(ctx, operand, number, true);
                if self.cfg.color {
                    ctx.out.sgr("1;31");
                    ctx.out.write(&line[start..end]);
                    ctx.out.sgr("0");
                } else {
                    ctx.out.write(&line[start..end]);
                }
                ctx.out.nl();
                from = if end > start { end } else { start + 1 };
                if from > line.len() || ctx.out.broken() {
                    break;
                }
            }
            return;
        }

        self.prefix(ctx, operand, number, true);
        if !self.cfg.color || self.cfg.invert {
            ctx.out.write(line);
            ctx.out.nl();
            return;
        }
        let mut from = 0usize;
        while from <= line.len() {
            let Some((start, end)) = self.first(line, from) else {
                break;
            };
            ctx.out.write(&line[from..start]);
            if end > start {
                ctx.out.sgr("1;31");
                ctx.out.write(&line[start..end]);
                ctx.out.sgr("0");
                from = end;
            } else {
                if start < line.len() {
                    ctx.out.write(&line[start..start + 1]);
                }
                from = start + 1;
            }
        }
        if from <= line.len() {
            ctx.out.write(&line[from..]);
        }
        ctx.out.nl();
    }

    fn prefix(&mut self, ctx: &mut Ctx, operand: &[u8], number: u64, numbered: bool) {
        if self.show_name {
            self.name(ctx, operand);
            ctx.out.b(b':');
        }
        if numbered && self.cfg.number {
            ctx.out.u(number);
            ctx.out.b(b':');
        }
    }

    fn name(&mut self, ctx: &mut Ctx, operand: &[u8]) {
        if operand == b"-" {
            ctx.out.write(STDIN_NAME);
        } else {
            ctx.out.write(operand);
        }
    }
}

fn find_span(cfg: &Config, matcher: &Matcher, line: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut at = from;
    loop {
        let (start, end) = match matcher {
            Matcher::Lit(needle) => {
                if cfg.line {
                    if at > 0 {
                        return None;
                    }
                    let same = if cfg.icase {
                        needle.eq_ignore_ascii_case(line)
                    } else {
                        needle.as_slice() == line
                    };
                    return if same { Some((0, line.len())) } else { None };
                }
                if at > line.len() {
                    return None;
                }
                let found = regex::find_literal(needle, &line[at..], cfg.icase)?;
                (at + found, at + found + needle.len())
            }
            Matcher::Re(re) => {
                let found = re.find(line, at)?;
                (found.start, found.end)
            }
        };
        if !cfg.word || word_bounded(line, start, end) {
            return Some((start, end));
        }
        if start >= line.len() {
            return None;
        }
        at = start + 1;
    }
}

fn word_bounded(line: &[u8], start: usize, end: usize) -> bool {
    let before = start > 0 && regex::is_word_byte(line[start - 1]);
    let after = end < line.len() && regex::is_word_byte(line[end]);
    !before && !after
}
