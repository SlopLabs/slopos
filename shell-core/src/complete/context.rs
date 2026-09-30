//! The word under the cursor, read from the line up to it. Lenient where the
//! lexer is strict: an open quote or `$(` is where the user is typing.

use alloc::vec;
use alloc::vec::Vec;

use crate::lexer::assignment_split;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Command,
    /// An argument of the command [`Context::words`] begins with.
    Argument,
    /// A redirection's operand or an assignment's value: a path.
    Path,
    /// A comment, a here-document delimiter, a descriptor number, or a word
    /// holding an expansion, whose value is not known until it runs.
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quote {
    None,
    Single,
    Double,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Context {
    pub position: Position,
    /// The simple command's words before the cursor's, quotes removed, the
    /// command name first. Empty unless the position is an argument.
    pub words: Vec<Vec<u8>>,
    /// The cursor's word up to the cursor, quotes removed.
    pub prefix: Vec<u8>,
    /// The quote open at the cursor, which completed text must stay inside.
    pub quote: Quote,
}

/// Before the command name, these leave the next word in command position.
/// Matched against raw text, so `"if"` is an ordinary word.
const RESERVED: &[&[u8]] = &[
    b"if", b"then", b"else", b"elif", b"fi", b"do", b"done", b"while", b"until", b"!", b"{", b"}",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    Word,
    Path,
    Nothing,
}

/// One simple command being read; `$(` opens another inside the word it
/// interrupts, and `)` returns to that word.
struct Frame {
    words: Vec<Vec<u8>>,
    next: Next,
    quote: Quote,
    word: Vec<u8>,
    /// Where the current word's raw text starts; `None` between words.
    start: Option<usize>,
    opaque: bool,
}

impl Frame {
    fn new() -> Self {
        Self {
            words: Vec::new(),
            next: Next::Word,
            quote: Quote::None,
            word: Vec::new(),
            start: None,
            opaque: false,
        }
    }

    fn begin(&mut self, at: usize) {
        if self.start.is_none() {
            self.start = Some(at);
        }
    }

    fn end_word(&mut self, line: &[u8], at: usize) {
        let Some(start) = self.start.take() else {
            return;
        };
        let word = core::mem::take(&mut self.word);
        self.opaque = false;
        if self.next != Next::Word {
            self.next = Next::Word;
            return;
        }
        if self.words.is_empty() {
            let raw = &line[start..at];
            if RESERVED.contains(&raw) || assignment_split(raw).is_some() {
                return;
            }
        }
        self.words.push(word);
    }

    fn separate(&mut self, line: &[u8], at: usize) {
        self.end_word(line, at);
        self.words.clear();
        self.next = Next::Word;
    }
}

pub fn analyze(line: &[u8]) -> Context {
    let mut stack = vec![Frame::new()];
    let mut i = 0usize;
    while i < line.len() {
        let c = line[i];
        let depth = stack.len() - 1;
        let top = &mut stack[depth];
        match top.quote {
            Quote::Single => {
                if c == b'\'' {
                    top.quote = Quote::None;
                } else {
                    top.word.push(c);
                }
                i += 1;
            }
            Quote::Double => match c {
                b'"' => {
                    top.quote = Quote::None;
                    i += 1;
                }
                b'\\' => match line.get(i + 1) {
                    Some(&next @ (b'$' | b'`' | b'"' | b'\\')) => {
                        top.word.push(next);
                        i += 2;
                    }
                    Some(b'\n') => i += 2,
                    Some(_) => {
                        top.word.push(c);
                        i += 1;
                    }
                    None => i += 1,
                },
                b'$' | b'`' => i = expansion(line, i, &mut stack),
                _ => {
                    top.word.push(c);
                    i += 1;
                }
            },
            Quote::None => match c {
                b' ' | b'\t' | b'\r' => {
                    top.end_word(line, i);
                    i += 1;
                }
                b'\n' | b';' | b'|' | b'(' => {
                    top.separate(line, i);
                    i += 1;
                }
                b'&' if line.get(i + 1) == Some(&b'>') => {
                    top.end_word(line, i);
                    top.next = Next::Path;
                    i += 2;
                }
                b'&' => {
                    top.separate(line, i);
                    i += 1;
                }
                b')' if depth > 0 => {
                    stack.pop();
                    i += 1;
                }
                b')' => {
                    top.separate(line, i);
                    i += 1;
                }
                b'<' | b'>' => i = redirection(line, i, top),
                b'#' if top.start.is_none() => {
                    return Context {
                        position: Position::Nothing,
                        words: Vec::new(),
                        prefix: Vec::new(),
                        quote: Quote::None,
                    };
                }
                b'\\' => match line.get(i + 1) {
                    Some(b'\n') => i += 2,
                    Some(&next) => {
                        top.begin(i);
                        top.word.push(next);
                        i += 2;
                    }
                    None => i += 1,
                },
                b'\'' => {
                    top.begin(i);
                    top.quote = Quote::Single;
                    i += 1;
                }
                b'"' => {
                    top.begin(i);
                    top.quote = Quote::Double;
                    i += 1;
                }
                b'$' | b'`' => i = expansion(line, i, &mut stack),
                _ => {
                    top.begin(i);
                    top.word.push(c);
                    i += 1;
                }
            },
        }
    }

    let top = stack.pop().unwrap_or_else(Frame::new);
    let nothing = Context {
        position: Position::Nothing,
        words: Vec::new(),
        prefix: Vec::new(),
        quote: top.quote,
    };
    if top.opaque {
        return nothing;
    }
    match top.next {
        Next::Nothing => nothing,
        Next::Path => Context {
            position: Position::Path,
            words: Vec::new(),
            prefix: top.word,
            quote: top.quote,
        },
        Next::Word if !top.words.is_empty() => Context {
            position: Position::Argument,
            words: top.words,
            prefix: top.word,
            quote: top.quote,
        },
        Next::Word => {
            let assigned = top
                .start
                .and_then(|start| assignment_split(&line[start..]))
                .map(|(name, _)| name.len() + 1);
            match assigned {
                // The name is unquoted, so its bytes lead the unquoted word.
                Some(value_at) => Context {
                    position: Position::Path,
                    words: Vec::new(),
                    prefix: top.word[value_at..].to_vec(),
                    quote: top.quote,
                },
                None => Context {
                    position: Position::Command,
                    words: Vec::new(),
                    prefix: top.word,
                    quote: top.quote,
                },
            }
        }
    }
}

fn redirection(line: &[u8], at: usize, top: &mut Frame) -> usize {
    let io_number = top.start.is_some_and(|start| {
        top.quote == Quote::None
            && line[start..at].iter().all(u8::is_ascii_digit)
            && !line[start..at].is_empty()
    });
    if io_number {
        top.start = None;
        top.word.clear();
    } else {
        top.end_word(line, at);
    }
    let (len, next) = match (line[at], line.get(at + 1)) {
        (b'<', Some(b'<')) if line.get(at + 2) == Some(&b'-') => (3, Next::Nothing),
        (b'<', Some(b'<')) => (2, Next::Nothing),
        (_, Some(b'&')) => (2, Next::Nothing),
        (b'<', Some(b'>')) | (b'>', Some(b'>')) | (b'>', Some(b'|')) => (2, Next::Path),
        _ => (1, Next::Path),
    };
    top.next = next;
    at + len
}

/// `$(` opens a command of its own; any other expansion leaves the word's
/// value unknowable until it runs.
fn expansion(line: &[u8], at: usize, stack: &mut Vec<Frame>) -> usize {
    let top = stack
        .last_mut()
        .expect("the outermost frame is never popped");
    top.begin(at);
    top.opaque = true;
    if line[at] == b'`' {
        return match line[at + 1..].iter().position(|&b| b == b'`') {
            Some(end) => at + 1 + end + 1,
            None => line.len(),
        };
    }
    match (line.get(at + 1), line.get(at + 2)) {
        (Some(b'('), Some(b'(')) => skip_balanced(line, at + 1, b'(', b')'),
        (Some(b'('), _) => {
            stack.push(Frame::new());
            at + 2
        }
        (Some(b'{'), _) => skip_balanced(line, at + 1, b'{', b'}'),
        _ => at + 1,
    }
}

fn skip_balanced(line: &[u8], open_at: usize, open: u8, close: u8) -> usize {
    let mut depth = 0usize;
    for (i, &b) in line.iter().enumerate().skip(open_at) {
        if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(ctx: &Context) -> Vec<&[u8]> {
        ctx.words.iter().map(Vec::as_slice).collect()
    }

    #[test]
    fn the_first_word_is_a_command() {
        for line in [
            b"gi".as_slice(),
            b"  gi",
            b"echo x | gi",
            b"a && gi",
            b"a; gi",
            b"(gi",
        ] {
            let ctx = analyze(line);
            assert_eq!(ctx.position, Position::Command, "{line:?}");
            assert_eq!(ctx.prefix, b"gi", "{line:?}");
        }
    }

    #[test]
    fn reserved_words_and_assignments_keep_command_position() {
        for line in [
            b"if gi".as_slice(),
            b"while true; do gi",
            b"! gi",
            b"CC=clang gi",
            b"A=1 B=2 gi",
        ] {
            assert_eq!(analyze(line).position, Position::Command, "{line:?}");
        }
        assert_eq!(analyze(b"\"if\" x").position, Position::Argument);
        assert_eq!(analyze(b"'A'=1 x").position, Position::Argument);
    }

    #[test]
    fn arguments_carry_the_words_before_them() {
        let ctx = analyze(b"git -C 'my dir' push or");
        assert_eq!(ctx.position, Position::Argument);
        assert_eq!(words(&ctx), [&b"git"[..], b"-C", b"my dir", b"push"]);
        assert_eq!(ctx.prefix, b"or");

        let ctx = analyze(b"ls ");
        assert_eq!(ctx.position, Position::Argument);
        assert_eq!(ctx.prefix, b"");
    }

    #[test]
    fn quotes_and_escapes_are_removed_from_the_prefix() {
        let ctx = analyze(b"cat my\\ fi");
        assert_eq!(
            (ctx.prefix.as_slice(), ctx.quote),
            (&b"my fi"[..], Quote::None)
        );
        let ctx = analyze(b"cat \"my fi");
        assert_eq!(
            (ctx.prefix.as_slice(), ctx.quote),
            (&b"my fi"[..], Quote::Double)
        );
        let ctx = analyze(b"cat 'a\"b");
        assert_eq!(
            (ctx.prefix.as_slice(), ctx.quote),
            (&b"a\"b"[..], Quote::Single)
        );
        let ctx = analyze(b"cat \"a\\$b");
        assert_eq!(ctx.prefix, b"a$b");
    }

    #[test]
    fn redirection_operands_are_paths() {
        for line in [
            b"echo x >ou".as_slice(),
            b"echo x > ou",
            b"cmd 2>>ou",
            b"cmd &>ou",
            b"cmd <ou",
        ] {
            let ctx = analyze(line);
            assert_eq!(ctx.position, Position::Path, "{line:?}");
            assert_eq!(ctx.prefix, b"ou", "{line:?}");
        }
        let ctx = analyze(b"cmd 2>err ar");
        assert_eq!(words(&ctx), [&b"cmd"[..]]);
        assert_eq!(ctx.prefix, b"ar");
        assert_eq!(words(&analyze(b"echo 2 >x a")), [&b"echo"[..], b"2"]);
    }

    #[test]
    fn delimiters_and_descriptors_complete_nothing() {
        for line in [
            b"cat <<EO".as_slice(),
            b"cmd 2>&",
            b"echo # com",
            b"echo $HO",
            b"echo `da",
        ] {
            assert_eq!(analyze(line).position, Position::Nothing, "{line:?}");
        }
    }

    #[test]
    fn an_assignment_value_is_a_path() {
        let ctx = analyze(b"CC=/usr/lo");
        assert_eq!(ctx.position, Position::Path);
        assert_eq!(ctx.prefix, b"/usr/lo");
    }

    #[test]
    fn command_substitution_is_a_command_of_its_own() {
        let ctx = analyze(b"echo $(gi");
        assert_eq!(ctx.position, Position::Command);
        assert_eq!(ctx.prefix, b"gi");

        let ctx = analyze(b"echo \"$(git br");
        assert_eq!(ctx.position, Position::Argument);
        assert_eq!(words(&ctx), [&b"git"[..]]);
        assert_eq!(ctx.quote, Quote::None);

        assert_eq!(analyze(b"echo $(date)x").position, Position::Nothing);
        let ctx = analyze(b"echo $(date) fi");
        assert_eq!(words(&ctx), [&b"echo"[..], b""]);
        assert_eq!(ctx.prefix, b"fi");
    }
}
