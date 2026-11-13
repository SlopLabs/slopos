//! Source text for the items whose shape rustdoc does not evaluate: comments
//! dropped, tokens joined by single spaces, a comma before a closing bracket
//! removed, so a reformat or a comment edit leaves the text unchanged.

use super::text::{is_space, is_word};

const PUNCT: [&str; 24] = [
    "..=", "...", "<<=", ">>=", "::", "->", "=>", "==", "!=", "<=", ">=", "&&", "||", "<<", ">>",
    "..", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
];

/// `src` as its tokens, one space apart.
pub fn normalise(src: &str) -> String {
    rust_tokens(src).join(" ")
}

fn starts(src: &[char], i: usize, pat: &str) -> bool {
    let mut k = i;
    for p in pat.chars() {
        if src.get(k) != Some(&p) {
            return false;
        }
        k += 1;
    }
    true
}

fn find(src: &[char], pat: &str, from: usize) -> Option<usize> {
    (from..src.len()).find(|&i| starts(src, i, pat))
}

fn text(src: &[char], from: usize, to: usize) -> String {
    src[from..to.min(src.len())].iter().collect()
}

pub fn rust_tokens(src: &str) -> Vec<String> {
    let src: Vec<char> = src.chars().collect();
    let n = src.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = src[i];
        if is_space(c) {
            i += 1;
        } else if starts(&src, i, "//") {
            i = find(&src, "\n", i).unwrap_or(n);
        } else if starts(&src, i, "/*") {
            let mut depth = 1;
            i += 2;
            while i < n && depth > 0 {
                if starts(&src, i, "/*") {
                    depth += 1;
                    i += 2;
                } else if starts(&src, i, "*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else if is_word(c) {
            let mut j = i;
            while j < n && is_word(src[j]) {
                j += 1;
            }
            let word = text(&src, i, j);
            if matches!(word.as_str(), "r" | "br" | "cr") && j < n && matches!(src[j], '#' | '"') {
                let mut k = j;
                while k < n && src[k] == '#' {
                    k += 1;
                }
                if k < n && src[k] == '"' {
                    let close = format!("\"{}", "#".repeat(k - j));
                    let end = find(&src, &close, k + 1).map_or(n, |e| e + close.chars().count());
                    out.push(text(&src, i, end));
                    i = end;
                    continue;
                }
            }
            if matches!(word.as_str(), "b" | "c") && j < n && matches!(src[j], '"' | '\'') {
                let end = string_end(&src, j);
                out.push(text(&src, i, end));
                i = end;
                continue;
            }
            out.push(word);
            i = j;
        } else if c == '"' {
            let end = string_end(&src, i);
            out.push(text(&src, i, end));
            i = end;
        } else if c == '\'' {
            let end = if i + 1 < n && src[i + 1] == '\\' {
                string_end(&src, i)
            } else if i + 2 < n && src[i + 2] == '\'' {
                i + 3
            } else {
                let mut end = i + 1;
                while end < n && is_word(src[end]) {
                    end += 1;
                }
                end
            };
            out.push(text(&src, i, end));
            i = end;
        } else {
            let p = PUNCT
                .iter()
                .find(|p| starts(&src, i, p))
                .map_or_else(|| c.to_string(), |p| p.to_string());
            i += p.chars().count();
            out.push(p);
        }
    }
    let keep: Vec<bool> = (0..out.len())
        .map(|k| {
            !(out[k] == ","
                && out
                    .get(k + 1)
                    .is_some_and(|next| matches!(next.as_str(), ")" | "]" | "}")))
        })
        .collect();
    out.into_iter()
        .zip(keep)
        .filter_map(|(t, keep)| keep.then_some(t))
        .collect()
}

fn string_end(src: &[char], i: usize) -> usize {
    let quote = src[i];
    let mut j = i + 1;
    while j < src.len() {
        if src[j] == '\\' {
            j += 2;
        } else if src[j] == quote {
            return j + 1;
        } else {
            j += 1;
        }
    }
    src.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_layout_vanish_and_literals_stay_whole() {
        assert_eq!(
            normalise("fn f(a: u8,)->u8 {\n  // why\n  a /* x /* nested */ y */ <<= 1\n}"),
            "fn f ( a : u8 ) -> u8 { a <<= 1 }"
        );
        assert_eq!(
            rust_tokens(r###"x(r#"a "b"#, b"c\"d", 'e', '\n', 'f: g, [1,]);"###),
            [
                "x",
                "(",
                r###"r#"a "b"#"###,
                ",",
                r#"b"c\"d""#,
                ",",
                "'e'",
                ",",
                r"'\n'",
                ",",
                "'f",
                ":",
                "g",
                ",",
                "[",
                "1",
                "]",
                ")",
                ";"
            ]
        );
    }
}
