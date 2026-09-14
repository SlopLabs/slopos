//! POSIX regular expressions over bytes: a BRE/ERE parser, a flat program, and
//! a backtracking VM with an explicit stack.
//!
//! `find` is leftmost-first with greedy quantifiers (the Perl rule), not POSIX
//! leftmost-longest: the alternative that matches earliest in the pattern wins
//! at a given start offset.

/// `\1`..`\9` is what POSIX guarantees, so nine is the bound the capture table
/// is sized for; a tenth group is compiled without a capture.
const NGROUP: usize = 9;
const NSLOT: usize = (NGROUP + 1) * 2;
/// One repetition slot per nesting level: the loops active at any instant form
/// a nested chain, so a loop's nesting depth names a slot nothing else holds.
const MAX_LOOPS: usize = 8;
const MAX_NEST: usize = 32;
const MAX_PROG: usize = 1 << 16;
/// Backtrack points one search may hold — ~8 MiB of `Thread`. The step budget
/// bounds time; this bounds the memory a deep search tree would take first.
const MAX_BACKTRACK: usize = 1 << 16;
/// `RE_DUP_MAX`: the interval bound POSIX requires, and the bound on how far
/// `X{m,n}` unrolls the body.
const MAX_DUP: u32 = 255;
const UNSET: u32 = u32::MAX;

type Bitmap = [u64; 4];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Paren,
    Bracket,
    Brace,
    Interval,
    Backref,
    Trailing,
    Repeat,
    Range,
    Nest,
    Size,
}

impl Error {
    pub fn message(&self) -> &'static str {
        match self {
            Error::Paren => "unmatched ( or \\(",
            Error::Bracket => "unterminated bracket expression",
            Error::Brace => "unmatched { or \\{",
            Error::Interval => "invalid repetition count",
            Error::Backref => "invalid back reference",
            Error::Trailing => "trailing backslash",
            Error::Repeat => "nothing to repeat",
            Error::Range => "invalid range end",
            Error::Nest => "regular expression nested too deeply",
            Error::Size => "regular expression too large",
        }
    }
}

pub struct Match {
    pub start: usize,
    pub end: usize,
    /// `\1`..`\9` as byte ranges into the searched text.
    pub groups: [Option<(usize, usize)>; NGROUP],
}

pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `grep -F` and `sed`'s literal delimiters need a substring search, not an
/// engine.
pub fn find_literal(needle: &[u8], text: &[u8], icase: bool) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > text.len() {
        return None;
    }
    let last = text.len() - needle.len();
    for i in 0..=last {
        let window = &text[i..i + needle.len()];
        let hit = if icase {
            window
                .iter()
                .zip(needle.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        } else {
            window == needle
        };
        if hit {
            return Some(i);
        }
    }
    None
}

enum Ast {
    Empty,
    Byte(u8),
    Any,
    Class(usize),
    Bol,
    Eol,
    WordStart,
    WordEnd,
    WordBoundary,
    /// Capture index `1..=9`, or `0` for a group past the ninth.
    Group(u8, Box<Ast>),
    Backref(u8),
    Cat(Vec<Ast>),
    Alt(Vec<Ast>),
    Repeat {
        node: Box<Ast>,
        min: u32,
        max: Option<u32>,
    },
}

#[derive(Clone, Copy)]
enum Inst {
    Byte(u8),
    Any,
    Class(u32),
    Bol,
    Eol,
    WordStart,
    WordEnd,
    WordBoundary,
    Save(u8),
    Mark(u8),
    Jmp(u32),
    /// Take the first target, keep the second for backtracking.
    Split(u32, u32),
    /// Loop back only when the iteration consumed input, which is what stops
    /// `\(a*\)*` from spinning on an empty body.
    Rep(u32, u32, u8),
    Backref(u8),
    Match,
}

pub struct Regex {
    prog: Vec<Inst>,
    classes: Vec<Bitmap>,
    icase: bool,
    groups: u8,
}

