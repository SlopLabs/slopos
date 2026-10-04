//! The wire protocol between `remoted`, which runs on a SlopOS machine and
//! dials out, and `scripts/remote.py`, the broker and CLI on the developer's
//! host.
//!
//! An agent connection is TLS 1.3 to the broker's one port, authenticated by
//! the broker's certificate under a pinned CA and by the token the agent sends
//! first. It carries frames: a 5-byte header — the kind and a big-endian `u32`
//! payload length — then at most [`MAX_PAYLOAD`] bytes. A control payload is
//! [`Fields`]; data frames carry raw bytes, at most [`CHUNK`] of them. A
//! connection carries one request and is then closed:
//!
//! ```text
//! agent  HELLO{token, proto, host, version, tag, base_tag, boot, uptime_ms}
//! broker WELCOME                  idle: either side PINGs, the other PONGs
//! broker EXEC{arg.., cwd?, env.. (K=V), stdin?, timeout_ms?}
//!   agent  ACCEPT{pid} | ERROR{msg}
//!   broker DATA.. EOF             the child's stdin, when `stdin` was asked for
//!   agent  STDOUT.. STDERR.. EXIT{code | signal, timed_out?}
//! broker PUT{path, mode?}
//!   agent  ACCEPT | ERROR{msg}
//!   broker DATA.. EOF
//!   agent  DONE{size, sha256} | ERROR{msg}
//! broker GET{path}
//!   agent  ACCEPT{size} | ERROR{msg}
//!   agent  DATA.. EOF DONE{size, sha256} | ERROR{msg}
//! ```
//!
//! The host pairs a machine by building the base it boots with the CA, the
//! token and a [`Conf`] naming the broker in it (`remote.py provision`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_tls_core::hash::{Hash, Sha256};

/// What HELLO's `proto` says; a broker refuses another.
pub const PROTOCOL: &str = "1";
pub const HEADER_LEN: usize = 5;
pub const MAX_PAYLOAD: usize = 1 << 20;
/// The largest DATA, STDOUT or STDERR payload either side sends.
pub const CHUNK: usize = 64 * 1024;
pub const DEFAULT_PORT: u16 = 7330;
pub const DEFAULT_SERVER_NAME: &str = "slopos-remote";

pub mod kind {
    pub const HELLO: u8 = 0x01;
    pub const WELCOME: u8 = 0x02;
    pub const PING: u8 = 0x03;
    pub const PONG: u8 = 0x04;
    pub const EXEC: u8 = 0x10;
    pub const PUT: u8 = 0x11;
    pub const GET: u8 = 0x12;
    pub const DATA: u8 = 0x20;
    pub const STDOUT: u8 = 0x21;
    pub const STDERR: u8 = 0x22;
    pub const EOF: u8 = 0x23;
    pub const ACCEPT: u8 = 0x30;
    pub const EXIT: u8 = 0x31;
    pub const DONE: u8 = 0x32;
    pub const ERROR: u8 = 0x3f;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// A header announcing more than [`MAX_PAYLOAD`] bytes.
    TooLarge(usize),
    /// A control payload that is not a well-formed [`Fields`].
    BadFields,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::TooLarge(len) => {
                write!(
                    f,
                    "a frame of {len} bytes, past the {MAX_PAYLOAD}-byte bound"
                )
            }
            WireError::BadFields => f.write_str("a malformed control payload"),
        }
    }
}

pub fn header(kind: u8, len: usize) -> [u8; HEADER_LEN] {
    assert!(len <= MAX_PAYLOAD, "frame payload past MAX_PAYLOAD");
    let len = (len as u32).to_be_bytes();
    [kind, len[0], len[1], len[2], len[3]]
}

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header(kind, payload.len()));
    out.extend_from_slice(payload);
    out
}

/// Frames out of a byte stream that arrives in pieces of any size.
#[derive(Default)]
pub struct Deframer {
    buf: Vec<u8>,
    start: usize,
}

impl Deframer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= CHUNK {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame; `None` until one has arrived. An oversized header
    /// is an error at once, before its payload is buffered.
    pub fn next_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>, WireError> {
        let pending = &self.buf[self.start..];
        if pending.len() < HEADER_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(WireError::TooLarge(len));
        }
        if pending.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let kind = pending[0];
        let payload = pending[HEADER_LEN..HEADER_LEN + len].to_vec();
        self.start += HEADER_LEN + len;
        Ok(Some((kind, payload)))
    }
}

/// A control payload: `(key, value)` pairs in order, a key repeating for a
/// list. Each is a `u8` key length, the key (printable ASCII), a big-endian
/// `u32` value length and the value, which is any bytes.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Fields {
    entries: Vec<(String, Vec<u8>)>,
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 255 && key.bytes().all(|b| b.is_ascii_graphic())
}

