//! Phase 5A/5B integration tests — Two-Queue Listen Model & TCP Demux Table.
//!
//! Tests the SYN queue, accept queue, SYN-ACK retransmission, overflow
//! behavior of [`TcpListenState`], and the [`TcpDemuxTable`] for fast
//! connection/listener lookup.

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use super::tcp_socket::{
    SYN_QUEUE_MAX, SYN_RETRIES_MAX, TcpDemuxTable, TcpListenState, reset_syn_entry_keys,
};
use super::types::{Ipv4Addr, Port, SockAddr};
/// Helper: create a local listening address.
fn local_addr() -> SockAddr {
    SockAddr {
        ip: Ipv4Addr([10, 0, 0, 1]),
        port: Port(8080),
    }
}

/// Helper: create a unique remote client address.
fn client_addr(n: u16) -> SockAddr {
    SockAddr {
        ip: Ipv4Addr([192, 168, 1, (n & 0xff) as u8]),
        port: Port(40000 + n),
    }
}

// =============================================================================
// T1: SYN queue overflow — fill to SYN_QUEUE_MAX, verify next SYN returns None
// =============================================================================

pub fn test_syn_queue_overflow() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(64, local_addr());

    // Fill the SYN queue to capacity.
    for i in 0..SYN_QUEUE_MAX as u16 {
        let client = client_addr(i);
        let result = listen.on_syn(client, 1000 + i as u32, 1460, 0);
        assert_test!(
            result.is_some(),
            "SYN {} should succeed (queue not full yet)"
        );
    }

    assert_eq_test!(
        listen.syn_queue_len(),
        SYN_QUEUE_MAX,
        "SYN queue at capacity"
    );

    // Next SYN should be silently dropped (no RST).
    let overflow_client = client_addr(SYN_QUEUE_MAX as u16);
    let overflow_result = listen.on_syn(overflow_client, 9999, 1460, 0);
    assert_test!(
        overflow_result.is_none(),
        "SYN queue full -> silently dropped (no RST)"
    );

    // Queue length unchanged.
    assert_eq_test!(
        listen.syn_queue_len(),
        SYN_QUEUE_MAX,
        "SYN queue still at capacity after overflow"
    );

    pass!()
}

// =============================================================================
// T2: Accept queue overflow — backlog=2, complete 3 connections, 3rd stays in
//     SYN queue
// =============================================================================

pub fn test_accept_queue_overflow() -> TestResult {
    reset_syn_entry_keys();
    let backlog = 2usize;
    let mut listen = TcpListenState::new(backlog, local_addr());

    // Send 3 SYNs.
    for i in 0..3u16 {
        let client = client_addr(i);
        let syn_ack = listen.on_syn(client, 1000 + i as u32, 1460, 0);
        assert_test!(syn_ack.is_some(), "SYN should succeed");
    }
    assert_eq_test!(listen.syn_queue_len(), 3, "3 entries in SYN queue");

    // Complete first two with ACK (should move to accept queue).
    for i in 0..2u16 {
        let client = client_addr(i);
        // The SYN-ACK's ack_num will be iss+1; we need to know the ISS.
        // Each on_syn generates a unique ISS. We need to get the ISS from the
        // returned SYN-ACK. But we already consumed those. Re-approach: send a
        // duplicate SYN to get the SYN-ACK back (which retransmits existing).
        let retransmit = listen.on_syn(client, 1000 + i as u32, 1460, 0);
        let syn_ack = retransmit.expect("duplicate SYN retransmits SYN-ACK");
        let iss = syn_ack.seq_num;
        let ack_num = iss.wrapping_add(1);

        let accepted = listen.on_ack(client, ack_num);
        assert_test!(
            accepted.is_some(),
            "connection should complete (accept queue has room)"
        );
    }

    assert_eq_test!(listen.accept_queue_len(), 2, "accept queue has 2");
    assert_eq_test!(listen.syn_queue_len(), 1, "1 remaining in SYN queue");

    // Complete third — accept queue is full, should stay in SYN queue.
    let client2 = client_addr(2);
    let retransmit2 = listen.on_syn(client2, 1002, 1460, 0);
    let syn_ack2 = retransmit2.expect("duplicate SYN retransmits SYN-ACK");
    let iss2 = syn_ack2.seq_num;
    let ack_num2 = iss2.wrapping_add(1);

    let overflow = listen.on_ack(client2, ack_num2);
    assert_test!(
        overflow.is_none(),
        "accept queue full -> connection stays in SYN queue"
    );
    assert_eq_test!(listen.syn_queue_len(), 1, "still 1 in SYN queue");
    assert_eq_test!(listen.accept_queue_len(), 2, "accept queue still full");

    // Drain one from accept queue, then retry ACK — should succeed now.
    let _ = listen.accept();
    assert_eq_test!(listen.accept_queue_len(), 1, "accept queue drained to 1");

    let accepted_now = listen.on_ack(client2, ack_num2);
    assert_test!(
        accepted_now.is_some(),
        "after draining accept queue, 3rd connection completes"
    );
    assert_eq_test!(listen.accept_queue_len(), 2, "accept queue back to 2");
    assert_eq_test!(listen.syn_queue_len(), 0, "SYN queue drained");

    pass!()
}

