//! The layout of every struct and union `slibc/include` defines, from clang's
//! `-fdump-record-layouts-complete`, in its canonical form:
//!
//! ```text
//! <record> size <n> align <n>
//! <record> <dotted.member.path> <offset>     (one per named field, in order)
//! ```
//!
//! `<record>` is `struct X` / `union X`, or `typedef X` for an anonymous
//! record named by a typedef, found by reading the headers themselves. Field
//! types are dropped: their spelling varies across clang releases and the
//! offsets already carry what the ABI depends on. A record the canonical form
//! cannot name is an error, never a skip.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

const HEADING: &str = "*** Dumping AST Record Layout";
const COMPILER_RECORDS: &[&str] = &["struct __va_list_tag", "struct __NSConstantString_tag"];

/// Compile one translation unit including every header under `include` with
/// `clang`, and canonicalise its layout dump.
pub(super) fn measure(include: &Path, clang: &str) -> Result<String> {
    let mut headers = Vec::new();
    walk(include, &mut |path| {
        if path.extension().is_some_and(|e| e == "h") && path.is_file() && !path.is_symlink() {
            headers.push(path.to_path_buf());
        }
    })?;
    let mut names: Vec<String> = headers
        .iter()
        .filter_map(|h| h.strip_prefix(include).ok())
        .map(|h| h.to_string_lossy().replace('\\', "/"))
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!("no headers under {}", include.display()).into());
    }
    let unit: String = names.iter().map(|n| format!("#include <{n}>\n")).collect();
    let tu = std::env::temp_dir().join(format!("slopos-libc-layouts-{}.c", std::process::id()));
    std::fs::write(&tu, unit).map_err(|e| format!("{}: {e}", tu.display()))?;
    let out = Command::new(clang)
        .args(["--target=x86_64-unknown-slopos", "-nostdlibinc", "-isystem"])
        .arg(include)
        .args([
            "-std=c11",
            "-fsyntax-only",
            "-Xclang",
            "-fdump-record-layouts-complete",
        ])
        .arg(&tu)
        .output();
    let _ = std::fs::remove_file(&tu);
    let out = out.map_err(|e| format!("cannot run {clang} (set CLANG to a clang): {e}"))?;
    if !out.status.success() {
        let err: String = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(|l| format!("\n    {l}"))
            .collect();
        return Err(format!(
            "{clang} could not compile the headers under {}:{err}",
            include.display()
        )
        .into());
    }
    canonical(
        &String::from_utf8_lossy(&out.stdout),
        &Headers::read(include)?,
    )
    .map_err(|e| {
        crate::Error::from(format!(
            "the layout dump of {} did not parse: {e}",
            include.display()
        ))
    })
}

fn walk(dir: &Path, found: &mut dyn FnMut(&Path)) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk(&path, found)?;
        } else {
            found(&path);
        }
    }
    Ok(())
}

/// The headers' text with comments blanked, and the record names they define.
pub(super) struct Headers {
    root: PathBuf,
    text: HashMap<PathBuf, Vec<u8>>,
    tags: HashSet<String>,
    typedefs: HashSet<String>,
}

impl Headers {
    pub(super) fn read(root: &Path) -> Result<Self> {
        let root = std::fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
        let mut paths = Vec::new();
        walk(&root, &mut |path| {
            if path.extension().is_some_and(|e| e == "h") {
                paths.push(path.to_path_buf());
            }
        })?;
        paths.sort();
        let mut headers = Self {
            root,
            text: HashMap::new(),
            tags: HashSet::new(),
            typedefs: HashSet::new(),
        };
        for path in paths {
            let raw = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let text = blank_comments(&raw);
            headers.tags.extend(tag_definitions(&text));
            for start in anonymous_typedefs(&text) {
                if let Some(name) = typedef_name(&text, start, &path.display().to_string())? {
                    headers.typedefs.insert(name);
                }
            }
            headers.text.insert(path, text);
        }
        Ok(headers)
    }

    fn owns(&self, path: &str) -> bool {
        std::fs::canonicalize(path)
            .unwrap_or_else(|_| PathBuf::from(path))
            .starts_with(&self.root)
    }

