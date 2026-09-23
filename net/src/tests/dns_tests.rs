//! DNS client test suite.

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use super::net_scope::NetTestScope;
use crate::dns;
use crate::types::{Ipv4Addr, Port, SockAddr};

pub fn test_dns_t1_name_encoding() -> TestResult {
    let mut buf = [0u8; 128];

    let len = dns::dns_encode_name(b"example.com", &mut buf);
    assert_test!(len.is_some(), "encode example.com");
    let len = len.unwrap();
    // Expected: [7, 'e','x','a','m','p','l','e', 3, 'c','o','m', 0]
    assert_eq_test!(len, 13, "example.com wire length");
    assert_eq_test!(buf[0], 7, "first label length");
    assert_eq_test!(buf[8], 3, "second label length");
    assert_eq_test!(buf[12], 0, "root label");

    let len = dns::dns_encode_name(b"a.b", &mut buf).unwrap();
    assert_eq_test!(len, 5, "a.b wire length");
    assert_eq_test!(buf[0], 1, "label 'a' length");
    assert_eq_test!(buf[1], b'a', "label 'a' content");
    assert_eq_test!(buf[2], 1, "label 'b' length");
    assert_eq_test!(buf[3], b'b', "label 'b' content");
    assert_eq_test!(buf[4], 0, "root label");

    assert_test!(
        dns::dns_encode_name(b"example..com", &mut buf).is_none(),
        "reject empty label"
    );

    assert_test!(
        dns::dns_encode_name(b".example.com", &mut buf).is_none(),
        "reject leading dot"
    );

    let len = dns::dns_encode_name(b"example.com.", &mut buf).unwrap();
    assert_eq_test!(len, 13, "trailing dot same as without");

    let len = dns::dns_encode_name(b"", &mut buf).unwrap();
    assert_eq_test!(len, 1, "root label only");
    assert_eq_test!(buf[0], 0, "root is zero byte");

    let long_label = [b'a'; 64];
    assert_test!(
        dns::dns_encode_name(&long_label, &mut buf).is_none(),
        "reject label > 63 bytes"
    );

    pass!()
}

pub fn test_dns_t2_query_construction() -> TestResult {
    let mut buf = [0u8; 512];

    let len = dns::dns_build_query(0x1234, b"example.com", dns::DnsType::A, &mut buf);
    assert_test!(len.is_some(), "build query succeeds");
    let len = len.unwrap();

    assert_eq_test!(buf[0], 0x12, "query ID high byte");
    assert_eq_test!(buf[1], 0x34, "query ID low byte");
    assert_eq_test!(buf[2], 0x01, "flags high byte (RD)");
    assert_eq_test!(buf[3], 0x00, "flags low byte");
    assert_eq_test!(buf[4], 0x00, "qdcount high");
    assert_eq_test!(buf[5], 0x01, "qdcount low");
    assert_eq_test!(buf[6], 0x00, "ancount high");
    assert_eq_test!(buf[7], 0x00, "ancount low");

    // Question section starts after the 12-byte header.
    assert_eq_test!(buf[12], 7, "question name label 1 len");
    let name_end = 12 + 13; // "example.com" encodes to 13 bytes
    assert_eq_test!(
        u16::from_be_bytes([buf[name_end], buf[name_end + 1]]),
        1,
        "QTYPE = A"
    );
    assert_eq_test!(
        u16::from_be_bytes([buf[name_end + 2], buf[name_end + 3]]),
        1,
        "QCLASS = IN"
    );

    assert_eq_test!(len, name_end + 4, "total query length");

    let mut tiny = [0u8; 10];
    assert_test!(
        dns::dns_build_query(0x1234, b"example.com", dns::DnsType::A, &mut tiny).is_none(),
        "reject small buffer"
    );

    pass!()
}

