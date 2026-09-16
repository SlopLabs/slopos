//! Syntax spans, one line at a time.
//!
//! A line is tokenized against the state its predecessor left — a block comment
//! or an unterminated raw string is exactly that state — so highlighting a
//! viewport costs the lines above it once and the viewport each frame, not the
//! file. That is the whole reason [`LineState`] is `Copy` and small: the
//! document keeps one per line and re-runs only the lines an edit invalidated.
//!
//! This is a lexer, not a parser. It resolves what a *character run* is, never
//! what a name means, which is why a keyword list and a delimiter table are
//! enough and why no language here needs a grammar.

use alloc::vec::Vec;

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Language {
    #[default]
    PlainText,
    Rust,
    Toml,
    Json,
    Markdown,
    Shell,
    C,
    Python,
}

impl Language {
    pub fn label(&self) -> &'static str {
        match self {
            Language::PlainText => "Plain Text",
            Language::Rust => "Rust",
            Language::Toml => "TOML",
            Language::Json => "JSON",
            Language::Markdown => "Markdown",
            Language::Shell => "Shell",
            Language::C => "C",
            Language::Python => "Python",
        }
    }

    /// What "toggle comment" inserts, or `None` for a language with no line
    /// comment.
    pub fn line_comment(&self) -> Option<&'static str> {
        match self {
            Language::Rust | Language::C => Some("//"),
            Language::Toml | Language::Shell | Language::Python => Some("#"),
            Language::Json | Language::Markdown | Language::PlainText => None,
        }
    }
}

/// Language for a path, by file name then by extension.
pub fn detect_language(path: &str) -> Language {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name {
        "Cargo.toml" | "Cargo.lock" => return Language::Toml,
        "justfile" | "Justfile" | ".bashrc" | ".profile" => return Language::Shell,
        _ => {}
    }
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext,
        _ => return Language::PlainText,
    };
    match ext {
        "rs" => Language::Rust,
        "toml" => Language::Toml,
        "json" => Language::Json,
        "md" | "markdown" => Language::Markdown,
        "sh" | "bash" | "slop" => Language::Shell,
        "c" | "h" | "cc" | "cpp" | "hpp" => Language::C,
        "py" => Language::Python,
        _ => Language::PlainText,
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    Text,
    Keyword,
    Type,
    Function,
    Macro,
    Constant,
    Number,
    Str,
    Comment,
    Attribute,
    Operator,
    Punctuation,
    /// A key in a key/value language, and a Markdown heading.
    Property,
    Emphasis,
    Link,
}

/// A run of one kind within a line, in **character** indices.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: TokenKind,
}

/// What a line hands its successor.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum LineState {
    #[default]
    Normal,
    /// Inside `/* */`; the depth is Rust's nesting, 1 for C.
    BlockComment(u16),
    /// Inside a Rust raw string with this many `#`s to close.
    RawString(u16),
    /// Inside a Python triple-quoted string; the flag picks `'''` over `"""`.
    TripleString(bool),
    /// Inside a Markdown fenced code block.
    CodeFence,
}

struct LangSpec {
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    constants: &'static [&'static str],
    line_comment: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str, bool)>,
    /// Quote characters that open a string.
    quotes: &'static [char],
    /// `r"…"` / `r#"…"#` raw strings.
    raw_strings: bool,
    /// `#[…]` attributes.
    attributes: bool,
    /// `name(` is a call, `name!` is a macro.
    call_syntax: bool,
    /// A capitalized identifier is a type.
    capitalized_types: bool,
    /// `"""…"""` strings, which span lines.
    triple_quotes: bool,
}

const RUST: LangSpec = LangSpec {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "unsafe", "use", "where", "while", "union", "yield",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8",
        "u16", "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result", "Box",
    ],
    constants: &["None", "Some", "Ok", "Err"],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/", true)),
    quotes: &['"', '\''],
    raw_strings: true,
    attributes: true,
    call_syntax: true,
    capitalized_types: true,
    triple_quotes: false,
};

const C_LANG: LangSpec = LangSpec {
    keywords: &[
        "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern",
        "for", "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static",
        "struct", "switch", "typedef", "union", "volatile", "while",
    ],
    types: &[
        "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void", "bool",
        "size_t", "uint8_t", "uint16_t", "uint32_t", "uint64_t",
    ],
    constants: &["NULL", "true", "false"],
    line_comment: &["//"],
    block_comment: Some(("/*", "*/", false)),
    quotes: &['"', '\''],
    raw_strings: false,
    attributes: false,
    call_syntax: true,
    capitalized_types: false,
    triple_quotes: false,
};

const SHELL: LangSpec = LangSpec {
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case",
        "esac", "function", "return", "export", "local", "readonly", "set", "unset", "shift",
        "break", "continue", "exit", "echo", "cd", "test",
    ],
    types: &[],
    constants: &["true", "false"],
    line_comment: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    raw_strings: false,
    attributes: false,
    call_syntax: false,
    capitalized_types: false,
    triple_quotes: false,
};

