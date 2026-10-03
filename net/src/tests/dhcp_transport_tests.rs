//! Tests for the DHCP transport: the glue between the state machine and a real
//! interface.
//!
//! Each test registers its own mock device and drives only that one.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KVec};

use slopos_abi::net::{NET_DHCP_INIT, NET_DHCP_REBINDING, NET_DHCP_REQUESTING, NET_DHCP_SELECTING};

use crate::clock::MockClock;
use crate::dhcp::client::{RETRY_JITTER_MS, RETRY_MAX_MS};
use crate::dhcp::codec::{self, BOOTP_HEADER_LEN, DHCP_FRAME_LEN, MSG_ACK, MSG_NAK, MSG_OFFER};
use crate::iface::{self, AddrOrigin, IfaceKind};
use crate::netdev::{DEVICE_REGISTRY, NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::packetbuf::PacketBuf;
use crate::pool::PacketPool;
use crate::route::ROUTE_TABLE;
use crate::tests::net_scope::NetTestScope;
use crate::timer::TimerKind;
use crate::types::{DevIndex, Ipv4Addr, MacAddr, NetError};

pub(crate) const SERVER: [u8; 4] = [10, 77, 0, 1];
pub(crate) const CLIENT_IP: [u8; 4] = [10, 77, 0, 55];
const MASK: [u8; 4] = [255, 255, 255, 0];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MockEvent {
    /// Carries the frame's DHCP message type, or 0 for a non-DHCP frame.
    Tx(u8),
    SetDown,
    SetUp,
}

const MAX_EVENTS: usize = 16;

struct EventLog {
    events: [MockEvent; MAX_EVENTS],
    len: usize,
}

struct RecordingMock {
    mac: MacAddr,
    log: SpinLock<EventLog>,
    carrier: AtomicBool,
    tx_calls: AtomicUsize,
}

impl RecordingMock {
    fn new(mac: MacAddr) -> Self {
        Self {
            mac,
            log: SpinLock::new(
                EventLog {
                    events: [MockEvent::SetUp; MAX_EVENTS],
                    len: 0,
                },
                lock_class!("test.dhcp_mock.log", LOCK_LEVEL_RESOURCE),
            ),
            carrier: AtomicBool::new(true),
            tx_calls: AtomicUsize::new(0),
        }
    }

    fn record(&self, event: MockEvent) {
        let mut log = self.log.lock();
        if log.len < MAX_EVENTS {
            let at = log.len;
            log.events[at] = event;
            log.len += 1;
        }
    }

    /// Copy the log out, so assertions run with the lock released.
    fn snapshot(&self, out: &mut [MockEvent]) -> usize {
        let log = self.log.lock();
        let n = log.len.min(out.len());
        out[..n].copy_from_slice(&log.events[..n]);
        n
    }

    fn clear(&self) {
        self.log.lock().len = 0;
    }
}

impl NetDevice for RecordingMock {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        self.tx_calls.fetch_add(1, Ordering::Relaxed);
        self.record(MockEvent::Tx(dhcp_msg_type(pkt.payload()).unwrap_or(0)));
        Ok(())
    }
    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }
    fn set_up(&self) {
        self.record(MockEvent::SetUp);
    }
    fn set_down(&self) {
        self.record(MockEvent::SetDown);
    }
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
    fn kind(&self) -> IfaceKind {
        IfaceKind::Ethernet
    }
    fn carrier(&self) -> bool {
        self.carrier.load(Ordering::Acquire)
    }
    fn carrier_detect(&self) -> bool {
        true
    }
}

/// Ethernet(14) + IPv4(20) + UDP(8) is the only shape this client emits, so the
/// offsets are fixed rather than parsed.
fn dhcp_msg_type(frame: &[u8]) -> Option<u8> {
    const L2: usize = 14;
    const L3: usize = 20;
    const L4: usize = 8;
    const PAYLOAD: usize = L2 + L3 + L4;
    if frame.len() < PAYLOAD + BOOTP_HEADER_LEN {
        return None;
    }
    // EtherType IPv4, protocol UDP.
    if frame[12..14] != [0x08, 0x00] || frame[L2 + 9] != 17 {
        return None;
    }
    let dst_port = u16::from_be_bytes([frame[L2 + L3 + 2], frame[L2 + L3 + 3]]);
    if dst_port != codec::UDP_PORT_SERVER {
        return None;
    }
    option_53(&frame[PAYLOAD..])
}

