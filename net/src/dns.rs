//! DNS client: wire protocol, cache, and resolver.
//!
//! An in-kernel stub resolver for A records over UDP, `SYSCALL_RESOLVE`'s
//! backend. Each lookup holds its own query slot, source port and wait queue,
//! so a slow server delays only the lookups that asked it.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::net::NET_MAX_RESOLVERS;
use slopos_ostd::klog_debug;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock, WaitAbort, WaitQueue};

use crate::resolver::RESOLVER;
use crate::types::{Ipv4Addr, Port};

pub const DNS_PORT: u16 = 53;
/// Maximum DNS name length (RFC 1035).
const DNS_NAME_MAX: usize = 253;
/// Maximum label length (RFC 1035).
const DNS_LABEL_MAX: usize = 63;
const DNS_HEADER_LEN: usize = 12;
const DNS_MAX_RESPONSE: usize = 512;
const MAX_CNAME_HOPS: usize = 8;
/// Compression-pointer follow budget, for loop detection.
const MAX_POINTER_FOLLOWS: usize = 16;
const DNS_CACHE_SIZE: usize = 16;
/// A wire name is at most 255 bytes; the question adds its type and class.
const QUESTION_MAX: usize = 255 + 4;

/// Lookups in flight at once, machine-wide; one more is refused as
/// [`DnsResolveError::Busy`] rather than queued, the way a full socket table is.
pub const MAX_INFLIGHT: usize = 16;

/// RFC 5452 §9: the transaction ID carries no entropy unless it is drawn
/// unpredictably, and the source port must contribute entropy of its own
/// rather than being a function of the ID.
fn random_query_id() -> u16 {
    slopos_kernel_services::platform::rng_next() as u16
}

/// Ephemeral source port for a query, drawn independently of the ID.
fn random_source_port() -> u16 {
    const EPHEMERAL_BASE: u32 = 49_152;
    const EPHEMERAL_COUNT: u32 = 16_384;
    let r = (slopos_kernel_services::platform::rng_next() % EPHEMERAL_COUNT as u64) as u32;
    (EPHEMERAL_BASE + r) as u16
}

