//! `sed`: the stream editor.
//!
//! Branching (`b`, `t`, `:label`), the hold space and `D` are omitted, and
//! each reports itself as an unknown command rather than silently doing
//! nothing.

use super::input::{as_str, buffered, open, read_line, read_to_end};
use super::io::io_message;
use super::opts::{Opt, Opts};
use super::regex::{Match, Regex};
use super::{Ctx, Tool};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};

pub static TOOLS: &[Tool] = &[Tool {
    name: "sed",
    desc: "Edit a stream of text by script",
    usage: USAGE,
    run: sed,
}];

const USAGE: &str = "sed [-nEir] [-e script]... [-f file] [script] [file...]";

enum Bound {
    Line(u64),
    Last,
    Re(Regex),
}

enum Addr {
    All,
    One(Bound),
    Range(Bound, Bound),
}

struct Subst {
    re: Regex,
    rep: Vec<u8>,
    global: bool,
    occurrence: u32,
    print: bool,
}

enum Kind {
    Subst(Subst),
    Delete,
    Print,
    PrintFirst,
    Next,
    AppendNext,
    Quit,
    LineNumber,
    Translate(Box<[u8; 256]>),
    Append(Vec<u8>),
    Insert(Vec<u8>),
    Change(Vec<u8>),
    /// Index of the matching `}`; a block whose address misses jumps there.
    BlockStart(usize),
    BlockEnd,
}

struct Cmd {
    addr: Addr,
    negate: bool,
    kind: Kind,
}

struct Prog {
    cmds: Vec<Cmd>,
    quiet: bool,
}

struct Fault {
    message: &'static str,
    cmd: Option<u8>,
}

impl Fault {
    fn msg(message: &'static str) -> Fault {
        Fault { message, cmd: None }
    }

    fn at(cmd: u8, message: &'static str) -> Fault {
        Fault {
            message,
            cmd: Some(cmd),
        }
    }
}

fn sed(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut script: Vec<u8> = Vec::new();
    let mut given = false;
    let mut ere = false;
    let mut quiet = false;
    let mut inplace = false;

    let mut opts = Opts::new(argv, "nEire:f:");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'n') => quiet = true,
            Opt::Flag(b'E') | Opt::Flag(b'r') => ere = true,
            Opt::Flag(b'i') => inplace = true,
            Opt::Value(b'e', value) => {
                if !script.is_empty() {
                    script.push(b'\n');
                }
                script.extend_from_slice(value);
                given = true;
            }
            Opt::Value(b'f', value) => {
                let Some(mut input) = open(ctx, value) else {
                    return 1;
                };
                match read_to_end(&mut input) {
                    Ok(bytes) => {
                        if !script.is_empty() {
                            script.push(b'\n');
                        }
                        script.extend_from_slice(&bytes);
                    }
                    Err(e) => {
                        ctx.warn_io(value, &e);
                        return 1;
                    }
                }
                given = true;
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

    if !given {
        let Some((first, rest)) = operands.split_first() else {
            return ctx.usage(USAGE);
        };
        script.extend_from_slice(first);
        operands = rest;
    }

    let mut parser = Parser {
        src: &script,
        pos: 0,
        ere,
        cmds: Vec::new(),
        open_blocks: Vec::new(),
        quiet: false,
        last_re: Vec::new(),
    };
    if let Err(fault) = parser.parse() {
        match fault.cmd {
            Some(c) => ctx.warn_at(&[c], fault.message.as_bytes()),
            None => ctx.warn(fault.message.as_bytes()),
        }
        return 1;
    }
    let prog = Prog {
        quiet: quiet || parser.quiet,
        cmds: parser.cmds,
    };

    let mut status = 0;
    if inplace {
        if operands.is_empty() {
            ctx.warn(b"no input files while in place editing");
            return 1;
        }
        for operand in operands {
            if !edit_in_place(ctx, &prog, operand) {
                status = 1;
            }
        }
        return status;
    }

    let stdin: &[&[u8]] = &[b"-"];
    let inputs = if operands.is_empty() { stdin } else { operands };
    let mut source = Source::new(inputs);
    let mut out = Emit {
        file: None,
        failed: false,
        owed: false,
    };
    let mut state = State::new(prog.cmds.len());
    stream(ctx, &prog, &mut state, &mut source, &mut out);
    if source.failed || out.failed {
        status = 1;
    }
    status
}

