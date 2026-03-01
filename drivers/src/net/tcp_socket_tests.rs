//! Phase 5A integration tests — Two-Queue Listen Model.
//!
//! Tests the SYN queue, accept queue, SYN-ACK retransmission, and overflow
//! behavior of [`TcpListenState`].

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use super::tcp_socket::{SYN_QUEUE_MAX, SYN_RETRIES_MAX, TcpListenState, reset_syn_entry_keys};
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

slopos_lib::define_test_suite!(
    tcp_socket,
    [
        test_syn_queue_overflow,
        test_accept_queue_overflow,
        test_syn_ack_retransmit_exhaustion,
        test_duplicate_syn_retransmits,
    ]
);