/// Errors returned by [`dns_resolve`]. Once every attempt is spent, the
/// lookup reports how the last one failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsResolveError {
    /// No nameserver configured, statically or by DHCP.
    NoDnsServer,
    InvalidHostname,
    Timeout,
    TransmitFailed,
    ParseFailed,
    /// The server answered that the name has no address: NXDOMAIN, or a
    /// NOERROR carrying no A record. Authoritative, so never retried.
    NameNotFound,
    /// SERVFAIL, REFUSED, or an answer too large for a datagram.
    ServerFailure,
    /// No query slot, source port or buffer was free; transient.
    Busy,
    /// The caller was killed while it waited.
    Interrupted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DnsType {
    A = 1,
    CNAME = 5,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DnsClass {
    IN = 1,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum DnsRcode {
    NoError = 0,
    ServFail = 2,
    NXDomain = 3,
    Refused = 5,
}

/// Parsed DNS header (12 bytes).
#[derive(Clone, Copy, Default)]
pub struct DnsHeader {
    pub id: u16,
    pub flags: u16,
    pub qdcount: u16,
    pub ancount: u16,
    pub nscount: u16,
    pub arcount: u16,
}

impl DnsHeader {
    /// QR bit: 0 = query, 1 = response.
    pub fn qr(&self) -> bool {
        (self.flags & 0x8000) != 0
    }

    /// TC bit: the answer did not fit the datagram.
    pub fn tc(&self) -> bool {
        (self.flags & 0x0200) != 0
    }

    /// RCODE (low 4 bits of flags).
    pub fn rcode(&self) -> u8 {
        (self.flags & 0x000F) as u8
    }

    fn to_bytes(&self, buf: &mut [u8]) {
        buf[0..2].copy_from_slice(&self.id.to_be_bytes());
        buf[2..4].copy_from_slice(&self.flags.to_be_bytes());
        buf[4..6].copy_from_slice(&self.qdcount.to_be_bytes());
        buf[6..8].copy_from_slice(&self.ancount.to_be_bytes());
        buf[8..10].copy_from_slice(&self.nscount.to_be_bytes());
        buf[10..12].copy_from_slice(&self.arcount.to_be_bytes());
    }

    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < DNS_HEADER_LEN {
            return None;
        }
        Some(DnsHeader {
            id: u16::from_be_bytes([buf[0], buf[1]]),
            flags: u16::from_be_bytes([buf[2], buf[3]]),
            qdcount: u16::from_be_bytes([buf[4], buf[5]]),
            ancount: u16::from_be_bytes([buf[6], buf[7]]),
            nscount: u16::from_be_bytes([buf[8], buf[9]]),
            arcount: u16::from_be_bytes([buf[10], buf[11]]),
        })
    }
}

#[derive(Clone, Copy)]
pub struct DnsResponse {
    pub addr: [u8; 4],
    pub ttl: u32,
}

/// Encode a hostname into DNS wire format (length-prefixed labels):
/// `"example.com"` → `[7, e,x,a,m,p,l,e, 3, c,o,m, 0]`.
///
/// Returns the number of bytes written, or `None` on invalid input.
pub fn dns_encode_name(hostname: &[u8], buf: &mut [u8]) -> Option<usize> {
    if hostname.is_empty() {
        if buf.is_empty() {
            return None;
        }
        buf[0] = 0;
        return Some(1);
    }

    if hostname.len() > DNS_NAME_MAX {
        return None;
    }

    let hostname = if hostname.last() == Some(&b'.') {
        &hostname[..hostname.len() - 1]
    } else {
        hostname
    };

    if hostname.is_empty() {
        if buf.is_empty() {
            return None;
        }
        buf[0] = 0;
        return Some(1);
    }

    let mut out_pos = 0usize;

    for label in hostname.split(|&b| b == b'.') {
        if label.is_empty() || label.len() > DNS_LABEL_MAX {
            return None;
        }
        // Length byte + label + room for the trailing zero.
        if out_pos + 1 + label.len() >= buf.len() {
            return None;
        }
        buf[out_pos] = label.len() as u8;
        out_pos += 1;
        buf[out_pos..out_pos + label.len()].copy_from_slice(label);
        out_pos += label.len();
    }

    if out_pos >= buf.len() {
        return None;
    }
    buf[out_pos] = 0;
    out_pos += 1;

    Some(out_pos)
}

/// Build a DNS query packet, returning its total length or `None` on error.
pub fn dns_build_query(id: u16, hostname: &[u8], qtype: DnsType, buf: &mut [u8]) -> Option<usize> {
    if buf.len() < DNS_HEADER_LEN + 4 {
        return None;
    }

    let header = DnsHeader {
        id,
        flags: 0x0100, // RD = 1
        qdcount: 1,
        ancount: 0,
        nscount: 0,
        arcount: 0,
    };
    header.to_bytes(&mut buf[..DNS_HEADER_LEN]);

    let name_len = dns_encode_name(hostname, &mut buf[DNS_HEADER_LEN..])?;
    let q_start = DNS_HEADER_LEN + name_len;

    if q_start + 4 > buf.len() {
        return None;
    }

    buf[q_start..q_start + 2].copy_from_slice(&(qtype as u16).to_be_bytes());
    buf[q_start + 2..q_start + 4].copy_from_slice(&(DnsClass::IN as u16).to_be_bytes());

    Some(q_start + 4)
}

/// Decode a DNS name from wire format, following compression pointers.
///
/// Writes a dotted string (`example.com`) into `out` and returns
/// `(decoded_name_len, wire_bytes_consumed)`, or `None` on error.
pub fn dns_decode_name(packet: &[u8], offset: usize, out: &mut [u8]) -> Option<(usize, usize)> {
    let mut pos = offset;
    let mut out_pos = 0usize;
    let mut jumped = false;
    let mut wire_consumed = 0usize;
    let mut pointer_count = 0usize;

    loop {
        if pos >= packet.len() {
            return None;
        }

        let len_or_ptr = packet[pos];

        if len_or_ptr == 0 {
            if !jumped {
                wire_consumed = pos + 1 - offset;
            }
            break;
        }

        if (len_or_ptr & 0xC0) == 0xC0 {
            // Compression pointer
            if pos + 1 >= packet.len() {
                return None;
            }
            if !jumped {
                wire_consumed = pos + 2 - offset;
            }
            let ptr_offset = ((len_or_ptr as usize & 0x3F) << 8) | (packet[pos + 1] as usize);
            if ptr_offset >= packet.len() {
                return None;
            }
            pointer_count += 1;
            if pointer_count > MAX_POINTER_FOLLOWS {
                return None;
            }
            pos = ptr_offset;
            jumped = true;
            continue;
        }

        if (len_or_ptr & 0xC0) != 0 {
            // Reserved label type
            return None;
        }

        let label_len = len_or_ptr as usize;
        if label_len > DNS_LABEL_MAX {
            return None;
        }
        pos += 1;
        if pos + label_len > packet.len() {
            return None;
        }

        if out_pos > 0 {
            if out_pos >= out.len() {
                return None;
            }
            out[out_pos] = b'.';
            out_pos += 1;
        }

        if out_pos + label_len > out.len() {
            return None;
        }
        out[out_pos..out_pos + label_len].copy_from_slice(&packet[pos..pos + label_len]);
        out_pos += label_len;
        pos += label_len;
    }

    Some((out_pos, wire_consumed))
}

/// Parse a DNS response packet and extract the first A record.
///
/// Chases CNAME records up to `MAX_CNAME_HOPS` deep.
pub fn dns_parse_response(packet: &[u8], expected_id: u16) -> Option<DnsResponse> {
    let header = DnsHeader::from_bytes(packet)?;

    if !header.qr() {
        return None;
    }
    if header.id != expected_id {
        return None;
    }
    let rcode = header.rcode();
    if rcode != DnsRcode::NoError as u8 {
        return None;
    }

    let mut pos = DNS_HEADER_LEN;
    for _ in 0..header.qdcount {
        pos = skip_dns_name(packet, pos)?;
        // QTYPE + QCLASS.
        if pos + 4 > packet.len() {
            return None;
        }
        pos += 4;
    }

    let mut a_addr: Option<([u8; 4], u32)> = None;
    let mut _cname_hops = 0usize;

    for _ in 0..header.ancount {
        if pos >= packet.len() {
            break;
        }

        let name_end = skip_dns_name(packet, pos)?;
        pos = name_end;

        if pos + 10 > packet.len() {
            return None;
        }
        let rr_type = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
        let _rr_class = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]);
        let ttl = u32::from_be_bytes([
            packet[pos + 4],
            packet[pos + 5],
            packet[pos + 6],
            packet[pos + 7],
        ]);
        let rdlength = u16::from_be_bytes([packet[pos + 8], packet[pos + 9]]) as usize;
        pos += 10;

        if pos + rdlength > packet.len() {
            return None;
        }

        if rr_type == DnsType::A as u16 && rdlength == 4 {
            let mut addr = [0u8; 4];
            addr.copy_from_slice(&packet[pos..pos + 4]);
            klog_debug!(
                "dns: A record at offset {} -> {}.{}.{}.{} (raw: {:02x?})",
                pos,
                addr[0],
                addr[1],
                addr[2],
                addr[3],
                &packet[pos..pos + 4]
            );
            a_addr = Some((addr, ttl));
            // No break: a later answer may be the better one.
        } else if rr_type == DnsType::CNAME as u16 {
            _cname_hops += 1;
            if _cname_hops > MAX_CNAME_HOPS {
                return None;
            }
            // The A record for the CNAME target follows later in this section.
        }

        pos += rdlength;
    }

    a_addr.map(|(addr, ttl)| DnsResponse { addr, ttl })
}