pub fn test_dns_t3_name_decoding() -> TestResult {
    let mut out = [0u8; 256];

    let wire: &[u8] = &[
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
    ];
    let result = dns::dns_decode_name(wire, 0, &mut out);
    assert_test!(result.is_some(), "decode regular name");
    let (name_len, wire_consumed) = result.unwrap();
    assert_eq_test!(name_len, 11, "decoded name length (example.com)");
    assert_eq_test!(wire_consumed, 13, "wire bytes consumed");
    assert_eq_test!(&out[..name_len], b"example.com" as &[u8], "decoded text");

    // The name at offset 13 is "www" plus a pointer back to offset 0.
    let mut packet = [0u8; 64];
    packet[..13].copy_from_slice(wire);
    packet[13] = 3;
    packet[14] = b'w';
    packet[15] = b'w';
    packet[16] = b'w';
    packet[17] = 0xC0; // Compression pointer
    packet[18] = 0x00; // Points to offset 0

    let result = dns::dns_decode_name(&packet, 13, &mut out);
    assert_test!(result.is_some(), "decode with compression pointer");
    let (name_len, wire_consumed) = result.unwrap();
    assert_eq_test!(name_len, 15, "www.example.com length");
    assert_eq_test!(&out[..name_len], b"www.example.com" as &[u8], "decoded www");
    assert_eq_test!(wire_consumed, 6, "wire consumed with pointer");

    let mut loop_packet = [0u8; 4];
    loop_packet[0] = 0xC0;
    loop_packet[1] = 0x00; // Points to offset 0 = itself
    assert_test!(
        dns::dns_decode_name(&loop_packet, 0, &mut out).is_none(),
        "detect pointer loop"
    );

    pass!()
}

