//! Receive windows past 64 KiB and the window scaling that carries them.

use slopos_ostd::KVec;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::tcp::{
    self, DEFAULT_WINDOW_SIZE, TCP_FLAG_ACK, TCP_FLAG_PSH, TCP_FLAG_SYN, TcpTuple, chunk,
    our_window_scale,
};
use crate::tests::net_scope::{NetTestScope, ScopeError};
use crate::tests::tcp_common::{self, EstablishedConn, LOCAL_IP, PEER_ISS, REMOTE_IP, REMOTE_PORT};
use crate::with_data_state;

const PEER_SHIFT: u8 = 2;

#[cold]
#[inline(never)]
fn scope_error(e: ScopeError) -> TestResult {
    fail!("net scope: {:?}", e)
}

fn wscale_option(shift: u8) -> [u8; 4] {
    [1, 3, 3, shift]
}

fn establish(peer_shift: Option<u8>) -> (EstablishedConn, tcp::Actions) {
    let (id, syn) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).expect("connect");
    let local_port = syn.tuple.local_port;
    let opt = peer_shift.map(wscale_option);
    let options: &[u8] = match &opt {
        Some(o) => o,
        None => &[],
    };
    let actions = tcp_common::inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        local_port,
        PEER_ISS,
        syn.seq_num.wrapping_add(1),
        TCP_FLAG_SYN | TCP_FLAG_ACK,
        options,
        &[],
    );
    tcp::set_nodelay(id, true);
    let conn = EstablishedConn {
        id,
        local_port,
        our_iss: syn.seq_num,
        peer_iss: PEER_ISS,
    };
    (conn, actions)
}

pub fn test_tcp_syn_offers_the_ceiling_scale() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (_, syn) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).expect("connect");
    assert_eq_test!(our_window_scale(), 7, "4 MiB needs a shift of 7");
    assert_test!(
        chunk::TCP_BUFFER_CEILING >> our_window_scale() <= u16::MAX as usize,
        "the ceiling fits the field at that shift"
    );
    assert_eq_test!(syn.wscale, Some(our_window_scale()), "the SYN offers it");
    assert_eq_test!(
        syn.window_size,
        DEFAULT_WINDOW_SIZE,
        "a SYN's own window is unscaled"
    );
    pass!()
}

pub fn test_tcp_active_open_scales_once_both_offered() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (conn, actions) = establish(Some(PEER_SHIFT));
    let (enabled, rcv, snd) =
        with_data_state!(conn.id, |d| (d.wscale_enabled, d.rcv_wscale, d.snd_wscale));
    assert_test!(enabled, "scaling on");
    assert_eq_test!(rcv, our_window_scale(), "ours");
    assert_eq_test!(snd, PEER_SHIFT, "theirs");
    let Some(ack) = actions.segments().next() else {
        return fail!("no ACK completed the handshake");
    };
    assert_eq_test!(
        ack.window_size,
        DEFAULT_WINDOW_SIZE.div_ceil(1 << our_window_scale()),
        "the handshake's ACK is the first scaled window, rounded up so it takes back nothing the SYN granted"
    );
    pass!()
}

pub fn test_tcp_active_open_unscaled_without_peer_offer() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (conn, actions) = establish(None);
    let (enabled, rcv) = with_data_state!(conn.id, |d| (d.wscale_enabled, d.rcv_wscale));
    assert_test!(!enabled, "scaling off: the peer did not offer it");
    assert_eq_test!(rcv, 0, "no shift of ours applies");
    let Some(ack) = actions.segments().next() else {
        return fail!("no ACK completed the handshake");
    };
    assert_eq_test!(
        ack.window_size,
        DEFAULT_WINDOW_SIZE,
        "an unscaled window is bytes"
    );
    pass!()
}

/// Out of line so its `Actions` stays off the caller's frame.
#[inline(never)]
fn feed(conn: &EstablishedConn, seq: u32, ack: u32, data: &[u8]) -> Result<(), &'static str> {
    let actions = tcp_common::inject(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        conn.local_port,
        seq,
        ack,
        TCP_FLAG_ACK | TCP_FLAG_PSH,
        data,
    );
    match actions.segments().next() {
        Some(seg) if seg.window_size != with_data_state!(conn.id, |d| d.advertised()) => {
            Err("an ACK carried a window other than the one the connection recorded")
        }
        _ => Ok(()),
    }
}