/// Skip a DNS name in wire format, returning the offset after it.
fn skip_dns_name(packet: &[u8], mut pos: usize) -> Option<usize> {
    let mut pointer_count = 0usize;
    loop {
        if pos >= packet.len() {
            return None;
        }
        let b = packet[pos];
        if b == 0 {
            return Some(pos + 1);
        }
        if (b & 0xC0) == 0xC0 {
            // Compression pointer: 2 bytes, and skipping need not follow it.
            if pos + 1 >= packet.len() {
                return None;
            }
            return Some(pos + 2);
        }
        if (b & 0xC0) != 0 {
            return None; // Reserved
        }
        let label_len = b as usize;
        pos += 1 + label_len;
        pointer_count += 1;
        if pointer_count > MAX_POINTER_FOLLOWS {
            return None;
        }
    }
}

/// Keyed on the whole name, not a hash of it: a collision would let one name's
/// answer stand in for another's in a cache every local user shares.
#[derive(Clone, Copy)]
struct DnsCacheEntry {
    name: [u8; DNS_NAME_MAX],
    name_len: u8,
    addr: [u8; 4],
    /// Absolute expiry on the `clock::uptime_ms()` timeline.
    expiry_ms: u64,
    /// Drives LRU eviction.
    last_used_ms: u64,
    valid: bool,
}

