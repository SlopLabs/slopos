//! The chunked byte ring under the TCP buffers.

use slopos_ostd::KVec;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::chunk::{self, CHUNK_SIZE, ChunkRing, Spares};

fn filled(len: usize, byte: u8) -> KVec<u8> {
    let mut v = KVec::zeroed(len).expect("test alloc");
    v.as_mut_slice().fill(byte);
    v
}

fn pattern(pos: usize) -> u8 {
    (pos.wrapping_mul(31) ^ (pos >> 7)) as u8
}

pub fn test_chunk_ring_holds_only_what_is_buffered() -> TestResult {
    let mut ring = ChunkRing::new(64 * 1024, 64 * 1024).expect("alloc");
    assert_eq_test!(ring.chunks_held(), 0, "an empty ring holds no chunk");

    assert_eq_test!(
        ring.write(&[1u8; 10], &mut Spares::for_bytes(10, 0)),
        10,
        "small write"
    );
    assert_eq_test!(ring.chunks_held(), 1, "ten bytes take one chunk");

    let big = filled(3 * CHUNK_SIZE, 2);
    let wrote = ring.write(big.as_slice(), &mut Spares::for_bytes(big.len(), 0));
    assert_eq_test!(wrote, big.len(), "multi-chunk write");
    assert_eq_test!(
        ring.chunks_held(),
        4,
        "ten bytes plus three chunks span four"
    );

    let mut out = filled(2 * CHUNK_SIZE, 0);
    assert_eq_test!(
        ring.read(out.as_mut_slice()),
        2 * CHUNK_SIZE,
        "read two chunks' worth"
    );
    assert_eq_test!(ring.chunks_held(), 2, "the two emptied chunks went back");

    assert_eq_test!(
        ring.read(out.as_mut_slice()),
        CHUNK_SIZE + 10,
        "drain the rest"
    );
    assert_test!(ring.is_empty(), "stream empty");
    assert_eq_test!(
        ring.chunks_held(),
        1,
        "only the chunk the head sits in stays"
    );
    pass!()
}

pub fn test_chunk_ring_out_of_order_costs_its_own_chunk() -> TestResult {
    let mut ring = ChunkRing::new(64 * 1024, 64 * 1024).expect("alloc");
    let placed = ring.write_at(40_000, b"late", &mut Spares::for_bytes(4, 0));
    assert_eq_test!(placed, 4, "out-of-order bytes placed");
    assert_eq_test!(ring.len(), 0, "not part of the stream yet");
    assert_eq_test!(ring.chunks_held(), 1, "a nine-chunk hole costs nothing");

    let gap = filled(40_000, 7);
    let mut wrote = 0;
    while wrote < gap.len() {
        let rest = &gap.as_slice()[wrote..];
        let n = ring.write(rest, &mut Spares::for_bytes(rest.len(), 0));
        if n == 0 {
            return fail!("the gap stopped filling at {}", wrote);
        }
        wrote += n;
    }
    ring.advance(4);
    assert_eq_test!(ring.len(), 40_004, "joined");

    let mut tail = [0u8; 4];
    assert_eq_test!(ring.peek_at(40_000, &mut tail), 4, "peek the joined bytes");
    assert_eq_test!(
        &tail,
        b"late",
        "the out-of-order bytes are where they belong"
    );
    pass!()
}

pub fn test_chunk_ring_survives_wrapping() -> TestResult {
    let max = 4 * CHUNK_SIZE;
    let mut ring = ChunkRing::new(max, max).expect("alloc");
    let mut block = filled(3000, 0);
    let mut out = filled(2000, 0);
    let mut written = 0usize;
    let mut read = 0usize;
    while read < 200_000 {
        let n = ring.free_space().min(block.len());
        for (i, b) in block.as_mut_slice()[..n].iter_mut().enumerate() {
            *b = pattern(written + i);
        }
        let wrote = ring.write(&block.as_slice()[..n], &mut Spares::for_bytes(n, 0));
        if wrote != n {
            return fail!("short write {} of {} at {}", wrote, n, written);
        }
        written += wrote;

        let got = ring.read(out.as_mut_slice());
        for (i, b) in out.as_slice()[..got].iter().enumerate() {
            if *b != pattern(read + i) {
                return fail!(
                    "byte {} is {:#x}, want {:#x}",
                    read + i,
                    b,
                    pattern(read + i)
                );
            }
        }
        read += got;
    }
    assert_test!(
        ring.chunks_held() <= max / CHUNK_SIZE + 1,
        "never more chunks than the capacity spans"
    );
    pass!()
}