    /// The typedef naming the anonymous record at `path:line:col`, if one does.
    fn at(&self, path: &str, line: usize, col: usize) -> Result<Option<String>> {
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        let Some(text) = self.text.get(&real) else {
            return Err(format!("{path} is not a header under {}", self.root.display()).into());
        };
        let lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
        if line == 0 || line > lines.len() {
            return Err(format!("{path} has no line {line}").into());
        }
        let offset = lines[..line - 1].iter().map(|l| l.len() + 1).sum::<usize>() + col;
        typedef_name(
            text,
            offset.saturating_sub(1),
            &format!("{path}:{line}:{col}"),
        )
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn skip_space(text: &[u8], mut i: usize) -> usize {
    while i < text.len() && is_space(text[i]) {
        i += 1;
    }
    i
}

fn word_end(text: &[u8], i: usize) -> usize {
    i + text[i..].iter().take_while(|&&b| is_word(b)).count()
}

fn find(text: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    text.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

/// `needle` at `i` as a whole word on its left.
fn keyword_at(text: &[u8], i: usize, needle: &[u8]) -> bool {
    text[i..].starts_with(needle) && (i == 0 || !is_word(text[i - 1]))
}

/// Comments blanked to spaces, newlines kept, so line:col stays valid.
fn blank_comments(text: &[u8]) -> Vec<u8> {
    let mut out = text.to_vec();
    let mut i = 0;
    while i < text.len() {
        let end = if text[i..].starts_with(b"/*") {
            find(text, i + 2, b"*/").map(|e| e + 2)
        } else if text[i..].starts_with(b"//") {
            Some(find(text, i, b"\n").unwrap_or(text.len()))
        } else {
            None
        };
        let Some(end) = end else {
            i += 1;
            continue;
        };
        for b in &mut out[i..end] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
        i = end;
    }
    out
}

/// `struct X {` / `union X {`, attributes between allowed: each as `struct X`.
fn tag_definitions(text: &[u8]) -> Vec<String> {
    let mut tags = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let kind = [&b"struct"[..], b"union"]
            .into_iter()
            .find(|k| keyword_at(text, i, k));
        if let Some(kind) = kind
            && text.get(i + kind.len()).is_some_and(|&b| is_space(b))
            && let Some((name, end)) = attributes_then_name(text, skip_space(text, i + kind.len()))
        {
            tags.push(format!(
                "{} {}",
                String::from_utf8_lossy(kind),
                String::from_utf8_lossy(&text[name])
            ));
            i = end;
            continue;
        }
        i += 1;
    }
    tags
}

/// `(__attribute__((...)))* NAME {` at `i`: the name and the end past `{`.
/// Every `))` is tried as an attribute's end, as a backtracking matcher would.
fn attributes_then_name(text: &[u8], i: usize) -> Option<(std::ops::Range<usize>, usize)> {
    const ATTRIBUTE: &[u8] = b"__attribute__";
    if text[i..].starts_with(ATTRIBUTE) {
        let open = skip_space(text, i + ATTRIBUTE.len());
        if text[open..].starts_with(b"((") {
            let mut from = open + 2;
            while let Some(close) = find(text, from, b"))") {
                if let Some(found) = attributes_then_name(text, skip_space(text, close + 2)) {
                    return Some(found);
                }
                from = close + 1;
            }
        }
    }
    if i < text.len() && is_ident_start(text[i]) {
        let end = word_end(text, i);
        let brace = skip_space(text, end);
        if text.get(brace) == Some(&b'{') {
            return Some((i..end, brace + 1));
        }
    }
    None
}

/// Where `typedef struct {` / `typedef union {` puts its keyword.
fn anonymous_typedefs(text: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    for i in 0..text.len() {
        if !keyword_at(text, i, b"typedef") {
            continue;
        }
        let after = i + b"typedef".len();
        if !text.get(after).is_some_and(|&b| is_space(b)) {
            continue;
        }
        let kw = skip_space(text, after);
        for kind in [&b"struct"[..], b"union"] {
            if text[kw..].starts_with(kind)
                && text.get(skip_space(text, kw + kind.len())) == Some(&b'{')
            {
                starts.push(kw);
            }
        }
    }
    starts
}

/// The typedef closing the anonymous record whose keyword is at `start`: the
/// first identifier after its matching `}`. None when no typedef opens it.
fn typedef_name(text: &[u8], start: usize, place: &str) -> Result<Option<String>> {
    let start = start.min(text.len());
    let segment = text[..start]
        .iter()
        .rposition(|&b| matches!(b, b';' | b'{' | b'}'))
        .map_or(0, |p| p + 1);
    let segment = &text[segment..start];
    let typedef = (0..segment.len()).any(|i| {
        keyword_at(segment, i, b"typedef")
            && segment
                .get(i + b"typedef".len())
                .is_none_or(|&b| !is_word(b))
    });
    if !typedef {
        return Ok(None);
    }
    let opening =
        find(text, start, b"{").ok_or_else(|| format!("no body for the record at {place}"))?;
    let mut depth = 0usize;
    for i in opening..text.len() {
        match text[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let end = find(text, i, b";").unwrap_or(text.len());
                    return declarator(&text[i + 1..end]).map(Some).ok_or_else(|| {
                        format!("no typedef name closes the record at {place}").into()
                    });
                }
            }
            _ => {}
        }
    }
    Err(format!("unbalanced braces after {place}").into())
}