// =============================================================================
// T3: SYN-ACK retransmission — verify 5 retransmissions with backoff, then
//     removal
// =============================================================================

pub fn test_syn_ack_retransmit_exhaustion() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(16, local_addr());

    let client = client_addr(0);
    let syn_ack = listen.on_syn(client, 5000, 1460, 0);
    assert_test!(syn_ack.is_some(), "initial SYN accepted");
    assert_eq_test!(listen.syn_queue_len(), 1, "1 entry in SYN queue");

    // The SYN-ACK returned by on_syn tells us the ISS.
    let syn_ack = syn_ack.unwrap();
    let original_iss = syn_ack.seq_num;

    // Determine the entry's key — it's the first key allocated (we reset keys).
    // Key=1 because we reset_syn_entry_keys() above.
    let entry_key = 1u32;

    // Simulate 5 retransmit timer firings — each should return a SYN-ACK.
    for _retry in 1..=SYN_RETRIES_MAX {
        let retransmit = listen.on_retransmit(entry_key);
        assert_test!(retransmit.is_some(), "retransmit should succeed on retry");
        let seg = retransmit.unwrap();
        assert_eq_test!(
            seg.seq_num,
            original_iss,
            "ISS unchanged across retransmits"
        );
        assert_eq_test!(listen.syn_queue_len(), 1, "entry still in SYN queue");
    }

    // 6th retransmit (retry > SYN_RETRIES_MAX) — entry should be removed.
    let exhausted = listen.on_retransmit(entry_key);
    assert_test!(
        exhausted.is_none(),
        "retransmit returns None after max retries"
    );
    assert_eq_test!(
        listen.syn_queue_len(),
        0,
        "entry removed from SYN queue after exhaustion"
    );

    pass!()
}

// =============================================================================
// Additional coverage: duplicate SYN retransmits existing SYN-ACK
// =============================================================================

pub fn test_duplicate_syn_retransmits() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(16, local_addr());

    let client = client_addr(42);
    let first = listen.on_syn(client, 7000, 1460, 0);
    assert_test!(first.is_some(), "first SYN accepted");
    let first_iss = first.unwrap().seq_num;

    // Send duplicate SYN — should retransmit the same SYN-ACK.
    let dup = listen.on_syn(client, 7000, 1460, 100);
    assert_test!(dup.is_some(), "duplicate SYN triggers SYN-ACK retransmit");
    let dup_iss = dup.unwrap().seq_num;

    assert_eq_test!(
        first_iss,
        dup_iss,
        "duplicate SYN returns same ISS (same entry)"
    );
    assert_eq_test!(listen.syn_queue_len(), 1, "no duplicate entry created");

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — established connection lookup
// =============================================================================

pub fn test_demux_register_established_lookup() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(80);
    let remote_ip = Ipv4Addr([192, 168, 1, 100]);
    let remote_port = Port(40000);
    let conn_id = 7u32;

    let result = demux.register_established(local_ip, local_port, remote_ip, remote_port, conn_id);
    assert_test!(result.is_ok(), "register_established should succeed");

    // Exact 4-tuple match should find the connection.
    let found = demux.lookup_established(local_ip, local_port, remote_ip, remote_port);
    assert_eq_test!(
        found,
        Some(conn_id),
        "lookup should return the registered conn_id"
    );

    // Different remote port should NOT match.
    let not_found = demux.lookup_established(local_ip, local_port, remote_ip, Port(40001));
    assert_test!(
        not_found.is_none(),
        "different remote port should not match"
    );

    // Different remote IP should NOT match.
    let not_found2 = demux.lookup_established(
        local_ip,
        local_port,
        Ipv4Addr([192, 168, 1, 101]),
        remote_port,
    );
    assert_test!(not_found2.is_none(), "different remote IP should not match");

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — duplicate established registration rejected
// =============================================================================