const PYTHON: LangSpec = LangSpec {
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
        "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is",
        "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
        "yield",
    ],
    types: &[
        "bool", "bytes", "dict", "float", "int", "list", "set", "str", "tuple",
    ],
    constants: &["None", "True", "False", "self"],
    line_comment: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    raw_strings: false,
    attributes: false,
    call_syntax: true,
    capitalized_types: true,
    triple_quotes: true,
};

/// Every language answers `line()` before reaching the lexer except these, so
/// the empty spec exists to keep the table total rather than to be run.
#[allow(dead_code)]
const PLAIN: LangSpec = LangSpec {
    keywords: &[],
    types: &[],
    constants: &[],
    line_comment: &[],
    block_comment: None,
    quotes: &[],
    raw_strings: false,
    attributes: false,
    call_syntax: false,
    capitalized_types: false,
    triple_quotes: false,
};

pub struct Highlighter {
    language: Language,
}

impl Highlighter {
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn set_language(&mut self, language: Language) {
        self.language = language;
    }

    /// Spans for `text`, given the state its predecessor ended in, plus the
    /// state this line hands on.
    pub fn line(&self, text: &str, state: LineState) -> (Vec<Span>, LineState) {
        match self.language {
            Language::PlainText => (Vec::new(), LineState::Normal),
            Language::Markdown => markdown_line(text, state),
            Language::Toml => (toml_line(text), LineState::Normal),
            Language::Json => (json_line(text), LineState::Normal),
            Language::Rust => lex(&RUST, text, state),
            Language::C => lex(&C_LANG, text, state),
            Language::Shell => lex(&SHELL, text, state),
            Language::Python => lex(&PYTHON, text, state),
        }
    }
}

fn push(spans: &mut Vec<Span>, start: usize, end: usize, kind: TokenKind) {
    if end > start && kind != TokenKind::Text {
        spans.push(Span { start, end, kind });
    }
}

fn starts_with_at(chars: &[char], at: usize, needle: &str) -> bool {
    let mut i = at;
    for c in needle.chars() {
        if i >= chars.len() || chars[i] != c {
            return false;
        }
        i += 1;
    }
    true
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// One generic C-family lexer, steered by `spec`.
fn lex(spec: &LangSpec, text: &str, state: LineState) -> (Vec<Span>, LineState) {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut i = 0usize;
    let mut state = state;

    // Finish whatever the previous line left open before lexing anything new.
    match state {
        LineState::BlockComment(depth) => {
            let (next_i, next_state) = continue_block_comment(spec, &chars, 0, depth);
            push(&mut spans, 0, next_i, TokenKind::Comment);
            i = next_i;
            state = next_state;
        }
        LineState::RawString(hashes) => {
            let (next_i, next_state) = continue_raw_string(&chars, 0, hashes);
            push(&mut spans, 0, next_i, TokenKind::Str);
            i = next_i;
            state = next_state;
        }
        LineState::TripleString(single) => {
            let (next_i, next_state) = continue_triple_string(&chars, 0, single);
            push(&mut spans, 0, next_i, TokenKind::Str);
            i = next_i;
            state = next_state;
        }
        LineState::Normal | LineState::CodeFence => {}
    }

    while i < chars.len() {
        let c = chars[i];

        if c.is_whitespace() {
            i += 1;
            continue;
        }

        if spec
            .line_comment
            .iter()
            .any(|p| starts_with_at(&chars, i, p))
        {
            push(&mut spans, i, chars.len(), TokenKind::Comment);
            return (spans, LineState::Normal);
        }

        if let Some((open, _, _)) = spec.block_comment {
            if starts_with_at(&chars, i, open) {
                let start = i;
                let (next_i, next_state) =
                    continue_block_comment(spec, &chars, i + open.chars().count(), 1);
                push(&mut spans, start, next_i, TokenKind::Comment);
                i = next_i;
                state = next_state;
                if state != LineState::Normal {
                    return (spans, state);
                }
                continue;
            }
        }

        if spec.attributes && c == '#' && chars.get(i + 1).is_some_and(|c| *c == '[' || *c == '!') {
            let start = i;
            let mut depth = 0usize;
            while i < chars.len() {
                match chars[i] {
                    '[' => depth += 1,
                    // A `]` with no `[` before it ends the run: the buffer is
                    // whatever was opened, and a `.rs` file need not be Rust.
                    ']' => {
                        i += 1;
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                        continue;
                    }
                    _ => {}
                }
                i += 1;
            }
            push(&mut spans, start, i, TokenKind::Attribute);
            continue;
        }

        if spec.raw_strings && (c == 'r' || c == 'b') {
            let mut j = i + 1;
            if c == 'b' && chars.get(j) == Some(&'r') {
                j += 1;
            }
            let hash_start = j;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let hashes = (j - hash_start) as u16;
                let start = i;
                let (next_i, next_state) = continue_raw_string(&chars, j + 1, hashes);
                push(&mut spans, start, next_i, TokenKind::Str);
                i = next_i;
                state = next_state;
                if state != LineState::Normal {
                    return (spans, state);
                }
                continue;
            }
        }

        if spec.quotes.contains(&c) {
            // A triple quote spans lines; a doubled quote that is not tripled is
            // an empty string and must not be read as one.
            if spec.triple_quotes && chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c) {
                let start = i;
                let (next_i, next_state) = continue_triple_string(&chars, i + 3, c == '\'');
                push(&mut spans, start, next_i, TokenKind::Str);
                i = next_i;
                state = next_state;
                if state != LineState::Normal {
                    return (spans, state);
                }
                continue;
            }
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i = (i + 2).min(chars.len());
                    continue;
                }
                if chars[i] == c {
                    i += 1;
                    break;
                }
                i += 1;
            }
            push(&mut spans, start, i, TokenKind::Str);
            continue;
        }

        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                i += 1;
            }
            push(&mut spans, start, i, TokenKind::Number);
            continue;
        }

        if is_ident_start(c) {
            let start = i;
            while i < chars.len() && is_ident(chars[i]) {
                i += 1;
            }
            let word: alloc::string::String = chars[start..i].iter().collect();

            let kind = if spec.keywords.contains(&word.as_str()) {
                TokenKind::Keyword
            } else if spec.types.contains(&word.as_str()) {
                TokenKind::Type
            } else if spec.constants.contains(&word.as_str()) {
                TokenKind::Constant
            } else if spec.call_syntax && chars.get(i) == Some(&'!') {
                i += 1;
                TokenKind::Macro
            } else if spec.call_syntax && next_non_space(&chars, i) == Some('(') {
                TokenKind::Function
            } else if spec.capitalized_types && word.starts_with(char::is_uppercase) {
                TokenKind::Type
            } else if word.chars().all(|c| !c.is_lowercase()) && word.len() > 1 {
                TokenKind::Constant
            } else {
                TokenKind::Text
            };
            push(&mut spans, start, i, kind);
            continue;
        }

        let start = i;
        while i < chars.len() && is_operator(chars[i]) {
            i += 1;
        }
        if i > start {
            push(&mut spans, start, i, TokenKind::Operator);
            continue;
        }

        i += 1;
        push(&mut spans, start, i, TokenKind::Punctuation);
    }

    (spans, state)
}