/// The edit lands by `rename(2)` over the original, so an interrupted run
/// leaves the input intact rather than half-rewritten.
fn edit_in_place(ctx: &mut Ctx, prog: &Prog, operand: &[u8]) -> bool {
    let Some(path) = as_str(ctx, operand) else {
        return false;
    };
    // Exclusive: `File::create` on a fixed sibling name follows a symlink
    // already sitting there and truncates its target.
    let temp = format!("{path}.sed.tmp");
    let file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
    {
        Ok(file) => file,
        Err(e) => {
            ctx.warn_io(temp.as_bytes(), &e);
            return false;
        }
    };
    let mut out = Emit {
        file: Some(BufWriter::new(file)),
        failed: false,
        owed: false,
    };
    let only = [operand];
    let mut source = Source::new(&only);
    let mut state = State::new(prog.cmds.len());
    stream(ctx, prog, &mut state, &mut source, &mut out);
    let ok = out.finish() && !source.failed;
    if !ok {
        let _ = std::fs::remove_file(&temp);
        return false;
    }
    match std::fs::rename(&temp, path) {
        Ok(()) => true,
        Err(e) => {
            ctx.warn_io(operand, &e);
            let _ = std::fs::remove_file(&temp);
            false
        }
    }
}

fn stream(ctx: &mut Ctx, prog: &Prog, state: &mut State, source: &mut Source, out: &mut Emit) {
    loop {
        let Some((line, terminated)) = source.next(ctx) else {
            break;
        };
        state.space = line;
        state.terminated = terminated;
        state.number += 1;
        let deleted = cycle(ctx, prog, state, source, out);
        if !prog.quiet && !deleted {
            print_space(ctx, state, out);
        }
        flush_appends(ctx, state, out);
        if state.quit || out.failed {
            break;
        }
    }
}

struct State {
    space: Vec<u8>,
    terminated: bool,
    number: u64,
    quit: bool,
    active: Vec<bool>,
    appends: Vec<Vec<u8>>,
}

impl State {
    fn new(commands: usize) -> State {
        State {
            space: Vec::new(),
            terminated: true,
            number: 0,
            quit: false,
            active: vec![false; commands],
            appends: Vec::new(),
        }
    }
}

/// A line that arrived without its `\n` is written without one, and the
/// newline is owed to whatever output comes next — so only the very last line
/// of the stream loses it, which is how the absence round-trips.
fn print_space(ctx: &mut Ctx, state: &State, out: &mut Emit) {
    out.write(ctx, &state.space);
    if state.terminated {
        out.write(ctx, b"\n");
    } else {
        out.owe_newline();
    }
}

fn flush_appends(ctx: &mut Ctx, state: &mut State, out: &mut Emit) {
    for text in state.appends.split_off(0) {
        out.write(ctx, &text);
        out.write(ctx, b"\n");
    }
}