impl Fields {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, key: &str, value: impl AsRef<[u8]>) -> Self {
        self.push(key, value);
        self
    }

    pub fn push(&mut self, key: &str, value: impl AsRef<[u8]>) {
        assert!(
            valid_key(key),
            "field key must be 1..=255 printable ASCII bytes"
        );
        self.entries
            .push((key.to_string(), value.as_ref().to_vec()));
    }

    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| core::str::from_utf8(v).ok())
    }

    pub fn number(&self, key: &str) -> Option<u64> {
        self.text(key).and_then(|v| v.parse().ok())
    }

    pub fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.entries
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_slice())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (key, value) in &self.entries {
            out.push(key.len() as u8);
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(&(value.len() as u32).to_be_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    pub fn decode(mut bytes: &[u8]) -> Result<Self, WireError> {
        let mut fields = Fields::new();
        while let Some((&klen, rest)) = bytes.split_first() {
            let klen = klen as usize;
            if rest.len() < klen + 4 {
                return Err(WireError::BadFields);
            }
            let key = core::str::from_utf8(&rest[..klen]).map_err(|_| WireError::BadFields)?;
            if !valid_key(key) {
                return Err(WireError::BadFields);
            }
            let vlen =
                u32::from_be_bytes([rest[klen], rest[klen + 1], rest[klen + 2], rest[klen + 3]])
                    as usize;
            let value = rest
                .get(klen + 4..klen + 4 + vlen)
                .ok_or(WireError::BadFields)?;
            fields.entries.push((key.to_string(), value.to_vec()));
            bytes = &rest[klen + 4 + vlen..];
        }
        Ok(fields)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// SHA-256 over a stream, for what PUT wrote and GET read.
#[derive(Clone)]
pub struct Digest256(Sha256);

impl Default for Digest256 {
    fn default() -> Self {
        Self(Sha256::new())
    }
}

impl Digest256 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish_hex(self) -> String {
        hex(self.0.finish().as_ref())
    }
}

/// `remote.conf`: `key = value` lines, `#` comments.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Conf {
    /// `host:port`, the host a name or an IPv4 address.
    pub broker: String,
    pub server_name: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConfError {
    pub line: usize,
    pub msg: &'static str,
}

impl fmt::Display for ConfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(self.msg)
        } else {
            write!(f, "line {}: {}", self.line, self.msg)
        }
    }
}

impl Conf {
    pub fn parse(text: &str) -> Result<Conf, ConfError> {
        let mut broker = None;
        let mut server_name = None;
        for (i, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fail = |msg| ConfError { line: i + 1, msg };
            let (key, value) = line.split_once('=').ok_or(fail("expected key = value"))?;
            let value = value.trim();
            match key.trim() {
                "broker" => {
                    split_host_port(value).ok_or(fail("broker must be host:port"))?;
                    broker = Some(value.to_string());
                }
                "server_name" if !value.is_empty() => server_name = Some(value.to_string()),
                "server_name" => return Err(fail("server_name is empty")),
                _ => return Err(fail("unknown key")),
            }
        }
        Ok(Conf {
            broker: broker.ok_or(ConfError {
                line: 0,
                msg: "no broker = host:port line",
            })?,
            server_name: server_name.unwrap_or_else(|| DEFAULT_SERVER_NAME.to_string()),
        })
    }
}

pub fn split_host_port(addr: &str) -> Option<(&str, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let port = port.parse::<u16>().ok().filter(|&p| p != 0)?;
    (!host.is_empty() && !host.contains(':')).then_some((host, port))
}

/// The wait before the next dial: one second, doubling to thirty.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    secs: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { secs: 1 }
    }
}

impl Backoff {
    pub const MAX_SECS: u32 = 30;

    pub fn new() -> Self {
        Self::default()
    }

    /// This failure's wait; the next one waits twice as long.
    pub fn next_secs(&mut self) -> u32 {
        let now = self.secs;
        self.secs = (self.secs * 2).min(Self::MAX_SECS);
        now
    }