pub fn test_dns_t4_response_parsing() -> TestResult {
    let id: u16 = 0xABCD;
    let mut packet = [0u8; 128];
    let mut pos;

    packet[0..2].copy_from_slice(&id.to_be_bytes());
    packet[2..4].copy_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=0
    packet[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    packet[6..8].copy_from_slice(&1u16.to_be_bytes()); // ANCOUNT=1
    packet[8..10].copy_from_slice(&0u16.to_be_bytes()); // NSCOUNT=0
    packet[10..12].copy_from_slice(&0u16.to_be_bytes()); // ARCOUNT=0
    pos = 12;

    let name_wire: &[u8] = &[
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
    ];
    packet[pos..pos + 13].copy_from_slice(name_wire);
    pos += 13;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QTYPE=A
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QCLASS=IN
    pos += 2;

    packet[pos] = 0xC0;
    packet[pos + 1] = 0x0C; // Pointer to name at offset 12
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // TYPE=A
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // CLASS=IN
    pos += 2;
    packet[pos..pos + 4].copy_from_slice(&300u32.to_be_bytes()); // TTL=300
    pos += 4;
    packet[pos..pos + 2].copy_from_slice(&4u16.to_be_bytes()); // RDLENGTH=4
    pos += 2;
    packet[pos..pos + 4].copy_from_slice(&[93, 184, 216, 34]); // RDATA
    pos += 4;

    let resp = dns::dns_parse_response(&packet[..pos], id);
    assert_test!(resp.is_some(), "parse valid A response");
    let resp = resp.unwrap();
    assert_eq_test!(resp.addr, [93, 184, 216, 34], "resolved address");
    assert_eq_test!(resp.ttl, 300, "TTL");

    assert_test!(
        dns::dns_parse_response(&packet[..pos], 0x0000).is_none(),
        "reject ID mismatch"
    );

    let mut err_packet = packet;
    let flags = u16::from_be_bytes([err_packet[2], err_packet[3]]);
    let err_flags = (flags & 0xFFF0) | 3; // RCODE=3
    err_packet[2..4].copy_from_slice(&err_flags.to_be_bytes());
    assert_test!(
        dns::dns_parse_response(&err_packet[..pos], id).is_none(),
        "reject RCODE error"
    );

    pass!()
}

pub fn test_dns_t5_cache() -> TestResult {
    dns::dns_cache_flush();

    dns::dns_cache_insert(b"test.local", [1, 2, 3, 4], 3600);
    let result = dns::dns_cache_lookup(b"test.local");
    assert_test!(result.is_some(), "cache hit after insert");
    assert_eq_test!(result.unwrap(), [1, 2, 3, 4], "cached address");

    let miss = dns::dns_cache_lookup(b"unknown.local");
    assert_test!(miss.is_none(), "cache miss for unknown");

    dns::dns_cache_insert(b"test.local", [5, 6, 7, 8], 3600);
    let result = dns::dns_cache_lookup(b"test.local");
    assert_eq_test!(result.unwrap(), [5, 6, 7, 8], "updated address");

    // Fill the cache to capacity to force LRU eviction.
    for i in 0u8..16 {
        let mut name = [b'h', b'o', b's', b't', b'-', b'0', b'0', 0];
        name[5] = b'a' + (i / 10);
        name[6] = b'0' + (i % 10);
        dns::dns_cache_insert(&name[..7], [10, 0, 0, i], 3600);
    }

    dns::dns_cache_insert(b"overflow", [99, 99, 99, 99], 3600);
    let result = dns::dns_cache_lookup(b"overflow");
    assert_test!(result.is_some(), "overflow entry exists");
    assert_eq_test!(result.unwrap(), [99, 99, 99, 99], "overflow address");

    dns::dns_cache_flush();
    assert_test!(
        dns::dns_cache_lookup(b"test.local").is_none(),
        "flushed cache is empty"
    );
    assert_test!(
        dns::dns_cache_lookup(b"overflow").is_none(),
        "flushed overflow entry gone"
    );

    pass!()
}

/// Build a minimal valid DNS A-record response for transaction `id`: header,
/// question (`example.com A IN`), one answer RR.
#[inline(never)]
fn build_a_reply(id: u16, addr: [u8; 4], ttl: u32) -> ([u8; 128], usize) {
    let mut packet = [0u8; 128];
    packet[0..2].copy_from_slice(&id.to_be_bytes());
    packet[2..4].copy_from_slice(&0x8180u16.to_be_bytes()); // QR=1, RD=1, RA=1, RCODE=0
    packet[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    packet[6..8].copy_from_slice(&1u16.to_be_bytes()); // ANCOUNT=1
    let mut pos = 12;

    let name_wire: &[u8] = &[
        7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
    ];
    packet[pos..pos + 13].copy_from_slice(name_wire);
    pos += 13;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QTYPE=A
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // QCLASS=IN
    pos += 2;

    packet[pos] = 0xC0; // compression pointer to the question name at offset 12
    packet[pos + 1] = 0x0C;
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // TYPE=A
    pos += 2;
    packet[pos..pos + 2].copy_from_slice(&1u16.to_be_bytes()); // CLASS=IN
    pos += 2;
    packet[pos..pos + 4].copy_from_slice(&ttl.to_be_bytes()); // TTL
    pos += 4;
    packet[pos..pos + 2].copy_from_slice(&4u16.to_be_bytes()); // RDLENGTH=4
    pos += 2;
    packet[pos..pos + 4].copy_from_slice(&addr);
    pos += 4;

    (packet, pos)
}

pub fn test_dns_t6_resolver_rotates_then_succeeds() -> TestResult {
    use dns::{DnsOutcome, DnsResolver, DnsStep};

    let mut r = DnsResolver::new(b"example.com", 2, 2, 5000).expect("resolver builds");
    assert_eq_test!(
        r.step(DnsOutcome::Start),
        DnsStep::Query {
            server: 0,
            timeout_ms: 5000
        },
        "the first query goes to the first server"
    );
    let id1 = r.query_id();
    assert_eq_test!(
        r.step(DnsOutcome::Timeout),
        DnsStep::Query {
            server: 1,
            timeout_ms: 5000
        },
        "a timeout moves on to the next server"
    );
    let id2 = r.query_id();
    assert_test!(id2 != id1, "a retry draws a fresh transaction ID");

    let (reply, len) = build_a_reply(id2, [93, 184, 216, 34], 300);
    assert_eq_test!(
        r.step(DnsOutcome::Reply(&reply[..len])),
        DnsStep::Resolved {
            addr: [93, 184, 216, 34],
            ttl: 300
        },
        "valid reply resolves to the A record"
    );
    pass!()
}

pub fn test_dns_t7_resolver_exhaustion() -> TestResult {
    use dns::{DnsOutcome, DnsResolveError, DnsResolver, DnsStep};

    let mut r = DnsResolver::new(b"example.com", 2, 2, 100).expect("resolver builds");
    let mut servers = [0usize; 4];
    for (i, server) in servers.iter_mut().enumerate() {
        let outcome = if i == 0 {
            DnsOutcome::Start
        } else {
            DnsOutcome::Timeout
        };
        match r.step(outcome) {
            DnsStep::Query { server: s, .. } => *server = s,
            other => return fail!("round {} gave {:?}", i, other),
        }
    }
    assert_eq_test!(servers, [0, 1, 0, 1], "two rounds of two servers");
    assert_eq_test!(
        r.step(DnsOutcome::Timeout),
        DnsStep::Failed(DnsResolveError::Timeout),
        "attempts times servers queries, then the last transient error"
    );

    let mut r = DnsResolver::new(b"example.com", 1, 3, 100).expect("resolver builds");
    assert!(matches!(r.step(DnsOutcome::Start), DnsStep::Query { .. }));
    assert!(matches!(
        r.step(DnsOutcome::TransmitFailed),
        DnsStep::Query { .. }
    ));
    assert!(matches!(r.step(DnsOutcome::Timeout), DnsStep::Query { .. }));
    let garbage = [0u8; 4];
    assert_eq_test!(
        r.step(DnsOutcome::Reply(&garbage)),
        DnsStep::Failed(DnsResolveError::ParseFailed),
        "last failure (parse) wins over earlier transient errors"
    );

    assert_eq_test!(
        DnsResolver::new(b"example..com", 1, 1, 100).err(),
        Some(DnsResolveError::InvalidHostname),
        "double-dot hostname rejected as InvalidHostname"
    );
    pass!()
}

#[inline(never)]
fn set_rcode(packet: &mut [u8], rcode: dns::DnsRcode) {
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    packet[2..4].copy_from_slice(&((flags & 0xFFF0) | rcode as u16).to_be_bytes());
}

#[inline(never)]
fn answer(r: &mut dns::DnsResolver, rcode: dns::DnsRcode) -> dns::DnsStep {
    let (mut reply, len) = build_a_reply(r.query_id(), [1, 2, 3, 4], 60);
    set_rcode(&mut reply, rcode);
    r.step(dns::DnsOutcome::Reply(&reply[..len]))
}

#[inline(never)]
fn answer_with_no_records(r: &mut dns::DnsResolver, rcode: dns::DnsRcode) -> dns::DnsStep {
    let (mut reply, _) = build_a_reply(r.query_id(), [1, 2, 3, 4], 60);
    set_rcode(&mut reply, rcode);
    reply[6..8].copy_from_slice(&0u16.to_be_bytes());
    r.step(dns::DnsOutcome::Reply(&reply[..12 + 13 + 4]))
}

/// NXDOMAIN is the answer, not a failure to get one; SERVFAIL and REFUSED are
/// one server's failure, and the next server may do better.
pub fn test_dns_t11_rcode_policy() -> TestResult {
    use dns::{DnsOutcome, DnsRcode, DnsResolveError, DnsResolver, DnsStep};

    let mut r = DnsResolver::new(b"example.com", 2, 2, 100).expect("resolver builds");
    let _ = r.step(DnsOutcome::Start);
    assert_eq_test!(
        answer(&mut r, DnsRcode::NXDomain),
        DnsStep::Failed(DnsResolveError::NameNotFound),
        "NXDOMAIN ends the lookup at once"
    );

    let mut r = DnsResolver::new(b"example.com", 2, 1, 100).expect("resolver builds");
    let _ = r.step(DnsOutcome::Start);
    assert_eq_test!(
        answer(&mut r, DnsRcode::ServFail),
        DnsStep::Query {
            server: 1,
            timeout_ms: 100
        },
        "SERVFAIL asks the next server"
    );
    assert_eq_test!(
        answer(&mut r, DnsRcode::Refused),
        DnsStep::Failed(DnsResolveError::ServerFailure),
        "every server refusing is a server failure"
    );

    let mut r = DnsResolver::new(b"example.com", 1, 2, 100).expect("resolver builds");
    let _ = r.step(DnsOutcome::Start);
    assert_eq_test!(
        answer_with_no_records(&mut r, DnsRcode::NoError),
        DnsStep::Failed(DnsResolveError::NameNotFound),
        "NOERROR with no A record is a name without an address"
    );

    let mut r = DnsResolver::new(b"example.com", 1, 1, 100).expect("resolver builds");
    let _ = r.step(DnsOutcome::Start);
    let (mut reply, _) = build_a_reply(r.query_id(), [1, 2, 3, 4], 60);
    reply[2] |= 0x02;
    reply[6..8].copy_from_slice(&0u16.to_be_bytes());
    assert_eq_test!(
        r.step(DnsOutcome::Reply(&reply[..12 + 13 + 4])),
        DnsStep::Failed(DnsResolveError::ServerFailure),
        "a truncated reply with no A record says nothing about the name"
    );
    pass!()
}

pub fn test_dns_t8_regression_network_stack() -> TestResult {
    // The DNS interception in dispatch_rx_frame must not swallow ordinary UDP.
    use crate::socket::*;
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};

    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0, SocketOwner::UNOWNED);
    assert_test!(sock >= 0, "create UDP socket");
    let sock = sock as u32;

    let rc = socket_bind(sock, [0, 0, 0, 0], 41053);
    assert_eq_test!(rc, 0, "bind to port 41053");

    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    socket_deliver_udp_from_dispatch([10, 0, 2, 1], [10, 0, 2, 15], 9999, 41053, &payload);

    let mut buf = [0u8; 16];
    let mut peer = SockAddr::new(Ipv4Addr::UNSPECIFIED, Port(0));
    let got = socket_recvfrom(sock, &mut buf, Some(&mut peer));
    assert_eq_test!(got, 4, "received 4 bytes");
    assert_eq_test!(&buf[..4], &payload, "payload matches");
    assert_eq_test!(peer.ip.0, [10, 0, 2, 1], "source IP");
    assert_eq_test!(peer.port.0, 9999, "source port");

    let _ = socket_close(sock);
    pass!()
}