#[derive(Clone, Copy)]
struct Thread {
    pc: u32,
    sp: u32,
    caps: [u32; NSLOT],
    marks: [u32; MAX_LOOPS],
}

impl Regex {
    pub fn compile(pattern: &[u8], ere: bool, icase: bool) -> Result<Regex, Error> {
        Regex::build(pattern, ere, icase, false)
    }

    /// The whole text must match, as `grep -x` wants. Anchoring the tree
    /// rather than wrapping the source in `^\(`…`\)$` keeps alternation whole
    /// without shifting the pattern's own group numbers.
    pub fn compile_anchored(pattern: &[u8], ere: bool, icase: bool) -> Result<Regex, Error> {
        Regex::build(pattern, ere, icase, true)
    }

    fn build(pattern: &[u8], ere: bool, icase: bool, anchored: bool) -> Result<Regex, Error> {
        let mut parser = Parser {
            pat: pattern,
            pos: 0,
            ere,
            icase,
            classes: Vec::new(),
            groups: 0,
            depth: 0,
        };
        let mut ast = parser.parse()?;
        if anchored {
            ast = Ast::Cat(vec![Ast::Bol, ast, Ast::Eol]);
        }
        let mut prog = Vec::new();
        emit(&mut prog, &ast, 0)?;
        prog.push(Inst::Match);
        Ok(Regex {
            prog,
            classes: parser.classes,
            icase,
            groups: parser.groups.min(NGROUP as u32) as u8,
        })
    }

    /// The number of capturing groups, capped at nine — what a `sed`
    /// replacement may legally reference.
    pub fn group_count(&self) -> usize {
        self.groups as usize
    }

    pub fn matches(&self, text: &[u8]) -> bool {
        self.find(text, 0).is_some()
    }

    /// The pattern is user input, so the search is bounded in both steps and
    /// backtrack depth: an exponential pattern answers "no match".
    pub fn find(&self, text: &[u8], start: usize) -> Option<Match> {
        if start > text.len() {
            return None;
        }
        let mut budget = (text.len() as u64 + 1)
            .saturating_mul(self.prog.len() as u64)
            .saturating_mul(16)
            .saturating_add(4096);
        let mut at = start;
        loop {
            if let Some(th) = self.exec(text, at, &mut budget) {
                let mut groups = [None; NGROUP];
                for (i, slot) in groups.iter_mut().enumerate() {
                    let s = th.caps[2 * (i + 1)];
                    let e = th.caps[2 * (i + 1) + 1];
                    if s != UNSET && e != UNSET && e >= s {
                        *slot = Some((s as usize, e as usize));
                    }
                }
                return Some(Match {
                    start: th.caps[0] as usize,
                    end: th.caps[1] as usize,
                    groups,
                });
            }
            if at >= text.len() || budget == 0 {
                return None;
            }
            at += 1;
        }
    }

