//! Recursive-descent parser: [`Tok`] stream to [`List`].
//!
//! [`ParseError::Incomplete`] is distinct from a syntax error on purpose: it
//! is the whole mechanism behind a multi-line script and a PS2 prompt alike,
//! since the reader appends input until the parse stops asking for more.
//!
//! Reserved words are recognized positionally (POSIX §2.4), which costs
//! nothing because the lexer kept a word's quotes: `"if"` is not `if`.

use crate::ast::{
    AndOr, AndOrOp, CaseItem, Command, CommandKind, List, ListItem, Pipeline, RedirKind,
    RedirTarget, Redirect, Word,
};
use crate::lexer::{Op, RedirOp, Tok, assignment_split};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The input ends mid-construct. More input would complete it.
    Incomplete,
    Syntax(&'static str),
}

/// Words that can only close a construct, so finding one where a command was
/// expected is a syntax error rather than a command named `fi`.
const CLOSERS: &[&[u8]] = &[
    b"then", b"else", b"elif", b"fi", b"do", b"done", b"esac", b"}",
];

pub fn parse(toks: &[Tok]) -> Result<List, ParseError> {
    let mut parser = Parser { toks, pos: 0 };
    let list = parser.list(&[])?;
    parser.skip_newlines();
    if parser.pos < parser.toks.len() {
        return Err(ParseError::Syntax("unexpected token"));
    }
    Ok(list)
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

fn word_text(tok: &Tok) -> Option<&[u8]> {
    match tok {
        Tok::Word(w) => Some(w),
        _ => None,
    }
}

fn is_name(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&'a Tok> {
        self.toks.get(self.pos + offset)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn at_word(&self, kw: &[u8]) -> bool {
        self.peek().and_then(word_text) == Some(kw)
    }

    fn at_op(&self, op: Op) -> bool {
        matches!(self.peek(), Some(Tok::Op(o)) if *o == op)
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.pos += 1;
        }
    }

    fn at_stop(&self, stops: &[&[u8]]) -> bool {
        match self.peek().and_then(word_text) {
            Some(w) => stops.contains(&w),
            None => false,
        }
    }

    /// Tokens that end a list without being consumed by it.
    fn at_list_close(&self) -> bool {
        self.at_op(Op::RParen) || self.at_op(Op::DSemi)
    }

    /// Why a required list came back empty. A condition not typed yet is
    /// *unfinished*, and answering `Syntax` there makes the reader abandon the
    /// command and run its condition and body as loose top-level ones.
    fn empty_list(&self, msg: &'static str) -> ParseError {
        if self.at_end() {
            ParseError::Incomplete
        } else {
            ParseError::Syntax(msg)
        }
    }

    fn expect_reserved(&mut self, kw: &'static [u8]) -> Result<(), ParseError> {
        self.skip_newlines();
        if self.at_end() {
            return Err(ParseError::Incomplete);
        }
        if !self.at_word(kw) {
            return Err(ParseError::Syntax("missing keyword"));
        }
        self.pos += 1;
        Ok(())
    }

    /// The next token as a word operand. An operator there is a syntax error;
    /// running out of input is a request for more.
    fn expect_word(&mut self) -> Result<Word, ParseError> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                self.pos += 1;
                Ok(Word::raw(w.clone()))
            }
            Some(Tok::Literal(w)) => {
                self.pos += 1;
                Ok(Word::literal(w.clone()))
            }
            None => Err(ParseError::Incomplete),
            Some(Tok::Newline) => Err(ParseError::Syntax("missing operand")),
            Some(_) => Err(ParseError::Syntax("unexpected operator")),
        }
    }

    fn list(&mut self, stops: &[&[u8]]) -> Result<List, ParseError> {
        let mut items: Vec<ListItem> = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_end() || self.at_stop(stops) || self.at_list_close() {
                break;
            }
            let andor = self.and_or()?;
            let mut background = false;
            let mut separated = false;
            match self.peek() {
                Some(Tok::Op(Op::Amp)) => {
                    background = true;
                    separated = true;
                    self.pos += 1;
                }
                Some(Tok::Op(Op::Semi)) => {
                    separated = true;
                    self.pos += 1;
                }
                Some(Tok::Newline) => separated = true,
                _ => {}
            }
            items.push(ListItem { andor, background });
            if !separated {
                break;
            }
        }
        Ok(List { items })
    }

    fn and_or(&mut self) -> Result<AndOr, ParseError> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let op = if self.at_op(Op::AndIf) {
                AndOrOp::And
            } else if self.at_op(Op::OrIf) {
                AndOrOp::Or
            } else {
                break;
            };
            self.pos += 1;
            self.skip_newlines();
            if self.at_end() {
                return Err(ParseError::Incomplete);
            }
            rest.push((op, self.pipeline()?));
        }
        Ok(AndOr { first, rest })
    }

    fn pipeline(&mut self) -> Result<Pipeline, ParseError> {
        let mut negate = false;
        while self.at_word(b"!") {
            self.pos += 1;
            negate = !negate;
            self.skip_newlines();
        }
        let mut cmds = vec![self.command()?];
        while self.at_op(Op::Pipe) {
            self.pos += 1;
            self.skip_newlines();
            if self.at_end() {
                return Err(ParseError::Incomplete);
            }
            cmds.push(self.command()?);
        }
        Ok(Pipeline { negate, cmds })
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        if self.at_end() {
            return Err(ParseError::Incomplete);
        }

        // `name ( )` — a function definition, not a command called `name`.
        if let Some(name) = self.peek().and_then(word_text) {
            if is_name(name)
                && matches!(self.peek_at(1), Some(Tok::Op(Op::LParen)))
                && matches!(self.peek_at(2), Some(Tok::Op(Op::RParen)))
            {
                let name = name.to_vec();
                self.pos += 3;
                self.skip_newlines();
                if self.at_end() {
                    return Err(ParseError::Incomplete);
                }
                let body = self.compound()?;
                return Ok(Command {
                    kind: CommandKind::Function {
                        name,
                        body: Arc::new(body),
                    },
                    redirects: Vec::new(),
                });
            }
        }

        if self.at_compound_start() {
            return self.compound();
        }
        self.simple()
    }

    fn at_compound_start(&self) -> bool {
        if self.at_op(Op::LParen) {
            return true;
        }
        matches!(
            self.peek().and_then(word_text),
            Some(b"if")
                | Some(b"while")
                | Some(b"until")
                | Some(b"for")
                | Some(b"case")
                | Some(b"{")
        )
    }

    /// A compound command plus the redirections that apply to it as a whole.
    fn compound(&mut self) -> Result<Command, ParseError> {
        let kind = if self.at_op(Op::LParen) {
            self.pos += 1;
            let body = self.list(&[])?;
            if self.at_end() {
                return Err(ParseError::Incomplete);
            }
            if !self.at_op(Op::RParen) {
                return Err(ParseError::Syntax("missing )"));
            }
            self.pos += 1;
            CommandKind::Subshell(body)
        } else if self.at_word(b"{") {
            self.pos += 1;
            let body = self.list(&[b"}"])?;
            self.expect_reserved(b"}")?;
            CommandKind::Group(body)
        } else if self.at_word(b"if") {
            self.if_clause()?
        } else if self.at_word(b"while") || self.at_word(b"until") {
            let until = self.at_word(b"until");
            self.pos += 1;
            let cond = self.list(&[b"do"])?;
            if cond.is_empty() {
                return Err(self.empty_list("empty loop condition"));
            }
            self.expect_reserved(b"do")?;
            let body = self.list(&[b"done"])?;
            self.expect_reserved(b"done")?;
            CommandKind::Loop { until, cond, body }
        } else if self.at_word(b"for") {
            self.for_clause()?
        } else if self.at_word(b"case") {
            self.case_clause()?
        } else {
            return Err(ParseError::Syntax("expected a compound command"));
        };

        let mut redirects = Vec::new();
        while matches!(
            self.peek(),
            Some(Tok::Redir { .. }) | Some(Tok::Here { .. })
        ) {
            self.redirect(&mut redirects)?;
        }
        Ok(Command { kind, redirects })
    }

    fn if_clause(&mut self) -> Result<CommandKind, ParseError> {
        self.pos += 1;
        let mut arms = Vec::new();
        loop {
            let cond = self.list(&[b"then"])?;
            if cond.is_empty() {
                return Err(self.empty_list("empty if condition"));
            }
            self.expect_reserved(b"then")?;
            let body = self.list(&[b"elif", b"else", b"fi"])?;
            arms.push((cond, body));
            if self.at_word(b"elif") {
                self.pos += 1;
                continue;
            }
            break;
        }
        let otherwise = if self.at_word(b"else") {
            self.pos += 1;
            Some(self.list(&[b"fi"])?)
        } else {
            None
        };
        self.expect_reserved(b"fi")?;
        Ok(CommandKind::If { arms, otherwise })
    }

    fn for_clause(&mut self) -> Result<CommandKind, ParseError> {
        self.pos += 1;
        // Only running out of tokens can be completed; a non-word here is
        // wrong rather than unfinished.
        if self.at_end() {
            return Err(ParseError::Incomplete);
        }
        let name = match self.peek().and_then(word_text) {
            Some(n) if is_name(n) => n.to_vec(),
            _ => return Err(ParseError::Syntax("for needs a variable name")),
        };
        self.pos += 1;
        self.skip_newlines();

        let words = if self.at_word(b"in") {
            self.pos += 1;
            let mut collected = Vec::new();
            loop {
                match self.peek() {
                    Some(Tok::Word(w)) => {
                        collected.push(Word::raw(w.clone()));
                        self.pos += 1;
                    }
                    Some(Tok::Literal(w)) => {
                        collected.push(Word::literal(w.clone()));
                        self.pos += 1;
                    }
                    _ => break,
                }
            }
            Some(collected)
        } else {
            None
        };

        while self.at_op(Op::Semi) || matches!(self.peek(), Some(Tok::Newline)) {
            self.pos += 1;
        }
        self.expect_reserved(b"do")?;
        let body = self.list(&[b"done"])?;
        self.expect_reserved(b"done")?;
        Ok(CommandKind::For { name, words, body })
    }

    fn case_clause(&mut self) -> Result<CommandKind, ParseError> {
        self.pos += 1;
        let word = self.expect_word()?;
        self.skip_newlines();
        self.expect_reserved(b"in")?;

        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_end() {
                return Err(ParseError::Incomplete);
            }
            if self.at_word(b"esac") {
                self.pos += 1;
                break;
            }
            if self.at_op(Op::LParen) {
                self.pos += 1;
            }
            let mut patterns = Vec::new();
            loop {
                patterns.push(self.expect_word()?);
                if self.at_op(Op::Pipe) {
                    self.pos += 1;
                    continue;
                }
                if self.at_op(Op::RParen) {
                    self.pos += 1;
                    break;
                }
                if self.at_end() {
                    return Err(ParseError::Incomplete);
                }
                return Err(ParseError::Syntax("expected | or ) in a case pattern"));
            }
            let body = self.list(&[b"esac"])?;
            items.push(CaseItem { patterns, body });
            self.skip_newlines();
            if self.at_op(Op::DSemi) {
                self.pos += 1;
                continue;
            }
            if self.at_word(b"esac") {
                self.pos += 1;
                break;
            }
            if self.at_end() {
                return Err(ParseError::Incomplete);
            }
            return Err(ParseError::Syntax("expected ;; or esac"));
        }
        Ok(CommandKind::Case { word, items })
    }

    fn simple(&mut self) -> Result<Command, ParseError> {
        let mut assigns: Vec<Word> = Vec::new();
        let mut words: Vec<Word> = Vec::new();
        let mut redirects: Vec<Redirect> = Vec::new();

        loop {
            match self.peek() {
                Some(Tok::Redir { .. }) | Some(Tok::Here { .. }) => {
                    self.redirect(&mut redirects)?;
                }
                Some(Tok::Word(raw)) => {
                    if words.is_empty() && CLOSERS.contains(&raw.as_slice()) {
                        if assigns.is_empty() && redirects.is_empty() {
                            return Err(ParseError::Syntax("unexpected keyword"));
                        }
                        break;
                    }
                    if words.is_empty() && assignment_split(raw).is_some() {
                        assigns.push(Word::raw(raw.clone()));
                    } else {
                        words.push(Word::raw(raw.clone()));
                    }
                    self.pos += 1;
                }
                Some(Tok::Literal(raw)) => {
                    if words.is_empty() && assignment_split(raw).is_some() {
                        assigns.push(Word::literal(raw.clone()));
                    } else {
                        words.push(Word::literal(raw.clone()));
                    }
                    self.pos += 1;
                }
                _ => break,
            }
        }

        if words.is_empty() && assigns.is_empty() && redirects.is_empty() {
            return Err(ParseError::Syntax("expected a command"));
        }
        Ok(Command {
            kind: CommandKind::Simple { assigns, words },
            redirects,
        })
    }

    fn redirect(&mut self, out: &mut Vec<Redirect>) -> Result<(), ParseError> {
        match self.peek() {
            Some(Tok::Here { fd, body, expand }) => {
                let fd = fd.unwrap_or(0);
                let word = if *expand {
                    Word::raw(body.clone())
                } else {
                    Word::literal(body.clone())
                };
                self.pos += 1;
                out.push(Redirect {
                    fd,
                    kind: RedirKind::Input,
                    target: RedirTarget::Here(word),
                });
                Ok(())
            }
            Some(Tok::Redir { fd, op }) => {
                let (fd, op) = (*fd, *op);
                self.pos += 1;
                let operand = self.expect_word()?;
                let (default_fd, kind, dup) = match op {
                    RedirOp::In => (0, RedirKind::Input, false),
                    RedirOp::InOut => (0, RedirKind::InputOutput, false),
                    RedirOp::Out | RedirOp::OutBoth => (1, RedirKind::OutputTruncate, false),
                    RedirOp::Append => (1, RedirKind::OutputAppend, false),
                    RedirOp::DupIn => (0, RedirKind::Input, true),
                    RedirOp::DupOut => (1, RedirKind::OutputTruncate, true),
                };
                out.push(Redirect {
                    fd: fd.unwrap_or(default_fd),
                    kind,
                    target: if dup {
                        RedirTarget::Dup(operand)
                    } else {
                        RedirTarget::Path(operand)
                    },
                });
                // `&>path` is `>path 2>&1`, so the second half is synthesized.
                if op == RedirOp::OutBoth {
                    out.push(Redirect {
                        fd: 2,
                        kind: RedirKind::OutputTruncate,
                        target: RedirTarget::Dup(Word::literal(b"1".to_vec())),
                    });
                }
                Ok(())
            }
            None => Err(ParseError::Incomplete),
            Some(_) => Err(ParseError::Syntax("expected a redirection")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    fn p(src: &[u8]) -> Result<List, ParseError> {
        parse(&lex(src).expect("lexes"))
    }

    fn only(src: &[u8]) -> CommandKind {
        let list = p(src).expect("parses");
        assert_eq!(list.items.len(), 1, "{src:?}");
        let pipeline = &list.items[0].andor.first;
        assert_eq!(pipeline.cmds.len(), 1);
        pipeline.cmds[0].kind.clone()
    }

    fn argv(kind: &CommandKind) -> Vec<Vec<u8>> {
        match kind {
            CommandKind::Simple { words, .. } => words.iter().map(|w| w.text.clone()).collect(),
            other => panic!("not a simple command: {other:?}"),
        }
    }

    #[test]
    fn a_pipeline_has_no_stage_ceiling() {
        let src = b"a | b | c | d | e | f | g | h | i | j | k | l";
        let list = p(src).expect("parses");
        assert_eq!(list.items[0].andor.first.cmds.len(), 12);
    }

    #[test]
    fn a_command_has_no_argument_ceiling() {
        let mut src = b"cmd".to_vec();
        for i in 0u32..200 {
            src.extend_from_slice(b" a");
            src.extend_from_slice(alloc::format!("{i}").as_bytes());
        }
        assert_eq!(argv(&only(&src)).len(), 201);
    }

    #[test]
    fn assignments_precede_the_command_name_only() {
        match only(b"A=1 B=2 env C=3") {
            CommandKind::Simple { assigns, words } => {
                assert_eq!(assigns.len(), 2);
                assert_eq!(words.len(), 2);
                assert_eq!(words[1].text, b"C=3");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn if_elif_else_nests() {
        match only(b"if a; then b; elif c; then d; else e; fi") {
            CommandKind::If { arms, otherwise } => {
                assert_eq!(arms.len(), 2);
                assert!(otherwise.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn loops_and_their_negation() {
        assert!(matches!(
            only(b"while a; do b; done"),
            CommandKind::Loop { until: false, .. }
        ));
        assert!(matches!(
            only(b"until a; do b; done"),
            CommandKind::Loop { until: true, .. }
        ));
    }

    #[test]
    fn for_with_and_without_in() {
        match only(b"for i in 1 2 3; do echo $i; done") {
            CommandKind::For { name, words, .. } => {
                assert_eq!(name, b"i");
                assert_eq!(words.expect("word list").len(), 3);
            }
            other => panic!("{other:?}"),
        }
        match only(b"for i; do echo $i; done") {
            CommandKind::For { words, .. } => assert!(words.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_for_word_may_be_named_do() {
        match only(b"for f in do re mi; do echo $f; done") {
            CommandKind::For { words, .. } => assert_eq!(words.expect("words").len(), 3),
            other => panic!("{other:?}"),
        }
    }

    /// The last item may omit `;;`, but `esac` still has to land in command
    /// position: after `echo other` it would be an argument.
    #[test]
    fn case_patterns_and_an_omitted_final_terminator() {
        match only(b"case $x in a|b) echo ab;; *) echo other\nesac") {
            CommandKind::Case { items, .. } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].patterns.len(), 2);
                assert_eq!(items[1].patterns[0].text, b"*");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_case_item_may_be_parenthesized_and_empty() {
        match only(b"case $x in (a) ;; (b) ;; esac") {
            CommandKind::Case { items, .. } => {
                assert_eq!(items.len(), 2);
                assert!(items[0].body.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn functions_take_a_compound_body() {
        assert!(matches!(
            only(b"greet() { echo hi; }"),
            CommandKind::Function { .. }
        ));
        assert!(matches!(
            only(b"greet() ( echo hi )"),
            CommandKind::Function { .. }
        ));
        assert_eq!(
            p(b"greet() echo hi"),
            Err(ParseError::Syntax("expected a compound command"))
        );
    }

    #[test]
    fn a_reserved_word_in_argument_position_is_a_word() {
        assert_eq!(
            argv(&only(b"echo done fi esac")),
            vec![
                b"echo".to_vec(),
                b"done".to_vec(),
                b"fi".to_vec(),
                b"esac".to_vec()
            ]
        );
    }

    #[test]
    fn a_closer_where_a_command_belongs_is_a_syntax_error() {
        assert_eq!(p(b"fi"), Err(ParseError::Syntax("unexpected keyword")));
        assert_eq!(p(b"done"), Err(ParseError::Syntax("unexpected keyword")));
    }

    /// A malformed construct must be a syntax error, never `Incomplete`: a
    /// reader that is told to expect more input waits for the rest of a
    /// command that can never arrive, and swallows the whole script with it.
    #[test]
    fn a_malformed_construct_is_not_a_request_for_more_input() {
        assert_eq!(
            p(b"for; do echo no; done"),
            Err(ParseError::Syntax("for needs a variable name"))
        );
        assert_eq!(
            p(b"for 1x in a; do echo no; done"),
            Err(ParseError::Syntax("for needs a variable name"))
        );
    }

    #[test]
    fn unfinished_constructs_ask_for_more_input() {
        for src in [
            b"if true; then".as_slice(),
            b"if true",
            b"while true; do echo",
            b"for i in 1 2",
            b"case x in a) echo",
            b"{ echo hi",
            b"echo a &&",
            b"echo a |",
            b"f() ",
            // A keyword alone on its line: POSIX puts a `linebreak` at the
            // head of every `compound_list`, so the condition begins on the
            // next line and this is unfinished, not wrong.
            b"if",
            b"while",
            b"until",
            b"if true; then b; elif",
            b"for",
        ] {
            assert_eq!(p(src), Err(ParseError::Incomplete), "{src:?}");
        }
    }

    #[test]
    fn a_subshell_is_not_a_group() {
        assert!(matches!(only(b"(echo hi)"), CommandKind::Subshell(_)));
        assert!(matches!(only(b"{ echo hi; }"), CommandKind::Group(_)));
    }

    #[test]
    fn redirections_default_their_descriptor() {
        let list = p(b"cmd <in >out 2>>log 2>&1 <&-").expect("parses");
        let redirs = &list.items[0].andor.first.cmds[0].redirects;
        assert_eq!(redirs.len(), 5);
        assert_eq!((redirs[0].fd, redirs[0].kind), (0, RedirKind::Input));
        assert_eq!(
            (redirs[1].fd, redirs[1].kind),
            (1, RedirKind::OutputTruncate)
        );
        assert_eq!((redirs[2].fd, redirs[2].kind), (2, RedirKind::OutputAppend));
        assert!(matches!(redirs[3].target, RedirTarget::Dup(_)));
        assert_eq!(redirs[4].fd, 0);
    }

    #[test]
    fn and_shorthand_expands_to_two_redirections() {
        let list = p(b"cmd &>out").expect("parses");
        let redirs = &list.items[0].andor.first.cmds[0].redirects;
        assert_eq!(redirs.len(), 2);
        assert_eq!(redirs[1].fd, 2);
        assert!(matches!(&redirs[1].target, RedirTarget::Dup(w) if w.text == b"1"));
    }

    #[test]
    fn a_compound_command_takes_redirections() {
        let list = p(b"while read x; do echo $x; done < f").expect("parses");
        assert_eq!(list.items[0].andor.first.cmds[0].redirects.len(), 1);
    }

    #[test]
    fn newlines_separate_and_continue() {
        let list = p(b"echo a\necho b\n").expect("parses");
        assert_eq!(list.items.len(), 2);
        // A newline after `&&` continues the and-or list.
        let list = p(b"true &&\necho b\n").expect("parses");
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].andor.rest.len(), 1);
    }

    #[test]
    fn background_is_per_list_item() {
        let list = p(b"sleep 1 & echo now").expect("parses");
        assert_eq!(list.items.len(), 2);
        assert!(list.items[0].background);
        assert!(!list.items[1].background);
    }

    #[test]
    fn negation_inverts_a_pipeline() {
        let list = p(b"! false").expect("parses");
        assert!(list.items[0].andor.first.negate);
    }

    #[test]
    fn a_heredoc_becomes_an_input_redirection() {
        let list = p(b"cat <<E\nhi\nE\n").expect("parses");
        let redirs = &list.items[0].andor.first.cmds[0].redirects;
        assert_eq!(redirs.len(), 1);
        match &redirs[0].target {
            RedirTarget::Here(w) => {
                assert_eq!(w.text, b"hi\n");
                assert!(!w.literal);
            }
            other => panic!("{other:?}"),
        }
    }
}