/// The first identifier at paren depth 0 that is not itself called or
/// followed by another word: `} __slibc_aligned(8) siginfo_t;`.
fn declarator(tail: &[u8]) -> Option<String> {
    let mut i = 0;
    while i < tail.len() {
        if is_ident_start(tail[i]) {
            let end = word_end(tail, i);
            let next = skip_space(tail, end);
            if !tail.get(next).is_some_and(|&b| is_word(b) || b == b'(') {
                let opens = tail[..i].iter().filter(|&&b| b == b'(').count();
                let closes = tail[..i].iter().filter(|&&b| b == b')').count();
                if opens == closes {
                    return Some(String::from_utf8_lossy(&tail[i..end]).into_owned());
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// A dump row, `<offset> | <text>`; the offset is empty on a heading's
/// continuation and the size row.
fn row(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let token = &trimmed[..trimmed.find(char::is_whitespace).unwrap_or(trimmed.len())];
    if let Some(text) = trimmed[token.len()..].strip_prefix(" | ") {
        return Some((token, text));
    }
    let lead = &line[..line.len() - trimmed.len()];
    match trimmed.strip_prefix("| ") {
        Some(text) if lead.ends_with(' ') => Some(("", text)),
        _ => None,
    }
}

/// `(anonymous|unnamed [struct|union] at PATH:LINE:COL)`.
fn unnamed(rest: &str) -> Option<(&str, usize, usize)> {
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
    let inner = inner
        .strip_prefix("anonymous")
        .or_else(|| inner.strip_prefix("unnamed"))?;
    let inner = inner
        .strip_prefix(" struct")
        .or_else(|| inner.strip_prefix(" union"))
        .unwrap_or(inner);
    let (place, col) = inner.strip_prefix(" at ")?.rsplit_once(':')?;
    let (path, line) = place.rsplit_once(':')?;
    let number = |s: &str| {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    (!path.is_empty()).then_some(())?;
    Some((path, number(line)?, number(col)?))
}

/// A member row that names no field: an anonymous struct or union member.
fn anonymous_member(body: &str) -> bool {
    let Some(body) = body.trim_end().strip_suffix(')') else {
        return false;
    };
    let Some(open) = body.rfind('(') else {
        return false;
    };
    let inner = &body[open + 1..];
    !inner.contains(')')
        && inner
            .strip_prefix("anonymous")
            .or_else(|| inner.strip_prefix("unnamed"))
            .is_some_and(|rest| rest.contains(" at "))
}

/// The canonical name of a dumped record, or None when it is not the headers'.
fn record_key(heading: &str, headers: &Headers) -> Result<Option<String>> {
    if heading.contains("::") {
        return Ok(None);
    }
    let (kind, rest) = heading.split_once(' ').unwrap_or((heading, ""));
    if kind != "struct" && kind != "union" {
        return Err(format!("unrecognised record heading: {heading:?}").into());
    }
    if let Some((path, line, col)) = unnamed(rest) {
        if !headers.owns(path) {
            return Ok(None);
        }
        // clang 18 heads a nested anonymous member without its parent's name.
        return Ok(headers
            .at(path, line, col)?
            .map(|name| format!("typedef {name}")));
    }
    let ident = rest.bytes().next().is_some_and(is_ident_start)
        && rest.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !ident {
        return Err(format!("unrecognised record heading: {heading:?}").into());
    }
    let tag = format!("{kind} {rest}");
    if headers.tags.contains(&tag) {
        return Ok(Some(tag));
    }
    if headers.typedefs.contains(rest) {
        return Ok(Some(format!("typedef {rest}")));
    }
    if COMPILER_RECORDS.contains(&tag.as_str()) {
        return Ok(None);
    }
    Err(format!(
        "{heading:?} is in the dump but defined in no header under {}",
        headers.root.display()
    )
    .into())
}

/// The first `<key>N` in `text` whose preceding character passes `before`.
fn number_after<'t>(text: &'t str, key: &str, before: fn(char) -> bool) -> Option<&'t str> {
    text.match_indices(key).find_map(|(at, _)| {
        if !text[..at].chars().next_back().is_none_or(before) {
            return None;
        }
        let digits = &text[at + key.len()..];
        let n = digits.bytes().take_while(u8::is_ascii_digit).count();
        (n > 0).then(|| &digits[..n])
    })
}

/// The canonical lines of every record in `dump` the headers define, sorted
/// by record.
pub(super) fn canonical(dump: &str, headers: &Headers) -> Result<String> {
    let lines: Vec<&str> = dump.split('\n').collect();
    let mut records: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen = 0;
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != HEADING {
            i += 1;
            continue;
        }
        seen += 1;
        i += 1;
        while i < lines.len() && lines[i].trim().is_empty() {
            i += 1;
        }
        let Some((_, heading)) = lines.get(i).and_then(|l| row(l)) else {
            return Err(format!("no record heading after dump marker {seen}").into());
        };
        let key = record_key(heading.trim(), headers)?;
        let shown = key.as_deref().unwrap_or("a skipped record");
        let mut fields = Vec::new();
        let mut stack: Vec<&str> = Vec::new();
        let mut size = None;
        i += 1;
        while i < lines.len() {
            let Some((offset, text)) = row(lines[i]) else {
                return Err(format!("unterminated layout for {shown}").into());
            };
            i += 1;
            if text.starts_with('[') {
                let s = number_after(text, "sizeof=", |c| !(c.is_alphanumeric() || c == '_'));
                let a = number_after(text, "align=", |c| !c.is_ascii_alphabetic());
                let (Some(s), Some(a)) = (s, a) else {
                    return Err(format!("no sizeof/align for {shown}: {text:?}").into());
                };
                size = Some((s, a));
                break;
            }
            let body = text.trim_start_matches(' ');
            let depth = (text.len() - body.len()) / 2;
            if depth < 1 || offset.is_empty() {
                return Err(format!("malformed field row in {shown}: {text:?}").into());
            }
            let anonymous = body.ends_with(' ') || !body.contains(' ') || anonymous_member(body);
            let name = match body.rsplit_once(' ') {
                Some((_, name)) if !anonymous => name,
                _ => "",
            };
            stack.truncate(depth - 1);
            stack.push(name);
            if !name.is_empty() {
                let path: Vec<&str> = stack.iter().copied().filter(|n| !n.is_empty()).collect();
                fields.push(format!("{shown} {} {offset}", path.join(".")));
            }
        }
        let Some((size, align)) = size else {
            return Err(format!("layout for {shown} ends without its size").into());
        };
        let Some(key) = key else {
            continue;
        };
        let mut block = vec![format!("{key} size {size} align {align}")];
        block.extend(fields);
        if records.get(&key).is_some_and(|b| *b != block) {
            return Err(format!("two different layouts are both named {key}").into());
        }
        records.insert(key, block);
    }
    if seen == 0 {
        return Err("no record layouts in the dump".into());
    }
    Ok(records.into_values().flatten().map(|l| l + "\n").collect())
}

#[cfg(test)]
pub(super) mod tests {
    use std::fmt::Write as _;

    use super::*;
    use crate::fixture::Tree;

    pub const MIN_RECORDS: usize = super::super::MIN_RECORDS;

    /// A header set whose records cover every form the canonical form handles:
    /// a nested record, an anonymous union member, a bit-field, an anonymous
    /// record named by an attributed typedef (siginfo_t's form; the attribute
    /// binds to the typedef, so the record keeps align 4). `pad` blank lines
    /// move every line number; `extra` adds one record.
    pub fn headers(tree: &Tree, dir: &str, pad: usize, extra: bool) {
        let mut fx = "\n".repeat(pad);
        fx.push_str(
            "/* fixture */
#include <stddef.h>
struct inner {
    int a;
    int b;
};
struct outer {
    int x;
    struct inner in;
    union {
        int u1;
        long u2;
    };
    unsigned int bf : 3;
};
typedef struct {
    int m;
} __attribute__((aligned(8))) anon_t;
union val {
    int i;
    void *p;
};
/* struct mystery { int m; }; is prose, not a definition */
",
        );
        if extra {
            fx.push_str("struct extra { int e; };\n");
        }
        tree.write(&format!("{dir}/fx.h"), &fx);
        let fill: String = (1..=MIN_RECORDS)
            .map(|i| format!("struct fill_{i:02} {{ int f; }};\n"))
            .collect();
        tree.write(&format!("{dir}/fill.h"), &fill);
    }

    /// The canonical lines of `headers(_, _, _, false)`.
    pub fn expected() -> String {
        let mut lines: Vec<String> = "struct inner size 8 align 4
struct inner a 0
struct inner b 4
struct outer size 32 align 8
struct outer x 0
struct outer in 4
struct outer in.a 4
struct outer in.b 8
struct outer u1 16
struct outer u2 16
struct outer bf 24:0-2
typedef anon_t size 4 align 4
typedef anon_t m 0
union val size 8 align 8
union val i 0
union val p 0"
            .lines()
            .map(str::to_string)
            .collect();
        for i in 1..=MIN_RECORDS {
            lines.push(format!("struct fill_{i:02} size 4 align 4"));
            lines.push(format!("struct fill_{i:02} f 0"));
        }
        let key = |l: &String| l.split(' ').take(2).collect::<Vec<_>>().join(" ");
        lines.sort_by_key(key);
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    #[derive(Clone, Copy, PartialEq)]
    pub enum Variant {
        Base,
        Size,
        Offset,
        Gone,
        New,
        Clang18,
    }

    /// What clang prints for `headers`, written out by hand in its dump
    /// format, plus the records the canonical form must skip. `Clang18` is
    /// `Base` as clang 18 spells a nested anonymous record, without its
    /// parent's name.
    pub fn dump(tree: &Tree, dir: &str, pad: usize, variant: Variant) -> String {
        let fx = tree.repo.path(dir).join("fx.h");
        let fx = fx.display();
        let inner_size = if variant == Variant::Size { 12 } else { 8 };
        let bf = if variant == Variant::Offset {
            "28:0-2"
        } else {
            "24:0-2"
        };
        let (anon, anon_head) = if variant == Variant::Clang18 {
            (
                format!("(anonymous at {fx}:{}:5)", 10 + pad),
                format!("(unnamed at {fx}:{}:5)", 10 + pad),
            )
        } else {
            let anon = format!("outer::(anonymous at {fx}:{}:5)", 10 + pad);
            (anon.clone(), anon)
        };
        let mut out = format!(
            "
*** Dumping AST Record Layout
         0 | struct __va_list_tag
         0 |   unsigned int gp_offset
         4 |   unsigned int fp_offset
         8 |   void * overflow_arg_area
        16 |   void * reg_save_area
           | [sizeof=24, align=8]

*** Dumping AST Record Layout
         0 | struct (unnamed at /nonexistent/clang/include/__stddef_max_align_t.h:19:9)
         0 |   long long __clang_max_align_nonce1
        16 |   long double __clang_max_align_nonce2
           | [sizeof=32, align=16]

*** Dumping AST Record Layout
         0 | struct inner
         0 |   int a
         4 |   int b
           | [sizeof={inner_size}, align=4]

*** Dumping AST Record Layout
         0 | union {anon_head}
         0 |   int u1
         0 |   long u2
           | [sizeof=8, align=8]

*** Dumping AST Record Layout
         0 | struct outer
         0 |   int x
         4 |   struct inner in
         4 |     int a
         8 |     int b
        16 |   union {anon} 
        16 |     int u1
        16 |     long u2
{bf:>10} |   unsigned int bf
           | [sizeof=32, align=8]

*** Dumping AST Record Layout
         0 | struct (unnamed at {fx}:{}:9)
         0 |   int m
           | [sizeof=4, align=4]
",
            16 + pad
        );
        if variant != Variant::Gone {
            out.push_str(
                "
*** Dumping AST Record Layout
         0 | union val
         0 |   int i
         0 |   void * p
           | [sizeof=8, align=8]
",
            );
        }
        if variant == Variant::New {
            out.push_str(
                "
*** Dumping AST Record Layout
         0 | struct extra
         0 |   int e
           | [sizeof=4, align=4]
",
            );
        }
        for i in 1..=MIN_RECORDS {
            let _ = write!(
                out,
                "\n*** Dumping AST Record Layout\n         0 | struct fill_{i:02}\n         0 |   int f\n           | [sizeof=4, align=4]\n"
            );
        }
        out
    }

    pub fn canonical_of(tree: &Tree, dir: &str, dump: &str) -> Result<String> {
        canonical(dump, &Headers::read(&tree.repo.path(dir))?)
    }

    #[test]
    fn the_canonical_form_names_nested_anonymous_and_typedef_records_and_skips_the_compilers() {
        let tree = Tree::new(&[]);
        headers(&tree, "inc", 0, false);
        let dump = dump(&tree, "inc", 0, Variant::Base);
        assert_eq!(canonical_of(&tree, "inc", &dump).unwrap(), expected());
    }

    #[test]
    fn clang_18s_heading_for_a_nested_anonymous_record_reads_the_same() {
        let tree = Tree::new(&[]);
        headers(&tree, "inc", 0, false);
        let dump = dump(&tree, "inc", 0, Variant::Clang18);
        assert_eq!(canonical_of(&tree, "inc", &dump).unwrap(), expected());
    }

    #[test]
    fn moved_line_numbers_give_the_same_canonical_lines() {
        let tree = Tree::new(&[]);
        headers(&tree, "moved", 3, false);
        let dump = dump(&tree, "moved", 3, Variant::Base);
        assert_eq!(canonical_of(&tree, "moved", &dump).unwrap(), expected());
    }

    #[test]
    fn a_named_record_no_header_defines_is_an_error_not_a_skip() {
        let tree = Tree::new(&[]);
        headers(&tree, "inc", 0, false);
        let mut dump = dump(&tree, "inc", 0, Variant::Base);
        dump.push_str(
            "\n*** Dumping AST Record Layout\n         0 | struct mystery\n         0 |   int m\n           | [sizeof=4, align=4]\n",
        );
        let err = canonical_of(&tree, "inc", &dump).unwrap_err().to_string();
        assert!(err.contains("defined in no header"), "{err}");
    }

    #[test]
    fn a_dump_with_no_record_is_an_error() {
        let tree = Tree::new(&[]);
        headers(&tree, "inc", 0, false);
        assert!(canonical_of(&tree, "inc", "").is_err());
    }

    #[test]
    fn a_typedef_name_after_an_attribute_macro_is_the_declarator() {
        assert_eq!(
            declarator(b" __slibc_aligned(8) siginfo_t"),
            Some("siginfo_t".into())
        );
        assert_eq!(
            declarator(b" __attribute__((aligned(8))) anon_t"),
            Some("anon_t".into())
        );
        assert_eq!(declarator(b" ("), None);
    }

    #[test]
    fn an_attributed_tag_definition_is_found_and_a_comment_hides_one() {
        let text = blank_comments(
            b"struct __attribute__((aligned(8))) a { int x; };\n/* struct b { int y; }; */ union c{};",
        );
        assert_eq!(tag_definitions(&text), ["struct a", "union c"]);
    }
}