pub fn test_demux_established_duplicate_rejected() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(80);
    let remote_ip = Ipv4Addr([192, 168, 1, 100]);
    let remote_port = Port(40000);

    let r1 = demux.register_established(local_ip, local_port, remote_ip, remote_port, 1);
    assert_test!(r1.is_ok(), "first registration should succeed");

    let r2 = demux.register_established(local_ip, local_port, remote_ip, remote_port, 2);
    assert_test!(r2.is_err(), "duplicate 4-tuple registration should fail");

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — unregister established
// =============================================================================

pub fn test_demux_unregister_established() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(80);
    let remote_ip = Ipv4Addr([192, 168, 1, 100]);
    let remote_port = Port(40000);
    let conn_id = 3u32;

    let _ = demux.register_established(local_ip, local_port, remote_ip, remote_port, conn_id);

    // Should find it before unregister.
    assert_test!(
        demux
            .lookup_established(local_ip, local_port, remote_ip, remote_port)
            .is_some(),
        "should find before unregister"
    );

    demux.unregister_established(conn_id);

    // Should NOT find it after unregister.
    assert_test!(
        demux
            .lookup_established(local_ip, local_port, remote_ip, remote_port)
            .is_none(),
        "should not find after unregister"
    );

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — listener registration and lookup
// =============================================================================

pub fn test_demux_register_listener_lookup() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(8080);
    let sock_idx = 5u32;

    let result = demux.register_listener(local_ip, local_port, sock_idx);
    assert_test!(result.is_ok(), "register_listener should succeed");

    // Exact 2-tuple match.
    let found = demux.lookup_listener(local_ip, local_port);
    assert_eq_test!(
        found,
        Some(sock_idx),
        "lookup should return registered sock_idx"
    );

    // Different port should NOT match.
    let not_found = demux.lookup_listener(local_ip, Port(9090));
    assert_test!(not_found.is_none(), "different port should not match");

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — listener wildcard (0.0.0.0) fallback
// =============================================================================

pub fn test_demux_listener_wildcard_fallback() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let wildcard = Ipv4Addr::UNSPECIFIED;
    let local_port = Port(80);
    let sock_idx = 10u32;

    let result = demux.register_listener(wildcard, local_port, sock_idx);
    assert_test!(
        result.is_ok(),
        "wildcard listener registration should succeed"
    );

    // Lookup with a specific IP should fall back to wildcard.
    let found = demux.lookup_listener(Ipv4Addr([10, 0, 0, 1]), local_port);
    assert_eq_test!(found, Some(sock_idx), "wildcard fallback should match");

    // Exact match takes priority over wildcard.
    let specific_sock = 20u32;
    let specific_ip = Ipv4Addr([10, 0, 0, 1]);
    let r2 = demux.register_listener(specific_ip, local_port, specific_sock);
    assert_test!(r2.is_ok(), "specific listener registration should succeed");

    let found2 = demux.lookup_listener(specific_ip, local_port);
    assert_eq_test!(
        found2,
        Some(specific_sock),
        "exact IP should take priority over wildcard"
    );

    // Other IPs should still fall back to wildcard.
    let found3 = demux.lookup_listener(Ipv4Addr([10, 0, 0, 2]), local_port);
    assert_eq_test!(
        found3,
        Some(sock_idx),
        "non-matching IP should fall back to wildcard"
    );

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — unregister listener
// =============================================================================

pub fn test_demux_unregister_listener() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let local_ip = Ipv4Addr([10, 0, 0, 1]);
    let local_port = Port(8080);
    let sock_idx = 5u32;

    let _ = demux.register_listener(local_ip, local_port, sock_idx);

    assert_test!(
        demux.lookup_listener(local_ip, local_port).is_some(),
        "should find before unregister"
    );

    demux.unregister_listener(sock_idx);

    assert_test!(
        demux.lookup_listener(local_ip, local_port).is_none(),
        "should not find after unregister"
    );

    pass!()
}