pub fn test_chunk_ring_without_spares_refuses() -> TestResult {
    let mut ring = ChunkRing::new(64 * 1024, 64 * 1024).expect("alloc");
    let mut none = Spares::new();
    assert_eq_test!(
        ring.write(&[1u8; 100], &mut none),
        0,
        "no chunk and no spare: nothing lands"
    );
    assert_eq_test!(
        ring.write(&[1u8; 100], &mut Spares::for_bytes(100, 0)),
        100,
        "with one"
    );
    assert_eq_test!(
        ring.write(&[2u8; 100], &mut none),
        100,
        "room left in the chunk needs no spare"
    );
    let fill = filled(CHUNK_SIZE, 3);
    assert_eq_test!(
        ring.write(fill.as_slice(), &mut none),
        CHUNK_SIZE - 200,
        "stops at the chunk boundary"
    );
    pass!()
}

pub fn test_chunk_ring_capacity_is_a_cap() -> TestResult {
    let mut ring = ChunkRing::new(64 * 1024, 1000).expect("alloc");
    let data = filled(3000, 9);
    assert_eq_test!(
        ring.write(data.as_slice(), &mut Spares::for_bytes(3000, 0)),
        1000,
        "capacity bounds a write"
    );
    ring.set_capacity(usize::MAX);
    assert_eq_test!(
        ring.capacity(),
        ring.max_capacity(),
        "raised no further than the ring can hold"
    );
    assert_test!(
        ring.max_capacity() >= 64 * 1024,
        "the ring holds what it was sized for"
    );
    let edge = ring.free_space();
    assert_eq_test!(
        ring.write_at(edge, b"x", &mut Spares::for_bytes(1, 0)),
        0,
        "nothing lands past the capacity"
    );
    pass!()
}

pub fn test_chunk_accounting_returns_to_baseline() -> TestResult {
    let before = chunk::live_chunks() - chunk::cached_chunks();
    {
        let mut ring = ChunkRing::new(256 * 1024, 256 * 1024).expect("alloc");
        let data = filled(16 * CHUNK_SIZE, 5);
        let wrote = ring.write(data.as_slice(), &mut Spares::for_bytes(data.len(), 0));
        assert_eq_test!(wrote, data.len(), "sixteen chunks written");
        assert_eq_test!(ring.chunks_held(), 16, "exactly the chunks the bytes need");
    }
    let after = chunk::live_chunks() - chunk::cached_chunks();
    assert_eq_test!(
        after,
        before,
        "a dropped ring's chunks are cached or freed, not leaked"
    );
    pass!()
}

pub fn test_a_spent_pool_leaves_each_ring_its_reserved_share() -> TestResult {
    let Ok(mut send) = crate::tcp::buffer::TcpSendState::new(64 * 1024, 64 * 1024) else {
        return fail!("send state alloc");
    };
    let old = chunk::swap_chunk_ceiling(chunk::live_chunks());
    let mut drained: KVec<Spares> = KVec::new();
    loop {
        let spares = Spares::for_bytes(SPARES_BYTES, chunk::RESERVED_CHUNKS);
        let empty = spares.is_empty();
        if drained.push(spares).is_err() || empty {
            break;
        }
    }
    let writable = send.writable();
    let mut spares = Spares::for_bytes(64 * 1024, send.chunks_held());
    let reserved = spares.len();
    let wrote = send.enqueue(filled(64 * 1024, 1).as_slice(), &mut spares);
    let past = Spares::for_bytes(SPARES_BYTES, send.chunks_held()).len();
    drop(spares);
    drop(drained);
    chunk::swap_chunk_ceiling(old);

    let share = chunk::RESERVED_CHUNKS * CHUNK_SIZE;
    assert_eq_test!(writable, share, "what a spent pool leaves writable");
    assert_eq_test!(
        reserved,
        chunk::RESERVED_CHUNKS,
        "the reserved chunks are found"
    );
    assert_eq_test!(wrote, share, "and filled");
    assert_eq_test!(past, 0, "and nothing past them");
    pass!()
}