impl DnsCacheEntry {
    const fn empty() -> Self {
        Self {
            name: [0; DNS_NAME_MAX],
            name_len: 0,
            addr: [0; 4],
            expiry_ms: 0,
            last_used_ms: 0,
            valid: false,
        }
    }

    fn names(&self, hostname: &[u8]) -> bool {
        self.valid && self.name[..self.name_len as usize].eq_ignore_ascii_case(hostname)
    }
}

struct DnsCache {
    entries: [DnsCacheEntry; DNS_CACHE_SIZE],
}

/// A trailing dot names the same host.
fn cache_key(hostname: &[u8]) -> &[u8] {
    hostname.strip_suffix(b".").unwrap_or(hostname)
}

impl DnsCache {
    const fn new() -> Self {
        Self {
            entries: [DnsCacheEntry::empty(); DNS_CACHE_SIZE],
        }
    }

    fn lookup(&mut self, hostname: &[u8]) -> Option<[u8; 4]> {
        let key = cache_key(hostname);
        let now = slopos_kernel_services::clock::uptime_ms();
        let entry = self.entries.iter_mut().find(|e| e.names(key))?;
        if now < entry.expiry_ms {
            entry.last_used_ms = now;
            Some(entry.addr)
        } else {
            entry.valid = false;
            None
        }
    }

    fn insert(&mut self, hostname: &[u8], addr: [u8; 4], ttl_secs: u32) {
        let key = cache_key(hostname);
        if key.len() > DNS_NAME_MAX {
            return;
        }
        let now = slopos_kernel_services::clock::uptime_ms();
        // Floor of 60 s: a shorter TTL thrashes the cache.
        let ttl_ms = (ttl_secs.max(60) as u64) * 1000;

        let victim = match self.entries.iter().position(|e| e.names(key)) {
            Some(i) => i,
            None => match self.entries.iter().position(|e| !e.valid) {
                Some(i) => i,
                None => self
                    .entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.last_used_ms)
                    .map_or(0, |(i, _)| i),
            },
        };
        let entry = &mut self.entries[victim];
        entry.name[..key.len()].copy_from_slice(key);
        entry.name_len = key.len() as u8;
        entry.addr = addr;
        entry.expiry_ms = now + ttl_ms;
        entry.last_used_ms = now;
        entry.valid = true;
    }

    fn flush(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.valid = false;
        }
    }
}

static DNS_CACHE: SpinLock<DnsCache> = SpinLock::new(
    DnsCache::new(),
    lock_class!("DNS_CACHE", LOCK_LEVEL_REGISTRY),
);

pub fn dns_cache_lookup(hostname: &[u8]) -> Option<[u8; 4]> {
    DNS_CACHE.lock().lookup(hostname)
}

pub fn dns_cache_insert(hostname: &[u8], addr: [u8; 4], ttl_secs: u32) {
    DNS_CACHE.lock().insert(hostname, addr, ttl_secs);
}

pub fn dns_cache_flush() {
    DNS_CACHE.lock().flush();
}

