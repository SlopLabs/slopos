use slopos_abi::net::{AF_INET, SOCK_DGRAM, SOCK_STREAM};
use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, fail, pass};

use crate::net::socket::*;
use crate::net::tcp::{self, TCP_FLAG_ACK, TCP_FLAG_SYN, TcpHeader, TcpState};

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
    for _ in 0..64 {
        if socket_create(AF_INET, SOCK_STREAM, 0) < 0 {
            return fail!("early socket allocation failure");
        }
    }
    assert_test!(
        socket_create(AF_INET, SOCK_STREAM, 0) < 0,
        "65th socket fails"
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
    ]
);
