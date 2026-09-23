//! Tests that exercise the live NIC, its DHCP lease and the QEMU SLIRP peer.
//! They assert against the live stack's own tables and install no topology.

use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::iface::{self, IfaceKind};
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::DEVICE_REGISTRY;
use crate::route::{ROUTE_TABLE, RouteEntry};
use crate::socket;
use crate::tcp;
use crate::tests::env_wait::await_env;
use crate::tests::env_wait::errno_i32;
use crate::types::{DevIndex, Ipv4Addr};

const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const GATEWAY_PORT: u16 = 7;

/// Bound on a wait that would otherwise not end, not a budget for the exchange.
const ENV_FAILSAFE_MS: u64 = 2_000;

/// How long each pass leaves the peer alone before draining the NIC again.
const POLL_INTERVAL_MS: u32 = 1;

fn nic_dev() -> Option<DevIndex> {
    let mut found = None;
    iface::for_each(|i| {
        if found.is_none() && i.kind == IfaceKind::Ethernet {
            found = Some(i.dev);
        }
    });
    found
}

/// The lease is applied after the DHCP client drops its own lock, so the
/// address — not the `Bound` state — is what says the interface is configured.
fn await_dhcp_addr(dev: DevIndex) -> Option<(Ipv4Addr, u64)> {
    await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        iface::our_ip(dev).filter(|ip| !ip.is_unspecified())
    })
}

fn default_route_on(dev: DevIndex) -> Option<RouteEntry> {
    let routes = ROUTE_TABLE.all_routes();
    routes
        .iter()
        .copied()
        .find(|r| r.prefix_len == 0 && r.dev == dev)
}

fn test_route_table_has_default() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };

    let Some((route, waited)) =
        await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || default_route_on(dev))
    else {
        return fail!(
            "dev {} has no default route after {}ms (DHCP state {:?}) — the environment's DHCP server did not answer",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };
    klog_info!("tcp_live: default route {:?} after {}ms", route, waited);

    assert_test!(
        !route.gateway.is_unspecified(),
        "the default route on dev {} is directly connected, so nothing is reachable off-link: {:?}",
        dev,
        route
    );
    pass!()
}

fn test_iface_has_ipv4() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };

    let Some((addr, waited)) = await_dhcp_addr(dev) else {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — the environment's DHCP server did not answer",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };
    klog_info!(
        "tcp_live: our_ipv4={} on dev {} after {}ms",
        addr,
        dev,
        waited
    );

    assert_test!(
        !addr.is_loopback(),
        "the Ethernet interface took loopback address {}",
        addr
    );
    assert_eq_test!(
        iface::first_ipv4(),
        Some(addr),
        "first_ipv4 disagrees with the NIC's own address"
    );
    pass!()
}

fn test_arp_resolve_gateway() -> TestResult {
    let gw_ip = Ipv4Addr(GATEWAY_IP);

    let Some((dev, next_hop)) = ROUTE_TABLE.lookup(gw_ip) else {
        return fail!("no route to gateway {}", gw_ip);
    };
    klog_info!(
        "tcp_live: route to {} -> dev={} next_hop={}",
        gw_ip,
        dev,
        next_hop
    );

    if let Some(mac) = NEIGHBOR_CACHE.lookup(dev, next_hop) {
        klog_info!("tcp_live: {} already cached as {}", next_hop, mac);
        return pass!();
    }

    let Some(before) = DEVICE_REGISTRY.stats_by_index(dev) else {
        return fail!(
            "the route names dev {}, which the device registry does not hold",
            dev
        );
    };
    crate::arp::send_request_via_registry(dev, next_hop);
    let after = DEVICE_REGISTRY.stats_by_index(dev).unwrap_or(before);

    // Other CPUs transmit on this device too, so the counter is a floor.
    assert_test!(
        after.tx_packets > before.tx_packets,
        "dev {} transmitted nothing for an ARP request for {}",
        dev,
        next_hop
    );

    let Some((mac, waited)) = await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        NEIGHBOR_CACHE.lookup(dev, next_hop)
    }) else {
        return fail!(
            "{} did not answer ARP on dev {} within {}ms — the environment's gateway is not responding",
            next_hop,
            dev,
            ENV_FAILSAFE_MS
        );
    };
    klog_info!(
        "tcp_live: {} resolved to {} after {}ms",
        next_hop,
        mac,
        waited
    );
    pass!()
}