    fn exec(&self, text: &[u8], start: usize, budget: &mut u64) -> Option<Thread> {
        let mut stack: Vec<Thread> = Vec::new();
        let mut th = Thread {
            pc: 0,
            sp: start as u32,
            caps: [UNSET; NSLOT],
            marks: [0; MAX_LOOPS],
        };
        th.caps[0] = start as u32;
        loop {
            loop {
                if *budget == 0 {
                    return None;
                }
                *budget -= 1;
                let sp = th.sp as usize;
                match self.prog[th.pc as usize] {
                    Inst::Match => {
                        th.caps[1] = th.sp;
                        return Some(th);
                    }
                    Inst::Byte(b) => match text.get(sp) {
                        Some(&c) if self.eq(c, b) => {
                            th.sp += 1;
                            th.pc += 1;
                        }
                        _ => break,
                    },
                    Inst::Any => {
                        if sp >= text.len() {
                            break;
                        }
                        th.sp += 1;
                        th.pc += 1;
                    }
                    Inst::Class(i) => match text.get(sp) {
                        Some(&c) if in_class(&self.classes[i as usize], c) => {
                            th.sp += 1;
                            th.pc += 1;
                        }
                        _ => break,
                    },
                    Inst::Bol => {
                        if sp != 0 {
                            break;
                        }
                        th.pc += 1;
                    }
                    Inst::Eol => {
                        if sp != text.len() {
                            break;
                        }
                        th.pc += 1;
                    }
                    Inst::WordStart => {
                        let here = sp < text.len() && is_word_byte(text[sp]);
                        let before = sp > 0 && is_word_byte(text[sp - 1]);
                        if !here || before {
                            break;
                        }
                        th.pc += 1;
                    }
                    Inst::WordEnd => {
                        let here = sp < text.len() && is_word_byte(text[sp]);
                        let before = sp > 0 && is_word_byte(text[sp - 1]);
                        if !before || here {
                            break;
                        }
                        th.pc += 1;
                    }
                    Inst::WordBoundary => {
                        let here = sp < text.len() && is_word_byte(text[sp]);
                        let before = sp > 0 && is_word_byte(text[sp - 1]);
                        if here == before {
                            break;
                        }
                        th.pc += 1;
                    }
                    Inst::Save(k) => {
                        th.caps[k as usize] = th.sp;
                        th.pc += 1;
                    }
                    Inst::Mark(s) => {
                        th.marks[s as usize] = th.sp;
                        th.pc += 1;
                    }
                    Inst::Jmp(t) => th.pc = t,
                    Inst::Split(a, b) => {
                        let mut alt = th;
                        alt.pc = b;
                        if !push_alt(&mut stack, alt) {
                            return None;
                        }
                        th.pc = a;
                    }
                    Inst::Rep(body, out, slot) => {
                        if th.sp > th.marks[slot as usize] {
                            let mut alt = th;
                            alt.pc = out;
                            if !push_alt(&mut stack, alt) {
                                return None;
                            }
                            th.pc = body;
                        } else {
                            th.pc = out;
                        }
                    }
                    Inst::Backref(k) => {
                        let s = th.caps[2 * k as usize];
                        let e = th.caps[2 * k as usize + 1];
                        if s == UNSET || e == UNSET || e < s {
                            break;
                        }
                        let (s, e) = (s as usize, e as usize);
                        let len = e - s;
                        if sp + len > text.len() || !self.eq_slice(&text[sp..sp + len], &text[s..e])
                        {
                            break;
                        }
                        th.sp += len as u32;
                        th.pc += 1;
                    }
                }
            }
            match stack.pop() {
                Some(next) => th = next,
                None => return None,
            }
        }
    }
    fn eq(&self, c: u8, b: u8) -> bool {
        c == b || (self.icase && c.eq_ignore_ascii_case(&b))
    }

    fn eq_slice(&self, a: &[u8], b: &[u8]) -> bool {
        if self.icase {
            a.iter()
                .zip(b.iter())
                .all(|(x, y)| x.eq_ignore_ascii_case(y))
        } else {
            a == b
        }
    }
}

fn push_alt(stack: &mut Vec<Thread>, alt: Thread) -> bool {
    if stack.len() >= MAX_BACKTRACK {
        return false;
    }
    stack.push(alt);
    true
}

fn in_class(bits: &Bitmap, byte: u8) -> bool {
    (bits[(byte >> 6) as usize] >> (byte & 63)) & 1 == 1
}

fn set_bit(bits: &mut Bitmap, byte: u8) {
    bits[(byte >> 6) as usize] |= 1u64 << (byte & 63);
}

fn set_byte(bits: &mut Bitmap, byte: u8, icase: bool) {
    set_bit(bits, byte);
    if icase {
        if byte.is_ascii_lowercase() {
            set_bit(bits, byte.to_ascii_uppercase());
        } else if byte.is_ascii_uppercase() {
            set_bit(bits, byte.to_ascii_lowercase());
        }
    }
}

