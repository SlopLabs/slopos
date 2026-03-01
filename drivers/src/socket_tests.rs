use slopos_abi::net::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, fail, pass};

use crate::net::socket::*;
use crate::net::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader, TcpState};
use crate::net::types::NetError;

fn reset() {
    socket_reset_all();
}

fn connect_and_establish() -> Result<(u32, usize), &'static str> {
    let sock = socket_create(AF_INET, SOCK_STREAM, 0);
    if sock < 0 {
        return Err("socket_create failed");
    }
    if socket_connect(sock as u32, [10, 0, 0, 2], 80) < 0 {
        return Err("socket_connect failed");
    }

    let Some(tcp_idx) = socket_lookup_tcp_idx(sock as u32) else {
        return Err("no tcp idx");
    };
    let Some(conn) = tcp::tcp_get_connection(tcp_idx) else {
        return Err("no tcp conn");
    };

    let syn_ack = TcpHeader {
        src_port: conn.tuple.remote_port,
        dst_port: conn.tuple.local_port,
        seq_num: 9000,
        ack_num: conn.iss.wrapping_add(1),
        data_offset: 5,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: 32768,
        checksum: 0,
        urgent_ptr: 0,
    };
    let _ = tcp::tcp_input(
        conn.tuple.remote_ip,
        conn.tuple.local_ip,
        &syn_ack,
        &[],
        &[],
        0,
    );

    Ok((sock as u32, tcp_idx))
}

pub fn test_socket_create_tcp() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0);
    assert_test!(idx >= 0, "tcp socket create succeeds");
    pass!()
}

pub fn test_socket_create_udp() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_DGRAM, 0);
    assert_test!(idx >= 0, "udp socket create succeeds");
    pass!()
}

pub fn test_socket_create_invalid_domain() -> TestResult {
    reset();
    let idx = socket_create(1, SOCK_STREAM, 0);
    assert_test!(idx < 0, "invalid domain fails");
    pass!()
}

pub fn test_socket_create_invalid_type() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, 99, 0);
    assert_test!(idx < 0, "invalid type fails");
    pass!()
}

pub fn test_socket_table_full() -> TestResult {
    reset();
    // SlabSocketTable grows on demand up to MAX_CAPACITY (1024).
    // Verify we can allocate beyond the initial 64-slot capacity.
    for i in 0..128 {
        if socket_create(AF_INET, SOCK_STREAM, 0) < 0 {
            return fail!("socket allocation failed at index {}", i);
        }
    }
    // 129th socket should still succeed (table grows to 256).
    assert_test!(
        socket_create(AF_INET, SOCK_STREAM, 0) >= 0,
        "129th socket succeeds (growable table)"
    );
    pass!()
}

pub fn test_socket_bind_valid() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_bind(idx, [0, 0, 0, 0], 8080), 0);
    pass!()
}

pub fn test_socket_bind_specific_addr() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_bind(idx, [10, 0, 0, 1], 80), 0);
    pass!()
}

pub fn test_socket_bind_invalid_idx() -> TestResult {
    reset();
    assert_test!(
        socket_bind(999, [0, 0, 0, 0], 8080) < 0,
        "invalid socket idx fails"
    );
    pass!()
}

pub fn test_socket_bind_already_bound() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_bind(idx, [0, 0, 0, 0], 8080), 0);
    assert_test!(
        socket_bind(idx, [0, 0, 0, 0], 8081) < 0,
        "double bind fails"
    );
    pass!()
}

pub fn test_socket_listen_after_bind() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_bind(idx, [0, 0, 0, 0], 8080), 0);
    assert_eq_test!(socket_listen(idx, 16), 0);
    pass!()
}

pub fn test_socket_listen_without_bind() -> TestResult {
    reset();
    let idx = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_test!(socket_listen(idx, 16) < 0, "listen without bind fails");
    pass!()
}

pub fn test_socket_listen_on_connected() -> TestResult {
    reset();
    let (sock, _) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    assert_test!(
        socket_listen(sock, 4) < 0,
        "listen on connected socket fails"
    );
    pass!()
}

pub fn test_socket_connect_creates_tcp_connection() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_connect(sock, [10, 0, 0, 2], 80), 0);
    let tcp_idx = socket_lookup_tcp_idx(sock).unwrap();
    assert_eq_test!(tcp::tcp_get_state(tcp_idx), Some(TcpState::SynSent));
    pass!()
}

pub fn test_socket_connect_invalid_socket() -> TestResult {
    reset();
    assert_test!(
        socket_connect(12345, [10, 0, 0, 2], 80) < 0,
        "invalid connect fails"
    );
    pass!()
}

