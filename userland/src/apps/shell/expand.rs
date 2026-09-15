//! Word expansion (POSIX §2.6).
//!
//! One left-to-right pass does parameter, command and arithmetic expansion
//! together, recording per byte whether it was quoted and whether it came from
//! an expansion; splitting and pathname expansion then run over that record.
//! Quote removal needs no pass of its own, because the walk that decides a
//! byte is quoted is the walk that emits it.
//!
//! An expansion's result is never rescanned for further expansions, so
//! `x='$y'` expands to the two bytes `$y` — but it *is* subject to splitting
//! and globbing when unquoted, which is what [`Q_SPLIT`] records.

use slopos_shell_core::qbuf::{Q_QUOTED, Q_SPLIT, QBuf};
use slopos_shell_core::{arith, ast::Word, fields, param, pattern};

use std::sync::atomic::{AtomicBool, Ordering};

use super::display::shell_error_named;
use super::{args, env, funcs, glob};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpandError {
    Syntax(&'static str),
    /// `${name:?word}`, or `set -u` on an unset name.
    Unset {
        name: Vec<u8>,
        message: Vec<u8>,
    },
    Arith(&'static str),
    /// A command substitution could not be parsed or run.
    Substitution,
}

impl ExpandError {
    /// Report on stderr in the POSIX shape and return the status the failing
    /// command should carry.
    pub fn report(&self) -> i32 {
        match self {
            ExpandError::Syntax(msg) => {
                shell_error_named(b"bad substitution", msg.as_bytes());
                super::exec::STATUS_SYNTAX_ERROR
            }
            ExpandError::Unset { name, message } => {
                shell_error_named(name, message);
                1
            }
            ExpandError::Arith(msg) => {
                shell_error_named(b"arithmetic", msg.as_bytes());
                super::exec::STATUS_SYNTAX_ERROR
            }
            ExpandError::Substitution => {
                shell_error_named(b"command substitution", b"failed");
                1
            }
        }
    }
}

/// Expand a word as an argument list needs it: split, then pathname-expanded.
pub fn fields(word: &Word) -> Result<Vec<Vec<u8>>, ExpandError> {
    let expander = run(word)?;
    let keep_empty = expander.keep_empty();
    let parts = expander.into_parts();
    let ifs = ifs_value();
    let mut out = Vec::new();
    for field in fields::split(&parts, &ifs, keep_empty) {
        if funcs::noglob() {
            out.push(field.into_bytes());
            continue;
        }
        match glob::expand(&field) {
            Some(matches) => out.extend(matches),
            None => out.push(field.into_bytes()),
        }
    }
    Ok(out)
}

/// Expand every word of an argument list.
pub fn word_list(words: &[Word]) -> Result<Vec<Vec<u8>>, ExpandError> {
    let mut out = Vec::new();
    for word in words {
        out.extend(fields(word)?);
    }
    Ok(out)
}

/// Expand to one string, with no splitting or globbing — what an assignment's
/// value, a redirection's operand and a `case` subject each need.
pub fn single(word: &Word) -> Result<Vec<u8>, ExpandError> {
    let expander = run(word)?;
    Ok(expander.concat().into_bytes())
}

/// Expand keeping the record of which bytes are live pattern characters: a
/// `case` pattern, or the operand of `${x%word}`.
pub fn as_pattern(word: &Word) -> Result<QBuf, ExpandError> {
    Ok(run(word)?.concat())
}

/// Expand a here-document body.
///
/// A backslash there is special only before `$`, `` ` `` and `\`, as inside
/// double quotes, so the body is walked as quoted text.
pub fn here_body(word: &Word) -> Result<Vec<u8>, ExpandError> {
    if word.literal {
        return Ok(word.text.clone());
    }
    let mut expander = Expander::new();
    expander.walk(&word.text, true)?;
    Ok(expander.concat().into_bytes())
}

fn run(word: &Word) -> Result<Expander, ExpandError> {
    let mut expander = Expander::new();
    if word.literal {
        expander.cur.extend(&word.text, Q_QUOTED);
        expander.pushed += word.text.len();
        expander.quoted_region = true;
        return Ok(expander);
    }
    expander.walk(&word.text, false)?;
    Ok(expander)
}

fn ifs_value() -> Vec<u8> {
    env::get(b"IFS").unwrap_or_else(|| fields::DEFAULT_IFS.to_vec())
}

struct Expander {
    /// Completed hard segments; `"$@"` puts one boundary per positional.
    parts: Vec<QBuf>,
    cur: QBuf,
    /// Bytes emitted, only to tell `"$@"` over an empty list from `""`.
    pushed: usize,
    quoted_region: bool,
    /// Flags for an unquoted literal byte: zero in a word, since literal text
    /// never splits, but `Q_SPLIT` inside an unquoted `${x:-a b}`.
    literal_flags: u8,
}

impl Expander {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            cur: QBuf::new(),
            pushed: 0,
            quoted_region: false,
            literal_flags: 0,
        }
    }

    /// Whether an empty result is still a field: `""` is, an unquoted `$x`
    /// that expanded to nothing is not.
    fn keep_empty(&self) -> bool {
        self.quoted_region
    }

    fn into_parts(mut self) -> Vec<QBuf> {
        self.parts.push(self.cur);
        self.parts
    }

    fn concat(self) -> QBuf {
        let mut out = QBuf::new();
        for part in self.into_parts() {
            out.append(&part);
        }
        out
    }

    fn push(&mut self, byte: u8, flags: u8) {
        self.cur.push(byte, flags);
        self.pushed += 1;
    }

    fn extend(&mut self, bytes: &[u8], flags: u8) {
        self.cur.extend(bytes, flags);
        self.pushed += bytes.len();
    }

    /// Start a new field. `"$@"` is the only thing that does this.
    fn break_field(&mut self) {
        self.parts
            .push(core::mem::replace(&mut self.cur, QBuf::new()));
    }

    fn walk(&mut self, raw: &[u8], in_quotes: bool) -> Result<(), ExpandError> {
        let mut i = 0usize;
        while i < raw.len() {
            let c = raw[i];
            match c {
                b'\'' if !in_quotes => {
                    self.quoted_region = true;
                    i += 1;
                    while i < raw.len() && raw[i] != b'\'' {
                        self.push(raw[i], Q_QUOTED);
                        i += 1;
                    }
                    i += 1;
                }
                b'"' if !in_quotes => {
                    let end = double_end(raw, i);
                    let inner = &raw[i + 1..end.min(raw.len())];
                    let before = self.pushed;
                    self.walk(inner, true)?;
                    // A quoted region is an explicit empty field unless it is
                    // exactly `"$@"` over an empty list, which contributes none.
                    if inner.is_empty() || self.pushed > before || !is_at_only(inner) {
                        self.quoted_region = true;
                    }
                    i = end + 1;
                }
                b'\\' if i + 1 < raw.len() => {
                    let next = raw[i + 1];
                    if next == b'\n' {
                    } else if in_quotes && !matches!(next, b'$' | b'`' | b'"' | b'\\') {
                        // In double quotes a backslash is special before those
                        // four only; elsewhere it is itself.
                        self.push(b'\\', Q_QUOTED);
                        self.push(next, Q_QUOTED);
                    } else {
                        self.push(next, Q_QUOTED);
                    }
                    self.quoted_region = true;
                    i += 2;
                }
                b'$' => i = self.dollar(raw, i, in_quotes)?,
                b'`' => {
                    let end = backquote_end(raw, i);
                    let text = unescape_backquote(&raw[i + 1..end.min(raw.len())]);
                    self.substitute(&text, in_quotes)?;
                    i = end + 1;
                }
                _ => {
                    let flags = if in_quotes {
                        Q_QUOTED
                    } else {
                        self.literal_flags
                    };
                    self.push(c, flags);
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// Expand a `${...}` operand in place. Its unquoted literal bytes split,
    /// because an operand is an expansion's result and not word text:
    /// `set -- ${x:-a b}` gives two parameters.
    fn walk_operand(&mut self, raw: &[u8], in_quotes: bool) -> Result<(), ExpandError> {
        let saved = self.literal_flags;
        self.literal_flags = expansion_flags(in_quotes);
        let outcome = self.walk(raw, in_quotes);
        self.literal_flags = saved;
        outcome
    }

    /// Handle the `$` form starting at `at`, returning the index just past it.
    fn dollar(&mut self, raw: &[u8], at: usize, in_quotes: bool) -> Result<usize, ExpandError> {
        match raw.get(at + 1) {
            Some(b'{') => {
                let end = balanced_end(raw, at + 2, b'{', b'}', 1);
                if end < at + 3 || end > raw.len() {
                    return Err(ExpandError::Syntax("unterminated ${"));
                }
                let content = raw[at + 2..end - 1].to_vec();
                self.param(&content, in_quotes)?;
                Ok(end)
            }
            Some(b'(') if raw.get(at + 2) == Some(&b'(') => {
                let end = balanced_end(raw, at + 3, b'(', b')', 2);
                if end < at + 5 || end > raw.len() {
                    return Err(ExpandError::Syntax("unterminated $(("));
                }
                let content = raw[at + 3..end - 2].to_vec();
                self.arithmetic(&content, in_quotes)?;
                Ok(end)
            }
            Some(b'(') => {
                let end = balanced_end(raw, at + 2, b'(', b')', 1);
                if end < at + 3 || end > raw.len() {
                    return Err(ExpandError::Syntax("unterminated $("));
                }
                let text = raw[at + 2..end - 1].to_vec();
                self.substitute(&text, in_quotes)?;
                Ok(end)
            }
            Some(&c) if c.is_ascii_digit() => {
                self.insert(&[c], in_quotes)?;
                Ok(at + 2)
            }
            Some(&c) if matches!(c, b'?' | b'$' | b'!' | b'#' | b'*' | b'@' | b'-') => {
                self.insert(&[c], in_quotes)?;
                Ok(at + 2)
            }
            Some(&c) if c == b'_' || c.is_ascii_alphabetic() => {
                let mut end = at + 1;
                while end < raw.len() && (raw[end] == b'_' || raw[end].is_ascii_alphanumeric()) {
                    end += 1;
                }
                let name = raw[at + 1..end].to_vec();
                self.insert(&name, in_quotes)?;
                Ok(end)
            }
            _ => {
                let flags = if in_quotes {
                    Q_QUOTED
                } else {
                    self.literal_flags
                };
                self.push(b'$', flags);
                Ok(at + 1)
            }
        }
    }

    fn param(&mut self, content: &[u8], in_quotes: bool) -> Result<(), ExpandError> {
        let parsed = param::parse(content).ok_or(ExpandError::Syntax("bad parameter expansion"))?;
        let name = parsed.name.to_vec();
        let arg = parsed.arg.to_vec();

        match parsed.op {
            param::ParamOp::None => self.insert(&name, in_quotes),
            param::ParamOp::Length => {
                let len = match name.as_slice() {
                    b"@" | b"*" => args::positional_count(),
                    _ => value_of(&name).map_or(0, |v| v.len()),
                };
                self.extend(decimal(len as i64).as_slice(), expansion_flags(in_quotes));
                Ok(())
            }
            param::ParamOp::UseDefault { colon } => {
                if present(&name, colon) {
                    self.insert(&name, in_quotes)
                } else {
                    self.walk_operand(&arg, in_quotes)
                }
            }
            param::ParamOp::AssignDefault { colon } => {
                if present(&name, colon) {
                    return self.insert(&name, in_quotes);
                }
                if !is_assignable(&name) {
                    return Err(ExpandError::Syntax("cannot assign to this parameter"));
                }
                let value = expand_raw_single(&arg)?;
                env::set(&name, &value);
                self.extend(&value, expansion_flags(in_quotes));
                Ok(())
            }
            param::ParamOp::Error { colon } => {
                if present(&name, colon) {
                    return self.insert(&name, in_quotes);
                }
                let message = expand_raw_single(&arg)?;
                Err(ExpandError::Unset {
                    name,
                    message: if message.is_empty() {
                        b"parameter null or not set".to_vec()
                    } else {
                        message
                    },
                })
            }
            param::ParamOp::UseAlternate { colon } => {
                if present(&name, colon) {
                    self.walk_operand(&arg, in_quotes)
                } else {
                    Ok(())
                }
            }
            param::ParamOp::TrimPrefix { longest } => {
                let value = value_of(&name).unwrap_or_default();
                let pat = expand_raw_pattern(&arg)?;
                let cut = if longest {
                    pattern::match_prefix_longest(&pat, &value)
                } else {
                    pattern::match_prefix_shortest(&pat, &value)
                };
                let kept = &value[cut.unwrap_or(0)..];
                self.extend(kept, expansion_flags(in_quotes));
                Ok(())
            }
            param::ParamOp::TrimSuffix { longest } => {
                let value = value_of(&name).unwrap_or_default();
                let pat = expand_raw_pattern(&arg)?;
                let cut = if longest {
                    pattern::match_suffix_longest(&pat, &value)
                } else {
                    pattern::match_suffix_shortest(&pat, &value)
                };
                let kept = &value[..cut.unwrap_or(value.len())];
                self.extend(kept, expansion_flags(in_quotes));
                Ok(())
            }
        }
    }

    fn insert(&mut self, name: &[u8], in_quotes: bool) -> Result<(), ExpandError> {
        match name {
            b"@" => {
                self.insert_positional(in_quotes, false);
                return Ok(());
            }
            b"*" => {
                self.insert_positional(in_quotes, true);
                return Ok(());
            }
            _ => {}
        }
        match value_of(name) {
            Some(value) => {
                self.extend(&value, expansion_flags(in_quotes));
                Ok(())
            }
            None => {
                if funcs::nounset() && is_assignable(name) {
                    return Err(ExpandError::Unset {
                        name: name.to_vec(),
                        message: b"parameter not set".to_vec(),
                    });
                }
                Ok(())
            }
        }
    }

    /// Quoted, `$@` is one field per positional and `$*` one field joined by
    /// the first byte of IFS; unquoted, both are one per positional and still
    /// split.
    fn insert_positional(&mut self, in_quotes: bool, star: bool) {
        let all = args::positional_args();
        if star && in_quotes {
            let ifs = ifs_value();
            let mut joined = Vec::new();
            for (index, value) in all.iter().enumerate() {
                if index > 0 {
                    if let Some(&sep) = ifs.first() {
                        joined.push(sep);
                    }
                }
                joined.extend_from_slice(value);
            }
            self.extend(&joined, Q_QUOTED);
            return;
        }
        let flags = expansion_flags(in_quotes);
        // A non-empty list always contributes its fields, even when every
        // parameter is empty — which the byte count cannot see.
        if in_quotes && !all.is_empty() {
            self.quoted_region = true;
        }
        for (index, value) in all.iter().enumerate() {
            if index > 0 {
                self.break_field();
            }
            self.extend(value, flags);
        }
    }

    fn arithmetic(&mut self, content: &[u8], in_quotes: bool) -> Result<(), ExpandError> {
        let text = expand_raw_single(content)?;
        let mut vars = ShellVars;
        let value = arith::eval(&text, &mut vars).map_err(|e| match e {
            arith::ArithError::DivideByZero => ExpandError::Arith("division by zero"),
            arith::ArithError::Syntax => ExpandError::Arith("syntax error"),
        })?;
        self.extend(&decimal(value), expansion_flags(in_quotes));
        Ok(())
    }

    fn substitute(&mut self, text: &[u8], in_quotes: bool) -> Result<(), ExpandError> {
        SUBSTITUTED.store(true, Ordering::Relaxed);
        let output = super::exec::capture(text).map_err(|()| ExpandError::Substitution)?;
        let end = output
            .iter()
            .rposition(|&b| b != b'\n')
            .map_or(0, |i| i + 1);
        self.extend(&output[..end], expansion_flags(in_quotes));
        Ok(())
    }
}

/// Whether a command substitution ran since this was last taken. POSIX gives
/// an assignment-only command the status of its last one, and nothing in the
/// expanded bytes says whether one happened.
static SUBSTITUTED: AtomicBool = AtomicBool::new(false);

pub fn take_substituted() -> bool {
    SUBSTITUTED.swap(false, Ordering::Relaxed)
}

fn expansion_flags(in_quotes: bool) -> u8 {
    if in_quotes { Q_QUOTED } else { Q_SPLIT }
}

/// Expand raw word text to one string, with no splitting or globbing.
fn expand_raw_single(raw: &[u8]) -> Result<Vec<u8>, ExpandError> {
    let mut expander = Expander::new();
    expander.walk(raw, false)?;
    Ok(expander.concat().into_bytes())
}

/// Expand raw word text keeping live pattern characters live.
fn expand_raw_pattern(raw: &[u8]) -> Result<QBuf, ExpandError> {
    let mut expander = Expander::new();
    expander.walk(raw, false)?;
    Ok(expander.concat())
}

/// Whether the inner text of a double-quoted region is exactly `$@`.
fn is_at_only(inner: &[u8]) -> bool {
    inner == b"$@" || inner == b"${@}"
}

fn is_assignable(name: &[u8]) -> bool {
    !name.is_empty()
        && (name[0].is_ascii_alphabetic() || name[0] == b'_')
        && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

fn present(name: &[u8], colon: bool) -> bool {
    match value_of(name) {
        Some(value) => !colon || !value.is_empty(),
        None => false,
    }
}

/// A parameter's value. The specials are not variables, so they resolve here
/// rather than through the table.
fn value_of(name: &[u8]) -> Option<Vec<u8>> {
    if name.iter().all(|b| b.is_ascii_digit()) && !name.is_empty() {
        let index = name
            .iter()
            .fold(0usize, |acc, b| acc * 10 + (b - b'0') as usize);
        return args::positional(index);
    }
    match name {
        b"?" => Some(decimal(super::last_exit_code() as i64)),
        b"$" => Some(decimal(super::shell_pid() as i64)),
        b"!" => Some(decimal(super::last_bg_pid() as i64)),
        b"#" => Some(decimal(args::positional_count() as i64)),
        b"-" => Some(funcs::option_letters()),
        b"@" | b"*" => Some(args::positional_args().join(&b' ')),
        _ => env::get(name),
    }
}

fn decimal(value: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    let mut digits = [0u8; 20];
    let mut n = 0usize;
    loop {
        digits[n] = b'0' + (magnitude % 10) as u8;
        n += 1;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        out.push(b'-');
    }
    for i in (0..n).rev() {
        out.push(digits[i]);
    }
    out
}

/// Arithmetic's view of the variable table.
struct ShellVars;

impl arith::Vars for ShellVars {
    fn get(&mut self, name: &[u8]) -> i64 {
        value_of(name).map_or(0, |v| arith::value_of(&v))
    }
    fn set(&mut self, name: &[u8], value: i64) {
        if is_assignable(name) {
            env::set(name, &decimal(value));
        }
    }
}

/// Inside backticks, `\` is special only before `` ` ``, `\` and `$`.
fn unescape_backquote(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        if raw[i] == b'\\' && i + 1 < raw.len() && matches!(raw[i + 1], b'`' | b'\\' | b'$') {
            out.push(raw[i + 1]);
            i += 2;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

/// Index of the `"` closing the one at `open`, or `raw.len()`.
fn double_end(raw: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < raw.len() {
        match raw[i] {
            b'"' => return i,
            b'\\' => i += 2,
            b'`' => i = backquote_end(raw, i) + 1,
            b'$' => i = dollar_end(raw, i),
            _ => i += 1,
        }
    }
    raw.len()
}

/// Index of the backtick closing the one at `open`, or `raw.len()`.
fn backquote_end(raw: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < raw.len() {
        match raw[i] {
            b'\\' => i += 2,
            b'`' => return i,
            _ => i += 1,
        }
    }
    raw.len()
}

/// Index just past the `$...` construct at `at`, for the nesting-aware forms
/// only; `$name` is resolved by the caller.
fn dollar_end(raw: &[u8], at: usize) -> usize {
    match (raw.get(at + 1), raw.get(at + 2)) {
        (Some(b'('), Some(b'(')) => balanced_end(raw, at + 3, b'(', b')', 2),
        (Some(b'('), _) => balanced_end(raw, at + 2, b'(', b')', 1),
        (Some(b'{'), _) => balanced_end(raw, at + 2, b'{', b'}', 1),
        _ => at + 1,
    }
}

/// Index just past the `depth`-th closing delimiter, counting from `start`.
fn balanced_end(raw: &[u8], start: usize, open: u8, close: u8, mut depth: usize) -> usize {
    let mut i = start;
    while i < raw.len() && depth > 0 {
        match raw[i] {
            b'\'' => {
                i += 1;
                while i < raw.len() && raw[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            b'"' => i = double_end(raw, i) + 1,
            b'`' => i = backquote_end(raw, i) + 1,
            b'\\' => i += 2,
            c => {
                if c == open {
                    depth += 1;
                } else if c == close {
                    depth -= 1;
                }
                i += 1;
            }
        }
    }
    i
}