    pub fn reset(&mut self) {
        self.secs = 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;

    #[test]
    fn frames_survive_any_split() {
        let mut wire = frame(kind::STDOUT, b"hello");
        wire.extend(frame(kind::EOF, b""));
        wire.extend(frame(kind::DATA, &vec![7u8; CHUNK]));
        for step in [1, 2, 3, 5, 7, 4096, wire.len()] {
            let mut d = Deframer::new();
            let mut got = vec![];
            for piece in wire.chunks(step) {
                d.push(piece);
                while let Some(f) = d.next_frame().unwrap() {
                    got.push(f);
                }
            }
            assert_eq!(got.len(), 3, "step {step}");
            assert_eq!(got[0], (kind::STDOUT, b"hello".to_vec()));
            assert_eq!(got[1], (kind::EOF, vec![]));
            assert_eq!(got[2], (kind::DATA, vec![7u8; CHUNK]));
        }
    }

    #[test]
    fn an_oversized_header_fails_before_its_payload() {
        let mut d = Deframer::new();
        d.push(&[kind::DATA, 0, 0x10, 0, 1]);
        assert_eq!(d.next_frame(), Err(WireError::TooLarge(MAX_PAYLOAD + 1)));
    }

    #[test]
    fn the_header_is_kind_then_big_endian_length() {
        assert_eq!(header(0x21, 0x0102), [0x21, 0, 0, 1, 2]);
        assert_eq!(header(1, MAX_PAYLOAD), [1, 0, 0x10, 0, 0]);
    }

    #[test]
    #[should_panic]
    fn an_oversized_frame_is_never_built() {
        header(kind::DATA, MAX_PAYLOAD + 1);
    }

    #[test]
    fn fields_round_trip_with_repeats_and_binary_values() {
        let f = Fields::new()
            .with("arg", "/bin/shell")
            .with("arg", "-c")
            .with("arg", [0u8, 0xff, b'\n'])
            .with("cwd", "/")
            .with("timeout_ms", "1500");
        let back = Fields::decode(&f.encode()).unwrap();
        assert_eq!(back, f);
        let args: Vec<&[u8]> = back.all("arg").collect();
        assert_eq!(args, [&b"/bin/shell"[..], b"-c", &[0, 0xff, b'\n']]);
        assert_eq!(back.text("cwd"), Some("/"));
        assert_eq!(back.number("timeout_ms"), Some(1500));
        assert_eq!(back.get("missing"), None);
        assert_eq!(Fields::decode(&[]).unwrap(), Fields::new());
    }

    #[test]
    fn malformed_fields_are_refused() {
        let good = Fields::new().with("path", "/etc/motd").encode();
        for cut in 1..good.len() {
            assert_eq!(
                Fields::decode(&good[..cut]),
                Err(WireError::BadFields),
                "cut {cut}"
            );
        }
        assert_eq!(Fields::decode(&[0, 0, 0, 0, 0]), Err(WireError::BadFields));
        assert_eq!(
            Fields::decode(&[1, b' ', 0, 0, 0, 0]),
            Err(WireError::BadFields)
        );
        assert_eq!(
            Fields::decode(&[1, b'k', 0xff, 0xff, 0xff, 0xff]),
            Err(WireError::BadFields)
        );
    }

    #[test]
    fn hex_is_lower_case_pairs() {
        assert_eq!(hex(&[0, 0xab, 0x10]), "00ab10");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn digest_is_sha256() {
        let mut d = Digest256::new();
        d.update(b"a");
        d.update(b"bc");
        assert_eq!(
            d.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn conf_parses_what_the_host_writes() {
        // Byte for byte what `remote.py provision` writes.
        let written = "# Written by `remote.py provision`; remoted dials the broker here.\n\
                       broker = 10.0.2.2:7330\nserver_name = slopos-remote\n";
        let conf = Conf::parse(written).unwrap();
        assert_eq!(conf.broker, "10.0.2.2:7330");
        assert_eq!(conf.server_name, DEFAULT_SERVER_NAME);
        assert_eq!(
            Conf::parse("broker = h:1\n").unwrap().server_name,
            DEFAULT_SERVER_NAME
        );
        let named = Conf::parse("broker=host.lan:1\nserver_name = x\n").unwrap();
        assert_eq!(named.server_name, "x");
        assert_eq!(Conf::parse("").unwrap_err().line, 0);
        assert_eq!(Conf::parse("broker = nohost\n").unwrap_err().line, 1);
        assert_eq!(Conf::parse("#\nbroker\n").unwrap_err().line, 2);
        assert_eq!(Conf::parse("color = red\n").unwrap_err().msg, "unknown key");
        assert_eq!(
            Conf::parse("broker = h:1\nserver_name =\n")
                .unwrap_err()
                .msg,
            "server_name is empty"
        );
    }

    #[test]
    fn host_port_splits() {
        assert_eq!(split_host_port("10.0.2.2:7330"), Some(("10.0.2.2", 7330)));
        assert_eq!(split_host_port("host:0"), None);
        assert_eq!(split_host_port(":1"), None);
        assert_eq!(split_host_port("host"), None);
        assert_eq!(split_host_port("::1:5"), None);
    }

    #[test]
    fn backoff_doubles_to_thirty_and_resets() {
        let mut b = Backoff::new();
        let waits: Vec<u32> = (0..7).map(|_| b.next_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);
        b.reset();
        assert_eq!(b.next_secs(), 1);
    }
}
