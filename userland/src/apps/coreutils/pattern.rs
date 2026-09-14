//! `fnmatch(3)`'s glob grammar: `*`, `?`, `[a-z]`, `[!x]`, backslash escapes.
//!
//! Iterative with a single backtrack point rather than recursive: a pattern is
//! attacker-supplied in `find -name`, and a recursive matcher's depth is the
//! pattern's length.

/// Match `name` against `pattern`. `*` and `?` match `/` like any other byte:
/// `find -name` only ever sees a base name, and `find -path` is defined to
/// treat the separator as ordinary.
pub fn fnmatch(pattern: &[u8], name: &[u8]) -> bool {
    let mut p = 0usize;
    let mut n = 0usize;
    let mut star: Option<(usize, usize)> = None;

    loop {
        if p < pattern.len() {
            match pattern[p] {
                b'*' => {
                    while p < pattern.len() && pattern[p] == b'*' {
                        p += 1;
                    }
                    star = Some((p, n));
                    continue;
                }
                b'?' => {
                    if n < name.len() {
                        p += 1;
                        n += 1;
                        continue;
                    }
                }
                b'[' => {
                    if n < name.len() {
                        if let Some(end) = class_end(pattern, p) {
                            if class_matches(&pattern[p + 1..end], name[n]) {
                                p = end + 1;
                                n += 1;
                                continue;
                            }
                        } else if name[n] == b'[' {
                            // An unterminated `[` is a literal bracket.
                            p += 1;
                            n += 1;
                            continue;
                        }
                    }
                }
                b'\\' if p + 1 < pattern.len() => {
                    if n < name.len() && name[n] == pattern[p + 1] {
                        p += 2;
                        n += 1;
                        continue;
                    }
                }
                literal => {
                    if n < name.len() && name[n] == literal {
                        p += 1;
                        n += 1;
                        continue;
                    }
                }
            }
        } else if n == name.len() {
            return true;
        }

        match star {
            Some((resume_p, resume_n)) if resume_n < name.len() => {
                p = resume_p;
                n = resume_n + 1;
                star = Some((resume_p, n));
            }
            _ => return false,
        }
    }
}

/// The index of the `]` closing a bracket expression opened at `open`, or
/// `None` when there is none.
fn class_end(pattern: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if pattern.get(i) == Some(&b'!') || pattern.get(i) == Some(&b'^') {
        i += 1;
    }
    // A `]` first is a literal member, not the terminator.
    if pattern.get(i) == Some(&b']') {
        i += 1;
    }
    while i < pattern.len() {
        if pattern[i] == b']' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn class_matches(body: &[u8], byte: u8) -> bool {
    let (negated, body) = match body.first() {
        Some(b'!') | Some(b'^') => (true, &body[1..]),
        _ => (false, body),
    };

    let mut found = false;
    let mut i = 0usize;
    while i < body.len() {
        if i + 2 < body.len() && body[i + 1] == b'-' {
            if body[i] <= byte && byte <= body[i + 2] {
                found = true;
            }
            i += 3;
        } else {
            if body[i] == byte {
                found = true;
            }
            i += 1;
        }
    }
    found != negated
}