fn option_53(bootp: &[u8]) -> Option<u8> {
    let mut i = BOOTP_HEADER_LEN;
    while i + 1 < bootp.len() {
        let code = bootp[i];
        if code == 255 {
            return None;
        }
        if code == 0 {
            i += 1;
            continue;
        }
        let len = bootp[i + 1] as usize;
        if i + 2 + len > bootp.len() {
            return None;
        }
        if code == 53 && len >= 1 {
            return Some(bootp[i + 2]);
        }
        i += 2 + len;
    }
    None
}

struct Fixture {
    dev: DevIndex,
    ifindex: u32,
    mock: KArc<RecordingMock>,
}

impl Fixture {
    fn new(mac: MacAddr) -> Option<Self> {
        Self::with_carrier(mac, true)
    }

    fn with_carrier(mac: MacAddr, carrier: bool) -> Option<Self> {
        let mock = KArc::try_new(RecordingMock::new(mac)).ok()?;
        mock.carrier.store(carrier, Ordering::Release);
        let dyn_dev: KArc<dyn NetDevice + Send + Sync> = mock.clone();
        let handle = DEVICE_REGISTRY.register(dyn_dev)?;
        let dev = handle.index();
        let ifindex = iface::attach(dev, IfaceKind::Ethernet, mac, 1500, carrier, true).ok()?;
        Some(Self { dev, ifindex, mock })
    }

    /// The cable out and back in, as the net-timer's carrier sampling reports it.
    fn replug(&self) {
        let _ = iface::set_carrier(self.dev, false);
        let _ = iface::set_carrier(self.dev, true);
    }

    fn sent(&self, msg_type: u8) -> usize {
        let mut events = [MockEvent::SetUp; MAX_EVENTS];
        let n = self.mock.snapshot(&mut events);
        events[..n]
            .iter()
            .filter(|e| **e == MockEvent::Tx(msg_type))
            .count()
    }

    fn addresses(&self) -> usize {
        iface::get(self.ifindex).map_or(0, |i| i.addrs().len())
    }

    fn holds(&self, addr: [u8; 4]) -> bool {
        iface::get(self.ifindex).is_some_and(|i| i.addrs().iter().any(|a| a.addr == Ipv4Addr(addr)))
    }

    fn bind(&self, lease_secs: u32) -> bool {
        if !crate::dhcp::start(self.dev) {
            return false;
        }
        let xid = match current_xid(self.dev) {
            Some(x) => x,
            None => return false,
        };
        let mut buf = [0u8; DHCP_FRAME_LEN];
        let len = reply(&mut buf, xid, MSG_OFFER, CLIENT_IP, None);
        crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);
        let xid = match current_xid(self.dev) {
            Some(x) => x,
            None => return false,
        };
        let len = reply(&mut buf, xid, MSG_ACK, CLIENT_IP, Some(lease_secs));
        crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);
        iface::get(self.ifindex).is_some_and(|i| !i.addrs().is_empty())
    }

    fn teardown(self) {
        crate::dhcp::stop(self.dev);
        crate::route::remove_device_routes(self.dev);
        iface::detach(self.dev);
        DEVICE_REGISTRY.unregister(self.dev);
    }
}

fn current_xid(dev: DevIndex) -> Option<u32> {
    crate::dhcp::transport::xid_of(dev)
}