pub fn test_socket_connect_already_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_connect(sock, [10, 0, 0, 2], 80), 0);
    assert_test!(
        socket_connect(sock, [10, 0, 0, 2], 80) < 0,
        "double connect fails"
    );
    pass!()
}

pub fn test_socket_send_returns_error_not_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    let payload = [1u8, 2, 3];
    assert_test!(
        socket_send(sock, payload.as_ptr(), payload.len()) < 0,
        "send without connect fails"
    );
    pass!()
}

pub fn test_socket_recv_returns_error_not_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    let mut buf = [0u8; 8];
    assert_test!(
        socket_recv(sock, buf.as_mut_ptr(), buf.len()) < 0,
        "recv without connect fails"
    );
    pass!()
}

pub fn test_socket_send_buffer_space() -> TestResult {
    reset();
    let (sock, tcp_idx) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    let probe = socket_max_send_probe(sock, 1024);
    assert_test!(probe >= 0, "send probe succeeds");
    assert_test!(
        probe as usize <= tcp::tcp_send_buffer_space(tcp_idx),
        "probe <= tcp space"
    );
    pass!()
}

pub fn test_socket_recv_empty() -> TestResult {
    reset();
    let (sock, _) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    let mut buf = [0u8; 16];
    let n = socket_recv(sock, buf.as_mut_ptr(), buf.len());
    assert_test!(n == 0 || n < 0, "recv empty returns 0 or error");
    pass!()
}

pub fn test_socket_close_valid() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_close(sock), 0);
    pass!()
}

pub fn test_socket_close_invalid() -> TestResult {
    reset();
    assert_test!(socket_close(4444) < 0, "close invalid fails");
    pass!()
}

pub fn test_socket_close_frees_slot() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0);
    assert_eq_test!(socket_close(sock as u32), 0);
    let next = socket_create(AF_INET, SOCK_STREAM, 0);
    assert_eq_test!(next, sock);
    pass!()
}

pub fn test_socket_poll_readable_not_connected() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_poll_readable(sock), 0);
    pass!()
}

pub fn test_socket_poll_writable_connected() -> TestResult {
    reset();
    let (sock, _) = match connect_and_establish() {
        Ok(v) => v,
        Err(m) => return fail!("{}", m),
    };
    assert_eq_test!(
        socket_poll_writable(sock) & slopos_abi::syscall::POLLOUT as u32,
        slopos_abi::syscall::POLLOUT as u32
    );
    pass!()
}

pub fn test_socket_state_after_create() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_get_state(sock), Some(SocketState::Unbound));
    pass!()
}

pub fn test_socket_state_after_bind() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 8080), 0);
    assert_eq_test!(socket_get_state(sock), Some(SocketState::Bound));
    pass!()
}

pub fn test_socket_reset_all() -> TestResult {
    reset();
    let _ = socket_create(AF_INET, SOCK_STREAM, 0);
    let _ = socket_create(AF_INET, SOCK_DGRAM, 0);
    assert_test!(socket_count_active() >= 2, "active before reset");
    socket_reset_all();
    assert_eq_test!(socket_count_active(), 0);
    assert_eq_test!(tcp::tcp_active_count(), 0);
    pass!()
}

pub fn test_socket_accept_no_pending_returns_eagain() -> TestResult {
    reset();
    let sock = socket_create(AF_INET, SOCK_STREAM, 0) as u32;
    assert_eq_test!(socket_bind(sock, [0, 0, 0, 0], 8080), 0);
    assert_eq_test!(socket_listen(sock, 8), 0);
    assert_test!(
        socket_accept(sock, core::ptr::null_mut(), core::ptr::null_mut()) < 0,
        "accept without pending fails"
    );
    pass!()
}

pub fn test_bounded_queue_push_pop_capacity() -> TestResult {
    let mut q = BoundedQueue::new(3);
    assert_eq_test!(q.capacity(), 3);
    assert_test!(q.is_empty(), "queue starts empty");

    assert_test!(q.push(10), "push first element");
    assert_test!(q.push(20), "push second element");
    assert_test!(q.push(30), "push third element");
    assert_test!(q.is_full(), "queue should be full");

    assert_eq_test!(q.pop(), Some(10));
    assert_eq_test!(q.pop(), Some(20));
    assert_eq_test!(q.pop(), Some(30));
    assert_eq_test!(q.pop(), None);
    pass!()
}

pub fn test_bounded_queue_overflow_returns_false() -> TestResult {
    let mut q = BoundedQueue::new(1);
    assert_test!(q.push(1), "first push succeeds");
    assert_test!(!q.push(2), "overflow push must fail");
    assert_eq_test!(q.len(), 1);
    assert_eq_test!(q.pop(), Some(1));
    pass!()
}

