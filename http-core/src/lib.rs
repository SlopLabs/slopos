//! The I/O-free half of an HTTP/1.1 client: URLs and redirect targets, the
//! response head, and chunked transfer coding decoded as it arrives.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Url {
    pub scheme: Scheme,
    /// Lower-cased.
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`; the fragment is dropped.
    pub target: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum UrlError {
    NoScheme,
    UnsupportedScheme(String),
    Credentials,
    Ipv6,
    BadPort,
    BadHost,
    /// A control character in the path or query, or whitespace in a URL
    /// given rather than received.
    BadTarget,
}

fn scheme_of(s: &str) -> Option<(&str, &str)> {
    let (name, rest) = s.split_once(':')?;
    let mut bytes = name.bytes();
    let valid = bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    valid.then_some((name, rest))
}

fn percent_encode_loose(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b == b' ' || b >= 0x80 {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

fn digits(s: &str) -> Option<&str> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then_some(s)
}

/// RFC 3986 §5.2.4, over a path that starts with `/`.
fn remove_dot_segments(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *segment {
            "." if last => out.push(""),
            "." => {}
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

fn target(path: &str, query: Option<&str>) -> Result<String, UrlError> {
    let mut target = remove_dot_segments(path);
    if let Some(q) = query {
        target.push('?');
        target.push_str(q);
    }
    if target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(UrlError::BadTarget);
    }
    Ok(target)
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, UrlError> {
        let (name, rest) = scheme_of(url).ok_or(UrlError::NoScheme)?;
        let rest = rest.strip_prefix("//").ok_or(UrlError::NoScheme)?;
        let scheme = if name.eq_ignore_ascii_case("http") {
            Scheme::Http
        } else if name.eq_ignore_ascii_case("https") {
            Scheme::Https
        } else {
            return Err(UrlError::UnsupportedScheme(name.to_string()));
        };
        let rest = rest.split('#').next().unwrap_or("");
        let split = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, reference) = rest.split_at(split);
        if authority.contains('@') {
            return Err(UrlError::Credentials);
        }
        if authority.starts_with('[') {
            return Err(UrlError::Ipv6);
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, "")) => (host, scheme.default_port()),
            Some((host, port)) => {
                let port = digits(port)
                    .and_then(|p| p.parse::<u16>().ok())
                    .filter(|&p| p != 0)
                    .ok_or(UrlError::BadPort)?;
                (host, port)
            }
            None => (authority, scheme.default_port()),
        };
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
        if !host_ok {
            return Err(UrlError::BadHost);
        }
        let (path, query) = match reference.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (reference, None),
        };
        Ok(Url {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            target: target(if path.is_empty() { "/" } else { path }, query)?,
        })
    }

    /// The `Host` header: the port only when it is not the scheme's.
    pub fn host_header(&self) -> String {
        if self.port == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The same origin in RFC 6454's sense.
    pub fn same_origin(&self, other: &Url) -> bool {
        (self.scheme, &self.host, self.port) == (other.scheme, &other.host, other.port)
    }

    /// Resolve a `Location` against this URL (RFC 3986 §5.2), percent-encoding
    /// spaces and non-ASCII bytes first as browsers and curl do.
    pub fn join(&self, location: &str) -> Result<Url, UrlError> {
        let encoded = percent_encode_loose(location.trim());
        let location = encoded.as_str();
        if let Some((name, rest)) = scheme_of(location) {
            if rest.starts_with("//") {
                return Url::parse(location);
            }
            if name.eq_ignore_ascii_case("http") || name.eq_ignore_ascii_case("https") {
                return Err(UrlError::BadHost);
            }
            return Err(UrlError::UnsupportedScheme(name.to_string()));
        }
        if location.starts_with("//") {
            return Url::parse(&format!("{}:{location}", self.scheme.name()));
        }
        let location = location.split('#').next().unwrap_or("");
        let (path, query) = match location.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (location, None),
        };
        let (base_path, base_query) = match self.target.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (self.target.as_str(), None),
        };
        let target = if path.is_empty() {
            target(base_path, query.or(base_query))?
        } else if path.starts_with('/') {
            target(path, query)?
        } else {
            let dir = &base_path[..base_path.rfind('/').map_or(0, |i| i + 1)];
            target(&format!("{dir}{path}"), query)?
        };
        Ok(Url {
            target,
            ..self.clone()
        })
    }
}

/// Where the head ends: the index just past its blank line.
/// A bare LF ends a line as well as CRLF does (RFC 9112 §2.2).
pub fn head_end(raw: &[u8]) -> Option<usize> {
    raw.iter()
        .enumerate()
        .find_map(|(i, &b)| match (b, raw.get(i + 1), raw.get(i + 2)) {
            (b'\n', Some(b'\n'), _) => Some(i + 2),
            (b'\n', Some(b'\r'), Some(b'\n')) => Some(i + 3),
            _ => None,
        })
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Head {
    pub status: u16,
    pub content_length: Option<u64>,
    pub chunked: bool,
    pub location: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HeadError {
    NotHttp,
    BadContentLength,
}

impl Head {
    /// Parse a head ending in its blank line, as [`head_end`] found it.
    pub fn parse(raw: &[u8]) -> Result<Head, HeadError> {
        let text = String::from_utf8_lossy(raw);
        let mut lines = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .filter(|l| !l.is_empty());
        let status = lines
            .next()
            .and_then(|l| l.strip_prefix("HTTP/"))
            .and_then(|l| l.split_whitespace().nth(1))
            .filter(|code| code.len() == 3)
            .and_then(digits)
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or(HeadError::NotHttp)?;
        let mut fields: Vec<(&str, String)> = Vec::new();
        for line in lines {
            if line.starts_with([' ', '\t']) {
                let (_, value) = fields.last_mut().ok_or(HeadError::NotHttp)?;
                value.push(' ');
                value.push_str(line.trim());
            } else if let Some((name, value)) = line.split_once(':') {
                fields.push((name.trim(), value.trim().to_string()));
            }
        }
        let mut head = Head {
            status,
            content_length: None,
            chunked: false,
            location: None,
        };
        let mut transfer_coded = false;
        for (name, value) in fields {
            if name.eq_ignore_ascii_case("content-length") {
                let len = digits(&value)
                    .and_then(|v| v.parse().ok())
                    .ok_or(HeadError::BadContentLength)?;
                if head.content_length.is_some_and(|l| l != len) {
                    return Err(HeadError::BadContentLength);
                }
                head.content_length = Some(len);
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                transfer_coded = true;
                head.chunked = value
                    .rsplit(',')
                    .next()
                    .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
            } else if name.eq_ignore_ascii_case("location")
                && head.location.is_none()
                && !value.is_empty()
            {
                head.location = Some(value);
            }
        }
        if transfer_coded {
            head.content_length = None;
        }
        Ok(head)
    }

    /// An interim 1xx response, after which the real one follows. `101` is
    /// a protocol switch, not an interim answer.
    pub fn is_interim(&self) -> bool {
        (100..200).contains(&self.status) && self.status != 101
    }

    pub fn is_redirect(&self) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308)
    }

    /// Whether a body follows this head, per RFC 9112 §6.3.
    pub fn has_body(&self, method: &str) -> bool {
        method != "HEAD"
            && !(100..200).contains(&self.status)
            && self.status != 204
            && self.status != 304
    }
}

const MAX_CHUNK_LINE: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChunkError;

#[derive(Clone, Copy)]
enum State {
    Size,
    Data(u64),
    DataEnd,
    Trailer,
    Done,
}

/// `Transfer-Encoding: chunked` (RFC 9112 §7.1), decoded from input that
/// may be split anywhere.
pub struct Chunked {
    state: State,
    line: Vec<u8>,
}

impl Default for Chunked {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunked {
    pub fn new() -> Self {
        Self {
            state: State::Size,
            line: Vec::new(),
        }
    }

    /// The terminating chunk and the trailer section have both been read.
    pub fn done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    fn line(&mut self, input: &mut &[u8]) -> Result<Option<Vec<u8>>, ChunkError> {
        let Some(i) = input.iter().position(|&b| b == b'\n') else {
            self.line.extend_from_slice(input);
            *input = &[];
            return if self.line.len() > MAX_CHUNK_LINE {
                Err(ChunkError)
            } else {
                Ok(None)
            };
        };
        self.line.extend_from_slice(&input[..i]);
        *input = &input[i + 1..];
        if self.line.len() > MAX_CHUNK_LINE {
            return Err(ChunkError);
        }
        let mut line = core::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(Some(line))
    }

    /// The next run of body bytes in `input`, advancing past everything
    /// consumed; `None` once `input` is used up or the body is done.
    pub fn next<'a>(&mut self, input: &mut &'a [u8]) -> Result<Option<&'a [u8]>, ChunkError> {
        while !input.is_empty() {
            match self.state {
                State::Size => {
                    let Some(line) = self.line(input)? else {
                        return Ok(None);
                    };
                    let digits = line.split(|&b| b == b';').next().unwrap_or(&[]);
                    let digits = core::str::from_utf8(digits).map_err(|_| ChunkError)?.trim();
                    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(ChunkError);
                    }
                    let size = u64::from_str_radix(digits, 16).map_err(|_| ChunkError)?;
                    self.state = if size == 0 {
                        State::Trailer
                    } else {
                        State::Data(size)
                    };
                }
                State::Data(left) => {
                    let take = usize::try_from(left).unwrap_or(usize::MAX).min(input.len());
                    let (data, rest) = input.split_at(take);
                    *input = rest;
                    let left = left - take as u64;
                    self.state = if left == 0 {
                        State::DataEnd
                    } else {
                        State::Data(left)
                    };
                    return Ok(Some(data));
                }
                State::DataEnd => {
                    if let Some(line) = self.line(input)? {
                        if !line.is_empty() {
                            return Err(ChunkError);
                        }
                        self.state = State::Size;
                    }
                }
                State::Trailer => {
                    if let Some(line) = self.line(input)?
                        && line.is_empty()
                    {
                        self.state = State::Done;
                    }
                }
                State::Done => return Ok(None),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_with_scheme_defaults() {
        let u = Url::parse("HTTPS://Example.COM/a/b?x=1#frag").expect("parses");
        assert_eq!(
            u,
            Url {
                scheme: Scheme::Https,
                host: "example.com".into(),
                port: 443,
                target: "/a/b?x=1".into(),
            }
        );
        let u = Url::parse("http://10.0.2.2:8080").expect("parses");
        assert_eq!(
            (u.port, u.target.as_str(), u.host_header()),
            (8080, "/", "10.0.2.2:8080".into())
        );
        assert_eq!(Url::parse("http://h?q").expect("parses").target, "/?q");
        assert_eq!(
            Url::parse("ftp://x/"),
            Err(UrlError::UnsupportedScheme("ftp".into()))
        );
        assert_eq!(Url::parse("http://user@x/"), Err(UrlError::Credentials));
        assert_eq!(Url::parse("http://x:0/"), Err(UrlError::BadPort));
        assert_eq!(Url::parse("http://[::1]/"), Err(UrlError::Ipv6));
        assert_eq!(Url::parse("http://a b/"), Err(UrlError::BadHost));
        assert_eq!(Url::parse("http://x/a b"), Err(UrlError::BadTarget));
        assert_eq!(Url::parse("x.test/"), Err(UrlError::NoScheme));
        assert_eq!(Url::parse("x.test/?u=http://y"), Err(UrlError::NoScheme));
        assert_eq!(Url::parse("localhost:8080"), Err(UrlError::NoScheme));
        assert_eq!(Url::parse("http://x:+80/"), Err(UrlError::BadPort));
        assert_eq!(Url::parse("http://x:/").expect("empty port").port, 80);
        assert_eq!(
            Url::parse("http://x/a/./b/../c").expect("parses").target,
            "/a/c"
        );
    }

    #[test]
    fn redirects_resolve_against_the_base() {
        let base = Url::parse("http://a.test:81/dir/page?q").expect("base");
        let go = |loc: &str| base.join(loc).expect("resolves");
        let abs = go("https://b.test/x");
        assert_eq!((abs.scheme, abs.port), (Scheme::Https, 443));
        let rel = go("//c.test/y");
        assert_eq!(
            (rel.scheme, rel.host.as_str(), rel.port),
            (Scheme::Http, "c.test", 80)
        );
        assert_eq!(go("/root").target, "/root");
        assert_eq!(go("/root").port, 81);
        assert_eq!(go("sibling?z").target, "/dir/sibling?z");
        assert_eq!(go(" /spaced \r").target, "/spaced");
        assert!(base.join("/a\u{7f}").is_err());
        assert_eq!(go("?z").target, "/dir/page?z");
        assert_eq!(go("").target, "/dir/page?q");
        assert_eq!(go("#f").target, "/dir/page?q");
        assert_eq!(go("../up").target, "/up");
        assert_eq!(go("../../../up/./x/").target, "/up/x/");
        assert_eq!(
            go("/login?next=https://b.test/").target,
            "/login?next=https://b.test/"
        );
        assert_eq!(go("/a b/\u{e9}").target, "/a%20b/%C3%A9");
        assert_eq!(base.join("http:/g"), Err(UrlError::BadHost));
        assert_eq!(
            base.join("mailto:x@y"),
            Err(UrlError::UnsupportedScheme("mailto".into()))
        );
        let other_port = go("http://a.test/");
        assert!(!base.same_origin(&other_port) && base.same_origin(&go("/elsewhere")));
    }

    #[test]
    fn heads_parse() {
        let raw = b"HTTP/1.1 301 Moved\r\nLocation: /x\r\ncontent-length: 5\r\n\r\n";
        assert_eq!(head_end(raw), Some(raw.len()));
        let h = Head::parse(raw).expect("parses");
        assert_eq!(
            h,
            Head {
                status: 301,
                content_length: Some(5),
                chunked: false,
                location: Some("/x".into()),
            }
        );
        assert!(h.is_redirect() && h.has_body("GET") && !h.has_body("HEAD"));
        let h = Head::parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n")
            .expect("parses");
        assert!(h.chunked);
        assert_eq!(Head::parse(b"SSH-2.0-x\r\n\r\n"), Err(HeadError::NotHttp));
        assert_eq!(
            Head::parse(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n"),
            Err(HeadError::BadContentLength),
            "conflicting lengths are how a response gets smuggled"
        );
        assert!(
            !Head::parse(b"HTTP/1.1 204 No Content\r\n\r\n")
                .expect("parses")
                .has_body("GET")
        );
        assert_eq!(
            Head::parse(b"HTTP/1.1 +20 X\r\n\r\n"),
            Err(HeadError::NotHttp)
        );
        assert_eq!(
            Head::parse(b"HTTP/1.1 200 OK\r\nContent-Length: +5\r\n\r\n"),
            Err(HeadError::BadContentLength)
        );
        let folded =
            Head::parse(b"HTTP/1.1 200 OK\r\nX: a\r\n Location: /evil\r\n\r\n").expect("parses");
        assert_eq!(
            folded.location, None,
            "a folded line continues the field above it"
        );
        let gzip =
            Head::parse(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n")
                .expect("parses");
        assert_eq!(
            (gzip.chunked, gzip.content_length),
            (false, None),
            "a transfer coding overrides the length: the body runs to the close"
        );
        let bare = b"HTTP/1.0 200 OK\nContent-Length: 3\n\nhi\n";
        assert_eq!(
            head_end(bare),
            Some(bare.len() - 3),
            "a bare LF ends lines too"
        );
        let h = Head::parse(&bare[..bare.len() - 3]).expect("parses");
        assert_eq!((h.status, h.content_length), (200, Some(3)));
        let locations = Head::parse(
            b"HTTP/1.1 302 F\r\nLocation:\r\nLocation: /first\r\nLocation: /second\r\n\r\n",
        )
        .expect("parses");
        assert_eq!(
            locations.location.as_deref(),
            Some("/first"),
            "the first non-empty one"
        );
        let early = Head::parse(b"HTTP/1.1 103 Early Hints\r\n\r\n").expect("parses");
        assert!(early.is_interim());
        assert!(
            !Head::parse(b"HTTP/1.1 101 Switching\r\n\r\n")
                .expect("parses")
                .is_interim()
        );
    }

    fn decode(wire: &[u8], split: usize) -> Result<(Vec<u8>, bool), ChunkError> {
        let mut c = Chunked::new();
        let mut out = Vec::new();
        for piece in wire.chunks(split) {
            let mut input = piece;
            while let Some(data) = c.next(&mut input)? {
                out.extend_from_slice(data);
            }
        }
        Ok((out, c.done()))
    }

    #[test]
    fn chunked_bodies_decode_across_any_split() {
        let wire = b"4\r\nWiki\r\n6;ext=1\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\nX-Trailer: y\r\n\r\n";
        for split in 1..=wire.len() {
            let (out, done) = decode(wire, split).expect("decodes");
            assert!(done, "split {split}");
            assert_eq!(out, b"Wikipedia in \r\n\r\nchunks.");
        }
        assert_eq!(decode(b"zz\r\n", 4), Err(ChunkError));
        assert_eq!(decode(b"+4\r\nWiki\r\n", 16), Err(ChunkError));
        assert_eq!(
            decode(b"2\r\nabX\r\n", 16),
            Err(ChunkError),
            "data not followed by CRLF"
        );
        let long = [b'1'; 5000];
        assert_eq!(decode(&long, 1000), Err(ChunkError));
        let (_, done) = decode(b"3\r\nabc\r\n", 64).expect("partial");
        assert!(!done, "a body without its last chunk is not done");
    }
}