/// Build a server reply. **No DNS option is ever written**, so `bind()` leaves
/// the system resolver the DNS tests depend on untouched.
pub(crate) fn reply(
    buf: &mut [u8; DHCP_FRAME_LEN],
    xid: u32,
    msg_type: u8,
    yiaddr: [u8; 4],
    lease: Option<u32>,
) -> usize {
    buf.fill(0);
    buf[0] = 2; // BOOTREPLY
    buf[1] = 1;
    buf[2] = 6;
    buf[4..8].copy_from_slice(&xid.to_be_bytes());
    buf[16..20].copy_from_slice(&yiaddr);
    buf[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);

    let mut at = BOOTP_HEADER_LEN;
    let put = |buf: &mut [u8; DHCP_FRAME_LEN], at: &mut usize, code: u8, data: &[u8]| {
        buf[*at] = code;
        buf[*at + 1] = data.len() as u8;
        buf[*at + 2..*at + 2 + data.len()].copy_from_slice(data);
        *at += 2 + data.len();
    };
    put(buf, &mut at, 53, &[msg_type]);
    put(buf, &mut at, 54, &SERVER);
    if msg_type != MSG_NAK {
        put(buf, &mut at, 1, &MASK);
        put(buf, &mut at, 3, &SERVER);
    }
    if let Some(secs) = lease {
        put(buf, &mut at, 51, &secs.to_be_bytes());
        put(buf, &mut at, 58, &(secs / 2).to_be_bytes());
        put(buf, &mut at, 59, &(secs / 8 * 7).to_be_bytes());
    }
    buf[at] = 255;
    at + 1
}

fn position_of(events: &[MockEvent], wanted: MockEvent) -> Option<usize> {
    events.iter().position(|e| *e == wanted)
}

fn routes_for(dev: DevIndex) -> usize {
    ROUTE_TABLE
        .all_routes()
        .iter()
        .filter(|r| r.dev == dev)
        .count()
}

fn test_dhcp_tx_binds_address_and_routes() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 1])) else {
        return fail!("could not register a mock device");
    };

    if !crate::dhcp::start(f.dev) {
        f.teardown();
        return fail!("start failed");
    }

    let mut events = [MockEvent::SetUp; MAX_EVENTS];
    let n = f.mock.snapshot(&mut events);
    let Some(discover) = position_of(&events[..n], MockEvent::Tx(codec::MSG_DISCOVER)) else {
        f.teardown();
        return fail!("starting the client puts a DISCOVER on the wire");
    };

    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("no transaction in flight");
    };
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let len = reply(&mut buf, xid, MSG_OFFER, CLIENT_IP, None);
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);

    let n = f.mock.snapshot(&mut events);
    let Some(request) = position_of(&events[..n], MockEvent::Tx(codec::MSG_REQUEST)) else {
        f.teardown();
        return fail!("the offer produces a REQUEST");
    };
    // By position, not index: a retransmit may sit between the two.
    assert_test!(
        discover < request,
        "the DISCOVER precedes the REQUEST it answers"
    );

    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("no transaction in flight");
    };
    let len = reply(&mut buf, xid, MSG_ACK, CLIENT_IP, Some(3600));
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);

    let Some(row) = iface::get(f.ifindex) else {
        f.teardown();
        return fail!("interface vanished");
    };
    assert_eq_test!(row.addrs().len(), 1, "the ACK installed an address");
    assert_eq_test!(row.addrs()[0].addr.0, CLIENT_IP, "the one the server gave");
    assert_eq_test!(
        row.addrs()[0].origin,
        AddrOrigin::Dhcp,
        "recorded as a lease, so an administrative down invalidates it"
    );
    assert_eq_test!(
        routes_for(f.dev),
        2,
        "and both routes: the connected one and the lease's default"
    );

    f.teardown();
    pass!()
}

fn test_dhcp_port_listener_is_claimed() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 5])) else {
        return fail!("could not register a mock device");
    };

    // `LISTENER.init_once_then`: the boot client already holds the claim, so a
    // failure here must put the live one back rather than leave port 68 vacant.
    if !crate::dhcp::start(f.dev) {
        f.teardown();
        return fail!("start failed");
    }

    let claimed_twice = crate::udp::register_port_listener(codec::UDP_PORT_CLIENT, |_, _, _| {});
    if claimed_twice {
        crate::udp::unregister_port_listener(codec::UDP_PORT_CLIENT);
        crate::udp::register_port_listener(
            codec::UDP_PORT_CLIENT,
            crate::dhcp::transport::on_udp_receive,
        );
        f.teardown();
        return fail!("a second claim on port 68 was accepted");
    }

    f.teardown();
    pass!()
}

