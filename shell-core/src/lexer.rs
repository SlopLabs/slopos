//! Token recognition.
//!
//! Deliberately dumb, as POSIX §2.3 describes it: longest-match operators, a
//! quote that does not delimit a word and whose bytes stay *in* the token, and
//! no substitution. The one piece of state is the here-document queue, because
//! `<<` names a delimiter whose body begins after the next newline.
//!
//! [`LexError::Incomplete`] is the load-bearing answer: an unterminated quote,
//! `$(`, `` ` ``, `${` or here-document means *more input*, which is what lets
//! one reader serve a script file and an interactive PS2 prompt.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LexError {
    /// The input ends inside a construct. More bytes would complete it.
    Incomplete,
    Syntax(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Semi,
    DSemi,
    Amp,
    AndIf,
    OrIf,
    Pipe,
    LParen,
    RParen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirOp {
    /// `<`
    In,
    /// `<>`
    InOut,
    /// `>`, and `>|` which differs only under `set -C`, which does not exist.
    Out,
    /// `>>`
    Append,
    /// `<&`
    DupIn,
    /// `>&`
    DupOut,
    /// `&>` — stdout and stderr to one path.
    OutBoth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    /// Raw word text: quotes and escapes intact, expansion pending.
    Word(Vec<u8>),
    /// Final bytes: no expansion, splitting or globbing.
    Literal(Vec<u8>),
    Op(Op),
    Redir {
        /// The IO_NUMBER, when the operator carried one.
        fd: Option<i32>,
        op: RedirOp,
    },
    /// A complete here-document; the delimiter never reaches the parser.
    Here {
        fd: Option<i32>,
        body: Vec<u8>,
        /// The delimiter was unquoted, so the body expands at redirection time.
        expand: bool,
    },
    Newline,
}

/// Blanks POSIX recognizes, plus `\r` so a CRLF script is not one long word.
fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\r'
}

/// A byte that ends a word when it is not quoted.
fn is_word_break(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\r' | b'\n' | b'|' | b'&' | b';' | b'(' | b')' | b'<' | b'>'
    )
}

struct Pending {
    delim: Vec<u8>,
    strip_tabs: bool,
    /// Index of the `Tok::Here` whose body this fills.
    slot: usize,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    out: Vec<Tok>,
    pending: Vec<Pending>,
}

pub fn lex(src: &[u8]) -> Result<Vec<Tok>, LexError> {
    let mut lexer = Lexer {
        src,
        pos: 0,
        out: Vec::new(),
        pending: Vec::new(),
    };
    lexer.run()?;
    Ok(lexer.out)
}