/// An external destination must take its source IP from the NIC.
///
/// `first_ipv4()` returns registration order and loopback registers before any
/// NIC, so sourcing that way sends external traffic with `src_ip = 127.0.0.1`,
/// whose replies QEMU SLIRP's TCP forwarder drops.
fn test_source_ip_for_external_uses_nic() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };
    let Some((nic_addr, _)) = await_dhcp_addr(dev) else {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — source selection has nothing to pick",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };

    let external = Ipv4Addr(GATEWAY_IP);
    let Some(src) = iface::source_ip_for(external) else {
        return fail!("source_ip_for({}) returned None", external);
    };
    klog_info!("tcp_live: source_ip_for({}) -> {}", external, src);

    assert_eq_test!(
        src,
        nic_addr,
        "source_ip_for(external dst) must pick the NIC's address; outbound TCP sourced from anywhere else never receives replies"
    );
    pass!()
}

/// Loopback destinations must still resolve through the loopback interface:
/// `source_ip_for` is route-aware, not blanket-blacklisting loopback.
fn test_source_ip_for_loopback_uses_loopback() -> TestResult {
    let lo = Ipv4Addr([127, 0, 0, 1]);
    let src = match iface::source_ip_for(lo) {
        Some(ip) => ip,
        None => return fail!("source_ip_for(127.0.0.1) returned None"),
    };
    klog_info!("tcp_live: source_ip_for({}) -> {}", lo, src);
    assert_test!(
        src.is_loopback(),
        "source_ip_for(loopback dst) returned {} — loopback traffic should use 127.0.0.0/8 source",
        src
    );
    pass!()
}

fn test_tcp_syn_transmit() -> TestResult {
    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };
    let Some((our_ip, _)) = await_dhcp_addr(dev) else {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — a SYN has no source address",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    };

    let (tcp_id, syn) = match tcp::connect(our_ip.0, GATEWAY_IP, GATEWAY_PORT) {
        Ok(r) => r,
        Err(e) => return fail!("tcp_connect failed: {:?}", e),
    };

    klog_info!(
        "tcp_live: SYN built id={} seq={} local_port={}",
        tcp_id,
        syn.seq_num,
        syn.tuple.local_port,
    );

    let send_rc = socket::socket_send_tcp_segment(&syn, &[]);
    klog_info!("tcp_live: send_tcp_segment returned {}", send_rc);

    let _ = tcp::abort(tcp_id);

    assert_test!(send_rc == 0, "send_tcp_segment failed with {}", send_rc);
    pass!()
}

fn test_tcp_nonblocking_connect_returns_einprogress() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};
    use slopos_abi::syscall::ERRNO_EINPROGRESS;

    let Some(dev) = nic_dev() else {
        return fail!("no Ethernet interface is attached");
    };
    if await_dhcp_addr(dev).is_none() {
        return fail!(
            "dev {} has no IPv4 address after {}ms (DHCP state {:?}) — connect has no source address",
            dev,
            ENV_FAILSAFE_MS,
            crate::dhcp::state_of(dev)
        );
    }

    let sock_fd = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if sock_fd < 0 {
        return fail!("socket_create failed: {}", sock_fd);
    }
    let sock_idx = sock_fd as u32;
    let _ = socket::socket_set_nonblocking(sock_idx, true);

    let rc = socket::socket_connect(sock_idx, GATEWAY_IP, GATEWAY_PORT);
    klog_info!("tcp_live: nonblocking connect returned {}", rc);

    let _ = socket::socket_close(sock_idx);

    assert_test!(
        rc == 0 || rc == errno_i32(ERRNO_EINPROGRESS),
        "nonblocking connect: expected 0 or EINPROGRESS, got {}",
        rc,
    );
    pass!()
}