// =============================================================================
// Phase 5B: TcpDemuxTable — clear wipes all entries
// =============================================================================

pub fn test_demux_clear() -> TestResult {
    let mut demux = TcpDemuxTable::new();

    let _ = demux.register_established(
        Ipv4Addr([10, 0, 0, 1]),
        Port(80),
        Ipv4Addr([192, 168, 1, 1]),
        Port(40000),
        1,
    );
    let _ = demux.register_listener(Ipv4Addr([10, 0, 0, 1]), Port(8080), 5);

    demux.clear();

    assert_test!(
        demux
            .lookup_established(
                Ipv4Addr([10, 0, 0, 1]),
                Port(80),
                Ipv4Addr([192, 168, 1, 1]),
                Port(40000),
            )
            .is_none(),
        "established should be cleared"
    );
    assert_test!(
        demux
            .lookup_listener(Ipv4Addr([10, 0, 0, 1]), Port(8080))
            .is_none(),
        "listener should be cleared"
    );

    pass!()
}

// =============================================================================
// Phase 5C: TcpListenState — push_accepted enqueues completed connections
// =============================================================================

pub fn test_push_accepted_basic() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(4, local_addr());

    // Push an accepted connection directly.
    let accepted = super::tcp_socket::AcceptedConn {
        tuple: super::tcp::TcpTuple {
            local_ip: [10, 0, 0, 1],
            local_port: 8080,
            remote_ip: [192, 168, 1, 10],
            remote_port: 50000,
        },
        iss: 1000,
        irs: 2000,
        peer_mss: 1460,
    };

    let ok = listen.push_accepted(accepted);
    assert_test!(ok, "push_accepted should succeed when queue has room");
    assert_eq_test!(
        listen.accept_queue_len(),
        1,
        "accept queue should have 1 entry"
    );

    // Accept should dequeue it.
    let dequeued = listen.accept();
    assert_test!(
        dequeued.is_some(),
        "accept should return the pushed connection"
    );
    let conn = dequeued.unwrap();
    assert_eq_test!(conn.tuple.remote_port, 50000, "remote port should match");
    assert_eq_test!(conn.iss, 1000, "ISS should match");
    assert_eq_test!(conn.irs, 2000, "IRS should match");
    assert_eq_test!(conn.peer_mss, 1460, "peer MSS should match");

    assert_eq_test!(
        listen.accept_queue_len(),
        0,
        "accept queue should be empty after dequeue"
    );

    pass!()
}

// =============================================================================
// Phase 5C: TcpListenState — push_accepted respects backlog
// =============================================================================

pub fn test_push_accepted_respects_backlog() -> TestResult {
    reset_syn_entry_keys();
    let backlog = 2usize;
    let mut listen = TcpListenState::new(backlog, local_addr());

    // Fill to backlog.
    for i in 0..backlog as u16 {
        let accepted = super::tcp_socket::AcceptedConn {
            tuple: super::tcp::TcpTuple {
                local_ip: [10, 0, 0, 1],
                local_port: 8080,
                remote_ip: [192, 168, 1, i as u8],
                remote_port: 40000 + i,
            },
            iss: 1000 + i as u32,
            irs: 2000 + i as u32,
            peer_mss: 1460,
        };
        let ok = listen.push_accepted(accepted);
        assert_test!(ok, "push_accepted should succeed within backlog");
    }
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog,
        "accept queue at backlog"
    );

    // Next push should fail (queue full).
    let overflow = super::tcp_socket::AcceptedConn {
        tuple: super::tcp::TcpTuple {
            local_ip: [10, 0, 0, 1],
            local_port: 8080,
            remote_ip: [192, 168, 1, 99],
            remote_port: 59999,
        },
        iss: 9999,
        irs: 8888,
        peer_mss: 1460,
    };
    let rejected = listen.push_accepted(overflow);
    assert_test!(
        !rejected,
        "push_accepted should fail when accept queue is full"
    );
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog,
        "accept queue unchanged after overflow"
    );

    // Drain one, then push again — should succeed.
    let _ = listen.accept();
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog - 1,
        "accept queue drained by 1"
    );

    let ok = listen.push_accepted(overflow);
    assert_test!(ok, "push_accepted should succeed after draining");
    assert_eq_test!(
        listen.accept_queue_len(),
        backlog,
        "accept queue back to backlog"
    );

    pass!()
}

