//! A private call's signature, from the `name(...)` code span in its docs,
//! and the type names it mentions.

use super::rustdoc::CRATE;
use super::text::{collapse, is_space, is_word};

/// The text of each inline code span in markdown `docs`.
fn code_spans(docs: &str) -> Vec<&str> {
    let b = docs.as_bytes();
    let n = b.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < n {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && b[j] == b'`' {
            j += 1;
        }
        let fence = &docs[i..j];
        let after = |k: usize| k + fence.len() < n && b[k + fence.len()] == b'`';
        let mut k = docs[j..].find(fence).map(|k| k + j);
        while let Some(at) = k
            && after(at)
        {
            k = docs[at + fence.len() + 1..]
                .find(fence)
                .map(|x| x + at + fence.len() + 1);
        }
        let Some(k) = k else {
            return spans;
        };
        spans.push(&docs[j..k]);
        i = k + fence.len();
    }
    spans
}

/// The first code span reading `name(...`, whitespace collapsed.
pub fn find(docs: &str, name: &str) -> Option<String> {
    let open = format!("{name}(");
    code_spans(docs)
        .into_iter()
        .map(collapse)
        .find(|text| text.starts_with(&open))
}

fn split_args(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i64;
    let mut cur = String::new();
    for c in text.chars() {
        if matches!(c, '(' | '[') {
            depth += 1;
        } else if matches!(c, ')' | ']') {
            depth -= 1;
        }
        if c == ',' && depth == 0 {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
}

/// `[A-Za-z_]\w*(::[A-Za-z_]\w*)*`, every leftmost match.
fn ident_paths(text: &str) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let start = |i: usize| {
        c.get(i)
            .is_some_and(|&x| x.is_ascii_alphabetic() || x == '_')
    };
    let word_end = |mut i: usize| {
        i += 1;
        while i < c.len() && is_word(c[i]) {
            i += 1;
        }
        i
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if !start(i) {
            i += 1;
            continue;
        }
        let from = i;
        i = word_end(i);
        while c.get(i) == Some(&':') && c.get(i + 1) == Some(&':') && start(i + 2) {
            i = word_end(i + 2);
        }
        out.push(c[from..i].iter().collect());
    }
    out
}

/// An argument's type, after `name:` (and not `name::`), when it has one.
fn arg_type(arg: &str) -> Option<&str> {
    let rest = arg.trim_start_matches(is_space);
    let first = rest.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let rest = rest[first.len_utf8()..].trim_start_matches(is_word);
    let rest = rest.trim_start_matches(is_space).strip_prefix(':')?;
    (!rest.starts_with(':')).then_some(rest)
}

pub fn segments(path: &str) -> Vec<&str> {
    path.split("::")
        .filter(|s| !matches!(*s, "crate" | "super" | "self" | CRATE))
        .collect()
}

/// The capitalised identifiers in a signature's argument types and return:
/// the type and constant names it can mean. A lowercase one is a primitive
/// or a field of a packing, `hotspot: (hot_x << 16) | hot_y`; a return is
/// often prose, `-> entries written`. `None` when the parentheses never close.
pub fn type_idents(sig: &str, name: &str) -> Option<Vec<String>> {
    let body: Vec<char> = sig.chars().skip(name.chars().count() + 1).collect();
    let mut depth = 1i64;
    let mut close = None;
    for (i, &c) in body.iter().enumerate() {
        if matches!(c, '(' | '[') {
            depth += 1;
        } else if matches!(c, ')' | ']') {
            depth -= 1;
            if depth == 0 {
                close = Some(i);
                break;
            }
        }
    }
    let close = close?;
    let args: String = body[..close].iter().collect();
    let mut found = Vec::new();
    for arg in split_args(&args) {
        if let Some(ty) = arg_type(&arg) {
            found.extend(ident_paths(ty));
        }
    }
    let rest: String = body[close + 1..].iter().collect();
    if let Some(ret) = rest.trim_matches(is_space).strip_prefix("->") {
        found.extend(ident_paths(ret));
    }
    Some(
        found
            .into_iter()
            .filter(|p| {
                segments(p).last().is_some_and(|last| {
                    last.starts_with(|c: char| c.is_ascii_uppercase()) && *last != "Self"
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_is_the_span_that_opens_with_the_call() {
        let docs = "Drives it: `gadget_ctl(op, arg: u64)`, `op` from ``a ` b``.\n`x(";
        assert_eq!(
            code_spans(docs),
            ["gadget_ctl(op, arg: u64)", "op", "a ` b"]
        );
        assert_eq!(
            find("`widget_get(out: *mut Widget,\nmax) -> n`", "widget_get").as_deref(),
            Some("widget_get(out: *mut Widget, max) -> n")
        );
        assert_eq!(find("`other()`", "widget_get"), None);
    }

    #[test]
    fn type_names_come_from_typed_arguments_and_the_return() {
        assert_eq!(
            type_idents(
                "knob_set(level, value: crate::a::Knob, f: [u8; LEN], m::X) -> Mode written",
                "knob_set"
            ),
            Some(vec!["crate::a::Knob".into(), "LEN".into(), "Mode".into()])
        );
        assert_eq!(type_idents("f(a: (u8, Self)", "f"), None);
        assert_eq!(
            type_idents("f(h: (hot_x << 16) | Hot) -> entries", "f"),
            Some(vec!["Hot".into()])
        );
    }
}