/// A full loopback handshake through the live device, on a **wildcard** bind.
///
/// This is the shape `nc -l` installs and the one that was broken: the SYN-ACK
/// was sourced from `0.0.0.0`, so the client's PCB was never found and the
/// connection sat in `SYN_SENT` while a RST leaked out of the default route.
/// Covers the loopback drain too — nothing else delivers `lo` traffic.
fn test_loopback_wildcard_handshake_completes() -> TestResult {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};

    const PORT: u16 = 18080;

    let listener = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if listener < 0 {
        return fail!("socket_create(listener) failed: {}", listener);
    }
    let listener = listener as u32;

    let rc = socket::socket_bind(listener, [0, 0, 0, 0], PORT);
    if rc != 0 {
        let _ = socket::socket_close(listener);
        return fail!("bind(0.0.0.0:{}) failed: {}", PORT, rc);
    }
    let rc = socket::socket_listen(listener, 4);
    if rc != 0 {
        let _ = socket::socket_close(listener);
        return fail!("listen failed: {}", rc);
    }

    let client = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if client < 0 {
        let _ = socket::socket_close(listener);
        return fail!("socket_create(client) failed: {}", client);
    }
    let client = client as u32;
    let _ = socket::socket_set_nonblocking(client, true);
    let _ = socket::socket_connect(client, [127, 0, 0, 1], PORT);

    let established = await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        socket::socket_lookup_tcp_idx(client)
            .and_then(tcp::get_state)
            .filter(|s| *s == tcp::TcpState::Established)
    });

    let accepted = established.and_then(|_| {
        await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
            let rc = socket::socket_accept(listener, core::ptr::null_mut(), core::ptr::null_mut());
            (rc >= 0).then_some(rc)
        })
    });

    if let Some((fd, _)) = accepted {
        let _ = socket::socket_close(fd as u32);
    }
    let _ = socket::socket_close(client);
    let _ = socket::socket_close(listener);

    let Some((_, waited)) = established else {
        return fail!(
            "a loopback connect to a wildcard listener did not reach ESTABLISHED within {}ms",
            ENV_FAILSAFE_MS
        );
    };
    klog_info!(
        "tcp_live: loopback wildcard handshake completed in {}ms",
        waited
    );

    assert_test!(
        accepted.is_some(),
        "the handshake completed but accept() never yielded the connection"
    );
    pass!()
}

fn pattern_byte(i: usize) -> u8 {
    (i.wrapping_mul(131) ^ (i >> 9)) as u8
}

struct Sock(u32);

impl Drop for Sock {
    fn drop(&mut self) {
        let _ = socket::socket_close(self.0);
    }
}

fn stream_socket() -> Result<Sock, TestResult> {
    use slopos_abi::net::{AF_INET, SOCK_STREAM};
    let fd = socket::socket_create(AF_INET, SOCK_STREAM, 0, socket::SocketOwner::UNOWNED);
    if fd < 0 {
        return Err(fail!("socket_create: {}", fd));
    }
    Ok(Sock(fd as u32))
}

/// Fields drop in declaration order, so the listener closes last.
struct Loopback {
    server: Sock,
    client: Sock,
    _listener: Sock,
}

fn loopback_pair() -> Result<Loopback, TestResult> {
    let listener = stream_socket()?;
    let client = stream_socket()?;
    if socket::socket_bind(listener.0, [127, 0, 0, 1], 0) != 0
        || socket::socket_listen(listener.0, 4) != 0
    {
        return Err(fail!("listen on 127.0.0.1:0 failed"));
    }
    let port = socket::socket_get_local_addr(listener.0).map_or(0, |a| a.port.0);
    if port < 49_152 {
        return Err(fail!(
            "a listener bound to port 0 reports port {}, not an ephemeral one",
            port
        ));
    }
    let _ = socket::socket_set_nonblocking(client.0, true);
    let _ = socket::socket_connect(client.0, [127, 0, 0, 1], port);
    let Some((server, _)) = await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        let rc = socket::socket_accept(listener.0, core::ptr::null_mut(), core::ptr::null_mut());
        (rc >= 0).then_some(Sock(rc as u32))
    }) else {
        return Err(fail!("the loopback connection was never accepted"));
    };
    let _ = socket::socket_set_nonblocking(server.0, true);
    Ok(Loopback {
        server,
        client,
        _listener: listener,
    })
}