impl<'a> Lexer<'a> {
    fn at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    fn run(&mut self) -> Result<(), LexError> {
        loop {
            while self.at(0).is_some_and(is_blank) {
                self.pos += 1;
            }
            let Some(c) = self.at(0) else { break };

            if c == b'\\' && self.at(1) == Some(b'\n') {
                self.pos += 2;
                continue;
            }
            if c == b'#' {
                while self.at(0).is_some_and(|b| b != b'\n') {
                    self.pos += 1;
                }
                continue;
            }
            if c == b'\n' {
                self.pos += 1;
                self.out.push(Tok::Newline);
                self.drain_heredocs()?;
                continue;
            }
            if self.scan_operator()? {
                continue;
            }
            let word = self.scan_word()?;
            self.out.push(Tok::Word(word));
        }

        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(LexError::Incomplete)
        }
    }

    /// Recognize an operator at the cursor, longest match first. `Ok(false)`
    /// means the cursor is at the start of a word.
    fn scan_operator(&mut self) -> Result<bool, LexError> {
        let c = self.at(0).unwrap_or(0);
        let next = self.at(1);

        let simple = match (c, next) {
            (b';', Some(b';')) => Some((2, Op::DSemi)),
            (b';', _) => Some((1, Op::Semi)),
            (b'&', Some(b'&')) => Some((2, Op::AndIf)),
            (b'|', Some(b'|')) => Some((2, Op::OrIf)),
            (b'|', _) => Some((1, Op::Pipe)),
            (b'(', _) => Some((1, Op::LParen)),
            (b')', _) => Some((1, Op::RParen)),
            _ => None,
        };
        if let Some((len, op)) = simple {
            self.pos += len;
            self.out.push(Tok::Op(op));
            return Ok(true);
        }

        if c == b'&' && next == Some(b'>') {
            self.pos += 2;
            self.out.push(Tok::Redir {
                fd: None,
                op: RedirOp::OutBoth,
            });
            return Ok(true);
        }
        if c == b'&' {
            self.pos += 1;
            self.out.push(Tok::Op(Op::Amp));
            return Ok(true);
        }

        // IO_NUMBER: leading digits count as the redirected descriptor only
        // when they touch the arrow, so `echo 2 > out` passes `2` as a word.
        let mut digits = 0usize;
        while self.at(digits).is_some_and(|b| b.is_ascii_digit()) {
            digits += 1;
        }
        let arrow = match self.at(digits) {
            Some(b) if b == b'<' || b == b'>' => b,
            _ => return Ok(false),
        };
        let fd = if digits == 0 {
            None
        } else {
            let mut n = 0i32;
            for i in 0..digits {
                n = n
                    .saturating_mul(10)
                    .saturating_add((self.src[self.pos + i] - b'0') as i32);
            }
            Some(n)
        };
        let after = self.at(digits + 1);

        if arrow == b'<' && after == Some(b'<') {
            let strip_tabs = self.at(digits + 2) == Some(b'-');
            self.pos += digits + if strip_tabs { 3 } else { 2 };
            self.queue_heredoc(fd, strip_tabs)?;
            return Ok(true);
        }

        let (len, op) = match (arrow, after) {
            (b'<', Some(b'&')) => (2, RedirOp::DupIn),
            (b'<', Some(b'>')) => (2, RedirOp::InOut),
            (b'<', _) => (1, RedirOp::In),
            (b'>', Some(b'>')) => (2, RedirOp::Append),
            (b'>', Some(b'&')) => (2, RedirOp::DupOut),
            (b'>', Some(b'|')) => (2, RedirOp::Out),
            (b'>', _) => (1, RedirOp::Out),
            _ => return Ok(false),
        };
        self.pos += digits + len;
        self.out.push(Tok::Redir { fd, op });
        Ok(true)
    }

    /// Read the delimiter word that follows `<<` and queue its body.
    fn queue_heredoc(&mut self, fd: Option<i32>, strip_tabs: bool) -> Result<(), LexError> {
        while self.at(0).is_some_and(is_blank) {
            self.pos += 1;
        }
        if self.at(0).is_none_or(|b| b == b'\n') {
            return Err(LexError::Incomplete);
        }
        let raw = self.scan_word()?;
        // Any quoting anywhere in the delimiter makes the body literal.
        let expand = !raw.iter().any(|&b| b == b'\'' || b == b'"' || b == b'\\');
        let delim = remove_quotes(&raw);

        let slot = self.out.len();
        self.out.push(Tok::Here {
            fd,
            body: Vec::new(),
            expand,
        });
        self.pending.push(Pending {
            delim,
            strip_tabs,
            slot,
        });
        Ok(())
    }

    /// Collect every queued here-document body, in operator order, starting at
    /// the cursor (which sits just past a newline).
    fn drain_heredocs(&mut self) -> Result<(), LexError> {
        while !self.pending.is_empty() {
            let pending = self.pending.remove(0);
            let mut body = Vec::new();
            loop {
                if self.pos >= self.src.len() {
                    return Err(LexError::Incomplete);
                }
                let end = self.src[self.pos..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|i| self.pos + i);
                let line_end = end.unwrap_or(self.src.len());
                let mut line = &self.src[self.pos..line_end];
                if pending.strip_tabs {
                    while line.first() == Some(&b'\t') {
                        line = &line[1..];
                    }
                }
                if line == pending.delim.as_slice() {
                    self.pos = end.map_or(self.src.len(), |i| i + 1);
                    break;
                }
                let Some(nl) = end else {
                    return Err(LexError::Incomplete);
                };
                body.extend_from_slice(line);
                body.push(b'\n');
                self.pos = nl + 1;
            }
            if let Tok::Here { body: slot, .. } = &mut self.out[pending.slot] {
                *slot = body;
            }
        }
        Ok(())
    }

    fn scan_word(&mut self) -> Result<Vec<u8>, LexError> {
        let mut buf = Vec::new();
        while let Some(c) = self.at(0) {
            match c {
                b'\'' => self.scan_single(&mut buf)?,
                b'"' => self.scan_double(&mut buf)?,
                b'`' => self.scan_backquote(&mut buf)?,
                b'$' => self.scan_dollar(&mut buf)?,
                b'\\' => match self.at(1) {
                    Some(b'\n') => self.pos += 2,
                    Some(next) => {
                        buf.push(b'\\');
                        buf.push(next);
                        self.pos += 2;
                    }
                    None => return Err(LexError::Incomplete),
                },
                _ if is_word_break(c) => break,
                _ => {
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(buf)
    }

    fn scan_single(&mut self, buf: &mut Vec<u8>) -> Result<(), LexError> {
        buf.push(b'\'');
        self.pos += 1;
        loop {
            let Some(c) = self.at(0) else {
                return Err(LexError::Incomplete);
            };
            buf.push(c);
            self.pos += 1;
            if c == b'\'' {
                return Ok(());
            }
        }
    }

    fn scan_double(&mut self, buf: &mut Vec<u8>) -> Result<(), LexError> {
        buf.push(b'"');
        self.pos += 1;
        loop {
            let Some(c) = self.at(0) else {
                return Err(LexError::Incomplete);
            };
            match c {
                b'"' => {
                    buf.push(c);
                    self.pos += 1;
                    return Ok(());
                }
                b'\\' => match self.at(1) {
                    Some(next) => {
                        buf.push(b'\\');
                        buf.push(next);
                        self.pos += 2;
                    }
                    None => return Err(LexError::Incomplete),
                },
                b'`' => self.scan_backquote(buf)?,
                b'$' => self.scan_dollar(buf)?,
                _ => {
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn scan_backquote(&mut self, buf: &mut Vec<u8>) -> Result<(), LexError> {
        buf.push(b'`');
        self.pos += 1;
        loop {
            let Some(c) = self.at(0) else {
                return Err(LexError::Incomplete);
            };
            if c == b'\\' {
                let Some(next) = self.at(1) else {
                    return Err(LexError::Incomplete);
                };
                buf.push(b'\\');
                buf.push(next);
                self.pos += 2;
                continue;
            }
            buf.push(c);
            self.pos += 1;
            if c == b'`' {
                return Ok(());
            }
        }
    }

    fn scan_dollar(&mut self, buf: &mut Vec<u8>) -> Result<(), LexError> {
        buf.push(b'$');
        self.pos += 1;
        match (self.at(0), self.at(1)) {
            (Some(b'('), Some(b'(')) => {
                buf.push(b'(');
                buf.push(b'(');
                self.pos += 2;
                self.scan_balanced(buf, b'(', b')', 2)
            }
            (Some(b'('), _) => {
                buf.push(b'(');
                self.pos += 1;
                self.scan_balanced(buf, b'(', b')', 1)
            }
            (Some(b'{'), _) => {
                buf.push(b'{');
                self.pos += 1;
                self.scan_balanced(buf, b'{', b'}', 1)
            }
            _ => Ok(()),
        }
    }

    /// Copy through `depth` closing delimiters, quote-aware so a `)` inside
    /// `'...'` does not close a `$(`.
    fn scan_balanced(
        &mut self,
        buf: &mut Vec<u8>,
        open: u8,
        close: u8,
        mut depth: usize,
    ) -> Result<(), LexError> {
        while depth > 0 {
            let Some(c) = self.at(0) else {
                return Err(LexError::Incomplete);
            };
            match c {
                b'\'' => self.scan_single(buf)?,
                b'"' => self.scan_double(buf)?,
                b'`' => self.scan_backquote(buf)?,
                b'\\' => match self.at(1) {
                    Some(next) => {
                        buf.push(b'\\');
                        buf.push(next);
                        self.pos += 2;
                    }
                    None => return Err(LexError::Incomplete),
                },
                _ => {
                    if c == open {
                        depth += 1;
                    } else if c == close {
                        depth -= 1;
                    }
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(())
    }
}

/// Quote removal with no expansion — what POSIX does to a here-document
/// delimiter and nothing more.
pub fn remove_quotes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        match raw[i] {
            b'\'' => {
                i += 1;
                while i < raw.len() && raw[i] != b'\'' {
                    out.push(raw[i]);
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < raw.len() && raw[i] != b'"' {
                    if raw[i] == b'\\' && i + 1 < raw.len() {
                        i += 1;
                    }
                    out.push(raw[i]);
                    i += 1;
                }
                i += 1;
            }
            b'\\' if i + 1 < raw.len() => {
                out.push(raw[i + 1]);
                i += 2;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Whether raw word text is an assignment prefix. POSIX Rule 7b: the name
/// must be unquoted and non-empty, so `"a"=b` and `=b` are ordinary words.
pub fn assignment_split(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    let eq = raw.iter().position(|&b| b == b'=')?;
    if eq == 0 {
        return None;
    }
    let name = &raw[..eq];
    if !(name[0].is_ascii_alphabetic() || name[0] == b'_') {
        return None;
    }
    if !name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_') {
        return None;
    }
    Some((name, &raw[eq + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn words(src: &[u8]) -> Vec<Vec<u8>> {
        lex(src)
            .unwrap()
            .into_iter()
            .filter_map(|t| match t {
                Tok::Word(w) => Some(w),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn quotes_stay_in_the_token() {
        assert_eq!(
            words(b"echo \"a b\" 'c d'"),
            vec![b"echo".to_vec(), b"\"a b\"".to_vec(), b"'c d'".to_vec()]
        );
    }

    #[test]
    fn command_substitution_is_one_word() {
        assert_eq!(
            words(b"x=$(echo a; echo b)"),
            vec![b"x=$(echo a; echo b)".to_vec()]
        );
    }

    #[test]
    fn unterminated_constructs_ask_for_more() {
        for src in [
            b"echo 'x".as_slice(),
            b"echo \"x",
            b"echo $(x",
            b"echo ${x",
            b"echo `x",
            b"echo x \\",
            b"cat <<EOF\nbody\n",
        ] {
            assert_eq!(lex(src), Err(LexError::Incomplete), "{src:?}");
        }
    }

    #[test]
    fn io_number_binds_only_when_it_touches_the_arrow() {
        assert!(matches!(
            lex(b"echo 2>f").unwrap()[1],
            Tok::Redir {
                fd: Some(2),
                op: RedirOp::Out
            }
        ));
        assert_eq!(
            words(b"echo 2 >f"),
            vec![b"echo".to_vec(), b"2".to_vec(), b"f".to_vec()]
        );
    }

    #[test]
    fn heredoc_body_is_collected_and_tab_stripped() {
        let toks = lex(b"cat <<-END\n\tone\n\ttwo\n\tEND\necho done\n").unwrap();
        let body = toks.iter().find_map(|t| match t {
            Tok::Here { body, expand, .. } => Some((body.clone(), *expand)),
            _ => None,
        });
        assert_eq!(body, Some((b"one\ntwo\n".to_vec(), true)));
    }

    #[test]
    fn quoted_heredoc_delimiter_suppresses_expansion() {
        let toks = lex(b"cat <<'E'\n$x\nE\n").unwrap();
        let found = toks.iter().find_map(|t| match t {
            Tok::Here { body, expand, .. } => Some((body.clone(), *expand)),
            _ => None,
        });
        assert_eq!(found, Some((b"$x\n".to_vec(), false)));
    }

    #[test]
    fn two_heredocs_queue_in_operator_order() {
        let toks = lex(b"cat <<A <<B\nfirst\nA\nsecond\nB\n").unwrap();
        let bodies: Vec<Vec<u8>> = toks
            .iter()
            .filter_map(|t| match t {
                Tok::Here { body, .. } => Some(body.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(bodies, vec![b"first\n".to_vec(), b"second\n".to_vec()]);
    }

    #[test]
    fn comment_needs_a_word_boundary() {
        assert_eq!(
            words(b"echo a#b # tail"),
            vec![b"echo".to_vec(), b"a#b".to_vec()]
        );
    }

    #[test]
    fn line_continuation_joins_words() {
        assert_eq!(
            words(b"echo a\\\nb"),
            vec![b"echo".to_vec(), b"ab".to_vec()]
        );
    }
}
