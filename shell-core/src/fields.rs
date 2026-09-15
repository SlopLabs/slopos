//! Field splitting (POSIX §2.6.5).
//!
//! Only [`Q_SPLIT`](crate::qbuf::Q_SPLIT) bytes split, so `IFS=:` turns the
//! *value* of `$x` into fields but never the literal word `a:b`.

use crate::qbuf::QBuf;
use alloc::vec::Vec;

/// What `IFS` means when it is unset.
pub const DEFAULT_IFS: &[u8] = b" \t\n";

#[inline]
fn is_ifs_white(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n'
}

/// Split `parts` into fields.
///
/// `parts` are *hard* segments — `"$@"` contributes one per positional — and
/// those boundaries survive splitting. An empty `ifs` disables it outright.
///
/// `keep_empty` is whether the word held a quoted or literal byte, which
/// decides the case the bytes cannot: `cmd $x` with `x` empty passes no
/// argument, `cmd "$x"` passes one. The suppression is conditioned on the
/// input rather than the result, so `IFS=:` splitting a lone `:` still yields
/// a field.
pub fn split(parts: &[QBuf], ifs: &[u8], keep_empty: bool) -> Vec<QBuf> {
    let mut out: Vec<QBuf> = Vec::new();
    for part in parts {
        if ifs.is_empty() {
            out.push(part.clone());
            continue;
        }
        split_one(part, ifs, &mut out);
    }
    if !keep_empty
        && out.len() == 1
        && out[0].is_empty()
        && parts.iter().all(|part| part.is_empty())
    {
        out.clear();
    }
    out
}

fn split_one(part: &QBuf, ifs: &[u8], out: &mut Vec<QBuf>) {
    let mut cur = QBuf::new();
    let mut produced = false;
    let mut i = 0usize;

    while i < part.len() {
        let b = part.bytes[i];
        if !part.splittable(i) || !ifs.contains(&b) {
            cur.push(b, part.flags[i]);
            i += 1;
            continue;
        }

        // A delimiter run: any number of IFS whitespace bytes, at most one
        // non-whitespace IFS byte, and the whitespace that follows it.
        let mut seen_nonwhite = false;
        while i < part.len() && part.splittable(i) && ifs.contains(&part.bytes[i]) {
            if !is_ifs_white(part.bytes[i]) {
                if seen_nonwhite {
                    break;
                }
                seen_nonwhite = true;
            }
            i += 1;
        }

        if cur.is_empty() && !produced && !seen_nonwhite {
            continue;
        }
        if i >= part.len() && cur.is_empty() && !seen_nonwhite {
            continue;
        }
        out.push(core::mem::replace(&mut cur, QBuf::new()));
        produced = true;
    }

    if !cur.is_empty() {
        out.push(cur);
    } else if !produced && !part.is_empty() && part.bytes.iter().all(|&b| is_ifs_white(b)) {
        // A part that is wholly IFS whitespace yields no field at all.
    } else if !produced {
        out.push(cur);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qbuf::{Q_QUOTED, Q_SPLIT};
    use alloc::vec;

    fn expanded(bytes: &[u8]) -> QBuf {
        let mut b = QBuf::new();
        b.extend(bytes, Q_SPLIT);
        b
    }

    fn texts(fields: &[QBuf]) -> Vec<Vec<u8>> {
        fields.iter().map(|f| f.bytes.clone()).collect()
    }

    #[test]
    fn whitespace_runs_collapse_and_edges_are_trimmed() {
        let out = split(&[expanded(b"  a   b  ")], DEFAULT_IFS, false);
        assert_eq!(texts(&out), vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn a_nonwhitespace_delimiter_always_delimits() {
        let out = split(&[expanded(b"a::b")], b":", false);
        assert_eq!(
            texts(&out),
            vec![b"a".to_vec(), b"".to_vec(), b"b".to_vec()]
        );
    }

    #[test]
    fn literal_text_never_splits() {
        let mut part = QBuf::new();
        part.extend(b"a b", 0);
        assert_eq!(
            texts(&split(&[part], DEFAULT_IFS, true)),
            vec![b"a b".to_vec()]
        );
    }

    #[test]
    fn quoted_expansion_output_never_splits() {
        let mut part = QBuf::new();
        part.extend(b"a b", Q_QUOTED);
        assert_eq!(
            texts(&split(&[part], DEFAULT_IFS, true)),
            vec![b"a b".to_vec()]
        );
    }

    #[test]
    fn empty_ifs_disables_splitting() {
        assert_eq!(
            texts(&split(&[expanded(b"a b c")], b"", false)),
            vec![b"a b c".to_vec()]
        );
    }

    #[test]
    fn a_wholly_blank_expansion_yields_no_field() {
        assert!(split(&[expanded(b"   ")], DEFAULT_IFS, false).is_empty());
    }

    /// Splitting that *produces* an empty field keeps it; only an expansion
    /// that was itself null contributes nothing.
    #[test]
    fn a_lone_delimiter_still_yields_one_field() {
        assert_eq!(
            texts(&split(&[expanded(b":")], b":", false)),
            vec![b"".to_vec()]
        );
        assert_eq!(
            texts(&split(&[expanded(b"::")], b":", false)),
            vec![b"".to_vec(), b"".to_vec()]
        );
    }

    /// `cmd $x` with `x` empty passes no argument; `cmd "$x"` passes one.
    #[test]
    fn an_empty_unquoted_expansion_yields_no_field() {
        assert!(split(&[expanded(b"")], DEFAULT_IFS, false).is_empty());
        assert_eq!(
            texts(&split(&[expanded(b"")], DEFAULT_IFS, true)),
            vec![b"".to_vec()]
        );
    }

    #[test]
    fn hard_parts_survive_without_a_delimiter() {
        let mut a = QBuf::new();
        a.extend(b"one two", Q_QUOTED);
        let mut b = QBuf::new();
        b.extend(b"three", Q_QUOTED);
        assert_eq!(
            texts(&split(&[a, b], DEFAULT_IFS, true)),
            vec![b"one two".to_vec(), b"three".to_vec()]
        );
    }
}