/// Runs the script once over the pattern space. `true` means the cycle ended
/// without its automatic print.
fn cycle(
    ctx: &mut Ctx,
    prog: &Prog,
    state: &mut State,
    source: &mut Source,
    out: &mut Emit,
) -> bool {
    let mut pc = 0usize;
    while pc < prog.cmds.len() {
        let last = source.at_last(ctx);
        let (on, ended) = select(prog, state, pc, last);
        match &prog.cmds[pc].kind {
            Kind::BlockStart(end) => {
                if !on {
                    pc = *end;
                }
                pc += 1;
                continue;
            }
            Kind::BlockEnd => {
                pc += 1;
                continue;
            }
            _ => {}
        }
        if !on {
            pc += 1;
            continue;
        }
        match &prog.cmds[pc].kind {
            Kind::Subst(subst) => {
                if let Some(replaced) = substitute(subst, &state.space) {
                    state.space = replaced;
                    if subst.print {
                        print_space(ctx, state, out);
                    }
                }
            }
            Kind::Delete => return true,
            Kind::Print => print_space(ctx, state, out),
            Kind::PrintFirst => {
                let end = state
                    .space
                    .iter()
                    .position(|&b| b == b'\n')
                    .unwrap_or(state.space.len());
                out.write(ctx, &state.space[..end]);
                out.write(ctx, b"\n");
            }
            Kind::Next => {
                if !prog.quiet {
                    print_space(ctx, state, out);
                }
                flush_appends(ctx, state, out);
                match source.next(ctx) {
                    Some((line, terminated)) => {
                        state.space = line;
                        state.terminated = terminated;
                        state.number += 1;
                    }
                    None => {
                        state.quit = true;
                        return true;
                    }
                }
            }
            Kind::AppendNext => {
                flush_appends(ctx, state, out);
                match source.next(ctx) {
                    Some((line, terminated)) => {
                        state.space.push(b'\n');
                        state.space.extend_from_slice(&line);
                        state.terminated = terminated;
                        state.number += 1;
                    }
                    None => {
                        // GNU prints the pattern space when `N` hits the end of
                        // input; POSIX quits silently, and every script here
                        // was written against the former.
                        state.quit = true;
                        return false;
                    }
                }
            }
            Kind::Quit => {
                state.quit = true;
                return false;
            }
            Kind::LineNumber => {
                let text = format!("{}\n", state.number);
                out.write(ctx, text.as_bytes());
            }
            Kind::Translate(table) => {
                for byte in state.space.iter_mut() {
                    *byte = table[*byte as usize];
                }
            }
            Kind::Append(text) => state.appends.push(text.clone()),
            Kind::Insert(text) => {
                out.write(ctx, text);
                out.write(ctx, b"\n");
            }
            Kind::Change(text) => {
                if ended {
                    out.write(ctx, text);
                    out.write(ctx, b"\n");
                }
                return true;
            }
            Kind::BlockStart(_) | Kind::BlockEnd => {}
        }
        if out.failed {
            return true;
        }
        pc += 1;
    }
    false
}

/// Whether the command at `index` applies, and whether this line is the last
/// of its range — which is where `c` prints its text.
fn select(prog: &Prog, state: &mut State, index: usize, last: bool) -> (bool, bool) {
    let cmd = &prog.cmds[index];
    let (on, ended) = match &cmd.addr {
        Addr::All => (true, true),
        Addr::One(bound) => (hit(bound, state, last), true),
        Addr::Range(from, to) => {
            if state.active[index] {
                let done = match to {
                    Bound::Line(n) => state.number >= *n,
                    Bound::Last => last,
                    Bound::Re(re) => re.matches(&state.space),
                };
                if done {
                    state.active[index] = false;
                }
                (true, done)
            } else if hit(from, state, last) {
                // A numeric end at or before the start makes the range one line.
                let single = matches!(to, Bound::Line(n) if *n <= state.number);
                state.active[index] = !single;
                (true, single)
            } else {
                (false, false)
            }
        }
    };
    (on != cmd.negate, ended)
}

fn hit(bound: &Bound, state: &State, last: bool) -> bool {
    match bound {
        Bound::Line(n) => state.number == *n,
        Bound::Last => last,
        Bound::Re(re) => re.matches(&state.space),
    }
}

fn substitute(subst: &Subst, space: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(space.len());
    let mut at = 0usize;
    let mut seen = 0u32;
    let mut changed = false;
    while at <= space.len() {
        let Some(found) = subst.re.find(space, at) else {
            break;
        };
        seen += 1;
        let wanted = if subst.global {
            seen >= subst.occurrence
        } else {
            seen == subst.occurrence
        };
        if wanted {
            out.extend_from_slice(&space[at..found.start]);
            expand(&mut out, &subst.rep, space, &found);
            changed = true;
        } else {
            out.extend_from_slice(&space[at..found.end]);
        }
        if found.end == found.start {
            if found.start < space.len() {
                out.push(space[found.start]);
            }
            at = found.start + 1;
        } else {
            at = found.end;
        }
        if !subst.global && seen >= subst.occurrence {
            break;
        }
    }
    if !changed {
        return None;
    }
    if at < space.len() {
        out.extend_from_slice(&space[at..]);
    }
    Some(out)
}