/// More than an unscaled window carries, and the reader holds off until the
/// sender stalls, so the window update that reopens the send must be heard.
fn test_loopback_bulk_transfer() -> TestResult {
    use slopos_abi::syscall::{SO_RCVBUF, SO_SNDBUF, SOL_SOCKET};
    use slopos_ostd::KVec;

    const TOTAL: usize = 1 << 20;
    const STALL_MS: u64 = 5_000;
    const BUF: usize = 64 * 1024;
    const HOLD_MS: u64 = 200;

    let Loopback {
        server,
        client,
        _listener,
    } = match loopback_pair() {
        Ok(pair) => pair,
        Err(result) => return result,
    };
    let (Ok(mut out), Ok(mut inbox)) =
        (KVec::<u8>::zeroed(16 * 1024), KVec::<u8>::zeroed(64 * 1024))
    else {
        return fail!("test buffers");
    };
    let buf = (BUF as i32).to_ne_bytes();
    let _ = socket::socket_setsockopt(server.0, SOL_SOCKET, SO_RCVBUF, &buf);
    let _ = socket::socket_setsockopt(client.0, SOL_SOCKET, SO_SNDBUF, &buf);
    let (mut sent, mut received) = (0usize, 0usize);
    let mut last_progress = slopos_kernel_services::clock::uptime_ms();
    let mut last_send = last_progress;
    let mut held_at = None;
    let mut corrupt = None;
    let finished = await_env(60_000, POLL_INTERVAL_MS, || {
        let before = (sent, received);
        let now = slopos_kernel_services::clock::uptime_ms();
        if sent < TOTAL {
            let n = (TOTAL - sent).min(out.len());
            for (i, b) in out.as_mut_slice()[..n].iter_mut().enumerate() {
                *b = pattern_byte(sent + i);
            }
            let rc = socket::socket_send(client.0, &out.as_slice()[..n]);
            if rc > 0 {
                sent += rc as usize;
                last_send = now;
            }
        }
        if held_at.is_none() {
            if now - last_send < HOLD_MS {
                return None;
            }
            held_at = Some(sent);
        }
        loop {
            let rc = socket::socket_recv(server.0, inbox.as_mut_slice());
            if rc <= 0 {
                break;
            }
            for (i, b) in inbox.as_slice()[..rc as usize].iter().enumerate() {
                if corrupt.is_none() && *b != pattern_byte(received + i) {
                    corrupt = Some(received + i);
                }
            }
            received += rc as usize;
        }
        if (sent, received) != before {
            last_progress = now;
        }
        (received >= TOTAL || corrupt.is_some() || now - last_progress > STALL_MS).then_some(())
    });
    if held_at.is_none_or(|at| at >= TOTAL) {
        return fail!("the sender never stalled on a shut window: {:?}", held_at);
    }
    let client_tcp = socket::socket_lookup_tcp_idx(client.0);
    let server_tcp = socket::socket_lookup_tcp_idx(server.0);
    let diag = (
        client_tcp.map(tcp::send_buffer_space),
        client_tcp.and_then(tcp::get_state),
        server_tcp.map(tcp::recv_available),
        crate::tcp::chunk::live_chunks(),
    );
    if let Some(at) = corrupt {
        return fail!("byte {} arrived altered", at);
    }
    if finished.is_none() || received < TOTAL {
        return fail!(
            "the transfer stalled: sent {} received {} of {}; client send space, state, server unread, live chunks = {:?}",
            sent,
            received,
            TOTAL,
            diag
        );
    }
    pass!()
}