/// With the pool spent, out-of-order chunks do not use up the stream's share.
pub fn test_out_of_order_chunks_leave_the_gap_its_chunk() -> TestResult {
    let mut ring = ChunkRing::new(1024 * 1024, 1024 * 1024).expect("alloc");
    for k in 1..=chunk::RESERVED_CHUNKS + 1 {
        let held = ring.chunks_held();
        ring.write_at(k * CHUNK_SIZE, b"x", &mut Spares::for_bytes(1, held));
    }
    let old = chunk::swap_chunk_ceiling(chunk::live_chunks());
    let mut drained: KVec<Spares> = KVec::new();
    loop {
        let spares = Spares::for_bytes(SPARES_BYTES, chunk::RESERVED_CHUNKS);
        let empty = spares.is_empty();
        if drained.push(spares).is_err() || empty {
            break;
        }
    }
    let held = ring.chunks_held();
    let stream = ring.stream_chunks();
    let mut spares = Spares::for_bytes(8, stream);
    let wrote = ring.write(b"the gap!", &mut spares);
    drop(spares);
    drop(drained);
    chunk::swap_chunk_ceiling(old);

    assert_eq_test!(held, chunk::RESERVED_CHUNKS + 1, "out-of-order chunks held");
    assert_eq_test!(stream, 0, "none of them the stream's");
    assert_eq_test!(wrote, 8, "the gap's bytes land");
    pass!()
}

pub fn test_out_of_order_bytes_have_a_chunk_budget() -> TestResult {
    let mut ring = ChunkRing::new(4 * 1024 * 1024, 4 * 1024 * 1024).expect("alloc");
    let mut placed = 0;
    for k in 1..(chunk::OUT_OF_ORDER_CHUNKS + 64) {
        let held = ring.chunks_held();
        placed += ring.write_at(k * CHUNK_SIZE, b"x", &mut Spares::for_bytes(1, held));
    }
    assert_eq_test!(placed, chunk::OUT_OF_ORDER_CHUNKS + 1, "bytes placed");
    assert_test!(
        ring.chunks_held() <= chunk::OUT_OF_ORDER_CHUNKS + 1,
        "chunks pinned ahead of an empty stream: {}",
        ring.chunks_held()
    );
    pass!()
}

pub fn test_advance_joins_only_what_the_capacity_holds() -> TestResult {
    let mut ring = ChunkRing::new(64 * 1024, 4096).expect("alloc");
    let ooo = ring.write_at(100, &[2u8; 100], &mut Spares::for_bytes(100, 0));
    assert_eq_test!(ooo, 100);
    ring.set_capacity(150);
    assert_eq_test!(ring.write(&[1u8; 100], &mut Spares::for_bytes(100, 0)), 100);
    assert_eq_test!(ring.advance(100), 50, "joined up to the capacity");
    assert_eq_test!(ring.len(), 150);
    pass!()
}

const SPARES_BYTES: usize = chunk::SPARES_MAX * CHUNK_SIZE;

pub fn test_chunk_limits_follow_memory() -> TestResult {
    let mib = 1024 * 1024u64;
    assert_eq_test!(
        chunk::derive_limits(64 * mib),
        (512 * 1024, 8 * 1024 * 1024),
        "a small machine: 1/128 per connection, the total floor"
    );
    assert_eq_test!(
        chunk::derive_limits(1024 * mib),
        (chunk::TCP_BUFFER_CEILING, 64 * 1024 * 1024),
        "a gigabyte: the ceiling per connection, 1/16 in total"
    );
    assert_eq_test!(
        chunk::derive_limits(8 * mib),
        (chunk::TCP_BUFFER_FLOOR, 8 * 1024 * 1024),
        "never below the floors"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_chunk_ring_holds_only_what_is_buffered,
    suite = tcp_chunk
);
slopos_testing::stest!(
    name = test_chunk_ring_out_of_order_costs_its_own_chunk,
    suite = tcp_chunk
);
slopos_testing::stest!(name = test_chunk_ring_survives_wrapping, suite = tcp_chunk);
slopos_testing::stest!(
    name = test_chunk_ring_without_spares_refuses,
    suite = tcp_chunk
);
slopos_testing::stest!(name = test_chunk_ring_capacity_is_a_cap, suite = tcp_chunk);
slopos_testing::stest!(
    name = test_chunk_accounting_returns_to_baseline,
    suite = tcp_chunk
);
slopos_testing::stest!(name = test_chunk_limits_follow_memory, suite = tcp_chunk);
slopos_testing::stest!(
    name = test_a_spent_pool_leaves_each_ring_its_reserved_share,
    suite = tcp_chunk
);
slopos_testing::stest!(
    name = test_out_of_order_chunks_leave_the_gap_its_chunk,
    suite = tcp_chunk
);
slopos_testing::stest!(
    name = test_out_of_order_bytes_have_a_chunk_budget,
    suite = tcp_chunk
);
slopos_testing::stest!(
    name = test_advance_joins_only_what_the_capacity_holds,
    suite = tcp_chunk
);