/// The bug the per-slot epoch exists for: by the time the stale timer fires the
/// client is legitimately `Bound` again, so state alone cannot distinguish it.
fn test_dhcp_stale_expiry_cannot_unbind_a_later_lease() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 2])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }

    let Some(stale_key) = crate::dhcp::transport::expire_key(f.dev) else {
        f.teardown();
        return fail!("no expiry timer armed");
    };

    // The expiry timer armed for the first lease stays in flight across the NAK.
    crate::dhcp::renew_now(f.dev);
    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("no renewal in flight");
    };
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let len = reply(&mut buf, xid, MSG_NAK, [0; 4], None);
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);
    assert_test!(
        iface::get(f.ifindex).is_some_and(|i| i.addrs().is_empty()),
        "a NAK gives the address up at once"
    );

    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("discovery did not restart");
    };
    let len = reply(&mut buf, xid, MSG_OFFER, CLIENT_IP, None);
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);
    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("no request in flight");
    };
    let len = reply(&mut buf, xid, MSG_ACK, CLIENT_IP, Some(7200));
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);

    let live_key = crate::dhcp::transport::expire_key(f.dev);
    assert_test!(
        live_key != Some(stale_key),
        "the second lease's timers carry a different epoch"
    );
    assert_test!(
        iface::get(f.ifindex).is_some_and(|i| !i.addrs().is_empty()),
        "the second lease is installed"
    );

    crate::dhcp::on_expire_timer(stale_key);

    assert_test!(
        iface::get(f.ifindex).is_some_and(|i| !i.addrs().is_empty()),
        "a stale expiry must not tear down the lease that replaced it"
    );
    assert_eq_test!(
        routes_for(f.dev),
        2,
        "and must not withdraw its routes either"
    );

    if let Some(key) = live_key {
        crate::dhcp::on_expire_timer(key);
        assert_test!(
            iface::get(f.ifindex).is_some_and(|i| i.addrs().is_empty()),
            "the current lease's own expiry still tears it down"
        );
    }

    f.teardown();
    pass!()
}

/// A RELEASE is identified by its source address, so one sent after the unbind
/// names an address the server may not match to a binding.
fn test_dhcp_release_precedes_unbind() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 3])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    f.mock.clear();

    crate::dhcp::stop(f.dev);

    let mut events = [MockEvent::SetUp; MAX_EVENTS];
    let n = f.mock.snapshot(&mut events);
    let released = events[..n]
        .iter()
        .position(|e| *e == MockEvent::Tx(codec::MSG_RELEASE));
    assert_test!(released.is_some(), "stopping puts a RELEASE on the wire");
    assert_test!(
        iface::get(f.ifindex).is_some_and(|i| i.addrs().is_empty()),
        "and the address is withdrawn afterwards"
    );
    assert_eq_test!(routes_for(f.dev), 0, "along with its routes");

    f.teardown();
    pass!()
}

/// Without this an operator's `ip link set eth0 down` silently keeps the
/// address reserved on the server for the rest of the lease.
fn test_dhcp_admin_down_releases_before_set_down() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 4])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    f.mock.clear();

    if let Err(e) = crate::iface_ctl::set_admin_up(f.ifindex, false) {
        f.teardown();
        return fail!("set_admin_up(false) failed: {:?}", e);
    }

    let mut events = [MockEvent::SetUp; MAX_EVENTS];
    let n = f.mock.snapshot(&mut events);
    let released = events[..n]
        .iter()
        .position(|e| *e == MockEvent::Tx(codec::MSG_RELEASE));
    let downed = events[..n].iter().position(|e| *e == MockEvent::SetDown);

    let Some(released) = released else {
        f.teardown();
        return fail!("an administrative down must give the lease back");
    };
    let Some(downed) = downed else {
        f.teardown();
        return fail!("an administrative down must reach the driver");
    };
    assert_test!(
        released < downed,
        "the RELEASE must reach the wire before the interface goes down, or it names an address the client no longer holds"
    );

    f.teardown();
    pass!()
}