pub fn test_bounded_queue_clear_and_resize() -> TestResult {
    let mut q = BoundedQueue::new(3);
    let _ = q.push(1);
    let _ = q.push(2);
    let _ = q.push(3);
    q.clear();

    assert_test!(q.is_empty(), "clear empties queue");
    assert_eq_test!(q.pop(), None);

    let _ = q.push(4);
    let _ = q.push(5);
    let _ = q.push(6);
    q.resize(2);
    assert_eq_test!(q.capacity(), 2);
    assert_eq_test!(q.len(), 2);
    assert_eq_test!(q.pop(), Some(4));
    assert_eq_test!(q.pop(), Some(5));
    assert_eq_test!(q.pop(), None);
    pass!()
}

pub fn test_slab_socket_table_alloc_free_get_get_mut() -> TestResult {
    let mut table = SlabSocketTable::new(2, 8);
    let idx = table
        .alloc(SocketInner::Tcp(TcpSocketInner {
            conn_id: None,
            listen: None,
        }))
        .unwrap();

    assert_eq_test!(idx, 0);
    assert_eq_test!(table.count_active(), 1);
    assert_test!(table.get(idx).is_some(), "allocated socket is retrievable");

    {
        let sock = table.get_mut(idx).unwrap();
        sock.set_nonblocking(true);
        assert_test!(sock.is_nonblocking(), "mutable access updates flags");
    }

    table.free(idx);
    assert_test!(table.get(idx).is_none(), "freed slot is empty");
    assert_eq_test!(table.count_active(), 0);
    pass!()
}

pub fn test_slab_socket_table_grows_and_enforces_max() -> TestResult {
    let mut table = SlabSocketTable::new(2, 4);
    assert_eq_test!(table.capacity(), 2);

    for _ in 0..4 {
        let idx = table.alloc(SocketInner::Udp(UdpSocketInner));
        assert_test!(idx.is_some(), "allocation within max should succeed");
    }

    assert_eq_test!(table.capacity(), 4);
    assert_test!(
        table.alloc(SocketInner::Raw(RawSocketInner)).is_none(),
        "allocation beyond max must fail"
    );
    pass!()
}

pub fn test_ephemeral_port_allocator_alloc_release_round_robin() -> TestResult {
    let mut alloc = EphemeralPortAllocator::new();

    let p1 = alloc.alloc().unwrap();
    let p2 = alloc.alloc().unwrap();
    assert_eq_test!(p1.0, EphemeralPortAllocator::EPHEMERAL_PORT_START);
    assert_eq_test!(p2.0, EphemeralPortAllocator::EPHEMERAL_PORT_START + 1);

    alloc.release(p1);
    let p3 = alloc.alloc().unwrap();
    assert_eq_test!(p3.0, EphemeralPortAllocator::EPHEMERAL_PORT_START + 2);

    assert_test!(!alloc.is_in_use(p1), "released port should be free");
    assert_test!(alloc.is_in_use(p2), "second allocated port is still in use");
    assert_test!(alloc.is_in_use(p3), "newly allocated port is in use");
    pass!()
}

pub fn test_ephemeral_port_allocator_exhaustion_and_no_duplicates() -> TestResult {
    let mut alloc = EphemeralPortAllocator::new();
    let mut first_ports = [0u16; 64];

    for i in 0..first_ports.len() {
        let p = alloc.alloc().unwrap();
        first_ports[i] = p.0;
        for prev in first_ports.iter().take(i) {
            assert_test!(*prev != p.0, "ephemeral allocation must be unique");
        }
    }

    let mut total = first_ports.len();
    while alloc.alloc().is_some() {
        total += 1;
    }
    assert_eq_test!(total, EphemeralPortAllocator::EPHEMERAL_PORT_COUNT);
    assert_test!(
        alloc.alloc().is_none(),
        "allocator should report exhaustion"
    );
    assert_eq_test!(alloc.available(), 0);
    pass!()
}

pub fn test_socket_options_defaults_and_validation() -> TestResult {
    let opts = SocketOptions::new();
    assert_test!(!opts.reuse_addr, "reuse_addr default false");
    assert_eq_test!(opts.recv_buf_size, SocketOptions::RECV_BUF_DEFAULT);
    assert_eq_test!(opts.send_buf_size, SocketOptions::SEND_BUF_DEFAULT);
    assert_eq_test!(opts.recv_timeout, None);
    assert_eq_test!(opts.send_timeout, None);
    assert_test!(!opts.keepalive, "keepalive default false");
    assert_test!(!opts.tcp_nodelay, "tcp_nodelay default false");

    assert_eq_test!(
        SocketOptions::validate_recv_buf_size(SocketOptions::RECV_BUF_MIN),
        Ok(SocketOptions::RECV_BUF_MIN)
    );
    assert_eq_test!(
        SocketOptions::validate_send_buf_size(SocketOptions::SEND_BUF_MAX),
        Ok(SocketOptions::SEND_BUF_MAX)
    );
    assert_eq_test!(
        SocketOptions::validate_recv_buf_size(SocketOptions::RECV_BUF_MIN - 1),
        Err(NetError::InvalidArgument)
    );
    assert_eq_test!(
        SocketOptions::validate_send_buf_size(SocketOptions::SEND_BUF_MAX + 1),
        Err(NetError::InvalidArgument)
    );
    pass!()
}