/// RFC 5452 §9: the ID must not be a boot constant, and the source port must
/// carry entropy independent of it.
pub fn test_dns_t9_query_entropy() -> TestResult {
    let mut ids = [0u16; 8];
    let mut ports = [0u16; 8];
    for (id, port) in ids.iter_mut().zip(ports.iter_mut()) {
        *id = dns::DnsResolver::new(b"example.com", 1, 1, 100)
            .expect("resolver")
            .query_id();
        *port = dns::Inflight::claim().expect("slot").port();
    }
    assert_test!(
        ids.iter().any(|&id| id != ids[0]),
        "transaction IDs must not be a fixed constant"
    );
    assert_test!(
        ports.iter().any(|&p| p != ports[0]),
        "source ports must not be a fixed constant"
    );
    assert_test!(
        ports.iter().all(|&p| p >= 49_152),
        "source ports come from the ephemeral range"
    );
    pass!()
}

#[inline(never)]
fn query_for(id: u16, name: &[u8]) -> ([u8; 128], usize) {
    let mut q = [0u8; 128];
    let n = dns::dns_build_query(id, name, dns::DnsType::A, &mut q).expect("query");
    (q, n)
}

#[inline(never)]
fn udp_rx(src_ip: [u8; 4], src_port: u16, dst_port: u16, payload: &[u8]) {
    let mut pkt = crate::packetbuf::PacketBuf::alloc().expect("packet");
    let mut hdr = [0u8; 8];
    hdr[0..2].copy_from_slice(&src_port.to_be_bytes());
    hdr[2..4].copy_from_slice(&dst_port.to_be_bytes());
    hdr[4..6].copy_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    pkt.append(&hdr).expect("header");
    pkt.append(payload).expect("payload");
    crate::udp::handle_rx(src_ip, [10, 0, 2, 15], &pkt);
}