pub fn test_tcp_receive_window_past_64k() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (conn, _) = establish(Some(PEER_SHIFT));
    let snd_nxt = with_data_state!(conn.id, |d| d.snd_nxt.raw());
    let mut segment = KVec::<u8>::zeroed(1460).expect("test alloc");
    let total = 200 * 1024;
    let mut seq = conn.peer_iss.wrapping_add(1);
    let mut sent = 0usize;
    while sent < total {
        let n = segment.len().min(total - sent);
        segment.as_mut_slice()[..n].fill((sent / 1460) as u8);
        if let Err(msg) = feed(&conn, seq, snd_nxt, &segment.as_slice()[..n]) {
            return fail!("{}", msg);
        }
        seq = seq.wrapping_add(n as u32);
        sent += n;
    }
    assert_eq_test!(
        tcp::recv_available(conn.id),
        total,
        "every byte buffered and none of it read"
    );
    let (rcv_wnd, rcv_nxt, field) =
        with_data_state!(conn.id, |d| (d.rcv_wnd, d.rcv_nxt.raw(), d.advertised()));
    assert_eq_test!(rcv_nxt, seq, "every segment was in the window");
    assert_test!(
        (rcv_wnd as usize) < chunk::buffer_max() - total + (1 << our_window_scale()),
        "the window {} is what is left of the buffer, {} - {}, to within the one unit of scale an edge that never moves left may overhang",
        rcv_wnd,
        chunk::buffer_max(),
        total
    );
    assert_test!(
        rcv_wnd > u16::MAX as u32,
        "and still more than sixteen bits could have granted: {}",
        rcv_wnd
    );
    assert_eq_test!(
        field as u32,
        rcv_wnd
            .div_ceil(1 << our_window_scale())
            .min(u16::MAX as u32),
        "the field is the window shifted, rounded up"
    );
    pass!()
}

pub fn test_tcp_send_buffer_past_64k() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (conn, _) = establish(Some(PEER_SHIFT));
    let block = KVec::<u8>::zeroed(8192).expect("test alloc");
    let mut queued = 0usize;
    while queued < 256 * 1024 {
        let n = tcp::send(conn.id, block.as_slice()).expect("send");
        if n == 0 {
            return fail!("send buffer full at {} bytes", queued);
        }
        queued += n;
    }
    assert_test!(
        tcp::send_buffer_space(conn.id) >= chunk::buffer_max() - queued,
        "a quarter megabyte queued and the rest of the default still free"
    );
    pass!()
}

pub fn test_tcp_rcvbuf_set_before_buffers_exist() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (id, syn) = tcp::connect(LOCAL_IP, REMOTE_IP, REMOTE_PORT).expect("connect");
    tcp::set_rcvbuf(id, 8192);
    tcp::set_sndbuf(id, 12_288);
    assert_test!(
        !tcp::table::has_buffer(id),
        "a half-open connection has no buffers yet"
    );
    let _ = tcp_common::inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        REMOTE_PORT,
        syn.tuple.local_port,
        PEER_ISS,
        syn.seq_num.wrapping_add(1),
        TCP_FLAG_SYN | TCP_FLAG_ACK,
        &wscale_option(PEER_SHIFT),
        &[],
    );
    let caps = tcp::table::with_bufs(id, |b| {
        (b.recv.effective_capacity(), b.send.effective_capacity())
    });
    assert_eq_test!(
        caps,
        Some((8192, 12_288)),
        "the buffers were made to the requested size"
    );
    pass!()
}

pub fn test_tcp_passive_open_scales_only_when_offered() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    const PORT: u16 = 8088;
    let listener = tcp::listen(LOCAL_IP, PORT).expect("listen");
    tcp::set_rcvbuf(listener, 16_384);

    let plain = tcp_common::inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        40_001,
        PORT,
        5000,
        0,
        TCP_FLAG_SYN,
        &[],
        &[],
    );
    let Some(plain_synack) = plain.segments().next() else {
        return fail!("no SYN-ACK for a SYN without the option");
    };
    assert_eq_test!(plain_synack.wscale, None, "not offered, not answered");

    let scaled = tcp_common::inject_with_options(
        REMOTE_IP,
        LOCAL_IP,
        40_002,
        PORT,
        9000,
        0,
        TCP_FLAG_SYN,
        &wscale_option(PEER_SHIFT),
        &[],
    );
    let Some(synack) = scaled.segments().next() else {
        return fail!("no SYN-ACK for a SYN offering a scale");
    };
    assert_eq_test!(synack.wscale, Some(our_window_scale()), "offered, answered");

    let _ = tcp_common::inject(
        REMOTE_IP,
        LOCAL_IP,
        40_002,
        PORT,
        9001,
        synack.seq_num.wrapping_add(1),
        TCP_FLAG_ACK,
        &[],
    );
    let child_tuple = TcpTuple {
        local_ip: LOCAL_IP,
        local_port: PORT,
        remote_ip: REMOTE_IP,
        remote_port: 40_002,
    };
    let Some(child) = tcp::find(&child_tuple) else {
        return fail!("the handshake did not install a child");
    };
    let (enabled, rcv, snd) =
        with_data_state!(child, |d| (d.wscale_enabled, d.rcv_wscale, d.snd_wscale));
    assert_test!(enabled, "the child scales");
    assert_eq_test!(rcv, our_window_scale(), "with our shift");
    assert_eq_test!(snd, PEER_SHIFT, "and the peer's");
    let cap = tcp::table::with_bufs(child, |b| b.recv.effective_capacity());
    assert_eq_test!(
        cap,
        Some(16_384),
        "the child inherits the listener's SO_RCVBUF"
    );
    pass!()
}