fn test_time_wait_keeps_unread_bytes() -> TestResult {
    use slopos_abi::syscall::SHUT_WR;
    use slopos_ostd::KVec;

    const TOTAL: usize = 4096;

    let Loopback {
        server,
        client,
        _listener,
    } = match loopback_pair() {
        Ok(pair) => pair,
        Err(result) => return result,
    };
    let Some(client_tcp) = socket::socket_lookup_tcp_idx(client.0) else {
        return fail!("the client has no connection");
    };
    let (Ok(mut payload), Ok(mut got)) = (KVec::<u8>::zeroed(TOTAL), KVec::<u8>::zeroed(TOTAL))
    else {
        return fail!("test buffers");
    };
    for (i, b) in payload.as_mut_slice().iter_mut().enumerate() {
        *b = pattern_byte(i);
    }
    if socket::socket_shutdown(client.0, SHUT_WR) != 0 {
        return fail!("shutdown(SHUT_WR) failed");
    }
    let mut probe = [0u8; 16];
    if await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        (socket::socket_recv(server.0, &mut probe) == 0).then_some(())
    })
    .is_none()
    {
        return fail!("the server never read the client's FIN");
    }
    if socket::socket_send(server.0, payload.as_slice()) != TOTAL as i64 {
        return fail!("the server could not queue {} bytes", TOTAL);
    }
    drop(server);
    if await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        (tcp::get_state(client_tcp) == Some(tcp::TcpState::TimeWait)).then_some(())
    })
    .is_none()
    {
        return fail!(
            "the client never reached TIME_WAIT: {:?}",
            tcp::get_state(client_tcp)
        );
    }
    let mut received = 0usize;
    while received < TOTAL {
        let rc = socket::socket_recv(client.0, &mut got.as_mut_slice()[received..]);
        if rc <= 0 {
            break;
        }
        received += rc as usize;
    }
    assert_eq_test!(received, TOTAL, "bytes read after the FIN");
    assert_test!(
        got.as_slice() == payload.as_slice(),
        "the bytes arrived altered"
    );
    assert_eq_test!(
        socket::socket_recv(client.0, &mut probe),
        0,
        "a drained TIME_WAIT socket reads EOF"
    );
    pass!()
}

fn test_last_ack_keeps_unread_bytes() -> TestResult {
    use slopos_abi::syscall::SHUT_WR;

    const PAYLOAD: &[u8] = b"the reply the server sent before its FIN";

    let Loopback {
        server,
        client,
        _listener,
    } = match loopback_pair() {
        Ok(pair) => pair,
        Err(result) => return result,
    };
    let Some(client_tcp) = socket::socket_lookup_tcp_idx(client.0) else {
        return fail!("the client has no connection");
    };
    if socket::socket_send(server.0, PAYLOAD) != PAYLOAD.len() as i64
        || socket::socket_shutdown(server.0, SHUT_WR) != 0
    {
        return fail!("the server could not send and half-close");
    }
    if await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        tcp::is_peer_closed(client_tcp).then_some(())
    })
    .is_none()
    {
        return fail!("the server's FIN never arrived");
    }
    if socket::socket_shutdown(client.0, SHUT_WR) != 0 {
        return fail!("the client could not half-close");
    }
    if await_env(ENV_FAILSAFE_MS, POLL_INTERVAL_MS, || {
        (tcp::get_state(client_tcp) != Some(tcp::TcpState::LastAck)).then_some(())
    })
    .is_none()
    {
        return fail!("the client's FIN was never acknowledged");
    }
    tcp::on_time_wait_expire(client_tcp.raw());
    assert_test!(
        tcp::with_pcb(client_tcp, |pcb| {
            matches!(&pcb.state, tcp::PcbState::TimeWait(tw) if tw.expire_token.is_some())
        }) == Some(true),
        "the expiry is armed again while the reader is owed bytes"
    );
    let mut got = [0u8; 64];
    let rc = socket::socket_recv(client.0, &mut got);
    assert_eq_test!(rc, PAYLOAD.len() as i64, "the unread reply survived");
    assert_test!(
        &got[..PAYLOAD.len()] == PAYLOAD,
        "the reply arrived altered"
    );
    assert_eq_test!(socket::socket_recv(client.0, &mut got), 0, "then EOF");
    pass!()
}

