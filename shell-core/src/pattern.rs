//! POSIX pattern matching (§2.14), byte-wise in the C locale.
//!
//! The pattern is a [`QBuf`] because only *unquoted* `*`, `?` and `[` are
//! pattern characters: `echo "*"` must not glob.

use crate::qbuf::QBuf;
use alloc::vec::Vec;

/// Whether matching `pat` can mean anything other than byte equality.
pub fn has_meta(pat: &QBuf) -> bool {
    (0..pat.len()).any(|i| !pat.quoted(i) && matches!(pat.bytes[i], b'*' | b'?' | b'['))
}

/// Whether `name` matches `pat` in full.
pub fn matches(pat: &QBuf, name: &[u8]) -> bool {
    match_from(pat, name, false)
}

/// As [`matches`], but `/` is matched only by a literal `/`, which is what one
/// pathname component needs.
pub fn matches_component(pat: &QBuf, name: &[u8]) -> bool {
    match_from(pat, name, true)
}

/// The longest prefix of `name` that `pat` matches, or `None`.
pub fn match_prefix_longest(pat: &QBuf, name: &[u8]) -> Option<usize> {
    (0..=name.len())
        .rev()
        .find(|&end| matches(pat, &name[..end]))
}

/// The shortest prefix of `name` that `pat` matches, or `None`.
pub fn match_prefix_shortest(pat: &QBuf, name: &[u8]) -> Option<usize> {
    (0..=name.len()).find(|&end| matches(pat, &name[..end]))
}

/// The start of the longest suffix of `name` that `pat` matches, or `None`.
pub fn match_suffix_longest(pat: &QBuf, name: &[u8]) -> Option<usize> {
    (0..=name.len()).find(|&start| matches(pat, &name[start..]))
}

/// The start of the shortest suffix of `name` that `pat` matches, or `None`.
pub fn match_suffix_shortest(pat: &QBuf, name: &[u8]) -> Option<usize> {
    (0..=name.len())
        .rev()
        .find(|&start| matches(pat, &name[start..]))
}

/// Iterative matcher with a single backtrack point: a mismatch resumes the
/// most recent `*`, which is enough and makes the cost O(pattern × name) with
/// a constant stack. A recursion per `*` is exponential in their number —
/// `*a*a*a*a*a*a*a*a*b` against forty `a`s took eleven seconds — and both a
/// `case` subject and a `${x##pattern}` value are script-controlled.
fn match_from(pat: &QBuf, name: &[u8], no_slash: bool) -> bool {
    let mut pi = 0usize;
    let mut ni = 0usize;
    // (pattern index just past the last `*`, name index to resume from).
    let mut star: Option<(usize, usize)> = None;

    loop {
        if pi < pat.len() {
            if !pat.quoted(pi) && pat.bytes[pi] == b'*' {
                while pi < pat.len() && !pat.quoted(pi) && pat.bytes[pi] == b'*' {
                    pi += 1;
                }
                star = Some((pi, ni));
                continue;
            }
            if ni < name.len() {
                if let Some(next) = step(pat, pi, name[ni], no_slash) {
                    pi = next;
                    ni += 1;
                    continue;
                }
            }
        } else if ni == name.len() {
            return true;
        }

        let Some((resume_pi, resume_ni)) = star else {
            return false;
        };
        if resume_ni >= name.len() {
            return false;
        }
        // A wildcard inside one pathname component may not consume the
        // separator, so it cannot reach past it however far it backtracks.
        if no_slash && name[resume_ni] == b'/' {
            return false;
        }
        star = Some((resume_pi, resume_ni + 1));
        pi = resume_pi;
        ni = resume_ni + 1;
    }
}