fn next_non_space(chars: &[char], from: usize) -> Option<char> {
    chars[from.min(chars.len())..]
        .iter()
        .copied()
        .find(|c| !c.is_whitespace())
}

fn is_operator(c: char) -> bool {
    matches!(
        c,
        '+' | '-' | '*' | '/' | '%' | '=' | '<' | '>' | '!' | '&' | '|' | '^' | '~' | '?' | ':'
    )
}

/// Scans from `at` while inside a block comment, returning where it stopped.
fn continue_block_comment(
    spec: &LangSpec,
    chars: &[char],
    at: usize,
    depth: u16,
) -> (usize, LineState) {
    let Some((open, close, nested)) = spec.block_comment else {
        return (chars.len(), LineState::Normal);
    };
    let mut depth = depth;
    let mut i = at;
    while i < chars.len() {
        if nested && starts_with_at(chars, i, open) {
            // Saturating: a line of ten thousand `/*` is a pathological input,
            // not a reason to wrap the depth counter round to zero.
            depth = depth.saturating_add(1);
            i += open.chars().count();
            continue;
        }
        if starts_with_at(chars, i, close) {
            i += close.chars().count();
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return (i, LineState::Normal);
            }
            continue;
        }
        i += 1;
    }
    (chars.len(), LineState::BlockComment(depth))
}

fn continue_raw_string(chars: &[char], at: usize, hashes: u16) -> (usize, LineState) {
    let mut i = at;
    while i < chars.len() {
        if chars[i] == '"' {
            let mut seen = 0u16;
            let mut j = i + 1;
            while seen < hashes && chars.get(j) == Some(&'#') {
                seen += 1;
                j += 1;
            }
            if seen == hashes {
                return (j, LineState::Normal);
            }
        }
        i += 1;
    }
    (chars.len(), LineState::RawString(hashes))
}

fn continue_triple_string(chars: &[char], at: usize, single: bool) -> (usize, LineState) {
    let quote = if single { '\'' } else { '"' };
    let mut i = at;
    while i < chars.len() {
        if chars[i] == quote && chars.get(i + 1) == Some(&quote) && chars.get(i + 2) == Some(&quote)
        {
            return (i + 3, LineState::Normal);
        }
        i += 1;
    }
    (chars.len(), LineState::TripleString(single))
}