#[inline(never)]
fn answered(inflight: &dns::Inflight) -> Option<usize> {
    let mut out = [0u8; 128];
    inflight.wait(0, &mut out).ok()
}

/// RFC 5452 §9.1: a reply counts only if it comes from the server asked, to
/// the port the query left from, carrying the query's ID and its question.
pub fn test_dns_t10_response_provenance() -> TestResult {
    const SERVER_A: [u8; 4] = [192, 0, 2, 53];
    const SERVER_B: [u8; 4] = [198, 51, 100, 53];

    let inflight = dns::Inflight::claim().expect("slot");
    let port = inflight.port();
    let (query, qlen) = query_for(0x1234, b"example.com");
    inflight.arm(SERVER_A, &query[..qlen]);

    let (reply, rlen) = build_a_reply(0x1234, [93, 184, 216, 34], 300);
    let reply = &reply[..rlen];

    udp_rx(SERVER_B, dns::DNS_PORT, port, reply);
    assert_eq_test!(answered(&inflight), None, "another host's reply is dropped");

    let (wrong_id, _) = build_a_reply(0x1235, [6, 6, 6, 6], 300);
    udp_rx(SERVER_A, dns::DNS_PORT, port, &wrong_id[..rlen]);
    assert_eq_test!(
        answered(&inflight),
        None,
        "a reply to another ID is dropped"
    );

    let mut other_question = [0u8; 128];
    other_question[..rlen].copy_from_slice(reply);
    other_question[13] = b'X';
    other_question[14] = b'Y';
    udp_rx(SERVER_A, dns::DNS_PORT, port, &other_question[..rlen]);
    assert_eq_test!(
        answered(&inflight),
        None,
        "a reply to another question is dropped"
    );

    let mut mixed_case = [0u8; 128];
    mixed_case[..rlen].copy_from_slice(reply);
    mixed_case[13] = b'E';
    udp_rx(SERVER_A, dns::DNS_PORT, port, &mixed_case[..rlen]);
    assert_eq_test!(
        answered(&inflight),
        Some(rlen),
        "the question echoed in another case is still the question"
    );

    udp_rx(SERVER_A, dns::DNS_PORT, port, reply);
    let mut out = [0u8; 128];
    let n = inflight.wait(0, &mut out).unwrap_or(0);
    assert_eq_test!(
        &out[..n],
        &mixed_case[..rlen],
        "the first genuine reply is kept; a second does not overwrite it"
    );

    assert_test!(
        !dns::deliver(SERVER_A, dns::DNS_PORT, reply),
        "a port the resolver does not hold is not the resolver's"
    );
    pass!()
}