/// Match one name byte against the pattern element at `pi`, returning the
/// pattern index just past that element.
fn step(pat: &QBuf, pi: usize, byte: u8, no_slash: bool) -> Option<usize> {
    let literal = pat.quoted(pi);
    let element = pat.bytes[pi];

    if !literal && element == b'?' {
        if no_slash && byte == b'/' {
            return None;
        }
        return Some(pi + 1);
    }
    if !literal && element == b'[' {
        return match bracket(pat, pi, no_slash) {
            Some((end, negated, set)) => {
                if no_slash && byte == b'/' {
                    return None;
                }
                if set_contains(&set, byte) == negated {
                    return None;
                }
                Some(end)
            }
            // Not a valid bracket expression: `[` matches itself.
            None => (byte == b'[').then_some(pi + 1),
        };
    }
    (byte == element).then_some(pi + 1)
}

enum Member {
    One(u8),
    Range(u8, u8),
    Class(Class),
}

#[derive(Clone, Copy)]
enum Class {
    Alpha,
    Digit,
    Alnum,
    Lower,
    Upper,
    Space,
    Blank,
    Punct,
    Print,
    Graph,
    Cntrl,
    Xdigit,
}

fn set_contains(set: &[Member], b: u8) -> bool {
    set.iter().any(|m| match *m {
        Member::One(x) => x == b,
        Member::Range(lo, hi) => (lo..=hi).contains(&b),
        Member::Class(c) => match c {
            Class::Alpha => b.is_ascii_alphabetic(),
            Class::Digit => b.is_ascii_digit(),
            Class::Alnum => b.is_ascii_alphanumeric(),
            Class::Lower => b.is_ascii_lowercase(),
            Class::Upper => b.is_ascii_uppercase(),
            Class::Space => b.is_ascii_whitespace() || b == 0x0b,
            Class::Blank => b == b' ' || b == b'\t',
            Class::Punct => b.is_ascii_punctuation(),
            Class::Print => b.is_ascii_graphic() || b == b' ',
            Class::Graph => b.is_ascii_graphic(),
            Class::Cntrl => b.is_ascii_control(),
            Class::Xdigit => b.is_ascii_hexdigit(),
        },
    })
}

fn class_by_name(name: &[u8]) -> Option<Class> {
    Some(match name {
        b"alpha" => Class::Alpha,
        b"digit" => Class::Digit,
        b"alnum" => Class::Alnum,
        b"lower" => Class::Lower,
        b"upper" => Class::Upper,
        b"space" => Class::Space,
        b"blank" => Class::Blank,
        b"punct" => Class::Punct,
        b"print" => Class::Print,
        b"graph" => Class::Graph,
        b"cntrl" => Class::Cntrl,
        b"xdigit" => Class::Xdigit,
        _ => return None,
    })
}

/// Parse a bracket expression at `pat[open]`, returning the index past the
/// `]`, whether it is negated, and its members.
///
/// `None` means the `[` opens nothing and POSIX requires it to match a literal
/// `[` — which a `/` inside a pathname-component bracket also causes.
fn bracket(pat: &QBuf, open: usize, no_slash: bool) -> Option<(usize, bool, Vec<Member>)> {
    let mut i = open + 1;
    let negated = matches!(pat.bytes.get(i), Some(b'!') | Some(b'^')) && !pat.quoted(i);
    if negated {
        i += 1;
    }
    let mut set: Vec<Member> = Vec::new();
    let first = i;
    while i < pat.len() {
        let b = pat.bytes[i];
        let raw = !pat.quoted(i);

        if raw && b == b']' && i > first {
            return Some((i + 1, negated, set));
        }
        if no_slash && b == b'/' {
            return None;
        }
        if raw && b == b'[' && pat.bytes.get(i + 1) == Some(&b':') {
            let mut j = i + 2;
            while j + 1 < pat.len() && !(pat.bytes[j] == b':' && pat.bytes[j + 1] == b']') {
                j += 1;
            }
            if j + 1 >= pat.len() {
                return None;
            }
            let class = class_by_name(&pat.bytes[i + 2..j])?;
            set.push(Member::Class(class));
            i = j + 2;
            continue;
        }
        // `a-z`, but a `-` that is first or last in the set is itself.
        let dash = pat.bytes.get(i + 1) == Some(&b'-') && !pat.quoted(i + 1);
        let end = pat.bytes.get(i + 2).copied();
        if dash && end.is_some_and(|e| e != b']') {
            set.push(Member::Range(b, end.unwrap()));
            i += 3;
            continue;
        }
        set.push(Member::One(b));
        i += 1;
    }
    None
}

