//! Tests for bringing a NIC into service through `nic::publish`, as every NIC
//! driver does.
//!
//! Each test publishes its own mock device and retires it before returning.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use slopos_abi::net::{AF_INET, NET_DHCP_SELECTING, SOCK_DGRAM, SOCK_STREAM};
use slopos_abi::syscall::{ERRNO_EINPROGRESS, IP_TOS, IPPROTO_IP, POLLIN};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_ostd::{KArc, KVec};

use crate::dhcp::codec::{DHCP_FRAME_LEN, MSG_ACK, MSG_OFFER, UDP_PORT_CLIENT, UDP_PORT_SERVER};
use crate::iface::{self, AddrOrigin, AddrScope, IfaceAddr};
use crate::ingress;
use crate::neighbor::NEIGHBOR_CACHE;
use crate::netdev::{DEVICE_REGISTRY, DeviceHandle, NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::nic;
use crate::packetbuf::PacketBuf;
use crate::pool::PacketPool;
use crate::route::{self, RouteEntry};
use crate::socket::{
    SocketOwner, socket_bind, socket_close, socket_connect, socket_create, socket_getsockopt,
    socket_poll_readable, socket_send, socket_set_nonblocking, socket_setsockopt,
};
use crate::tests::dhcp_transport_tests::{self, CLIENT_IP, SERVER};
use crate::tests::env_wait::errno_i32;
use crate::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

const PROBE_PORT: u16 = 40_999;
const TCP_PROBE_PORT: u16 = 40_998;
const ETH_LEN: usize = 14;
const IP_LEN: usize = 20;

struct MockNic {
    mac: MacAddr,
    polls: AtomicUsize,
    dhcp_frames: AtomicUsize,
    probe_seen: AtomicBool,
    probe_src: AtomicU64,
    probe_dst: AtomicU64,
    arp_request_seen: AtomicBool,
    arp_sender_ip: AtomicU32,
    arp_replies: AtomicUsize,
    echo_replies: AtomicUsize,
    probe_tos: AtomicU8,
    tcp_seen: AtomicBool,
    tcp_tos: AtomicU8,
    ip_csum_bad: AtomicBool,
}

impl MockNic {
    fn new(mac: MacAddr) -> Self {
        Self {
            mac,
            polls: AtomicUsize::new(0),
            dhcp_frames: AtomicUsize::new(0),
            probe_seen: AtomicBool::new(false),
            probe_src: AtomicU64::new(0),
            probe_dst: AtomicU64::new(0),
            arp_request_seen: AtomicBool::new(false),
            arp_sender_ip: AtomicU32::new(0),
            arp_replies: AtomicUsize::new(0),
            echo_replies: AtomicUsize::new(0),
            probe_tos: AtomicU8::new(0),
            tcp_seen: AtomicBool::new(false),
            tcp_tos: AtomicU8::new(0),
            ip_csum_bad: AtomicBool::new(false),
        }
    }

    fn probe(&self) -> Option<(MacAddr, MacAddr)> {
        self.probe_seen.load(Ordering::Acquire).then(|| {
            (
                unpack(self.probe_src.load(Ordering::Relaxed)),
                unpack(self.probe_dst.load(Ordering::Relaxed)),
            )
        })
    }

    fn arp_request_sender(&self) -> Option<Ipv4Addr> {
        self.arp_request_seen
            .load(Ordering::Acquire)
            .then(|| Ipv4Addr(self.arp_sender_ip.load(Ordering::Relaxed).to_be_bytes()))
    }

    /// The TOS byte of the latest segment sent to `TCP_PROBE_PORT`.
    fn tcp_probe_tos(&self) -> Option<u8> {
        self.tcp_seen
            .load(Ordering::Acquire)
            .then(|| self.tcp_tos.load(Ordering::Relaxed))
    }
}

fn pack(mac: &[u8]) -> u64 {
    mac.iter().fold(0, |acc, b| (acc << 8) | *b as u64)
}

fn unpack(packed: u64) -> MacAddr {
    let bytes = packed.to_be_bytes();
    MacAddr([bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
}

/// The UDP destination port of an Ethernet + IPv4 (no options) + UDP frame.
fn udp_dst_port(frame: &[u8]) -> Option<u16> {
    const L2: usize = 14;
    const L3: usize = 20;
    if frame.len() < L2 + L3 + 8 || frame[12..14] != [0x08, 0x00] || frame[L2 + 9] != 17 {
        return None;
    }
    Some(u16::from_be_bytes([frame[L2 + L3 + 2], frame[L2 + L3 + 3]]))
}

/// The TCP destination port of an Ethernet + IPv4 (no options) + TCP frame.
fn tcp_dst_port(frame: &[u8]) -> Option<u16> {
    if frame.len() < ETH_LEN + IP_LEN + 20
        || frame[12..14] != [0x08, 0x00]
        || frame[ETH_LEN + 9] != 6
    {
        return None;
    }
    Some(u16::from_be_bytes([
        frame[ETH_LEN + IP_LEN + 2],
        frame[ETH_LEN + IP_LEN + 3],
    ]))
}

/// The TOS byte of an IPv4 frame and whether its header checksum holds.
fn ipv4_tos(frame: &[u8]) -> (u8, bool) {
    let header = &frame[ETH_LEN..ETH_LEN + IP_LEN];
    (header[1], crate::checksum::internet_checksum(header) == 0)
}

/// The sender protocol address of an Ethernet ARP request.
fn arp_request_sender_ip(frame: &[u8]) -> Option<[u8; 4]> {
    const L2: usize = 14;
    if frame.len() < L2 + 28 || frame[12..14] != [0x08, 0x06] || frame[L2 + 6..L2 + 8] != [0, 1] {
        return None;
    }
    Some([
        frame[L2 + 14],
        frame[L2 + 15],
        frame[L2 + 16],
        frame[L2 + 17],
    ])
}

fn is_arp_reply(frame: &[u8]) -> bool {
    frame.len() >= ETH_LEN + 28
        && frame[12..14] == [0x08, 0x06]
        && frame[ETH_LEN + 6..ETH_LEN + 8] == [0, 2]
}

fn is_echo_reply(frame: &[u8]) -> bool {
    frame.len() >= ETH_LEN + IP_LEN + 8
        && frame[12..14] == [0x08, 0x00]
        && frame[ETH_LEN + 9] == 1
        && frame[ETH_LEN + IP_LEN] == 0
}

impl NetDevice for MockNic {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        let frame = pkt.payload();
        match udp_dst_port(frame) {
            Some(UDP_PORT_SERVER) => {
                self.dhcp_frames.fetch_add(1, Ordering::Relaxed);
            }
            Some(PROBE_PORT) => {
                let (tos, csum_ok) = ipv4_tos(frame);
                self.probe_tos.store(tos, Ordering::Relaxed);
                self.ip_csum_bad.fetch_or(!csum_ok, Ordering::Relaxed);
                self.probe_dst.store(pack(&frame[0..6]), Ordering::Relaxed);
                self.probe_src.store(pack(&frame[6..12]), Ordering::Relaxed);
                self.probe_seen.store(true, Ordering::Release);
            }
            _ => {}
        }
        if tcp_dst_port(frame) == Some(TCP_PROBE_PORT) {
            let (tos, csum_ok) = ipv4_tos(frame);
            self.tcp_tos.store(tos, Ordering::Relaxed);
            self.ip_csum_bad.fetch_or(!csum_ok, Ordering::Relaxed);
            self.tcp_seen.store(true, Ordering::Release);
        }
        if let Some(ip) = arp_request_sender_ip(frame) {
            self.arp_sender_ip
                .store(u32::from_be_bytes(ip), Ordering::Relaxed);
            self.arp_request_seen.store(true, Ordering::Release);
        }
        if is_arp_reply(frame) {
            self.arp_replies.fetch_add(1, Ordering::Relaxed);
        }
        if is_echo_reply(frame) {
            self.echo_replies.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        KVec::new()
    }
    fn set_up(&self) {}
    fn set_down(&self) {}
    fn mtu(&self) -> u16 {
        1500
    }
    fn mac(&self) -> MacAddr {
        self.mac
    }
    fn stats(&self) -> NetDeviceStats {
        NetDeviceStats::new()
    }
    fn features(&self) -> NetDeviceFeatures {
        NetDeviceFeatures::empty()
    }
}

fn publish(mac: MacAddr) -> Option<(DevIndex, KArc<MockNic>)> {
    let mock = KArc::try_new(MockNic::new(mac)).ok()?;
    let dev: KArc<dyn NetDevice + Send + Sync> = mock.clone();
    Some((nic::publish(dev)?, mock))
}

fn add_address(dev: DevIndex, addr: [u8; 4]) -> bool {
    iface::get_by_dev(dev).is_some_and(|row| {
        iface::add_addr(
            row.ifindex,
            IfaceAddr::permanent(Ipv4Addr(addr), 24, AddrScope::Global, AddrOrigin::Static),
        )
        .is_ok()
    })
}

const LAN_LOCAL: [u8; 4] = [10, 96, 0, 1];
const LAN_PEER: [u8; 4] = [10, 96, 0, 2];
const LAN_STRANGER: [u8; 4] = [10, 96, 0, 9];
const LAN_PEER_MAC: MacAddr = MacAddr([2, 0, 0, 0, 0x61, 0xfd]);

/// A published mock NIC holding `LAN_LOCAL/24`, with a route and a resolved
/// neighbour for `LAN_PEER` so replies leave through it. Retired on drop.
struct Lan {
    dev: DevIndex,
    mock: KArc<MockNic>,
    handle: KArc<DeviceHandle>,
}

impl Lan {
    fn up(mac: MacAddr) -> Option<Self> {
        let (dev, mock) = publish(mac)?;
        let Some(handle) = nic::handle(dev) else {
            nic::retire(dev);
            return None;
        };
        let lan = Self { dev, mock, handle };
        let routed = route::add(RouteEntry {
            prefix: Ipv4Addr([10, 96, 0, 0]),
            prefix_len: 24,
            gateway: Ipv4Addr::UNSPECIFIED,
            dev,
            metric: 0,
        });
        if !add_address(dev, LAN_LOCAL) || !routed {
            return None;
        }
        let _ = NEIGHBOR_CACHE.insert_or_update(
            dev,
            Ipv4Addr(LAN_PEER),
            LAN_PEER_MAC,
            crate::clock::now_ms(),
        );
        Some(lan)
    }

    fn inject(&self, frame: &[u8]) {
        if let Some(pkt) = PacketBuf::from_raw_copy(frame) {
            ingress::net_rx_injected(&self.handle, pkt);
        }
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        nic::retire(self.dev);
    }
}

const FRAME_CAP: usize = 96;

fn ipv4_frame<const CAP: usize>(
    dst_mac: MacAddr,
    proto: u8,
    src: [u8; 4],
    dst: [u8; 4],
    l4: &[u8],
) -> ([u8; CAP], usize) {
    let mut frame = [0u8; CAP];
    let len = ETH_LEN + IP_LEN + l4.len();
    frame[0..6].copy_from_slice(&dst_mac.0);
    frame[6..12].copy_from_slice(&LAN_PEER_MAC.0);
    frame[12..14].copy_from_slice(&[0x08, 0x00]);
    let ip = &mut frame[ETH_LEN..ETH_LEN + IP_LEN];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((IP_LEN + l4.len()) as u16).to_be_bytes());
    ip[8] = 64;
    ip[9] = proto;
    ip[12..16].copy_from_slice(&src);
    ip[16..20].copy_from_slice(&dst);
    let csum = crate::checksum::internet_checksum(ip);
    ip[10..12].copy_from_slice(&csum.to_be_bytes());
    frame[ETH_LEN + IP_LEN..len].copy_from_slice(l4);
    (frame, len)
}

fn udp_frame(
    dst_mac: MacAddr,
    src: ([u8; 4], u16),
    dst: ([u8; 4], u16),
) -> ([u8; FRAME_CAP], usize) {
    const UDP_LEN: u16 = 12;
    let mut udp = [0u8; UDP_LEN as usize];
    udp[0..2].copy_from_slice(&src.1.to_be_bytes());
    udp[2..4].copy_from_slice(&dst.1.to_be_bytes());
    udp[4..6].copy_from_slice(&UDP_LEN.to_be_bytes());
    udp[8..].copy_from_slice(&[0x5a; 4]);
    ipv4_frame(dst_mac, 17, src.0, dst.0, &udp)
}

fn echo_request_frame(dst_mac: MacAddr, dst: [u8; 4]) -> ([u8; FRAME_CAP], usize) {
    let mut icmp = [0u8; 12];
    icmp[0] = 8;
    icmp[4..6].copy_from_slice(&0x6196u16.to_be_bytes());
    icmp[6..8].copy_from_slice(&1u16.to_be_bytes());
    icmp[8..].copy_from_slice(&[0xec; 4]);
    let csum = crate::checksum::internet_checksum(&icmp);
    icmp[2..4].copy_from_slice(&csum.to_be_bytes());
    ipv4_frame(dst_mac, 1, LAN_PEER, dst, &icmp)
}

const UDP_HEADER_LEN: usize = 8;
const DHCP_REPLY_CAP: usize = ETH_LEN + IP_LEN + UDP_HEADER_LEN + DHCP_FRAME_LEN;

/// The server's reply to `dev`'s transaction in flight, unicast to the
/// address it offers rather than broadcast.
#[inline(never)]
fn unicast_dhcp_reply(
    dst_mac: MacAddr,
    dev: DevIndex,
    msg_type: u8,
    lease: Option<u32>,
) -> Option<([u8; DHCP_REPLY_CAP], usize)> {
    let xid = crate::dhcp::transport::xid_of(dev)?;
    let mut udp = [0u8; UDP_HEADER_LEN + DHCP_FRAME_LEN];
    let (head, bootp) = udp.split_at_mut(UDP_HEADER_LEN);
    let bootp = bootp.try_into().ok()?;
    let len = UDP_HEADER_LEN + dhcp_transport_tests::reply(bootp, xid, msg_type, CLIENT_IP, lease);
    head[0..2].copy_from_slice(&UDP_PORT_SERVER.to_be_bytes());
    head[2..4].copy_from_slice(&UDP_PORT_CLIENT.to_be_bytes());
    head[4..6].copy_from_slice(&(len as u16).to_be_bytes());
    Some(ipv4_frame(dst_mac, 17, SERVER, CLIENT_IP, &udp[..len]))
}

/// `part` as the fragment at byte `offset` of datagram `id`, from the server to
/// the address it offers.
#[inline(never)]
fn inject_fragment(
    handle: &DeviceHandle,
    dst_mac: MacAddr,
    id: u16,
    offset: usize,
    more: bool,
    part: &[u8],
) {
    let (mut frame, len) = ipv4_frame::<DHCP_REPLY_CAP>(dst_mac, 17, SERVER, CLIENT_IP, part);
    let ip = &mut frame[ETH_LEN..ETH_LEN + IP_LEN];
    ip[4..6].copy_from_slice(&id.to_be_bytes());
    let field = (offset / 8) as u16 | if more { 0x2000 } else { 0 };
    ip[6..8].copy_from_slice(&field.to_be_bytes());
    ip[10..12].fill(0);
    let csum = crate::checksum::internet_checksum(ip);
    ip[10..12].copy_from_slice(&csum.to_be_bytes());
    if let Some(pkt) = PacketBuf::from_raw_copy(&frame[..len]) {
        ingress::net_rx_injected(handle, pkt);
    }
}

fn arp_request_frame(sender_mac: MacAddr, sender: [u8; 4], target: [u8; 4]) -> [u8; 42] {
    let mut frame = [0u8; 42];
    frame[0..6].copy_from_slice(&MacAddr::BROADCAST.0);
    frame[6..12].copy_from_slice(&sender_mac.0);
    frame[12..14].copy_from_slice(&[0x08, 0x06]);
    let arp = &mut frame[ETH_LEN..];
    arp[0..2].copy_from_slice(&1u16.to_be_bytes());
    arp[2..4].copy_from_slice(&0x0800u16.to_be_bytes());
    arp[4] = 6;
    arp[5] = 4;
    arp[6..8].copy_from_slice(&1u16.to_be_bytes());
    arp[8..14].copy_from_slice(&sender_mac.0);
    arp[14..18].copy_from_slice(&sender);
    arp[24..28].copy_from_slice(&target);
    frame
}

fn udp_socket(local: [u8; 4], port: u16) -> Option<u32> {
    let sock = socket_create(AF_INET, SOCK_DGRAM, 0, SocketOwner::UNOWNED);
    if sock < 0 {
        return None;
    }
    let sock = sock as u32;
    if socket_bind(sock, local, port) != 0 {
        let _ = socket_close(sock);
        return None;
    }
    Some(sock)
}

fn readable(sock: u32) -> bool {
    socket_poll_readable(sock) & POLLIN as u32 != 0
}

fn test_nic_publish_runs_dhcp_and_retire_clears_it() -> TestResult {
    let Some((dev, mock)) = publish(MacAddr([2, 0, 0, 0, 0x61, 1])) else {
        return fail!("could not publish a mock NIC");
    };

    let running = crate::dhcp::is_running(dev);
    let attached = iface::get_by_dev(dev).is_some();
    let published = nic::is_published(dev);
    let discovers = mock.dhcp_frames.load(Ordering::Relaxed);
    nic::force_napi_poll();
    let polls = mock.polls.load(Ordering::Relaxed);
    let retired = nic::retire(dev);

    assert_test!(running, "publishing starts a DHCP client on the NIC");
    assert_test!(attached, "publishing attaches an interface");
    assert_test!(published, "publishing enters the NIC in the poll table");
    assert_test!(
        discovers > 0,
        "the client's DISCOVER leaves through the NIC"
    );
    assert_test!(polls > 0, "a netpoll burst polls the published NIC");
    assert_test!(retired, "retire finds the published NIC");

    assert_test!(
        !crate::dhcp::is_running(dev),
        "retiring stops the DHCP client"
    );
    assert_test!(
        iface::get_by_dev(dev).is_none(),
        "retiring detaches the interface"
    );
    assert_test!(!nic::is_published(dev), "retiring leaves the poll table");
    assert_test!(
        DEVICE_REGISTRY.device_at(dev).is_none(),
        "retiring frees the registry slot"
    );
    pass!()
}

fn test_nic_udp_egress_carries_the_nic_mac() -> TestResult {
    const MAC: MacAddr = MacAddr([2, 0, 0, 0, 0x61, 2]);
    const PEER_MAC: MacAddr = MacAddr([2, 0, 0, 0, 0x61, 0xfe]);
    const LOCAL: [u8; 4] = [10, 97, 0, 1];
    const PEER: [u8; 4] = [10, 97, 0, 2];

    let Some((dev, mock)) = publish(MAC) else {
        return fail!("could not publish a mock NIC");
    };

    route::add(RouteEntry {
        prefix: Ipv4Addr([10, 97, 0, 0]),
        prefix_len: 24,
        gateway: Ipv4Addr::UNSPECIFIED,
        dev,
        metric: 0,
    });
    let _ = NEIGHBOR_CACHE.insert_or_update(dev, Ipv4Addr(PEER), PEER_MAC, crate::clock::now_ms());

    let sent = crate::udp::udp_sendto(LOCAL, PEER, 40_000, PROBE_PORT, 0, &[0x5a; 16]);
    let probe = mock.probe();
    nic::retire(dev);

    assert_eq_test!(sent, Ok(16), "the datagram is routed and sent");
    let Some((src, dst)) = probe else {
        return fail!("the datagram did not leave through the NIC its route names");
    };
    assert_eq_test!(src, MAC, "the Ethernet source is the egress NIC's MAC");
    assert_eq_test!(dst, PEER_MAC, "the Ethernet destination is the neighbour's");
    pass!()
}

fn ip_tos_of(sock: u32) -> Option<i32> {
    let mut out = [0u8; 4];
    (socket_getsockopt(sock, IPPROTO_IP, IP_TOS, &mut out) == 4).then(|| i32::from_ne_bytes(out))
}

fn set_ip_tos(sock: u32, tos: i32) -> i32 {
    socket_setsockopt(sock, IPPROTO_IP, IP_TOS, &tos.to_ne_bytes())
}

fn test_nic_udp_ip_tos_marks_the_datagram() -> TestResult {
    const PORT: u16 = 40_963;

    let Some(lan) = Lan::up(MacAddr([2, 0, 0, 0, 0x61, 10])) else {
        return fail!("could not bring up the mock NIC");
    };
    let Some(sock) = udp_socket(LAN_LOCAL, PORT) else {
        return fail!("could not bind a UDP socket");
    };
    let set_rc = set_ip_tos(sock, 0xb8);
    let stored = ip_tos_of(sock);
    let connect_rc = socket_connect(sock, LAN_PEER, PROBE_PORT);
    let sent = socket_send(sock, &[0x5a; 16]);
    let seen = lan.mock.probe().is_some();
    let tos = lan.mock.probe_tos.load(Ordering::Relaxed);
    let csum_bad = lan.mock.ip_csum_bad.load(Ordering::Relaxed);
    let _ = socket_close(sock);
    drop(lan);

    assert_eq_test!(set_rc, 0, "IP_TOS is accepted on a datagram socket");
    assert_eq_test!(stored, Some(0xb8), "getsockopt reports the stored TOS");
    assert_eq_test!(connect_rc, 0, "the socket connects to its peer");
    assert_eq_test!(sent, 16, "the datagram is sent");
    assert_test!(seen, "the datagram leaves through the NIC");
    assert_eq_test!(tos, 0xb8, "the datagram's IPv4 header carries the TOS");
    assert_test!(!csum_bad, "the IPv4 header checksum covers the TOS");
    pass!()
}

fn test_nic_tcp_ip_tos_marks_segments_without_ecn() -> TestResult {
    let Some(lan) = Lan::up(MacAddr([2, 0, 0, 0, 0x61, 11])) else {
        return fail!("could not bring up the mock NIC");
    };
    let sock = socket_create(AF_INET, SOCK_STREAM, 0, SocketOwner::UNOWNED);
    if sock < 0 {
        return fail!("socket_create failed");
    }
    let sock = sock as u32;
    let nb_rc = socket_set_nonblocking(sock, true);
    let set_rc = set_ip_tos(sock, 0xbb);
    let stored = ip_tos_of(sock);
    let connect_rc = socket_connect(sock, LAN_PEER, TCP_PROBE_PORT);
    let syn_tos = lan.mock.tcp_probe_tos();

    let reset_rc = set_ip_tos(sock, 0x29);
    let conn = crate::socket::socket_lookup_tcp_idx(sock);
    if let Some(id) = conn {
        crate::timer::dispatch_retransmit_action(id.raw(), crate::tcp::on_retransmit(id.raw()));
    }
    let resent_tos = lan.mock.tcp_probe_tos();
    let csum_bad = lan.mock.ip_csum_bad.load(Ordering::Relaxed);
    let _ = socket_close(sock);
    drop(lan);

    assert_eq_test!(nb_rc, 0, "the socket goes non-blocking");
    assert_eq_test!(set_rc, 0, "IP_TOS is accepted on a stream socket");
    assert_eq_test!(
        stored,
        Some(0xb8),
        "a stream socket's TOS leaves the ECN bits to TCP"
    );
    assert_eq_test!(
        connect_rc,
        errno_i32(ERRNO_EINPROGRESS),
        "the SYN is on the wire"
    );
    assert_eq_test!(syn_tos, Some(0xb8), "the SYN carries the TOS");
    assert_eq_test!(reset_rc, 0, "IP_TOS changes on a connecting socket");
    assert_test!(conn.is_some(), "the socket has a connection");
    assert_eq_test!(
        resent_tos,
        Some(0x28),
        "the retransmitted SYN carries the new TOS"
    );
    assert_test!(!csum_bad, "the IPv4 header checksum covers the TOS");
    pass!()
}

/// With an address on each of two NICs, a request leaving the second must
/// announce the second's address, not whichever interface answers first.
fn test_nic_arp_request_carries_the_egress_nic_address() -> TestResult {
    const ADDR_A: [u8; 4] = [10, 98, 0, 1];
    const ADDR_B: [u8; 4] = [10, 99, 0, 1];

    let Some((dev_a, _mock_a)) = publish(MacAddr([2, 0, 0, 0, 0x61, 3])) else {
        return fail!("could not publish the first mock NIC");
    };
    let Some((dev_b, mock_b)) = publish(MacAddr([2, 0, 0, 0, 0x61, 4])) else {
        nic::retire(dev_a);
        return fail!("could not publish the second mock NIC");
    };

    let addressed = add_address(dev_a, ADDR_A) && add_address(dev_b, ADDR_B);
    crate::arp::send_request(dev_b, Ipv4Addr([10, 99, 0, 2]));
    let sender = mock_b.arp_request_sender();
    nic::retire(dev_b);
    nic::retire(dev_a);

    assert_test!(addressed, "both NICs take a static address");
    assert_eq_test!(
        sender,
        Some(Ipv4Addr(ADDR_B)),
        "the ARP request announces the address of the NIC it leaves on"
    );
    pass!()
}

/// Off the wire, 127.0.0.1 would reach a service bound to loopback only.
fn test_nic_rx_drops_loopback_destination() -> TestResult {
    const PORT: u16 = 40_961;

    let Some(lan) = Lan::up(MacAddr([2, 0, 0, 0, 0x61, 5])) else {
        return fail!("could not bring up the mock NIC");
    };
    let lo_sock = udp_socket(Ipv4Addr::LOCALHOST.0, PORT);
    let lan_sock = udp_socket(LAN_LOCAL, PORT);

    let (frame, len) = udp_frame(
        lan.mock.mac,
        (LAN_PEER, 9000),
        (Ipv4Addr::LOCALHOST.0, PORT),
    );
    lan.inject(&frame[..len]);
    let (frame, len) = udp_frame(lan.mock.mac, (LAN_PEER, 9000), (LAN_LOCAL, PORT));
    lan.inject(&frame[..len]);
    let leaked = lo_sock.map(readable);
    let delivered = lan_sock.map(readable);

    for sock in [lo_sock, lan_sock].into_iter().flatten() {
        let _ = socket_close(sock);
    }
    drop(lan);

    assert_eq_test!(
        leaked,
        Some(false),
        "a datagram for 127.0.0.1 arriving on a NIC is dropped"
    );
    assert_eq_test!(
        delivered,
        Some(true),
        "the same datagram for the NIC's address is delivered: the control"
    );
    pass!()
}

/// A reply would carry the broadcast address on the wire as its source.
fn test_nic_rx_ignores_broadcast_echo_request() -> TestResult {
    let Some(lan) = Lan::up(MacAddr([2, 0, 0, 0, 0x61, 6])) else {
        return fail!("could not bring up the mock NIC");
    };
    let replies = || lan.mock.echo_replies.load(Ordering::Relaxed);

    let (frame, len) = echo_request_frame(MacAddr::BROADCAST, [255; 4]);
    lan.inject(&frame[..len]);
    let to_limited = replies();
    let (frame, len) = echo_request_frame(MacAddr::BROADCAST, [10, 96, 0, 255]);
    lan.inject(&frame[..len]);
    let to_directed = replies() - to_limited;
    let (frame, len) = echo_request_frame(lan.mock.mac, LAN_LOCAL);
    lan.inject(&frame[..len]);
    let to_us = replies() - to_limited - to_directed;
    drop(lan);

    assert_eq_test!(
        to_limited,
        0,
        "an echo request to 255.255.255.255 is not answered"
    );
    assert_eq_test!(
        to_directed,
        0,
        "an echo request to the subnet broadcast is not answered"
    );
    assert_eq_test!(
        to_us,
        1,
        "an echo request to the NIC's address is: the control"
    );
    pass!()
}

/// Poll must not report a datagram recv would discard: a resolver that polls
/// and then blocks in recv would hang on it.
fn test_nic_rx_connected_udp_takes_only_its_peer() -> TestResult {
    const PORT: u16 = 40_962;
    const PEER_PORT: u16 = 7000;

    let Some(lan) = Lan::up(MacAddr([2, 0, 0, 0, 0x61, 7])) else {
        return fail!("could not bring up the mock NIC");
    };
    let connected = udp_socket(LAN_LOCAL, PORT);
    let wildcard = udp_socket([0; 4], PORT);
    let connect_rc = connected.map(|sock| socket_connect(sock, LAN_PEER, PEER_PORT));

    let (frame, len) = udp_frame(lan.mock.mac, (LAN_STRANGER, PEER_PORT), (LAN_LOCAL, PORT));
    lan.inject(&frame[..len]);
    let connected_after_stranger = connected.map(readable);
    let wildcard_after_stranger = wildcard.map(readable);
    let (frame, len) = udp_frame(lan.mock.mac, (LAN_PEER, PEER_PORT), (LAN_LOCAL, PORT));
    lan.inject(&frame[..len]);
    let connected_after_peer = connected.map(readable);

    for sock in [connected, wildcard].into_iter().flatten() {
        let _ = socket_close(sock);
    }
    drop(lan);

    assert_eq_test!(connect_rc, Some(0), "the socket connects to its peer");
    assert_eq_test!(
        connected_after_stranger,
        Some(false),
        "a connected socket is not readable after a datagram from another source"
    );
    assert_eq_test!(
        wildcard_after_stranger,
        Some(true),
        "that datagram goes to the wildcard socket on the same port"
    );
    assert_eq_test!(
        connected_after_peer,
        Some(true),
        "the peer's datagram reaches the connected socket"
    );
    pass!()
}

/// RFC 826: an ARP refreshes a sender already known, and adds one only when it
/// is addressed to us.
fn test_nic_rx_arp_learns_only_when_targeted() -> TestResult {
    const ASKER_MAC: MacAddr = MacAddr([2, 0, 0, 0, 0x61, 0xfc]);
    const ASKER: [u8; 4] = [10, 96, 0, 7];
    const BYSTANDER: [u8; 4] = [10, 96, 0, 8];

    let Some(lan) = Lan::up(MacAddr([2, 0, 0, 0, 0x61, 8])) else {
        return fail!("could not bring up the mock NIC");
    };
    let replies = || lan.mock.arp_replies.load(Ordering::Relaxed);

    lan.inject(&arp_request_frame(ASKER_MAC, ASKER, BYSTANDER));
    let learned_from_other = NEIGHBOR_CACHE.lookup(lan.dev, Ipv4Addr(ASKER));
    let answered_other = replies();
    lan.inject(&arp_request_frame(ASKER_MAC, ASKER, LAN_LOCAL));
    let learned_from_ours = NEIGHBOR_CACHE.lookup(lan.dev, Ipv4Addr(ASKER));
    let answered_ours = replies() - answered_other;
    drop(lan);

    assert_eq_test!(
        learned_from_other,
        None,
        "a request for another host adds no entry"
    );
    assert_eq_test!(answered_other, 0, "nor is it answered");
    assert_eq_test!(
        learned_from_ours,
        Some(ASKER_MAC),
        "a request for our address adds its sender"
    );
    assert_eq_test!(answered_ours, 1, "and is answered");
    pass!()
}

/// RFC 2131 §4.1: a server that ignores the broadcast flag unicasts its OFFER
/// and ACK to the address it offers, which the NIC does not hold yet. Nothing
/// else addressed elsewhere gets in meanwhile.
fn test_nic_rx_unicast_dhcp_reply_binds_an_unaddressed_nic() -> TestResult {
    const PORT: u16 = 40_962;
    const ELSEWHERE: [u8; 4] = [10, 77, 0, 99];

    let Some((dev, mock)) = publish(MacAddr([2, 0, 0, 0, 0x61, 8])) else {
        return fail!("could not publish a mock NIC");
    };
    let Some(handle) = nic::handle(dev) else {
        nic::retire(dev);
        return fail!("the published NIC has no handle");
    };
    let inject = |frame: &[u8]| {
        if let Some(pkt) = PacketBuf::from_raw_copy(frame) {
            ingress::net_rx_injected(&handle, pkt);
        }
    };

    let sock = udp_socket([0; 4], PORT);
    let (frame, len) = udp_frame(mock.mac, (SERVER, 9000), (ELSEWHERE, PORT));
    inject(&frame[..len]);
    let stray = sock.map(readable);

    let offered = unicast_dhcp_reply(mock.mac, dev, MSG_OFFER, None)
        .map(|(frame, len)| inject(&frame[..len]))
        .is_some();
    let acked = unicast_dhcp_reply(mock.mac, dev, MSG_ACK, Some(3600))
        .map(|(frame, len)| inject(&frame[..len]))
        .is_some();
    let bound = iface::get_by_dev(dev)
        .is_some_and(|row| row.addrs().iter().any(|a| a.addr == Ipv4Addr(CLIENT_IP)));

    if let Some(sock) = sock {
        let _ = socket_close(sock);
    }
    nic::retire(dev);

    assert_eq_test!(
        stray,
        Some(false),
        "a datagram to another port at a foreign address is dropped before a lease"
    );
    assert_test!(offered && acked, "the client had a transaction in flight");
    assert_test!(bound, "the unicast OFFER and ACK bind the offered address");
    pass!()
}

/// The DHCP exception tests the UDP port, which only an unfragmented datagram
/// is sure to carry where it looks: a later fragment's bytes there are payload.
fn test_nic_rx_drops_dhcp_port_fragments_before_a_lease() -> TestResult {
    const SPLIT: usize = 56;
    const ID: u16 = 0x6168;

    let Some((dev, mock)) = publish(MacAddr([2, 0, 0, 0, 0x61, 9])) else {
        return fail!("could not publish a mock NIC");
    };
    let Some(handle) = nic::handle(dev) else {
        nic::retire(dev);
        return fail!("the published NIC has no handle");
    };
    let Some((mut frame, len)) = unicast_dhcp_reply(mock.mac, dev, MSG_OFFER, None) else {
        nic::retire(dev);
        return fail!("the client had no transaction in flight");
    };
    let udp = &mut frame[ETH_LEN + IP_LEN..len];
    // Into `sname`, which the client ignores: the second fragment now opens
    // with what reads as a UDP header to port 68.
    udp[SPLIT + 2..SPLIT + 4].copy_from_slice(&UDP_PORT_CLIENT.to_be_bytes());

    inject_fragment(&handle, mock.mac, ID, 0, true, &udp[..SPLIT]);
    inject_fragment(&handle, mock.mac, ID, SPLIT, false, &udp[SPLIT..]);
    let state = crate::dhcp::state_of(dev);
    nic::retire(dev);

    assert_eq_test!(
        state,
        Some(NET_DHCP_SELECTING),
        "a fragmented OFFER to an unaddressed NIC never reaches the client"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_nic_publish_runs_dhcp_and_retire_clears_it,
    suite = nic
);
slopos_testing::stest!(name = test_nic_udp_egress_carries_the_nic_mac, suite = nic);
slopos_testing::stest!(name = test_nic_udp_ip_tos_marks_the_datagram, suite = nic);
slopos_testing::stest!(
    name = test_nic_tcp_ip_tos_marks_segments_without_ecn,
    suite = nic
);
slopos_testing::stest!(
    name = test_nic_arp_request_carries_the_egress_nic_address,
    suite = nic
);
slopos_testing::stest!(name = test_nic_rx_drops_loopback_destination, suite = nic);
slopos_testing::stest!(
    name = test_nic_rx_ignores_broadcast_echo_request,
    suite = nic
);
slopos_testing::stest!(
    name = test_nic_rx_connected_udp_takes_only_its_peer,
    suite = nic
);
slopos_testing::stest!(
    name = test_nic_rx_arp_learns_only_when_targeted,
    suite = nic
);
slopos_testing::stest!(
    name = test_nic_rx_unicast_dhcp_reply_binds_an_unaddressed_nic,
    suite = nic
);
slopos_testing::stest!(
    name = test_nic_rx_drops_dhcp_port_fragments_before_a_lease,
    suite = nic
);