fn toml_line(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let first = chars.iter().position(|c| !c.is_whitespace());
    let Some(first) = first else {
        return spans;
    };

    if chars[first] == '#' {
        push(&mut spans, first, chars.len(), TokenKind::Comment);
        return spans;
    }
    if chars[first] == '[' {
        let end = chars
            .iter()
            .rposition(|c| *c == ']')
            .map(|i| i + 1)
            .unwrap_or(chars.len());
        push(&mut spans, first, end, TokenKind::Property);
        return spans;
    }

    // `key = value`: the key is everything up to the first unquoted `=`.
    let eq = chars.iter().position(|c| *c == '=');
    let value_start = match eq {
        Some(eq) => {
            push(&mut spans, first, eq, TokenKind::Property);
            push(&mut spans, eq, eq + 1, TokenKind::Operator);
            eq + 1
        }
        None => first,
    };
    spans.extend(value_spans(&chars, value_start, &["#"]));
    spans
}

fn json_line(text: &str) -> Vec<Span> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i = (i + 2).min(chars.len());
                    continue;
                }
                if chars[i] == '"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let kind = if next_non_space(&chars, i) == Some(':') {
                TokenKind::Property
            } else {
                TokenKind::Str
            };
            push(&mut spans, start, i, kind);
            continue;
        }
        if c.is_ascii_digit() || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_digit()
                    || chars[i] == '.'
                    || chars[i] == 'e'
                    || chars[i] == 'E'
                    || chars[i] == '-'
                    || chars[i] == '+')
            {
                i += 1;
            }
            push(&mut spans, start, i, TokenKind::Number);
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < chars.len() && is_ident(chars[i]) {
                i += 1;
            }
            push(&mut spans, start, i, TokenKind::Constant);
            continue;
        }
        i += 1;
    }
    spans
}

/// Strings, numbers, booleans and a trailing comment in a value position.
fn value_spans(chars: &[char], from: usize, comments: &[&str]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut i = from;
    while i < chars.len() {
        let c = chars[i];
        if comments.iter().any(|p| starts_with_at(chars, i, p)) {
            push(&mut spans, i, chars.len(), TokenKind::Comment);
            break;
        }
        if c == '"' || c == '\'' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(chars.len());
            push(&mut spans, start, i, TokenKind::Str);
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            push(&mut spans, start, i, TokenKind::Number);
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            while i < chars.len() && is_ident(chars[i]) {
                i += 1;
            }
            let word: alloc::string::String = chars[start..i].iter().collect();
            if word == "true" || word == "false" {
                push(&mut spans, start, i, TokenKind::Constant);
            }
            continue;
        }
        i += 1;
    }
    spans
}

fn markdown_line(text: &str, state: LineState) -> (Vec<Span>, LineState) {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let fence = starts_with_at(&chars, 0, "```");

    if state == LineState::CodeFence {
        push(&mut spans, 0, chars.len(), TokenKind::Str);
        return (
            spans,
            if fence {
                LineState::Normal
            } else {
                LineState::CodeFence
            },
        );
    }
    if fence {
        push(&mut spans, 0, chars.len(), TokenKind::Comment);
        return (spans, LineState::CodeFence);
    }

    if chars.first() == Some(&'#') {
        push(&mut spans, 0, chars.len(), TokenKind::Property);
        return (spans, LineState::Normal);
    }
    if matches!(chars.first(), Some('>')) {
        push(&mut spans, 0, chars.len(), TokenKind::Comment);
        return (spans, LineState::Normal);
    }

    let mut i = 0usize;
    // A list marker is the bullet, not the item's text.
    let indent = chars.iter().take_while(|c| c.is_whitespace()).count();
    if matches!(chars.get(indent), Some('-') | Some('*') | Some('+'))
        && chars.get(indent + 1).is_some_and(|c| c.is_whitespace())
    {
        push(&mut spans, indent, indent + 1, TokenKind::Operator);
        i = indent + 1;
    }

    while i < chars.len() {
        match chars[i] {
            '`' => {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                push(&mut spans, start, i, TokenKind::Str);
            }
            '*' | '_' => {
                let marker = chars[i];
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != marker {
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                push(&mut spans, start, i, TokenKind::Emphasis);
            }
            '[' => {
                let start = i;
                while i < chars.len() && chars[i] != ']' {
                    i += 1;
                }
                i = (i + 1).min(chars.len());
                if chars.get(i) == Some(&'(') {
                    while i < chars.len() && chars[i] != ')' {
                        i += 1;
                    }
                    i = (i + 1).min(chars.len());
                }
                push(&mut spans, start, i, TokenKind::Link);
            }
            _ => i += 1,
        }
    }

    (spans, LineState::Normal)
}