fn expand(out: &mut Vec<u8>, rep: &[u8], space: &[u8], found: &Match) {
    let mut i = 0usize;
    while i < rep.len() {
        let byte = rep[i];
        if byte == b'&' {
            out.extend_from_slice(&space[found.start..found.end]);
            i += 1;
            continue;
        }
        if byte != b'\\' || i + 1 >= rep.len() {
            out.push(byte);
            i += 1;
            continue;
        }
        let next = rep[i + 1];
        i += 2;
        match next {
            b'0' => out.extend_from_slice(&space[found.start..found.end]),
            b'1'..=b'9' => {
                if let Some((start, end)) = found.groups[(next - b'1') as usize] {
                    out.extend_from_slice(&space[start..end]);
                }
            }
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            other => out.push(other),
        }
    }
}

struct Emit {
    file: Option<BufWriter<File>>,
    failed: bool,
    owed: bool,
}

impl Emit {
    fn write(&mut self, ctx: &mut Ctx, bytes: &[u8]) {
        if self.owed {
            self.owed = false;
            self.raw(ctx, b"\n");
        }
        self.raw(ctx, bytes);
    }

    fn owe_newline(&mut self) {
        self.owed = true;
    }

    fn raw(&mut self, ctx: &mut Ctx, bytes: &[u8]) {
        match &mut self.file {
            Some(file) => {
                if file.write_all(bytes).is_err() {
                    self.failed = true;
                }
            }
            None => ctx.out.write(bytes),
        }
    }

    fn finish(&mut self) -> bool {
        match &mut self.file {
            Some(file) => file.flush().is_ok() && !self.failed,
            None => !self.failed,
        }
    }
}

/// The input as one stream with a one-line lookahead, which is what `$` needs
/// to know before the script runs on the line before it.
struct Source<'a> {
    files: &'a [&'a [u8]],
    index: usize,
    name: Vec<u8>,
    reader: Option<BufReader<super::input::Input>>,
    pending: Option<(Vec<u8>, bool)>,
    failed: bool,
}

impl<'a> Source<'a> {
    fn new(files: &'a [&'a [u8]]) -> Source<'a> {
        Source {
            files,
            index: 0,
            name: Vec::new(),
            reader: None,
            pending: None,
            failed: false,
        }
    }

    fn next(&mut self, ctx: &mut Ctx) -> Option<(Vec<u8>, bool)> {
        if let Some(line) = self.pending.take() {
            return Some(line);
        }
        self.read(ctx)
    }

    fn at_last(&mut self, ctx: &mut Ctx) -> bool {
        if self.pending.is_none() {
            self.pending = self.read(ctx);
        }
        self.pending.is_none()
    }

    fn read(&mut self, ctx: &mut Ctx) -> Option<(Vec<u8>, bool)> {
        loop {
            if self.reader.is_none() {
                let operand = *self.files.get(self.index)?;
                self.index += 1;
                match open(ctx, operand) {
                    Some(input) => {
                        self.name = operand.to_vec();
                        self.reader = Some(buffered(input));
                    }
                    None => {
                        self.failed = true;
                        continue;
                    }
                }
            }
            let reader = self.reader.as_mut()?;
            let mut buf = Vec::new();
            match read_line(reader, &mut buf) {
                Ok(0) => self.reader = None,
                Ok(_) => {
                    let terminated = buf.last() == Some(&b'\n');
                    if terminated {
                        buf.pop();
                    }
                    return Some((buf, terminated));
                }
                Err(e) => {
                    let message = io_message(&e);
                    let name = self.name.clone();
                    ctx.warn_at(&name, message.as_bytes());
                    self.failed = true;
                    self.reader = None;
                }
            }
        }
    }
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    ere: bool,
    cmds: Vec<Cmd>,
    open_blocks: Vec<usize>,
    quiet: bool,
    last_re: Vec<u8>,
}