/// These two names share an FNV-1a-32 hash, which is what the cache once keyed on.
pub fn test_dns_t12_cache_keys_whole_names() -> TestResult {
    dns::dns_cache_flush();
    dns::dns_cache_insert(b"glbvs.example", [203, 0, 113, 66], 3600);
    assert_eq_test!(
        dns::dns_cache_lookup(b"yacxa.example"),
        None,
        "a colliding name does not read another name's entry"
    );
    assert_eq_test!(
        dns::dns_cache_lookup(b"GLBVS.Example."),
        Some([203, 0, 113, 66]),
        "case and a trailing dot name the same host"
    );
    dns::dns_cache_flush();
    pass!()
}

/// Each lookup has its own slot, port and wait queue; a full table refuses, and
/// a port the resolver holds is closed to every socket's bind.
pub fn test_dns_t13_inflight_slots() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return fail!("net scope: {:?}", e),
    };
    const SERVER: [u8; 4] = [192, 0, 2, 53];

    let mut held: [Option<dns::Inflight>; dns::MAX_INFLIGHT] = [const { None }; dns::MAX_INFLIGHT];
    for (i, slot) in held.iter_mut().enumerate() {
        let inflight = match dns::Inflight::claim() {
            Ok(f) => f,
            Err(e) => return fail!("slot {} refused: {:?}", i, e),
        };
        let (query, qlen) = query_for(0x1000 + i as u16, b"example.com");
        inflight.arm(SERVER, &query[..qlen]);
        *slot = Some(inflight);
    }
    assert_eq_test!(
        dns::Inflight::claim().err(),
        Some(dns::DnsResolveError::Busy),
        "a full table refuses"
    );

    let (reply, rlen) = build_a_reply(0x1000 + 5, [10, 9, 8, 7], 60);
    let port5 = held[5].as_ref().map_or(0, |f| f.port());
    udp_rx(SERVER, dns::DNS_PORT, port5, &reply[..rlen]);
    for (i, slot) in held.iter().enumerate() {
        let got = slot.as_ref().and_then(answered);
        if (i == 5) != got.is_some() {
            return fail!("slot {} answered={:?} after a reply to slot 5", i, got);
        }
    }

    assert_test!(
        port_closed_to_every_bind(port5),
        "a socket cannot bind a port a query is waiting on"
    );
    held[5] = None;
    assert_test!(
        dns::Inflight::claim().is_ok(),
        "a released slot is claimable"
    );
    assert_test!(
        udp_port_bindable(port5, [0, 0, 0, 0], false),
        "and its port is free again"
    );
    pass!()
}