/// What a reply that matched its query says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyVerdict {
    Address {
        addr: [u8; 4],
        ttl: u32,
    },
    /// NXDOMAIN, or NOERROR with no A record in the answer section.
    NoAddress,
    /// SERVFAIL, REFUSED, any other RCODE, or truncated: another server may do
    /// better.
    ServerFailure,
    Malformed,
}

pub fn classify_reply(packet: &[u8], expected_id: u16) -> ReplyVerdict {
    let Some(header) = DnsHeader::from_bytes(packet) else {
        return ReplyVerdict::Malformed;
    };
    if !header.qr() || header.id != expected_id {
        return ReplyVerdict::Malformed;
    }
    match header.rcode() {
        rcode if rcode == DnsRcode::NoError as u8 => {
            match dns_parse_response(packet, expected_id) {
                Some(resp) => ReplyVerdict::Address {
                    addr: resp.addr,
                    ttl: resp.ttl,
                },
                None if header.tc() => ReplyVerdict::ServerFailure,
                None if answer_section_parses(packet, &header) => ReplyVerdict::NoAddress,
                None => ReplyVerdict::Malformed,
            }
        }
        rcode if rcode == DnsRcode::NXDomain as u8 => ReplyVerdict::NoAddress,
        _ => ReplyVerdict::ServerFailure,
    }
}

fn answer_section_parses(packet: &[u8], header: &DnsHeader) -> bool {
    let mut pos = DNS_HEADER_LEN;
    for _ in 0..header.qdcount {
        let Some(end) = skip_dns_name(packet, pos) else {
            return false;
        };
        pos = end + 4;
    }
    for _ in 0..header.ancount {
        let Some(end) = skip_dns_name(packet, pos) else {
            return false;
        };
        if end + 10 > packet.len() {
            return false;
        }
        let rdlength = u16::from_be_bytes([packet[end + 8], packet[end + 9]]) as usize;
        pos = end + 10 + rdlength;
    }
    pos <= packet.len()
}

/// Same transaction ID and same question, name case-insensitive (RFC 5452
/// §9.1): the ID alone is sixteen bits an off-path attacker can walk.
pub fn reply_matches(query: &[u8], reply: &[u8]) -> bool {
    if query.len() < DNS_HEADER_LEN || reply.len() < DNS_HEADER_LEN {
        return false;
    }
    if query[..2] != reply[..2] || reply[4..6] != [0, 1] {
        return false;
    }
    let question = &query[DNS_HEADER_LEN..];
    reply
        .get(DNS_HEADER_LEN..DNS_HEADER_LEN + question.len())
        .is_some_and(|echoed| echoed.eq_ignore_ascii_case(question))
}

/// I/O-free DNS resolver state machine.
///
/// A driver pumps it: each [`step`](DnsResolver::step) reports the outcome of
/// the previous action ([`DnsOutcome`]) and gets back the next ([`DnsStep`]),
/// so retry counting, server rotation, error precedence and parsing are
/// testable from canned reply bytes.
pub struct DnsResolver {
    /// Query rounds still allowed (including the one currently outstanding).
    attempts_remaining: usize,
    server_count: usize,
    server: usize,
    timeout_ms: u32,
    cur_id: u16,
    /// Most recent transient failure, returned if all attempts are exhausted.
    last_error: DnsResolveError,
    /// Pre-built query packet; only the 2-byte ID changes between attempts.
    query_buf: slopos_ostd::KVec<u8>,
    query_len: usize,
}

/// Outcome of the previous [`DnsStep::Query`], reported back to
/// [`DnsResolver::step`].
pub enum DnsOutcome<'a> {
    /// No prior attempt — kick off the first query.
    Start,
    TransmitFailed,
    Timeout,
    /// A reply the driver has already matched to the query.
    Reply(&'a [u8]),
}

/// The next action the driver should take on behalf of a [`DnsResolver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsStep {
    /// Transmit [`DnsResolver::query_bytes`] to the `server`th configured
    /// nameserver, wait up to `timeout_ms` for a reply, then call `step`
    /// again with the outcome.
    Query { server: usize, timeout_ms: u32 },
    /// The driver should cache `(addr, ttl)`.
    Resolved { addr: [u8; 4], ttl: u32 },
    /// Resolution failed permanently.
    Failed(DnsResolveError),
}

