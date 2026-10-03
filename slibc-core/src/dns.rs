//! DNS messages for `res_query` and `dn_expand`: the header, question and
//! name formats of RFC 1035 section 4.1, and names written as the master
//! files of section 5.1 write them.

/// The fixed header.
pub const HFIXEDSZ: usize = 12;
/// A question's type and class, after its name.
pub const QFIXEDSZ: usize = 4;
/// The longest name on the wire, its length octets included.
pub const MAXCDNAME: usize = 255;
/// The longest name as text: every octet written `\DDD`, plus the NUL.
pub const MAXDNAME: usize = 1025;
/// The largest message UDP carries.
pub const PACKETSZ: usize = 512;
const MAXLABEL: usize = 63;

pub const NOERROR: u8 = 0;
pub const SERVFAIL: u8 = 2;
pub const NXDOMAIN: u8 = 3;

/// What a reply's header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub id: u16,
    pub response: bool,
    pub truncated: bool,
    pub rcode: u8,
    pub qdcount: u16,
    pub ancount: u16,
}

pub fn header(msg: &[u8]) -> Option<Header> {
    if msg.len() < HFIXEDSZ {
        return None;
    }
    let word = |at: usize| u16::from_be_bytes([msg[at], msg[at + 1]]);
    Some(Header {
        id: word(0),
        response: msg[2] & 0x80 != 0,
        truncated: msg[2] & 0x02 != 0,
        rcode: msg[3] & 0x0f,
        qdcount: word(4),
        ancount: word(6),
    })
}

/// Writes a standard query for `name` of `rtype` in `class` into `out`,
/// asking for recursion when `recurse`, and returns its length. `None` when
/// `name` is not a name or `out` is too short.
pub fn encode_query(
    id: u16,
    recurse: bool,
    name: &[u8],
    class: u16,
    rtype: u16,
    out: &mut [u8],
) -> Option<usize> {
    if out.len() < HFIXEDSZ {
        return None;
    }
    out[..HFIXEDSZ].fill(0);
    out[0..2].copy_from_slice(&id.to_be_bytes());
    if recurse {
        out[2] = 0x01;
    }
    out[4..6].copy_from_slice(&1u16.to_be_bytes());
    let end = HFIXEDSZ + encode_name(name, out.get_mut(HFIXEDSZ..)?)?;
    let tail = out.get_mut(end..end + QFIXEDSZ)?;
    tail[0..2].copy_from_slice(&rtype.to_be_bytes());
    tail[2..4].copy_from_slice(&class.to_be_bytes());
    Some(end + QFIXEDSZ)
}

/// `name` as length-prefixed labels ending in the root's empty one. A final
/// `.` is allowed; `\X` stands for `X` and `\DDD` for the octet `DDD`.
fn encode_name(name: &[u8], out: &mut [u8]) -> Option<usize> {
    let name = match name {
        b"." => &b""[..],
        _ => name,
    };
    let mut at = 0usize;
    let mut i = 0usize;
    while i < name.len() {
        let start = at;
        at += 1;
        let mut len = 0usize;
        while i < name.len() && name[i] != b'.' {
            let (octet, used) = unescape(&name[i..])?;
            i += used;
            *out.get_mut(at)? = octet;
            at += 1;
            len += 1;
        }
        if len == 0 || len > MAXLABEL {
            return None;
        }
        *out.get_mut(start)? = len as u8;
        if i < name.len() {
            i += 1;
            if i == name.len() {
                break;
            }
        }
    }
    *out.get_mut(at)? = 0;
    at += 1;
    (at <= MAXCDNAME).then_some(at)
}

/// The octet at the start of `text` and how many bytes spelled it.
fn unescape(text: &[u8]) -> Option<(u8, usize)> {
    if text[0] != b'\\' {
        return Some((text[0], 1));
    }
    match text.get(1..4) {
        Some(digits) if digits.iter().all(u8::is_ascii_digit) => {
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 10 + u32::from(d - b'0'));
            Some((u8::try_from(value).ok()?, 4))
        }
        _ => text.get(1).map(|&c| (c, 2)),
    }
}