/// Carrier returning mid-DISCOVER restarts the transaction; the retransmission
/// it arms must replace the one in flight, not run beside it at twice the rate.
fn test_dhcp_carrier_up_while_selecting_keeps_one_retransmit_chain() -> TestResult {
    const ROUNDS: usize = 3;
    let scope = match NetTestScope::enter_at_mock_ms(1_000_000) {
        Ok(scope) => scope,
        Err(e) => return fail!("could not enter the net test scope: {:?}", e),
    };
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 6])) else {
        return fail!("could not register a mock device");
    };
    if !crate::dhcp::start(f.dev) {
        f.teardown();
        return fail!("start failed");
    }
    f.replug();
    f.mock.clear();

    // Every armed delay is below the backoff ceiling plus jitter, so each
    // advance fires each live chain exactly once.
    for _ in 0..ROUNDS {
        MockClock::advance(u64::from(RETRY_MAX_MS + RETRY_JITTER_MS) + 1);
        for timer in scope.dispatch_due(TimerKind::DhcpRetransmit).iter() {
            crate::dhcp::on_retransmit_timer(timer.key);
        }
    }
    let discovers = f.sent(codec::MSG_DISCOVER);
    f.teardown();

    assert_eq_test!(
        discovers,
        ROUNDS,
        "one DISCOVER per backoff interval: the carrier's chain replaced the start's"
    );
    pass!()
}

/// A NIC that publishes while its link is still negotiating would otherwise
/// spend the first DISCOVER on a transmit the driver refuses.
fn test_dhcp_start_without_carrier_waits_for_it() -> TestResult {
    let Some(f) = Fixture::with_carrier(MacAddr([2, 0, 0, 0, 77, 7]), false) else {
        return fail!("could not register a mock device");
    };
    if !crate::dhcp::start(f.dev) {
        f.teardown();
        return fail!("start failed");
    }
    let sent_while_down = f.mock.tx_calls.load(Ordering::Relaxed);

    f.mock.carrier.store(true, Ordering::Release);
    let _ = iface::set_carrier(f.dev, true);
    let discovers = f.sent(codec::MSG_DISCOVER);
    let state = crate::dhcp::state_of(f.dev);
    f.teardown();

    assert_eq_test!(
        sent_while_down,
        0,
        "nothing is sent into a link that is down"
    );
    assert_eq_test!(discovers, 1, "carrier-up sends the first DISCOVER");
    assert_eq_test!(
        state,
        Some(NET_DHCP_SELECTING),
        "and the client is selecting"
    );
    pass!()
}

/// RFC 2131 §3.2: a NAK to INIT-REBOOT means the address is not ours on this
/// segment, and it was installed before the replug.
fn test_dhcp_nak_to_init_reboot_withdraws_the_old_address() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 8])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    f.mock.clear();
    f.replug();
    let requested = f.sent(codec::MSG_REQUEST);

    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("no INIT-REBOOT request in flight");
    };
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let len = reply(&mut buf, xid, MSG_NAK, [0; 4], None);
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);

    let addresses = f.addresses();
    let routes = routes_for(f.dev);
    let discovers = f.sent(codec::MSG_DISCOVER);
    f.teardown();

    assert_eq_test!(requested, 1, "the replug confirms the held address");
    assert_eq_test!(addresses, 0, "the NAK withdraws the address it refused");
    assert_eq_test!(routes, 0, "and the routes derived from it");
    assert_eq_test!(discovers, 1, "then the client discovers afresh");
    pass!()
}

/// RFC 2131 §4.4.2: an INIT-REBOOT nobody answers must not keep the address
/// past the lease it came from.
fn test_dhcp_expiry_while_requesting_withdraws_and_rediscovers() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 9])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    let Some(expire) = crate::dhcp::transport::expire_key(f.dev) else {
        f.teardown();
        return fail!("no expiry timer armed");
    };
    f.mock.clear();
    f.replug();
    let requesting = crate::dhcp::state_of(f.dev);

    crate::dhcp::on_expire_timer(expire);
    let addresses = f.addresses();
    let discovers = f.sent(codec::MSG_DISCOVER);
    let state = crate::dhcp::state_of(f.dev);
    f.teardown();

    assert_eq_test!(
        requesting,
        Some(NET_DHCP_REQUESTING),
        "the replug left an INIT-REBOOT in flight"
    );
    assert_eq_test!(addresses, 0, "the lease's expiry withdraws the address");
    assert_eq_test!(discovers, 1, "and falls back to DISCOVER");
    assert_eq_test!(state, Some(NET_DHCP_SELECTING), "selecting again");
    pass!()
}