impl DnsResolver {
    /// Asks `servers` nameservers in turn, `attempts` rounds of them, waiting
    /// `timeout_ms` for each.
    ///
    /// [`DnsResolveError::InvalidHostname`] if the name cannot be encoded;
    /// [`DnsResolveError::Busy`] on allocation failure.
    pub fn new(
        hostname: &[u8],
        servers: usize,
        attempts: u32,
        timeout_ms: u32,
    ) -> Result<Self, DnsResolveError> {
        let mut query_buf =
            slopos_ostd::KVec::<u8>::zeroed(512).map_err(|_| DnsResolveError::Busy)?;
        let cur_id = random_query_id();
        let query_len = dns_build_query(cur_id, hostname, DnsType::A, query_buf.as_mut())
            .ok_or(DnsResolveError::InvalidHostname)?;
        if query_len - DNS_HEADER_LEN > QUESTION_MAX {
            return Err(DnsResolveError::InvalidHostname);
        }
        let servers = servers.max(1);
        Ok(Self {
            attempts_remaining: (attempts.max(1) as usize) * servers,
            server_count: servers,
            server: 0,
            timeout_ms,
            cur_id,
            last_error: DnsResolveError::Timeout,
            query_buf,
            query_len,
        })
    }

    /// The wire bytes of the current query (transaction ID already patched in).
    pub fn query_bytes(&self) -> &[u8] {
        &self.query_buf[..self.query_len]
    }

    pub fn query_id(&self) -> u16 {
        self.cur_id
    }

    fn refresh_id(&mut self) {
        self.cur_id = random_query_id();
        self.query_buf.as_mut()[0..2].copy_from_slice(&self.cur_id.to_be_bytes());
    }

    fn query(&self) -> DnsStep {
        DnsStep::Query {
            server: self.server,
            timeout_ms: self.timeout_ms,
        }
    }

    /// Advance the state machine given the outcome of the previous action.
    pub fn step(&mut self, outcome: DnsOutcome<'_>) -> DnsStep {
        match outcome {
            DnsOutcome::Start => return self.query(),
            DnsOutcome::Reply(bytes) => match classify_reply(bytes, self.cur_id) {
                ReplyVerdict::Address { addr, ttl } => return DnsStep::Resolved { addr, ttl },
                ReplyVerdict::NoAddress => {
                    return DnsStep::Failed(DnsResolveError::NameNotFound);
                }
                ReplyVerdict::ServerFailure => self.last_error = DnsResolveError::ServerFailure,
                ReplyVerdict::Malformed => self.last_error = DnsResolveError::ParseFailed,
            },
            DnsOutcome::Timeout => self.last_error = DnsResolveError::Timeout,
            DnsOutcome::TransmitFailed => self.last_error = DnsResolveError::TransmitFailed,
        }

        self.attempts_remaining -= 1;
        if self.attempts_remaining == 0 {
            return DnsStep::Failed(self.last_error);
        }
        self.server = (self.server + 1) % self.server_count;
        self.refresh_id();
        self.query()
    }
}

/// The resolver's owner tag in the UDP demux, equal to no socket index. A port
/// it holds refuses every socket's bind, `SO_REUSEADDR` included.
pub const RESOLVER_SOCKET: u32 = u32::MAX;

struct Slot {
    busy: bool,
    server: [u8; 4],
    port: u16,
    query: [u8; DNS_HEADER_LEN + QUESTION_MAX],
    query_len: usize,
    reply: [u8; DNS_MAX_RESPONSE],
    reply_len: usize,
}

impl Slot {
    const fn new() -> Self {
        Self {
            busy: false,
            server: [0; 4],
            port: 0,
            query: [0; DNS_HEADER_LEN + QUESTION_MAX],
            query_len: 0,
            reply: [0; DNS_MAX_RESPONSE],
            reply_len: 0,
        }
    }
}