/// The name at `at` in `msg`, followed through compression pointers, as
/// NUL-terminated text in `out`; the root is the empty string. Returns how
/// many octets of `msg` the name occupies where it starts. `None` for a name
/// that runs off the message, loops, uses a label type other than a length
/// or a pointer, is longer than `MAXCDNAME`, or does not fit `out`.
pub fn expand(msg: &[u8], at: usize, out: &mut [u8]) -> Option<usize> {
    let mut pos = at;
    let mut consumed = None;
    let mut wire = 0usize;
    let mut written = 0usize;
    let mut hops = 0usize;
    loop {
        let len = *msg.get(pos)?;
        match len & 0xc0 {
            0xc0 => {
                let target = usize::from(len & 0x3f) << 8 | usize::from(*msg.get(pos + 1)?);
                consumed.get_or_insert(pos + 2 - at);
                hops += 1;
                if target >= msg.len() || hops > msg.len() / 2 {
                    return None;
                }
                pos = target;
            }
            0x00 if len == 0 => {
                *out.get_mut(written)? = 0;
                return Some(consumed.unwrap_or_else(|| pos + 1 - at));
            }
            0x00 => {
                let label = msg.get(pos + 1..pos + 1 + usize::from(len))?;
                wire += 1 + label.len();
                if wire + 1 > MAXCDNAME {
                    return None;
                }
                if written > 0 {
                    *out.get_mut(written)? = b'.';
                    written += 1;
                }
                for &octet in label {
                    written += escape(octet, out.get_mut(written..)?)?;
                }
                pos += 1 + label.len();
            }
            _ => return None,
        }
    }
}

/// `octet` as master-file text: itself, `\` before a character that would
/// otherwise mean something, or `\DDD`.
fn escape(octet: u8, out: &mut [u8]) -> Option<usize> {
    let text: &[u8] = match octet {
        b'.' | b'\\' | b'"' | b'(' | b')' | b';' | b'@' | b'$' => &[b'\\', octet],
        0x21..=0x7e => &[octet],
        _ => &[
            b'\\',
            b'0' + octet / 100,
            b'0' + octet / 10 % 10,
            b'0' + octet % 10,
        ],
    };
    out.get_mut(..text.len())?.copy_from_slice(text);
    Some(text.len())
}

