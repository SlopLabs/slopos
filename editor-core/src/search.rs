//! Literal find and replace over a buffer.
//!
//! Literal, not regular: what an editor's find bar is asked for is a string,
//! and `coreutils`' `grep` already owns the regular-expression engine. The
//! matcher is case-folded per ASCII rather than per Unicode, which is what the
//! rest of the tree does and what a source file needs.

use alloc::vec::Vec;

use crate::buffer::{Position, Range, TextBuffer};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    /// Matches must start and end on a word boundary.
    pub whole_word: bool,
}

/// Matches on one line, in character columns.
fn line_matches(line: &str, needle: &str, options: SearchOptions) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let hay: Vec<char> = line.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    if pat.len() > hay.len() {
        return out;
    }

    let eq = |a: char, b: char| {
        if options.case_sensitive {
            a == b
        } else {
            a.eq_ignore_ascii_case(&b)
        }
    };

    let is_word = |c: char| c.is_alphanumeric() || c == '_';

    let mut i = 0usize;
    while i + pat.len() <= hay.len() {
        if (0..pat.len()).all(|k| eq(hay[i + k], pat[k])) {
            let start_ok = !options.whole_word || i == 0 || !is_word(hay[i - 1]);
            let end = i + pat.len();
            let end_ok = !options.whole_word || end == hay.len() || !is_word(hay[end]);
            if start_ok && end_ok {
                out.push((i, end));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Every match in the buffer, in document order.
pub fn find_all(buffer: &TextBuffer, needle: &str, options: SearchOptions) -> Vec<Range> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    for line in 0..buffer.line_count() {
        for (start, end) in line_matches(buffer.line(line), needle, options) {
            out.push(Range::new(
                Position::new(line, start),
                Position::new(line, end),
            ));
        }
    }
    out
}

/// Matches on one line only — what the renderer needs for the visible rows.
pub fn find_in_line(
    buffer: &TextBuffer,
    line: usize,
    needle: &str,
    options: SearchOptions,
) -> Vec<Range> {
    line_matches(buffer.line(line), needle, options)
        .into_iter()
        .map(|(start, end)| Range::new(Position::new(line, start), Position::new(line, end)))
        .collect()
}

/// The first match at or after `from`, wrapping to the top of the buffer.
pub fn find_next(
    buffer: &TextBuffer,
    from: Position,
    needle: &str,
    options: SearchOptions,
) -> Option<Range> {
    let matches = find_all(buffer, needle, options);
    matches
        .iter()
        .find(|m| m.start >= from)
        .copied()
        .or_else(|| matches.first().copied())
}

/// The last match strictly before `from`, wrapping to the end of the buffer.
pub fn find_prev(
    buffer: &TextBuffer,
    from: Position,
    needle: &str,
    options: SearchOptions,
) -> Option<Range> {
    let matches = find_all(buffer, needle, options);
    matches
        .iter()
        .rev()
        .find(|m| m.end <= from)
        .copied()
        .or_else(|| matches.last().copied())
}

/// Fuzzy subsequence score of `needle` against `text`, or `None` for no match.
///
/// Used by the file finder: characters must appear in order, and a run that
/// starts a path segment or follows a separator scores higher, which is what
/// makes "mmsl" find `mm/src/lib.rs`.
pub fn fuzzy_score(text: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    // One forward pass over `text`, allocating nothing: the file finder scores
    // every path in the tree on every keystroke, so two `Vec<char>` per
    // candidate is the whole cost of the feature.
    let mut score = 0i32;
    let mut last_hit: Option<usize> = None;
    let mut hay = text.chars().enumerate();
    let mut before: Option<char> = None;
    let mut length = 0usize;

    for want in needle.chars() {
        let mut found = None;
        for (at, c) in hay.by_ref() {
            length = at + 1;
            let prev = before;
            before = Some(c);
            if c.eq_ignore_ascii_case(&want) {
                found = Some((at, c, prev));
                break;
            }
        }
        let (at, c, prev) = found?;
        let boundary = match prev {
            None => true,
            Some(prev) => {
                matches!(prev, '/' | '_' | '-' | '.' | ' ')
                    || (c.is_uppercase() && !prev.is_uppercase())
            }
        };
        score += if boundary { 8 } else { 2 };
        if last_hit == Some(at.wrapping_sub(1)) {
            score += 4;
        }
        if c == want {
            score += 1;
        }
        last_hit = Some(at);
    }

    // Shorter candidates win ties, so an exact file name beats a long path that
    // merely contains the same letters.
    for (at, _) in hay {
        length = at + 1;
    }
    score -= (length as i32) / 16;
    Some(score)
}

/// `candidates` that match `needle`, best first. Ties keep input order.
pub fn fuzzy_filter<'a>(candidates: &[&'a str], needle: &str) -> Vec<(&'a str, i32)> {
    let mut scored: Vec<(&str, i32)> = candidates
        .iter()
        .filter_map(|c| fuzzy_score(c, needle).map(|s| (*c, s)))
        .collect();
    // Stable so equal scores keep the caller's order.
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored
}