fn timed_out_pair() -> Result<Loopback, TestResult> {
    let pair = loopback_pair()?;
    let Some(client_tcp) = socket::socket_lookup_tcp_idx(pair.client.0) else {
        return Err(fail!("the client has no connection"));
    };
    tcp::with_pcb_mut(client_tcp, |pcb| {
        if let tcp::PcbState::Data(d) = &mut pcb.state {
            d.keepalive_probes_sent = u8::MAX;
        }
    });
    match tcp::on_keepalive(client_tcp.raw()) {
        action @ tcp::RetransmitAction::GaveUp(None) => {
            crate::timer::dispatch_retransmit_action(client_tcp.raw(), action)
        }
        _ => return Err(fail!("keepalive did not give up on an unanswered peer")),
    }
    Ok(pair)
}

fn test_a_connection_timers_give_up_on_reports_etimedout() -> TestResult {
    use slopos_abi::syscall::{ERRNO_EPIPE, ERRNO_ETIMEDOUT};

    let Loopback {
        server: _server,
        client,
        _listener,
    } = match timed_out_pair() {
        Ok(pair) => pair,
        Err(result) => return result,
    };
    let mut buf = [0u8; 8];
    assert_eq_test!(
        socket::socket_recv(client.0, &mut buf),
        ERRNO_ETIMEDOUT as i64,
        "the first call reports the timeout"
    );
    assert_eq_test!(
        socket::socket_send(client.0, b"x"),
        ERRNO_EPIPE as i64,
        "and only the first; a write then finds the pipe broken"
    );
    assert_eq_test!(
        socket::socket_recv(client.0, &mut buf),
        0,
        "and a read finds the end"
    );
    pass!()
}

/// `SO_ERROR` is a positive errno, as POSIX has it, and is taken by the read.
fn test_so_error_reports_the_timeout_once() -> TestResult {
    use slopos_abi::syscall::{ERRNO_ETIMEDOUT, SO_ERROR, SOL_SOCKET};

    let Loopback {
        server: _server,
        client,
        _listener,
    } = match timed_out_pair() {
        Ok(pair) => pair,
        Err(result) => return result,
    };
    let mut err = [0u8; 4];
    assert_eq_test!(
        socket::socket_getsockopt(client.0, SOL_SOCKET, SO_ERROR, &mut err),
        4
    );
    assert_eq_test!(i32::from_ne_bytes(err), -(ERRNO_ETIMEDOUT as i32));
    let _ = socket::socket_getsockopt(client.0, SOL_SOCKET, SO_ERROR, &mut err);
    assert_eq_test!(i32::from_ne_bytes(err), 0, "and a second read finds none");
    pass!()
}

slopos_testing::stest!(name = test_route_table_has_default, suite = tcp_live);
slopos_testing::stest!(name = test_iface_has_ipv4, suite = tcp_live);
slopos_testing::stest!(name = test_arp_resolve_gateway, suite = tcp_live);
slopos_testing::stest!(
    name = test_source_ip_for_external_uses_nic,
    suite = tcp_live
);
slopos_testing::stest!(
    name = test_source_ip_for_loopback_uses_loopback,
    suite = tcp_live
);
slopos_testing::stest!(name = test_tcp_syn_transmit, suite = tcp_live);
slopos_testing::stest!(
    name = test_tcp_nonblocking_connect_returns_einprogress,
    suite = tcp_live
);
slopos_testing::stest!(
    name = test_loopback_wildcard_handshake_completes,
    suite = tcp_live
);
slopos_testing::stest!(name = test_loopback_bulk_transfer, suite = tcp_live);
slopos_testing::stest!(name = test_time_wait_keeps_unread_bytes, suite = tcp_live);
slopos_testing::stest!(name = test_last_ack_keeps_unread_bytes, suite = tcp_live);
slopos_testing::stest!(
    name = test_a_connection_timers_give_up_on_reports_etimedout,
    suite = tcp_live
);
slopos_testing::stest!(
    name = test_so_error_reports_the_timeout_once,
    suite = tcp_live
);