/// Whether `reply` answers `query`: the same id, a response, and the one
/// question asked, its name compared without regard to ASCII case.
pub fn answers(query: &[u8], reply: &[u8]) -> bool {
    let (Some(asked), Some(got)) = (header(query), header(reply)) else {
        return false;
    };
    if asked.id != got.id || !got.response || got.qdcount != 1 {
        return false;
    }
    let mut ours = [0u8; MAXDNAME];
    let mut theirs = [0u8; MAXDNAME];
    let (Some(a), Some(b)) = (
        expand(query, HFIXEDSZ, &mut ours),
        expand(reply, HFIXEDSZ, &mut theirs),
    ) else {
        return false;
    };
    let text = |buf: &[u8]| buf.iter().position(|&c| c == 0).unwrap_or(0);
    let (ours, theirs) = (&ours[..text(&ours)], &theirs[..text(&theirs)]);
    ours.eq_ignore_ascii_case(theirs)
        && query.get(HFIXEDSZ + a..HFIXEDSZ + a + QFIXEDSZ)
            == reply.get(HFIXEDSZ + b..HFIXEDSZ + b + QFIXEDSZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(out: &[u8]) -> &[u8] {
        &out[..out.iter().position(|&c| c == 0).unwrap()]
    }

    #[test]
    fn a_query_is_header_labels_type_and_class() {
        let mut out = [0u8; PACKETSZ];
        let n = encode_query(0x1234, true, b"Example.com.", 1, 44, &mut out).unwrap();
        assert_eq!(
            &out[..n],
            b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\
              \x07Example\x03com\x00\x00\x2c\x00\x01"
        );
        let same = encode_query(0x1234, true, b"Example.com", 1, 44, &mut [0u8; PACKETSZ]);
        assert_eq!(same, Some(n));
    }

    #[test]
    fn the_root_is_one_empty_label() {
        let mut out = [0u8; PACKETSZ];
        let n = encode_query(1, false, b".", 1, 2, &mut out).unwrap();
        assert_eq!(&out[2..4], b"\x00\x00");
        assert_eq!(&out[HFIXEDSZ..n], b"\x00\x00\x02\x00\x01");
    }

    #[test]
    fn a_name_that_is_not_one_is_refused() {
        let mut out = [0u8; PACKETSZ];
        assert_eq!(encode_query(1, true, b"a..b", 1, 1, &mut out), None);
        assert_eq!(encode_query(1, true, b".a", 1, 1, &mut out), None);
        let long = [b'x'; 64];
        assert_eq!(encode_query(1, true, &long, 1, 1, &mut out), None);
        assert_eq!(encode_query(1, true, b"a", 1, 1, &mut [0u8; 16]), None);
    }

    #[test]
    fn escapes_spell_octets() {
        let mut out = [0u8; PACKETSZ];
        let n = encode_query(1, true, b"a\\.b\\065", 1, 1, &mut out).unwrap();
        assert_eq!(&out[HFIXEDSZ..n - QFIXEDSZ], b"\x04a.bA\x00");
        let mut name = [0u8; MAXDNAME];
        assert_eq!(expand(&out, HFIXEDSZ, &mut name), Some(6));
        assert_eq!(text(&name), b"a\\.bA");
    }

    #[test]
    fn a_pointer_is_followed_and_counted_as_two_octets() {
        let mut msg = [0u8; 40];
        msg[HFIXEDSZ..HFIXEDSZ + 13].copy_from_slice(b"\x07example\x03com\x00");
        msg[30..36].copy_from_slice(b"\x03www\xc0\x0c");
        let mut out = [0u8; MAXDNAME];
        assert_eq!(expand(&msg, 30, &mut out), Some(6));
        assert_eq!(text(&out), b"www.example.com");
        assert_eq!(expand(&msg, HFIXEDSZ, &mut out), Some(13));
        assert_eq!(text(&out), b"example.com");
    }

    #[test]
    fn a_name_that_loops_or_overruns_is_refused() {
        let mut out = [0u8; MAXDNAME];
        assert_eq!(expand(b"\xc0\x00", 0, &mut out), None);
        assert_eq!(expand(b"\x01a\xc0\x00", 0, &mut out), None);
        assert_eq!(expand(b"\x05ab", 0, &mut out), None);
        assert_eq!(expand(b"\x40", 0, &mut out), None);
        assert_eq!(expand(b"\x03www\x00", 0, &mut [0u8; 3]), None);
        assert_eq!(expand(b"\x00", 0, &mut out), Some(1));
        assert_eq!(text(&out), b"");
    }

    #[test]
    fn unprintable_octets_are_written_as_decimal() {
        let mut out = [0u8; MAXDNAME];
        assert_eq!(expand(b"\x02\x00 \x00", 0, &mut out), Some(4));
        assert_eq!(text(&out), b"\\000\\032");
    }

    #[test]
    fn a_reply_answers_the_question_it_repeats() {
        let mut query = [0u8; PACKETSZ];
        let n = encode_query(7, true, b"host.example", 1, 44, &mut query).unwrap();
        let mut reply = query;
        reply[2] |= 0x80;
        reply[HFIXEDSZ + 1] = b'H';
        assert!(answers(&query[..n], &reply[..n]));
        let got = header(&reply).unwrap();
        assert!(got.response && !got.truncated && got.rcode == NOERROR && got.qdcount == 1);
        let mut other = reply;
        other[1] = 8;
        assert!(!answers(&query[..n], &other[..n]));
        let mut other = reply;
        other[n - 1] = 3;
        assert!(!answers(&query[..n], &other[..n]));
        assert!(!answers(&query[..n], &query[..n]));
    }
}