/// Build a pattern whose every byte is a live pattern character.
pub fn unquoted(bytes: &[u8]) -> QBuf {
    let mut buf = QBuf::new();
    buf.extend(bytes, 0);
    buf
}

/// Build a pattern that can only match itself.
pub fn quoted(bytes: &[u8]) -> QBuf {
    let mut buf = QBuf::new();
    buf.extend(bytes, crate::qbuf::Q_QUOTED);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_question() {
        assert!(matches(&unquoted(b"*.rs"), b"main.rs"));
        assert!(!matches(&unquoted(b"*.rs"), b"main.rss"));
        assert!(matches(&unquoted(b"?at"), b"cat"));
        assert!(!matches(&unquoted(b"?at"), b"chat"));
        assert!(matches(&unquoted(b"*"), b""));
    }

    #[test]
    fn quoted_metacharacters_match_themselves() {
        assert!(!has_meta(&quoted(b"*")));
        assert!(matches(&quoted(b"*"), b"*"));
        assert!(!matches(&quoted(b"*"), b"anything"));
    }

    #[test]
    fn bracket_sets_ranges_and_negation() {
        assert!(matches(&unquoted(b"[abc]x"), b"bx"));
        assert!(!matches(&unquoted(b"[abc]x"), b"dx"));
        assert!(matches(&unquoted(b"[a-f]"), b"d"));
        assert!(matches(&unquoted(b"[!a-f]"), b"z"));
        assert!(!matches(&unquoted(b"[!a-f]"), b"c"));
        // `]` first in the set is a member, not the close.
        assert!(matches(&unquoted(b"[]a]"), b"]"));
        assert!(matches(&unquoted(b"[[:digit:]]"), b"7"));
        assert!(!matches(&unquoted(b"[[:digit:]]"), b"x"));
    }

    #[test]
    fn an_unclosed_bracket_is_a_literal_bracket() {
        assert!(matches(&unquoted(b"[abc"), b"[abc"));
    }

    /// Nine stars against forty bytes. The recursive matcher this replaced
    /// took eleven seconds to answer; a single-backtrack one is immediate,
    /// and both `${x##pattern}` and a `case` subject are script-controlled.
    #[test]
    fn many_stars_do_not_blow_up() {
        let name = [b'a'; 40];
        assert!(!matches(&unquoted(b"*a*a*a*a*a*a*a*a*b"), &name));
        assert!(matches(&unquoted(b"*a*a*a*a*a*a*a*a*a"), &name));
    }

    /// A run of stars is one star, including at the very end.
    #[test]
    fn adjacent_stars_collapse() {
        assert!(matches(&unquoted(b"a**b"), b"axyzb"));
        assert!(matches(&unquoted(b"a**"), b"a"));
        assert!(!matches_component(&unquoted(b"a**"), b"a/b"));
    }

    #[test]
    fn a_component_wildcard_never_crosses_a_slash() {
        assert!(!matches_component(&unquoted(b"*"), b"a/b"));
        assert!(!matches_component(&unquoted(b"a?c"), b"a/c"));
        assert!(matches(&unquoted(b"*"), b"a/b"));
    }

    #[test]
    fn trim_helpers_pick_the_right_edge() {
        assert_eq!(match_prefix_shortest(&unquoted(b"*."), b"a.b.c"), Some(2));
        assert_eq!(match_prefix_longest(&unquoted(b"*."), b"a.b.c"), Some(4));
        assert_eq!(match_suffix_longest(&unquoted(b".*"), b"a.b.c"), Some(1));
        assert_eq!(match_suffix_shortest(&unquoted(b".*"), b"a.b.c"), Some(3));
        assert_eq!(match_suffix_longest(&unquoted(b"z"), b"a.b.c"), None);
    }
}