fn emit(prog: &mut Vec<Inst>, ast: &Ast, loopd: usize) -> Result<(), Error> {
    if prog.len() >= MAX_PROG {
        return Err(Error::Size);
    }
    match ast {
        Ast::Empty => {}
        Ast::Byte(b) => prog.push(Inst::Byte(*b)),
        Ast::Any => prog.push(Inst::Any),
        Ast::Class(i) => prog.push(Inst::Class(*i as u32)),
        Ast::Bol => prog.push(Inst::Bol),
        Ast::Eol => prog.push(Inst::Eol),
        Ast::WordStart => prog.push(Inst::WordStart),
        Ast::WordEnd => prog.push(Inst::WordEnd),
        Ast::WordBoundary => prog.push(Inst::WordBoundary),
        Ast::Backref(k) => prog.push(Inst::Backref(*k)),
        Ast::Group(k, body) => {
            if *k > 0 {
                prog.push(Inst::Save(2 * k));
            }
            emit(prog, body, loopd)?;
            if *k > 0 {
                prog.push(Inst::Save(2 * k + 1));
            }
        }
        Ast::Cat(items) => {
            for item in items {
                emit(prog, item, loopd)?;
            }
        }
        Ast::Alt(branches) => {
            let mut jumps = Vec::new();
            let last = branches.len() - 1;
            for (i, branch) in branches.iter().enumerate() {
                if i == last {
                    emit(prog, branch, loopd)?;
                    continue;
                }
                let split = prog.len();
                prog.push(Inst::Split(0, 0));
                emit(prog, branch, loopd)?;
                jumps.push(prog.len());
                prog.push(Inst::Jmp(0));
                let next = prog.len() as u32;
                prog[split] = Inst::Split(split as u32 + 1, next);
            }
            let end = prog.len() as u32;
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Ast::Repeat { node, min, max } => {
            for _ in 0..*min {
                emit(prog, node, loopd)?;
            }
            match max {
                None => emit_star(prog, node, loopd)?,
                Some(n) => {
                    for _ in *min..*n {
                        emit_optional(prog, node, loopd)?;
                    }
                }
            }
        }
    }
    if prog.len() >= MAX_PROG {
        return Err(Error::Size);
    }
    Ok(())
}

fn emit_star(prog: &mut Vec<Inst>, node: &Ast, loopd: usize) -> Result<(), Error> {
    if loopd >= MAX_LOOPS {
        return Err(Error::Nest);
    }
    let split = prog.len();
    prog.push(Inst::Split(0, 0));
    let body = prog.len();
    prog.push(Inst::Mark(loopd as u8));
    emit(prog, node, loopd + 1)?;
    let rep = prog.len();
    prog.push(Inst::Rep(body as u32, rep as u32 + 1, loopd as u8));
    prog[split] = Inst::Split(body as u32, rep as u32 + 1);
    Ok(())
}

fn emit_optional(prog: &mut Vec<Inst>, node: &Ast, loopd: usize) -> Result<(), Error> {
    let split = prog.len();
    prog.push(Inst::Split(0, 0));
    emit(prog, node, loopd)?;
    let end = prog.len() as u32;
    prog[split] = Inst::Split(split as u32 + 1, end);
    Ok(())
}

struct Parser<'a> {
    pat: &'a [u8],
    pos: usize,
    ere: bool,
    icase: bool,
    classes: Vec<Bitmap>,
    groups: u32,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn parse(&mut self) -> Result<Ast, Error> {
        let ast = self.alt()?;
        if self.pos < self.pat.len() {
            return Err(Error::Paren);
        }
        Ok(ast)
    }

    fn peek(&self) -> Option<u8> {
        self.pat.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.pat.get(self.pos + off).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            return true;
        }
        false
    }

    fn at_pair(&self, a: u8, b: u8) -> bool {
        self.peek() == Some(a) && self.peek_at(1) == Some(b)
    }

    fn eat_pair(&mut self, a: u8, b: u8) -> bool {
        if self.at_pair(a, b) {
            self.pos += 2;
            return true;
        }
        false
    }

    fn at_alt(&self) -> bool {
        if self.ere {
            self.peek() == Some(b'|')
        } else {
            self.at_pair(b'\\', b'|')
        }
    }

    fn at_close(&self) -> bool {
        if self.ere {
            self.peek() == Some(b')')
        } else {
            self.at_pair(b'\\', b')')
        }
    }

    fn at_seq_end(&self) -> bool {
        self.pos >= self.pat.len() || self.at_alt() || (self.depth > 0 && self.at_close())
    }

    fn alt(&mut self) -> Result<Ast, Error> {
        let mut branches = vec![self.seq()?];
        while self.at_alt() {
            self.pos += if self.ere { 1 } else { 2 };
            branches.push(self.seq()?);
        }
        if branches.len() == 1 {
            return Ok(branches.pop().unwrap());
        }
        Ok(Ast::Alt(branches))
    }

    fn seq(&mut self) -> Result<Ast, Error> {
        let mut items: Vec<Ast> = Vec::new();
        while !self.at_seq_end() {
            let first = items.is_empty();
            let atom = self.atom(first)?;
            items.push(self.quantified(atom)?);
        }
        if items.is_empty() {
            return Ok(Ast::Empty);
        }
        if items.len() == 1 {
            return Ok(items.pop().unwrap());
        }
        Ok(Ast::Cat(items))
    }

    fn quantified(&mut self, atom: Ast) -> Result<Ast, Error> {
        let mut node = atom;
        let mut stacked = 0usize;
        while let Some((min, max)) = self.quant()? {
            stacked += 1;
            if stacked > MAX_LOOPS {
                return Err(Error::Nest);
            }
            node = Ast::Repeat {
                node: Box::new(node),
                min,
                max,
            };
        }
        Ok(node)
    }

    fn quant(&mut self) -> Result<Option<(u32, Option<u32>)>, Error> {
        match self.peek() {
            Some(b'*') => {
                self.pos += 1;
                Ok(Some((0, None)))
            }
            Some(b'+') if self.ere => {
                self.pos += 1;
                Ok(Some((1, None)))
            }
            Some(b'?') if self.ere => {
                self.pos += 1;
                Ok(Some((0, Some(1))))
            }
            Some(b'{') if self.ere && self.digit_at(1) => {
                self.pos += 1;
                self.interval().map(Some)
            }
            Some(b'\\') if !self.ere => match self.peek_at(1) {
                Some(b'+') => {
                    self.pos += 2;
                    Ok(Some((1, None)))
                }
                Some(b'?') => {
                    self.pos += 2;
                    Ok(Some((0, Some(1))))
                }
                Some(b'{') => {
                    self.pos += 2;
                    self.interval().map(Some)
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn digit_at(&self, off: usize) -> bool {
        self.peek_at(off).is_some_and(|b| b.is_ascii_digit())
    }

    fn interval(&mut self) -> Result<(u32, Option<u32>), Error> {
        let min = self.number()?;
        let closing = if self.ere {
            self.peek() == Some(b'}')
        } else {
            self.at_pair(b'\\', b'}')
        };
        let max = if self.eat(b',') {
            let done = if self.ere {
                self.peek() == Some(b'}')
            } else {
                self.at_pair(b'\\', b'}')
            };
            if done { None } else { Some(self.number()?) }
        } else {
            if !closing {
                return Err(Error::Interval);
            }
            Some(min)
        };
        let closed = if self.ere {
            self.eat(b'}')
        } else {
            self.eat_pair(b'\\', b'}')
        };
        if !closed {
            return Err(Error::Brace);
        }
        if min > MAX_DUP || max.is_some_and(|n| n > MAX_DUP || n < min) {
            return Err(Error::Interval);
        }
        Ok((min, max))
    }

    fn number(&mut self) -> Result<u32, Error> {
        let start = self.pos;
        let mut value: u32 = 0;
        while self.digit_at(0) {
            value = value
                .saturating_mul(10)
                .saturating_add((self.pat[self.pos] - b'0') as u32);
            self.pos += 1;
        }
        if self.pos == start {
            return Err(Error::Interval);
        }
        Ok(value)
    }

    fn atom(&mut self, first: bool) -> Result<Ast, Error> {
        let Some(c) = self.peek() else {
            return Err(Error::Trailing);
        };
        match c {
            b'.' => {
                self.pos += 1;
                Ok(Ast::Any)
            }
            b'[' => {
                let index = self.bracket()?;
                Ok(Ast::Class(index))
            }
            b'^' => {
                self.pos += 1;
                // BRE: `^` anchors only where a sequence starts; elsewhere it
                // is an ordinary character.
                if self.ere || first {
                    Ok(Ast::Bol)
                } else {
                    Ok(Ast::Byte(b'^'))
                }
            }
            b'$' => {
                self.pos += 1;
                if self.ere || self.at_seq_end() {
                    Ok(Ast::Eol)
                } else {
                    Ok(Ast::Byte(b'$'))
                }
            }
            b'*' => {
                self.pos += 1;
                if self.ere {
                    Err(Error::Repeat)
                } else {
                    Ok(Ast::Byte(b'*'))
                }
            }
            b'(' if self.ere => {
                self.pos += 1;
                self.group()
            }
            b')' if self.ere => Err(Error::Paren),
            b'+' | b'?' if self.ere => Err(Error::Repeat),
            b'\\' => self.escape(),
            _ => {
                self.pos += 1;
                Ok(Ast::Byte(c))
            }
        }
    }

    fn escape(&mut self) -> Result<Ast, Error> {
        let Some(c) = self.peek_at(1) else {
            return Err(Error::Trailing);
        };
        match c {
            b'(' if !self.ere => {
                self.pos += 2;
                self.group()
            }
            b')' if !self.ere => Err(Error::Paren),
            b'{' if !self.ere => Err(Error::Repeat),
            b'1'..=b'9' => {
                self.pos += 2;
                let k = c - b'0';
                if k as u32 > self.groups {
                    return Err(Error::Backref);
                }
                Ok(Ast::Backref(k))
            }
            b'n' => {
                self.pos += 2;
                Ok(Ast::Byte(b'\n'))
            }
            b't' => {
                self.pos += 2;
                Ok(Ast::Byte(b'\t'))
            }
            b'r' => {
                self.pos += 2;
                Ok(Ast::Byte(b'\r'))
            }
            b'<' => {
                self.pos += 2;
                Ok(Ast::WordStart)
            }
            b'>' => {
                self.pos += 2;
                Ok(Ast::WordEnd)
            }
            b'b' => {
                self.pos += 2;
                Ok(Ast::WordBoundary)
            }
            b'w' | b'W' | b's' | b'S' => {
                self.pos += 2;
                Ok(Ast::Class(self.shorthand(c)))
            }
            _ => {
                self.pos += 2;
                Ok(Ast::Byte(c))
            }
        }
    }

    fn group(&mut self) -> Result<Ast, Error> {
        if self.depth >= MAX_NEST {
            return Err(Error::Nest);
        }
        self.groups = self.groups.saturating_add(1);
        let index = if self.groups <= NGROUP as u32 {
            self.groups as u8
        } else {
            0
        };
        self.depth += 1;
        let body = self.alt()?;
        self.depth -= 1;
        let closed = if self.ere {
            self.eat(b')')
        } else {
            self.eat_pair(b'\\', b')')
        };
        if !closed {
            return Err(Error::Paren);
        }
        Ok(Ast::Group(index, Box::new(body)))
    }

    fn shorthand(&mut self, c: u8) -> usize {
        let mut bits: Bitmap = [0; 4];
        let word = c == b'w' || c == b'W';
        for byte in 0u8..=255 {
            let member = if word {
                is_word_byte(byte)
            } else {
                matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
            };
            if member {
                set_bit(&mut bits, byte);
            }
        }
        if c.is_ascii_uppercase() {
            for word in bits.iter_mut() {
                *word = !*word;
            }
        }
        self.classes.push(bits);
        self.classes.len() - 1
    }

    /// A backslash inside a bracket expression escapes here, which POSIX
    /// leaves as an ordinary backslash — `[\t]` is worth more than `[\]`.
    fn bracket(&mut self) -> Result<usize, Error> {
        self.pos += 1;
        let negated = self.eat(b'^');
        let mut bits: Bitmap = [0; 4];
        let mut first = true;
        loop {
            let Some(c) = self.bump() else {
                return Err(Error::Bracket);
            };
            if c == b']' && !first {
                break;
            }
            first = false;
            if c == b'[' && self.peek() == Some(b':') {
                let pat = self.pat;
                self.pos += 1;
                let start = self.pos;
                while self.peek().is_some() && self.peek() != Some(b':') {
                    self.pos += 1;
                }
                let name = &pat[start..self.pos];
                if !self.eat_pair(b':', b']') {
                    return Err(Error::Bracket);
                }
                add_named(&mut bits, name, self.icase)?;
                continue;
            }
            let mut lo = c;
            if c == b'\\' {
                let Some(e) = self.bump() else {
                    return Err(Error::Bracket);
                };
                lo = unescape_byte(e);
            }
            let ranged = self.peek() == Some(b'-')
                && self.peek_at(1).is_some()
                && self.peek_at(1) != Some(b']');
            if !ranged {
                set_byte(&mut bits, lo, self.icase);
                continue;
            }
            self.pos += 1;
            let Some(mut hi) = self.bump() else {
                return Err(Error::Bracket);
            };
            if hi == b'\\' {
                let Some(e) = self.bump() else {
                    return Err(Error::Bracket);
                };
                hi = unescape_byte(e);
            }
            if hi < lo {
                return Err(Error::Range);
            }
            let mut byte = lo;
            loop {
                set_byte(&mut bits, byte, self.icase);
                if byte == hi {
                    break;
                }
                byte += 1;
            }
        }
        if negated {
            for word in bits.iter_mut() {
                *word = !*word;
            }
        }
        self.classes.push(bits);
        Ok(self.classes.len() - 1)
    }
}

fn unescape_byte(byte: u8) -> u8 {
    match byte {
        b'n' => b'\n',
        b't' => b'\t',
        b'r' => b'\r',
        other => other,
    }
}

fn add_named(bits: &mut Bitmap, name: &[u8], icase: bool) -> Result<(), Error> {
    let member: fn(u8) -> bool = match name {
        b"alpha" => |b| b.is_ascii_alphabetic(),
        b"digit" => |b| b.is_ascii_digit(),
        b"alnum" => |b| b.is_ascii_alphanumeric(),
        b"upper" => |b| b.is_ascii_uppercase(),
        b"lower" => |b| b.is_ascii_lowercase(),
        b"space" => |b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b"blank" => |b| b == b' ' || b == b'\t',
        b"punct" => |b| b.is_ascii_punctuation(),
        b"print" => |b| b.is_ascii_graphic() || b == b' ',
        b"graph" => |b| b.is_ascii_graphic(),
        b"cntrl" => |b| b.is_ascii_control(),
        b"xdigit" => |b| b.is_ascii_hexdigit(),
        _ => return Err(Error::Bracket),
    };
    for byte in 0u8..=255 {
        if member(byte) {
            set_byte(bits, byte, icase);
        }
    }
    Ok(())
}
