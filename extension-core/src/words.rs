//! The pieces every file here is written in: `keyword value` lines, comma
//! lists with `-` for none, and capability names.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_priv_core::{capability, capability_names};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineError {
    /// From 1; 0 for the file as a whole.
    pub line: usize,
    pub msg: &'static str,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            0 => f.write_str(self.msg),
            line => write!(f, "line {line}: {}", self.msg),
        }
    }
}

/// Each line that is neither blank nor a `#` comment: its number, whether
/// it is indented, its keyword and the rest, trimmed.
pub(crate) fn lines(text: &str) -> impl Iterator<Item = (usize, bool, &str, &str)> {
    text.lines().enumerate().filter_map(|(i, raw)| {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }
        let indented = raw.starts_with([' ', '\t']);
        let (keyword, rest) = match trimmed.split_once(|c: char| c.is_ascii_whitespace()) {
            Some((keyword, rest)) => (keyword, rest.trim()),
            None => (trimmed, ""),
        };
        Some((i + 1, indented, keyword, rest))
    })
}

pub(crate) fn once<T>(slot: &mut Option<T>, value: T) -> Result<(), &'static str> {
    if slot.is_some() {
        return Err("the setting is given twice");
    }
    *slot = Some(value);
    Ok(())
}

pub(crate) fn flag(slot: &mut bool, rest: &str) -> Result<(), &'static str> {
    if !rest.is_empty() {
        return Err("the setting takes nothing after it");
    }
    if *slot {
        return Err("the setting is given twice");
    }
    *slot = true;
    Ok(())
}

pub(crate) fn single(rest: &str) -> Option<&str> {
    let mut words = rest.split_ascii_whitespace();
    let word = words.next()?;
    words.next().is_none().then_some(word)
}

/// A decimal number without leading zeros.
pub fn parse_number(text: &str) -> Option<u64> {
    let digits = text.as_bytes();
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0')
    {
        return None;
    }
    text.parse().ok()
}

/// Capability names separated by commas, or `-` for none, each once.
pub fn parse_caps(text: &str) -> Option<u64> {
    if text == "-" {
        return Some(0);
    }
    let mut mask = 0;
    for name in text.split(',') {
        let bit = capability(name)?;
        if mask & bit != 0 {
            return None;
        }
        mask |= bit;
    }
    Some(mask)
}

pub fn render_caps(mask: u64) -> String {
    render_list(capability_names(mask))
}

/// Capability names separated by spaces, as a `caps` setting gives them.
pub(crate) fn caps_words(rest: &str) -> Result<u64, &'static str> {
    let mut mask = 0;
    for word in rest.split_ascii_whitespace() {
        let bit = capability(word).ok_or("caps names capabilities")?;
        if mask & bit != 0 {
            return Err("a capability named twice");
        }
        mask |= bit;
    }
    Ok(mask)
}

/// Names separated by commas, or `-` for none, each once and each one
/// `valid` takes.
pub fn parse_names(text: &str, valid: impl Fn(&str) -> bool) -> Option<Vec<String>> {
    if text == "-" {
        return Some(Vec::new());
    }
    let mut out: Vec<String> = Vec::new();
    for name in text.split(',') {
        if !valid(name) || out.iter().any(|n| n == name) {
            return None;
        }
        out.push(name.to_string());
    }
    Some(out)
}

pub fn render_list<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for name in names {
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(name);
    }
    if out.is_empty() {
        out.push('-');
    }
    out
}

/// A lowercase letter, then lowercase letters, digits and `-`, at most
/// `max` bytes.
pub(crate) fn valid_word(word: &str, max: usize) -> bool {
    word.len() <= max
        && word.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Printable ASCII without quotes, at most `max` bytes.
pub(crate) fn valid_text(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'"')
}
