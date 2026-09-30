//! Tab completion's pure half. Finding candidates reads the filesystem and
//! runs commands, so the shell does it; what the candidates make of the line
//! is decided here.

pub mod context;
pub mod spec;

use alloc::vec::Vec;

pub use context::{Context, Position, Quote, analyze};

/// What follows a candidate once it is the only one left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suffix {
    /// A finished word: a blank, after closing any open quote.
    Space,
    Slash,
    /// A stem the user finishes, as `--name=` is.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The whole word, quotes removed; it begins with the prefix it completes.
    pub word: Vec<u8>,
    /// Where the part a listing shows begins: a path lists by its last name.
    pub shown_from: usize,
    pub description: Vec<u8>,
    pub suffix: Suffix,
}

impl Candidate {
    pub fn new(word: Vec<u8>) -> Self {
        Self {
            word,
            shown_from: 0,
            description: Vec::new(),
            suffix: Suffix::Space,
        }
    }

    pub fn described(mut self, description: &[u8]) -> Self {
        self.description = description.to_vec();
        self
    }

    fn label(&self) -> Vec<u8> {
        let mut label = self.word[self.shown_from.min(self.word.len())..].to_vec();
        if self.suffix == Suffix::Slash {
            label.push(b'/');
        }
        label
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
    /// Text to insert at the cursor, quoted for where it lands.
    pub insert: Vec<u8>,
    /// Sorted and unique.
    pub candidates: Vec<Candidate>,
}

/// Narrow `found` to the words `prefix` begins, and extend the line by what
/// they all share — the whole word and its suffix when only one is left.
pub fn resolve(prefix: &[u8], quote: Quote, mut found: Vec<Candidate>) -> Completion {
    found.retain(|c| c.word.starts_with(prefix));
    found.sort_by(|a, b| a.word.cmp(&b.word));
    let mut candidates: Vec<Candidate> = Vec::with_capacity(found.len());
    for candidate in found {
        match candidates.last_mut() {
            // `echo` the builtin and the program: keep the described one.
            Some(last) if last.word == candidate.word => {
                if last.description.is_empty() {
                    last.description = candidate.description;
                }
            }
            _ => candidates.push(candidate),
        }
    }

    let mut insert = Vec::new();
    let Some(first) = candidates.first() else {
        return Completion { insert, candidates };
    };
    let mut common = first.word.len();
    for other in &candidates[1..] {
        common = common.min(shared_len(&first.word, &other.word));
    }
    while common > prefix.len() && common < first.word.len() && is_continuation(first.word[common])
    {
        common -= 1;
    }
    quote_into(&first.word[prefix.len()..common], quote, &mut insert);
    if candidates.len() == 1 {
        match first.suffix {
            Suffix::Space => {
                match quote {
                    Quote::None => {}
                    Quote::Single => insert.push(b'\''),
                    Quote::Double => insert.push(b'"'),
                }
                insert.push(b' ');
            }
            Suffix::Slash => insert.push(b'/'),
            Suffix::None => {}
        }
    }
    Completion { insert, candidates }
}

fn shared_len(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

const ESCAPED_UNQUOTED: &[u8] = b" \t\\'\"`$|&;<>()*?[]#";

/// Append `text` as it must be typed to mean itself inside `quote`.
pub fn quote_into(text: &[u8], quote: Quote, out: &mut Vec<u8>) {
    for &b in text {
        match quote {
            // A backslash before a newline joins lines instead.
            Quote::None if b == b'\n' => out.extend_from_slice(b"'\n'"),
            Quote::None if ESCAPED_UNQUOTED.contains(&b) => out.extend_from_slice(&[b'\\', b]),
            Quote::Double if matches!(b, b'"' | b'\\' | b'$' | b'`') => {
                out.extend_from_slice(&[b'\\', b]);
            }
            Quote::Single if b == b'\'' => out.extend_from_slice(b"'\\''"),
            _ => out.push(b),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Line {
    pub text: Vec<u8>,
    /// The description, shown dimmed.
    pub note: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Listing {
    pub lines: Vec<Line>,
    /// Candidates there was no room for.
    pub hidden: usize,
}

/// Lay candidates out for a terminal `width` cells wide in at most
/// `max_lines` lines, one reserved for saying how many did not fit.
///
/// Described candidates get a line each while they fit; otherwise the names
/// fill columns top to bottom, as `ls` fills them.
pub fn listing(candidates: &[Candidate], width: usize, max_lines: usize) -> Listing {
    let width = width.max(1);
    let max_lines = max_lines.max(2);
    let labels: Vec<Vec<u8>> = candidates.iter().map(Candidate::label).collect();
    let widest = labels.iter().map(|l| cells(l)).max().unwrap_or(0);
    let gap = 2;

    let described = candidates.iter().any(|c| !c.description.is_empty());
    if described && candidates.len() <= max_lines {
        let column = widest + gap;
        let room = width.saturating_sub(column);
        let lines = candidates
            .iter()
            .zip(labels)
            .map(|(candidate, mut text)| {
                let note = truncate(&candidate.description, room).to_vec();
                if !note.is_empty() {
                    pad(&mut text, column);
                }
                Line { text, note }
            })
            .collect();
        return Listing { lines, hidden: 0 };
    }

    let column = widest + gap;
    let per_row = ((width + gap) / column).max(1);
    let mut rows = candidates.len().div_ceil(per_row);
    let mut shown = candidates.len();
    if rows > max_lines {
        rows = max_lines - 1;
        shown = rows * per_row;
    }
    let mut lines = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut text = Vec::new();
        let mut index = row;
        while index < shown {
            text.extend_from_slice(&labels[index]);
            index += rows;
            if index < shown {
                pad(&mut text, (index / rows) * column);
            }
        }
        lines.push(Line {
            text,
            note: Vec::new(),
        });
    }
    Listing {
        lines,
        hidden: candidates.len() - shown,
    }
}

/// One per character: no wide-character widths.
fn cells(text: &[u8]) -> usize {
    text.iter().filter(|&&b| !is_continuation(b)).count()
}

fn pad(text: &mut Vec<u8>, to: usize) {
    let have = cells(text);
    if have < to {
        text.resize(text.len() + (to - have), b' ');
    }
}

fn truncate(text: &[u8], room: usize) -> &[u8] {
    let mut seen = 0usize;
    for (i, &b) in text.iter().enumerate() {
        if !is_continuation(b) {
            if seen == room {
                return &text[..i];
            }
            seen += 1;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn words(list: &[&str]) -> Vec<Candidate> {
        list.iter()
            .map(|w| Candidate::new(w.as_bytes().to_vec()))
            .collect()
    }

    fn dir(word: &str) -> Candidate {
        Candidate {
            suffix: Suffix::Slash,
            ..Candidate::new(word.as_bytes().to_vec())
        }
    }

    #[test]
    fn one_candidate_is_finished() {
        let done = resolve(b"gi", Quote::None, words(&["git", "grep"]));
        assert_eq!(done.insert, b"t ");
        assert_eq!(done.candidates.len(), 1);
        assert_eq!(resolve(b"sr", Quote::None, vec![dir("src")]).insert, b"c/");
    }

    #[test]
    fn several_extend_to_what_they_share() {
        let done = resolve(
            b"c",
            Quote::None,
            words(&["cargo-fmt", "cargo", "cat", "cd"]),
        );
        assert_eq!(done.insert, b"");
        let done = resolve(b"c", Quote::None, words(&["cargo-fmt", "cargo", "cat"]));
        assert_eq!(done.insert, b"a");
        let done = resolve(b"ca", Quote::None, words(&["cargo-fmt", "cargo"]));
        assert_eq!(done.insert, b"rgo");
        assert_eq!(done.candidates.len(), 2);
    }

    #[test]
    fn duplicates_merge_keeping_a_description() {
        let mut found = words(&["echo"]);
        found.push(Candidate::new(b"echo".to_vec()).described(b"Print"));
        let done = resolve(b"ec", Quote::None, found);
        assert_eq!(done.candidates.len(), 1);
        assert_eq!(done.candidates[0].description, b"Print");
        assert_eq!(done.insert, b"ho ");
    }

    #[test]
    fn the_insertion_is_quoted_for_where_it_lands() {
        let found = || words(&["my file's $x"]);
        assert_eq!(
            resolve(b"my", Quote::None, found()).insert,
            b"\\ file\\'s\\ \\$x "
        );
        assert_eq!(
            resolve(b"my", Quote::Double, found()).insert,
            b" file's \\$x\" "
        );
        assert_eq!(
            resolve(b"my", Quote::Single, found()).insert,
            b" file'\\''s $x' "
        );
        let mut stem = Candidate::new(b"--target=".to_vec());
        stem.suffix = Suffix::None;
        assert_eq!(resolve(b"--t", Quote::None, vec![stem]).insert, b"arget=");
    }

    #[test]
    fn a_shared_start_never_splits_a_character() {
        let done = resolve(b"", Quote::None, words(&["\u{e9}a", "\u{e8}b"]));
        assert_eq!(done.insert, b"");
    }

    #[test]
    fn names_fill_columns_top_to_bottom() {
        let found = words(&["a", "b", "c", "d", "e"]);
        let listing = listing(&found, 7, 10);
        let lines: Vec<&[u8]> = listing.lines.iter().map(|l| l.text.as_slice()).collect();
        assert_eq!(lines, [&b"a  c  e"[..], b"b  d"]);
        assert_eq!(listing.hidden, 0);
    }

    #[test]
    fn what_does_not_fit_is_counted() {
        let found = words(&["a", "b", "c", "d", "e", "f", "g"]);
        let listing = listing(&found, 1, 4);
        assert_eq!(listing.lines.len(), 3);
        assert_eq!(listing.hidden, 4);
    }

    #[test]
    fn descriptions_get_a_line_each() {
        let found = vec![
            Candidate::new(b"--all".to_vec()).described(b"Everything"),
            Candidate::new(b"-v".to_vec()),
            Candidate::new(b"-x".to_vec()),
        ];
        let listing = listing(&found, 40, 10);
        assert_eq!(listing.lines[0].text, b"--all  ");
        assert_eq!(listing.lines[0].note, b"Everything");
        assert_eq!(listing.lines[1].text, b"-v");
        let listing = super::listing(&found, 40, 2);
        assert_eq!(listing.lines.len(), 1);
        assert!(listing.lines[0].note.is_empty());
        let listing = super::listing(&found[..1], 12, 10);
        assert_eq!(listing.lines[0].note, b"Every");
    }

    #[test]
    fn a_path_lists_by_its_last_name() {
        let mut found = vec![dir("src/complete"), Candidate::new(b"src/lib.rs".to_vec())];
        for candidate in &mut found {
            candidate.shown_from = 4;
        }
        let listing = listing(&found, 80, 10);
        assert_eq!(listing.lines[0].text, b"complete/  lib.rs");
    }
}