#[inline(never)]
fn port_closed_to_every_bind(port: u16) -> bool {
    [
        ([0, 0, 0, 0], false),
        ([0, 0, 0, 0], true),
        ([127, 0, 0, 1], true),
    ]
    .into_iter()
    .all(|(addr, reuse)| !udp_port_bindable(port, addr, reuse))
}

#[inline(never)]
fn udp_port_bindable(port: u16, addr: [u8; 4], reuse: bool) -> bool {
    use crate::socket::*;
    use slopos_abi::net::{AF_INET, SOCK_DGRAM};
    use slopos_abi::syscall::{SO_REUSEADDR, SOL_SOCKET};

    let sock = socket_create(AF_INET, SOCK_DGRAM, 0, SocketOwner::UNOWNED);
    if sock < 0 {
        return false;
    }
    if reuse {
        let _ = socket_setsockopt(sock as u32, SOL_SOCKET, SO_REUSEADDR, &1i32.to_ne_bytes());
    }
    let bound = socket_bind(sock as u32, addr, port) == 0;
    let _ = socket_close(sock as u32);
    bound
}

slopos_testing::stest!(name = test_dns_t1_name_encoding, suite = dns);
slopos_testing::stest!(name = test_dns_t2_query_construction, suite = dns);
slopos_testing::stest!(name = test_dns_t3_name_decoding, suite = dns);
slopos_testing::stest!(name = test_dns_t4_response_parsing, suite = dns);
slopos_testing::stest!(name = test_dns_t5_cache, suite = dns);
slopos_testing::stest!(
    name = test_dns_t6_resolver_rotates_then_succeeds,
    suite = dns
);
slopos_testing::stest!(name = test_dns_t7_resolver_exhaustion, suite = dns);
slopos_testing::stest!(name = test_dns_t8_regression_network_stack, suite = dns);
slopos_testing::stest!(name = test_dns_t9_query_entropy, suite = dns);
slopos_testing::stest!(name = test_dns_t10_response_provenance, suite = dns);
slopos_testing::stest!(name = test_dns_t11_rcode_policy, suite = dns);
slopos_testing::stest!(name = test_dns_t12_cache_keys_whole_names, suite = dns);
slopos_testing::stest!(name = test_dns_t13_inflight_slots, suite = dns);