pub fn test_tcp_window_bounded_by_the_machine() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    let (conn, _) = establish(Some(PEER_SHIFT));
    let old = chunk::swap_chunk_ceiling(chunk::live_chunks() + 2);
    let room = chunk::room_bytes();
    let window = tcp::table::with_bufs(conn.id, |b| b.recv.window());
    chunk::swap_chunk_ceiling(old);
    let Some(window) = window else {
        return fail!("no buffers");
    };
    assert_test!(
        window as usize <= room + chunk::RESERVED_CHUNKS * chunk::CHUNK_SIZE,
        "a window the machine cannot back is not advertised past the ring's reserved share: {} against {}",
        window,
        room
    );
    assert_test!(
        (window as usize) < chunk::buffer_max(),
        "a nearly exhausted machine shrinks it: {}",
        window
    );
    pass!()
}

pub fn test_tcp_read_reopening_the_window_sends_an_update() -> TestResult {
    let _scope = match NetTestScope::enter() {
        Ok(s) => s,
        Err(e) => return scope_error(e),
    };
    const BUF: usize = 64 * 1024;
    let (conn, _) = establish(Some(PEER_SHIFT));
    tcp::set_rcvbuf(conn.id, BUF);
    let snd_nxt = with_data_state!(conn.id, |d| d.snd_nxt.raw());
    let mut seq = conn.peer_iss.wrapping_add(1);
    let segment = KVec::<u8>::zeroed(1024).expect("test alloc");
    for _ in 0..BUF / 1024 {
        if let Err(msg) = feed(&conn, seq, snd_nxt, segment.as_slice()) {
            return fail!("{}", msg);
        }
        seq = seq.wrapping_add(1024);
    }
    let closed = with_data_state!(conn.id, |d| d.rcv_wnd);
    assert_eq_test!(closed, 0, "the handshake's 64 KiB grant is used up");

    let mut out = KVec::<u8>::zeroed(BUF).expect("test alloc");
    assert_eq_test!(
        tcp::recv(conn.id, &mut out.as_mut_slice()[..100]),
        Ok(100),
        "a nibble"
    );
    assert_test!(
        tcp::window_update(conn.id).is_none(),
        "a hundred bytes is not worth an update"
    );
    assert_eq_test!(
        tcp::recv(conn.id, out.as_mut_slice()),
        Ok(BUF - 100),
        "the rest"
    );
    let Some(update) = tcp::window_update(conn.id) else {
        return fail!("draining a closed window sent no update");
    };
    let advertised = with_data_state!(conn.id, |d| d.rcv_wnd);
    assert_eq_test!(advertised as usize, BUF, "the whole buffer is open again");
    assert_eq_test!(
        update.window_size as usize,
        BUF >> our_window_scale(),
        "and the update says so, scaled"
    );
    assert_test!(
        tcp::window_update(conn.id).is_none(),
        "one update per reopening"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_tcp_syn_offers_the_ceiling_scale,
    suite = tcp_window
);
slopos_testing::stest!(
    name = test_tcp_read_reopening_the_window_sends_an_update,
    suite = tcp_window
);
slopos_testing::stest!(
    name = test_tcp_active_open_scales_once_both_offered,
    suite = tcp_window
);
slopos_testing::stest!(
    name = test_tcp_active_open_unscaled_without_peer_offer,
    suite = tcp_window
);
slopos_testing::stest!(name = test_tcp_receive_window_past_64k, suite = tcp_window);
slopos_testing::stest!(name = test_tcp_send_buffer_past_64k, suite = tcp_window);
slopos_testing::stest!(
    name = test_tcp_rcvbuf_set_before_buffers_exist,
    suite = tcp_window
);
slopos_testing::stest!(
    name = test_tcp_passive_open_scales_only_when_offered,
    suite = tcp_window
);
slopos_testing::stest!(
    name = test_tcp_window_bounded_by_the_machine,
    suite = tcp_window
);
