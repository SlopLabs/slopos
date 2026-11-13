//! Integers, whitespace, slicing and JSON text, exactly as the ledger's
//! digests were first computed (by a Python parser). Every recorded row is a
//! digest of lines built with these rules, so they are part of the interface:
//! a rendering that drifted from them would move every digest at once.

use std::cmp::Ordering;
use std::fmt;

use serde_json::Value;

use crate::Result;

/// An integer of any size, as a decimal magnitude with no leading zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Int {
    neg: bool,
    mag: String,
}

impl Int {
    fn new(neg: bool, digits: &str) -> Self {
        let mag = digits.trim_start_matches('0');
        let mag = if mag.is_empty() { "0" } else { mag };
        Self {
            neg: neg && mag != "0",
            mag: mag.to_string(),
        }
    }

    pub fn from_u64(n: u64) -> Self {
        Self::new(false, &n.to_string())
    }

    /// `^[0-9]+$` text, as a ledger row's number.
    pub fn digits(text: &str) -> Option<Self> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| Self::new(false, text))
    }

    pub fn succ(&self) -> Self {
        if self.neg {
            Self::new(true, &step(&self.mag, false))
        } else {
            Self::new(false, &step(&self.mag, true))
        }
    }
}

/// A decimal magnitude plus or minus one; minus is only asked of one >= 1.
fn step(mag: &str, up: bool) -> String {
    let mut digits: Vec<u8> = mag.bytes().collect();
    let mut i = digits.len();
    loop {
        if i == 0 {
            digits.insert(0, b'1');
            break;
        }
        i -= 1;
        match (up, digits[i]) {
            (true, b'9') => digits[i] = b'0',
            (false, b'0') => digits[i] = b'9',
            (true, d) => {
                digits[i] = d + 1;
                break;
            }
            (false, d) => {
                digits[i] = d - 1;
                break;
            }
        }
    }
    String::from_utf8(digits).unwrap_or_default()
}

impl Ord for Int {
    fn cmp(&self, other: &Self) -> Ordering {
        let mag = self
            .mag
            .len()
            .cmp(&other.mag.len())
            .then_with(|| self.mag.cmp(&other.mag));
        match (self.neg, other.neg) {
            (false, false) => mag,
            (true, true) => mag.reverse(),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
        }
    }
}

impl PartialOrd for Int {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Int {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.neg {
            f.write_str("-")?;
        }
        f.write_str(&self.mag)
    }
}

/// rustdoc's evaluated value of an integer, `-?[0-9_]+` with an optional
/// integer-type suffix: `Ok(None)` for anything else, null included.
pub fn int_value(value: Option<&Value>) -> Result<Option<Int>> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.as_str(),
        Some(other) => return Err(format!("rustdoc JSON: a value is {other}, not text").into()),
    };
    let text = text.strip_suffix('\n').unwrap_or(text);
    let (neg, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    let (number, suffix) = rest.split_at(end);
    const SUFFIXES: [&str; 6] = ["8", "16", "32", "64", "128", "size"];
    let suffix_ok =
        suffix.is_empty() || (suffix.starts_with(['i', 'u']) && SUFFIXES.contains(&&suffix[1..]));
    if number.is_empty() || !suffix_ok {
        return Ok(None);
    }
    let digits = number.replace('_', "");
    if digits.is_empty() {
        return Err(format!("rustdoc JSON: `{text}` is not an integer").into());
    }
    Ok(Some(Int::new(neg, &digits)))
}

/// Python's `str.isspace`: Unicode `White_Space` and the four ASCII
/// separators `\x1c`..`\x1f`.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Whitespace runs collapsed to one space, the ends trimmed.
pub fn collapse(s: &str) -> String {
    s.split(is_space)
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A file read as text: `\r\n` and a lone `\r` both end a line.
pub fn universal_newlines(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

/// `seq[i]` for a possibly negative index.
pub fn index(len: usize, i: i64) -> Option<usize> {
    let len = len as i64;
    let i = if i < 0 { i + len } else { i };
    (0 <= i && i < len).then_some(i as usize)
}

/// `seq[start:end]` bounds for possibly negative or out-of-range ones.
pub fn slice(len: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let len = len as i64;
    let clamp = |i: i64| (if i < 0 { i + len } else { i }).clamp(0, len) as usize;
    let s = start.map_or(0, clamp);
    let e = end.map_or(len as usize, clamp);
    (s, e.max(s))
}

/// Python's `str(value)` for a scalar.
pub fn scalar(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => return Err(format!("rustdoc JSON: {other} where text was expected").into()),
    })
}

/// Python's truthiness.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `value` with every `id` key dropped, as compact JSON with sorted keys and
/// everything outside printable ASCII escaped.
pub fn dumps_without_ids(value: &Value) -> String {
    let mut out = String::new();
    dump(value, &mut out);
    out
}

fn dump(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => dump_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                dump(v, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().filter(|k| *k != "id").collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                dump_str(k, out);
                out.push(':');
                dump(&o[k], out);
            }
            out.push('}');
        }
    }
}

fn dump_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn int(s: &str) -> Option<Int> {
        int_value(Some(&json!(s))).unwrap()
    }

    #[test]
    fn integers_read_as_rustdoc_writes_them() {
        assert_eq!(int("1_024").unwrap().to_string(), "1024");
        assert_eq!(int("007u8").unwrap().to_string(), "7");
        assert_eq!(int("-0").unwrap().to_string(), "0");
        assert_eq!(int("-12i64").unwrap().to_string(), "-12");
        assert_eq!(int("4u7"), None);
        assert_eq!(int("1 << 3"), None);
        assert_eq!(int("0x10"), None);
        assert!(int_value(Some(&json!("_"))).is_err());
        assert_eq!(int_value(None).unwrap(), None);
        let big = int("340282366920938463463374607431768211455").unwrap();
        assert_eq!(
            big.succ().to_string(),
            "340282366920938463463374607431768211456"
        );
        assert_eq!(int("-1").unwrap().succ().to_string(), "0");
        assert_eq!(int("-10").unwrap().succ().to_string(), "-9");
        assert_eq!(int("99").unwrap().succ().to_string(), "100");
        assert!(int("-3").unwrap() < int("2").unwrap());
        assert!(int("-3").unwrap() < int("-2").unwrap());
        assert!(int("10").unwrap() > int("9").unwrap());
    }

    #[test]
    fn json_is_sorted_compact_ascii_without_ids() {
        let v = json!({"b": [1, true, null], "a": "é\u{1F600}\"\n\x7f", "id": 3, "c": {"id": 1, "z": 0}});
        assert_eq!(
            dumps_without_ids(&v),
            r#"{"a":"\u00e9\ud83d\ude00\"\n\u007f","b":[1,true,null],"c":{"z":0}}"#
        );
    }

    #[test]
    fn slices_wrap_negative_indices_and_clamp() {
        assert_eq!(slice(5, Some(-1), None), (4, 5));
        assert_eq!(slice(5, Some(1), Some(-1)), (1, 4));
        assert_eq!(slice(5, Some(4), Some(2)), (4, 4));
        assert_eq!(slice(5, Some(0), Some(99)), (0, 5));
        assert_eq!(index(5, -1), Some(4));
        assert_eq!(index(5, 5), None);
    }
}