// =============================================================================
// Phase 5C: TcpListenState — backlog clamping
// =============================================================================

pub fn test_listen_state_backlog_clamping() -> TestResult {
    reset_syn_entry_keys();

    // Backlog 0 should clamp to BACKLOG_MIN (1).
    let listen_min = TcpListenState::new(0, local_addr());
    assert_eq_test!(
        listen_min.backlog(),
        super::tcp_socket::BACKLOG_MIN,
        "backlog=0 should clamp to BACKLOG_MIN"
    );

    // Backlog 999 should clamp to BACKLOG_MAX (128).
    let listen_max = TcpListenState::new(999, local_addr());
    assert_eq_test!(
        listen_max.backlog(),
        super::tcp_socket::BACKLOG_MAX,
        "backlog=999 should clamp to BACKLOG_MAX"
    );

    // Normal backlog should pass through.
    let listen_normal = TcpListenState::new(16, local_addr());
    assert_eq_test!(
        listen_normal.backlog(),
        16,
        "backlog=16 should pass through"
    );

    pass!()
}

// =============================================================================
// Phase 5C: TcpListenState — accept returns FIFO order
// =============================================================================

pub fn test_accept_fifo_order() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(8, local_addr());

    // Push 3 connections with distinct remote ports.
    for i in 0..3u16 {
        let accepted = super::tcp_socket::AcceptedConn {
            tuple: super::tcp::TcpTuple {
                local_ip: [10, 0, 0, 1],
                local_port: 8080,
                remote_ip: [192, 168, 1, 1],
                remote_port: 40000 + i,
            },
            iss: 1000 + i as u32,
            irs: 2000 + i as u32,
            peer_mss: 1460,
        };
        listen.push_accepted(accepted);
    }

    // Accept should return in FIFO order.
    for i in 0..3u16 {
        let conn = listen.accept();
        assert_test!(conn.is_some(), "accept should return connection");
        assert_eq_test!(
            conn.unwrap().tuple.remote_port,
            40000 + i,
            "accept should return in FIFO order"
        );
    }

    // Queue should be empty now.
    let none = listen.accept();
    assert_test!(none.is_none(), "accept on empty queue returns None");

    pass!()
}

// =============================================================================
// Phase 5C: TcpListenState — clear wipes both queues
// =============================================================================

pub fn test_listen_state_clear() -> TestResult {
    reset_syn_entry_keys();
    let mut listen = TcpListenState::new(16, local_addr());

    // Add something to SYN queue.
    let _ = listen.on_syn(client_addr(0), 1000, 1460, 0);
    assert_eq_test!(listen.syn_queue_len(), 1, "SYN queue has 1 entry");

    // Push to accept queue.
    let accepted = super::tcp_socket::AcceptedConn {
        tuple: super::tcp::TcpTuple {
            local_ip: [10, 0, 0, 1],
            local_port: 8080,
            remote_ip: [192, 168, 1, 50],
            remote_port: 55000,
        },
        iss: 3000,
        irs: 4000,
        peer_mss: 1460,
    };
    listen.push_accepted(accepted);
    assert_eq_test!(listen.accept_queue_len(), 1, "accept queue has 1 entry");

    // Clear both queues.
    listen.clear();
    assert_eq_test!(listen.syn_queue_len(), 0, "SYN queue cleared");
    assert_eq_test!(listen.accept_queue_len(), 0, "accept queue cleared");

    pass!()
}

slopos_lib::define_test_suite!(
    tcp_socket,
    [
        // Phase 5A tests
        test_syn_queue_overflow,
        test_accept_queue_overflow,
        test_syn_ack_retransmit_exhaustion,
        test_duplicate_syn_retransmits,
        // Phase 5B tests
        test_demux_register_established_lookup,
        test_demux_established_duplicate_rejected,
        test_demux_unregister_established,
        test_demux_register_listener_lookup,
        test_demux_listener_wildcard_fallback,
        test_demux_unregister_listener,
        test_demux_clear,
        // Phase 5C tests
        test_push_accepted_basic,
        test_push_accepted_respects_backlog,
        test_listen_state_backlog_clamping,
        test_accept_fifo_order,
        test_listen_state_clear,
    ]
);