/// The held address must not stay first on the interface, where source
/// selection and ARP would keep answering with it.
fn test_dhcp_ack_for_another_address_replaces_the_held_one() -> TestResult {
    const MOVED: [u8; 4] = [10, 77, 0, 66];
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 10])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    f.replug();
    let Some(xid) = current_xid(f.dev) else {
        f.teardown();
        return fail!("no INIT-REBOOT request in flight");
    };
    let mut buf = [0u8; DHCP_FRAME_LEN];
    let len = reply(&mut buf, xid, MSG_ACK, MOVED, Some(3600));
    crate::dhcp::transport::on_udp_receive(SERVER, codec::UDP_PORT_SERVER, &buf[..len]);

    let addresses = f.addresses();
    let moved = f.holds(MOVED);
    f.teardown();

    assert_test!(moved, "the ACK installs the address it grants");
    assert_eq_test!(addresses, 1, "in place of the one held before");
    pass!()
}

/// RFC 2131 §4.4.5: a lease that runs out while rebinding sends the client
/// back to DISCOVER, so a server outage longer than the lease is survivable.
fn test_dhcp_expiry_while_rebinding_rediscovers() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 11])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    let Some(key) = crate::dhcp::transport::expire_key(f.dev) else {
        f.teardown();
        return fail!("no lease timers armed");
    };
    crate::dhcp::on_t1_timer(key);
    crate::dhcp::on_t2_timer(key);
    let rebinding = crate::dhcp::state_of(f.dev);
    f.mock.clear();

    crate::dhcp::on_expire_timer(key);
    let addresses = f.addresses();
    let discovers = f.sent(codec::MSG_DISCOVER);
    let state = crate::dhcp::state_of(f.dev);
    f.teardown();

    assert_eq_test!(rebinding, Some(NET_DHCP_REBINDING), "T2 left it rebinding");
    assert_eq_test!(addresses, 0, "expiry withdraws the address");
    assert_eq_test!(discovers, 1, "and sends a DISCOVER");
    assert_eq_test!(state, Some(NET_DHCP_SELECTING), "selecting again");
    pass!()
}

/// Stopping during INIT-REBOOT still holds the old lease, so it is released
/// and withdrawn like a bound one.
fn test_dhcp_stop_during_init_reboot_releases() -> TestResult {
    let Some(f) = Fixture::new(MacAddr([2, 0, 0, 0, 77, 12])) else {
        return fail!("could not register a mock device");
    };
    if !f.bind(3600) {
        f.teardown();
        return fail!("could not reach Bound");
    }
    f.replug();
    let requesting = crate::dhcp::state_of(f.dev);
    f.mock.clear();

    crate::dhcp::stop(f.dev);
    let releases = f.sent(codec::MSG_RELEASE);
    let addresses = f.addresses();
    let state = crate::dhcp::state_of(f.dev);
    f.teardown();

    assert_eq_test!(
        requesting,
        Some(NET_DHCP_REQUESTING),
        "the replug left an INIT-REBOOT in flight"
    );
    assert_eq_test!(releases, 1, "stop gives the held lease back");
    assert_eq_test!(addresses, 0, "and withdraws the address");
    assert_test!(
        state.is_none() || state == Some(NET_DHCP_INIT),
        "and the client is no longer running"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_dhcp_admin_down_releases_before_set_down,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_ack_for_another_address_replaces_the_held_one,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_carrier_up_while_selecting_keeps_one_retransmit_chain,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_expiry_while_requesting_withdraws_and_rediscovers,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_expiry_while_rebinding_rediscovers,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_stop_during_init_reboot_releases,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_nak_to_init_reboot_withdraws_the_old_address,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_port_listener_is_claimed,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_release_precedes_unbind,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_start_without_carrier_waits_for_it,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_stale_expiry_cannot_unbind_a_later_lease,
    suite = dhcp_transport
);
slopos_testing::stest!(
    name = test_dhcp_tx_binds_address_and_routes,
    suite = dhcp_transport
);