static SLOTS: [SpinLock<Slot>; MAX_INFLIGHT] = {
    const SLOT: SpinLock<Slot> = SpinLock::new(
        Slot::new(),
        lock_class!("DNS_QUERY_SLOT", LOCK_LEVEL_RESOURCE),
    );
    [SLOT; MAX_INFLIGHT]
};
static ANSWERED: [AtomicBool; MAX_INFLIGHT] = [const { AtomicBool::new(false) }; MAX_INFLIGHT];
static WAITERS: [WaitQueue; MAX_INFLIGHT] = {
    const WQ: WaitQueue =
        WaitQueue::new(lock_class!("DNS_QUERY_SLOT.waiters", LOCK_LEVEL_RESOURCE));
    [WQ; MAX_INFLIGHT]
};

/// One lookup's claim on a slot and a source port, given back on drop.
pub struct Inflight {
    slot: usize,
    port: u16,
}

impl Inflight {
    pub fn claim() -> Result<Self, DnsResolveError> {
        const SOURCE_PORT_DRAWS: usize = 32;
        let slot = (0..MAX_INFLIGHT)
            .find(|&i| {
                let mut s = SLOTS[i].lock();
                !core::mem::replace(&mut s.busy, true)
            })
            .ok_or(DnsResolveError::Busy)?;
        // Also claimed from the ephemeral allocator, so no socket's automatic
        // bind can draw it.
        for _ in 0..SOURCE_PORT_DRAWS {
            let port = Port(random_source_port());
            if !crate::socket::EPHEMERAL_PORTS.lock().claim(port) {
                continue;
            }
            if crate::udp::udp_bind(RESOLVER_SOCKET, Ipv4Addr::UNSPECIFIED, port, false).is_ok() {
                SLOTS[slot].lock().port = port.0;
                return Ok(Self { slot, port: port.0 });
            }
            crate::socket::EPHEMERAL_PORTS.lock().release(port);
        }
        SLOTS[slot].lock().busy = false;
        Err(DnsResolveError::Busy)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Expect a reply to `query` from `server`; one to anything asked before
    /// no longer counts.
    pub fn arm(&self, server: [u8; 4], query: &[u8]) {
        let mut s = SLOTS[self.slot].lock();
        let n = query.len().min(s.query.len());
        s.query[..n].copy_from_slice(&query[..n]);
        s.query_len = n;
        s.server = server;
        s.reply_len = 0;
        ANSWERED[self.slot].store(false, Ordering::Release);
    }

    /// Park until the armed query is answered or `timeout_ms` passes, then
    /// copy the reply into `out`.
    pub fn wait(&self, timeout_ms: u32, out: &mut [u8]) -> Result<usize, WaitAbort> {
        let answered = &ANSWERED[self.slot];
        WAITERS[self.slot].wait_event_timeout_until(
            || answered.load(Ordering::Acquire).then_some(()),
            timeout_ms as u64,
        )?;
        let s = SLOTS[self.slot].lock();
        let n = s.reply_len.min(out.len());
        out[..n].copy_from_slice(&s.reply[..n]);
        Ok(n)
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        crate::udp::udp_unbind(RESOLVER_SOCKET, Ipv4Addr::UNSPECIFIED, Port(self.port));
        crate::socket::EPHEMERAL_PORTS
            .lock()
            .release(Port(self.port));
        let mut s = SLOTS[self.slot].lock();
        s.busy = false;
        s.port = 0;
        s.query_len = 0;
        s.reply_len = 0;
        ANSWERED[self.slot].store(false, Ordering::Release);
    }
}

/// Offer a datagram from port 53 to the lookups in flight. `true` if it was
/// addressed to a resolver port and so consumed, even when it fails the match.
pub fn deliver(src_ip: [u8; 4], dst_port: u16, payload: &[u8]) -> bool {
    for i in 0..MAX_INFLIGHT {
        let mut s = SLOTS[i].lock();
        if !s.busy || s.port != dst_port {
            continue;
        }
        let genuine = s.server == src_ip
            && s.reply_len == 0
            && payload.len() <= s.reply.len()
            && reply_matches(&s.query[..s.query_len], payload);
        if genuine {
            s.reply[..payload.len()].copy_from_slice(payload);
            s.reply_len = payload.len();
            ANSWERED[i].store(true, Ordering::Release);
            drop(s);
            let _ = WAITERS[i].wake_all();
        }
        return true;
    }
    false
}

/// Resolve a hostname to an IPv4 address using the kernel's DNS client.
///
/// The transport driver that pumps the I/O-free [`DnsResolver`]; IP literals
/// and cache hits short-circuit first.
pub fn dns_resolve(hostname: &[u8]) -> Result<[u8; 4], DnsResolveError> {
    if let Some(addr) = parse_ip_literal(hostname) {
        return Ok(addr);
    }
    if let Some(addr) = dns_cache_lookup(hostname) {
        return Ok(addr);
    }

    // The resolver config is the authority: a static override outranks DHCP,
    // and a second interface's lease can carry a server the first NIC never sees.
    let mut servers = [Ipv4Addr::UNSPECIFIED; NET_MAX_RESOLVERS];
    let count = RESOLVER.servers(&mut servers);
    let servers = &servers[..count];
    if servers.is_empty() || servers.iter().any(|s| s.is_unspecified()) {
        return Err(DnsResolveError::NoDnsServer);
    }

    let mut resolver = DnsResolver::new(
        hostname,
        servers.len(),
        RESOLVER.attempts(),
        RESOLVER.timeout_ms(),
    )?;
    let inflight = Inflight::claim()?;
    // Heap, not stack: this frame is already near the stack-safety gate.
    let mut reply =
        slopos_ostd::KVec::<u8>::zeroed(DNS_MAX_RESPONSE).map_err(|_| DnsResolveError::Busy)?;
    let mut outcome = DnsOutcome::Start;

    loop {
        match resolver.step(outcome) {
            DnsStep::Resolved { addr, ttl } => {
                // Only this arm is evidence of connectivity: the cache hit and
                // the IP-literal shortcut above send no packet.
                crate::connectivity::note_dns_success();
                dns_cache_insert(hostname, addr, ttl);
                return Ok(addr);
            }
            DnsStep::Failed(err) => return Err(err),
            DnsStep::Query { server, timeout_ms } => {
                let server = servers[server];
                inflight.arm(server.0, resolver.query_bytes());
                let Some(src) = crate::iface::source_ip_for(server) else {
                    outcome = DnsOutcome::TransmitFailed;
                    continue;
                };
                if crate::udp::udp_sendto(
                    src.0,
                    server.0,
                    inflight.port(),
                    DNS_PORT,
                    resolver.query_bytes(),
                )
                .is_err()
                {
                    klog_debug!("dns: transmit to {} failed", server);
                    outcome = DnsOutcome::TransmitFailed;
                    continue;
                }
                outcome = match inflight.wait(timeout_ms, reply.as_mut()) {
                    Ok(n) => DnsOutcome::Reply(&reply[..n]),
                    Err(WaitAbort::Timeout | WaitAbort::NoRuntime) => DnsOutcome::Timeout,
                    Err(WaitAbort::Killed | WaitAbort::Interrupted) => {
                        return Err(DnsResolveError::Interrupted);
                    }
                };
            }
        }
    }
}

/// Try to parse a dotted-decimal IPv4 literal (e.g., `"10.0.2.3"`).
fn parse_ip_literal(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0usize;
    let mut current: u16 = 0;
    let mut digit_count = 0usize;

    for &b in s {
        if b == b'.' {
            if digit_count == 0 || octet_idx >= 3 {
                return None;
            }
            if current > 255 {
                return None;
            }
            octets[octet_idx] = current as u8;
            octet_idx += 1;
            current = 0;
            digit_count = 0;
        } else if b >= b'0' && b <= b'9' {
            current = current * 10 + (b - b'0') as u16;
            digit_count += 1;
            if digit_count > 3 {
                return None;
            }
        } else {
            return None;
        }
    }

    if digit_count == 0 || octet_idx != 3 || current > 255 {
        return None;
    }
    octets[3] = current as u8;
    Some(octets)
}