pub fn test_socket_flags_set_clear_contains() -> TestResult {
    let mut flags = SocketFlags::NONE;
    assert_test!(
        !flags.contains(SocketFlags::O_NONBLOCK),
        "starts without nonblocking"
    );

    flags.set(SocketFlags::O_NONBLOCK);
    flags.set(SocketFlags::SHUT_RD);
    assert_test!(flags.contains(SocketFlags::O_NONBLOCK), "nonblocking set");
    assert_test!(flags.contains(SocketFlags::SHUT_RD), "read shutdown set");

    flags.clear(SocketFlags::O_NONBLOCK);
    assert_test!(
        !flags.contains(SocketFlags::O_NONBLOCK),
        "nonblocking cleared"
    );
    assert_eq_test!(
        SocketFlags::from_bits(flags.bits()),
        SocketFlags::from_bits(SocketFlags::SHUT_RD.bits())
    );
    pass!()
}

pub fn test_socket_new_defaults_and_helpers() -> TestResult {
    let mut sock = Socket::new(SocketInner::Udp(UdpSocketInner));

    assert_eq_test!(sock.state, SocketState::Unbound);
    assert_test!(sock.local_addr.is_none(), "local addr starts unset");
    assert_test!(sock.remote_addr.is_none(), "remote addr starts unset");
    assert_eq_test!(
        sock.recv_queue.capacity(),
        Socket::RECV_QUEUE_DEFAULT_CAPACITY
    );
    assert_eq_test!(sock.pending_error, None);
    assert_test!(!sock.is_nonblocking(), "socket starts blocking");

    sock.set_nonblocking(true);
    assert_test!(sock.is_nonblocking(), "set_nonblocking enables flag");

    sock.flags.set(SocketFlags::SHUT_RD);
    sock.flags.set(SocketFlags::SHUT_WR);
    assert_test!(
        sock.is_read_shutdown(),
        "read shutdown helper reflects flag"
    );
    assert_test!(
        sock.is_write_shutdown(),
        "write shutdown helper reflects flag"
    );

    sock.pending_error = Some(NetError::WouldBlock);
    assert_eq_test!(sock.take_pending_error(), Some(NetError::WouldBlock));
    assert_eq_test!(sock.take_pending_error(), None);
    pass!()
}

slopos_lib::define_test_suite!(
    socket,
    [
        test_socket_create_tcp,
        test_socket_create_udp,
        test_socket_create_invalid_domain,
        test_socket_create_invalid_type,
        test_socket_table_full,
        test_socket_bind_valid,
        test_socket_bind_specific_addr,
        test_socket_bind_invalid_idx,
        test_socket_bind_already_bound,
        test_socket_listen_after_bind,
        test_socket_listen_without_bind,
        test_socket_listen_on_connected,
        test_socket_connect_creates_tcp_connection,
        test_socket_connect_invalid_socket,
        test_socket_connect_already_connected,
        test_socket_send_returns_error_not_connected,
        test_socket_recv_returns_error_not_connected,
        test_socket_send_buffer_space,
        test_socket_recv_empty,
        test_socket_close_valid,
        test_socket_close_invalid,
        test_socket_close_frees_slot,
        test_socket_poll_readable_not_connected,
        test_socket_poll_writable_connected,
        test_socket_state_after_create,
        test_socket_state_after_bind,
        test_socket_reset_all,
        test_socket_accept_no_pending_returns_eagain,
        test_bounded_queue_push_pop_capacity,
        test_bounded_queue_overflow_returns_false,
        test_bounded_queue_clear_and_resize,
        test_slab_socket_table_alloc_free_get_get_mut,
        test_slab_socket_table_grows_and_enforces_max,
        test_ephemeral_port_allocator_alloc_release_round_robin,
        test_ephemeral_port_allocator_exhaustion_and_no_duplicates,
        test_socket_options_defaults_and_validation,
        test_socket_flags_set_clear_contains,
        test_socket_new_defaults_and_helpers,
    ]
);