impl<'a> Parser<'a> {
    fn parse(&mut self) -> Result<(), Fault> {
        // POSIX: `#n` as the script's first two bytes suppresses the default
        // output, whatever else that comment line says.
        if self.src.starts_with(b"#n") {
            self.quiet = true;
            while self.peek().is_some() && self.peek() != Some(b'\n') {
                self.pos += 1;
            }
        }
        loop {
            self.skip_separators();
            if self.pos >= self.src.len() {
                break;
            }
            if self.peek() == Some(b'#') {
                while self.peek().is_some() && self.peek() != Some(b'\n') {
                    self.pos += 1;
                }
                continue;
            }
            let addr = self.address()?;
            let mut negate = false;
            while self.peek() == Some(b'!') {
                negate = !negate;
                self.pos += 1;
            }
            self.skip_blanks();
            let Some(letter) = self.bump() else {
                return Err(Fault::msg("missing command"));
            };
            let kind = match letter {
                b'{' => {
                    self.open_blocks.push(self.cmds.len());
                    Kind::BlockStart(0)
                }
                b'}' => {
                    let Some(start) = self.open_blocks.pop() else {
                        return Err(Fault::at(b'}', "unexpected `}'"));
                    };
                    let end = self.cmds.len();
                    self.cmds[start].kind = Kind::BlockStart(end);
                    Kind::BlockEnd
                }
                b's' => Kind::Subst(self.subst()?),
                b'y' => Kind::Translate(self.translate()?),
                b'd' => Kind::Delete,
                b'p' => Kind::Print,
                b'P' => Kind::PrintFirst,
                b'n' => Kind::Next,
                b'N' => Kind::AppendNext,
                b'q' => Kind::Quit,
                b'=' => Kind::LineNumber,
                b'a' => Kind::Append(self.text()),
                b'i' => Kind::Insert(self.text()),
                b'c' => Kind::Change(self.text()),
                other => return Err(Fault::at(other, "unknown command")),
            };
            self.cmds.push(Cmd { addr, negate, kind });
            if !matches!(letter, b'{' | b'}') {
                self.end_of_command(letter)?;
            }
        }
        if !self.open_blocks.is_empty() {
            return Err(Fault::at(b'{', "unmatched `{'"));
        }
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(b' ') | Some(b'\t')) {
            self.pos += 1;
        }
    }

    fn skip_separators(&mut self) {
        while matches!(
            self.peek(),
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b';')
        ) {
            self.pos += 1;
        }
    }

    fn end_of_command(&mut self, letter: u8) -> Result<(), Fault> {
        self.skip_blanks();
        match self.peek() {
            None | Some(b';') | Some(b'\n') | Some(b'}') | Some(b'{') => Ok(()),
            Some(_) => Err(Fault::at(letter, "extra characters after command")),
        }
    }

    fn number(&mut self) -> u64 {
        let mut value = 0u64;
        while let Some(byte) = self.peek() {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value
                .saturating_mul(10)
                .saturating_add((byte - b'0') as u64);
            self.pos += 1;
        }
        value
    }

    fn address(&mut self) -> Result<Addr, Fault> {
        self.skip_blanks();
        let Some(first) = self.bound()? else {
            return Ok(Addr::All);
        };
        self.skip_blanks();
        if !self.eat(b',') {
            return Ok(Addr::One(first));
        }
        self.skip_blanks();
        let Some(second) = self.bound()? else {
            return Err(Fault::msg("expected an address after `,'"));
        };
        Ok(Addr::Range(first, second))
    }

    fn bound(&mut self) -> Result<Option<Bound>, Fault> {
        match self.peek() {
            Some(b'$') => {
                self.pos += 1;
                Ok(Some(Bound::Last))
            }
            Some(byte) if byte.is_ascii_digit() => Ok(Some(Bound::Line(self.number()))),
            Some(b'/') => {
                self.pos += 1;
                Ok(Some(Bound::Re(self.address_regex(b'/')?)))
            }
            Some(b'\\') => {
                self.pos += 1;
                let Some(delim) = self.bump() else {
                    return Err(Fault::msg("unterminated address"));
                };
                Ok(Some(Bound::Re(self.address_regex(delim)?)))
            }
            _ => Ok(None),
        }
    }

    fn address_regex(&mut self, delim: u8) -> Result<Regex, Fault> {
        let body = self.until(delim, "unterminated address")?;
        let icase = self.eat(b'I');
        self.compile(&body, icase)
    }

    /// An empty regex reuses the previous one, as POSIX asks — textually,
    /// which is what makes `/re/s//x/` work without a runtime last-match slot.
    fn compile(&mut self, body: &[u8], icase: bool) -> Result<Regex, Fault> {
        let source = if body.is_empty() {
            if self.last_re.is_empty() {
                return Err(Fault::msg("no previous regular expression"));
            }
            self.last_re.clone()
        } else {
            self.last_re = body.to_vec();
            body.to_vec()
        };
        Regex::compile(&source, self.ere, icase).map_err(|e| Fault::msg(e.message()))
    }

    /// Read up to an unescaped `delim`, turning `\<delim>` into a literal and
    /// leaving every other escape for the regex or the replacement to read.
    fn until(&mut self, delim: u8, unterminated: &'static str) -> Result<Vec<u8>, Fault> {
        let mut out = Vec::new();
        loop {
            let Some(byte) = self.bump() else {
                return Err(Fault::msg(unterminated));
            };
            if byte == delim {
                return Ok(out);
            }
            if byte == b'\n' {
                return Err(Fault::msg(unterminated));
            }
            if byte != b'\\' {
                out.push(byte);
                continue;
            }
            match self.bump() {
                None => return Err(Fault::msg(unterminated)),
                Some(next) if next == delim => out.push(delim),
                Some(b'\n') => out.push(b'\n'),
                Some(next) => {
                    out.push(b'\\');
                    out.push(next);
                }
            }
        }
    }

    fn subst(&mut self) -> Result<Subst, Fault> {
        let Some(delim) = self.bump() else {
            return Err(Fault::at(b's', "unterminated `s' command"));
        };
        if delim == b'\\' || delim == b'\n' {
            return Err(Fault::at(b's', "invalid delimiter"));
        }
        let pattern = self.until(delim, "unterminated `s' command")?;
        let rep = self.until(delim, "unterminated `s' command")?;
        let mut global = false;
        let mut print = false;
        let mut icase = false;
        let mut occurrence = 0u32;
        loop {
            match self.peek() {
                Some(b'g') => {
                    self.pos += 1;
                    global = true;
                }
                Some(b'p') => {
                    self.pos += 1;
                    print = true;
                }
                Some(b'i') | Some(b'I') => {
                    self.pos += 1;
                    icase = true;
                }
                Some(byte) if byte.is_ascii_digit() => {
                    occurrence = self.number().min(u32::MAX as u64) as u32;
                }
                _ => break,
            }
        }
        let re = self.compile(&pattern, icase)?;
        check_references(&rep, re.group_count())?;
        Ok(Subst {
            re,
            rep,
            global,
            occurrence: occurrence.max(1),
            print,
        })
    }

    fn translate(&mut self) -> Result<Box<[u8; 256]>, Fault> {
        let Some(delim) = self.bump() else {
            return Err(Fault::at(b'y', "unterminated `y' command"));
        };
        let from = unescape(&self.until(delim, "unterminated `y' command")?);
        let to = unescape(&self.until(delim, "unterminated `y' command")?);
        if from.len() != to.len() {
            return Err(Fault::at(b'y', "strings are not the same length"));
        }
        let mut table = Box::new([0u8; 256]);
        for (index, slot) in table.iter_mut().enumerate() {
            *slot = index as u8;
        }
        for (source, target) in from.iter().zip(to.iter()) {
            table[*source as usize] = *target;
        }
        Ok(table)
    }

    /// `a`, `i` and `c` text: the POSIX `a\` + newline form and the one-line
    /// form both end at an unescaped newline.
    fn text(&mut self) -> Vec<u8> {
        self.skip_blanks();
        if self.peek() == Some(b'\\') {
            self.pos += 1;
            self.eat(b'\n');
        } else {
            self.skip_blanks();
        }
        let mut out = Vec::new();
        loop {
            match self.bump() {
                None | Some(b'\n') => break,
                Some(b'\\') => match self.bump() {
                    None => break,
                    Some(next) => out.push(if next == b'n' { b'\n' } else { next }),
                },
                Some(byte) => out.push(byte),
            }
        }
        out
    }
}

fn check_references(rep: &[u8], groups: usize) -> Result<(), Fault> {
    let mut i = 0usize;
    while i + 1 < rep.len() {
        if rep[i] != b'\\' {
            i += 1;
            continue;
        }
        let next = rep[i + 1];
        if next.is_ascii_digit() && next != b'0' && (next - b'0') as usize > groups {
            return Err(Fault::at(
                next,
                "invalid reference on `s' command's right side",
            ));
        }
        i += 2;
    }
    Ok(())
}

fn unescape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let next = bytes[i + 1];
        i += 2;
        out.push(match next {
            b'n' => b'\n',
            b't' => b'\t',
            b'r' => b'\r',
            other => other,
        });
    }
    out
}
