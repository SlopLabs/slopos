//! IPv4 ingress and egress: [`handle_rx`] validates the header and dispatches to
//! TCP/UDP/ICMP; [`send`] routes, then transmits directly or via the neighbor
//! cache for ARP resolution.

use slopos_ostd::klog_debug;

use super::iface;
use super::netdev::{DeviceHandle, NetDevice};
use super::socket;
use super::tcp;
use super::types::{DevIndex, IpProtocol, Ipv4Addr};
use crate::{self as net, NetError, packetbuf::PacketBuf};

/// Handle an incoming IPv4 packet; `head` points at the first byte of the IP
/// header, the Ethernet header having been consumed already. Ingress has
/// already asked [`admits`].
///
/// Packets failing validation are silently dropped with a debug log. The header
/// checksum is skipped when the device set `CHECKSUM_RX`. TTL=0 is dropped
/// rather than forwarded: this stack does not forward.
pub fn handle_rx(mut pkt: PacketBuf, checksum_rx: bool) {
    let mut reassembled: Option<super::reassembly::ReassembledPacket> = None;
    let mut is_fragmented = false;

    // Scoped: the payload borrow must end before set_l4()/pull_header().
    let (proto, src_ip, dst_ip, ihl, ip_total_len) = {
        let ip_data = pkt.payload();
        if ip_data.len() < net::IPV4_HEADER_LEN {
            klog_debug!(
                "ipv4: packet too short ({} < {})",
                ip_data.len(),
                net::IPV4_HEADER_LEN
            );
            return;
        }

        let version = (ip_data[0] >> 4) & 0x0F;
        if version != 4 {
            klog_debug!("ipv4: bad version {}", version);
            return;
        }

        let ihl = ((ip_data[0] & 0x0F) as usize) * 4;
        if ihl < net::IPV4_HEADER_LEN || ip_data.len() < ihl {
            klog_debug!("ipv4: bad IHL {} (packet len {})", ihl, ip_data.len());
            return;
        }

        let total_len = u16::from_be_bytes([ip_data[2], ip_data[3]]) as usize;
        if total_len > ip_data.len() {
            klog_debug!(
                "ipv4: total_len {} > packet len {}",
                total_len,
                ip_data.len()
            );
            return;
        }

        if total_len < ihl {
            klog_debug!("ipv4: total_len {} < ihl {}", total_len, ihl);
            return;
        }

        if !checksum_rx && net::checksum::internet_checksum(&ip_data[..ihl]) != 0 {
            klog_debug!("ipv4: bad header checksum");
            return;
        }

        let ttl = ip_data[8];
        if ttl == 0 {
            klog_debug!("ipv4: TTL=0, dropping");
            return;
        }

        let proto = ip_data[9];
        let src_ip: [u8; 4] = ip_data[12..16].try_into().unwrap_or([0; 4]);
        let dst_ip: [u8; 4] = ip_data[16..20].try_into().unwrap_or([0; 4]);

        let identification = u16::from_be_bytes([ip_data[4], ip_data[5]]);
        let flags_fragment = u16::from_be_bytes([ip_data[6], ip_data[7]]);
        let more_fragments = (flags_fragment & 0x2000) != 0;
        let frag_offset = (flags_fragment & 0x1fff) * 8;
        if more_fragments || frag_offset > 0 {
            is_fragmented = true;
            reassembled = super::reassembly::REASSEMBLY_TABLE.lock().insert(
                Ipv4Addr(src_ip),
                Ipv4Addr(dst_ip),
                identification,
                proto,
                frag_offset,
                more_fragments,
                &ip_data[ihl..total_len],
            );
        }

        (proto, src_ip, dst_ip, ihl, total_len)
    };

    if is_fragmented {
        let Some(assembled) = reassembled else {
            return;
        };

        let Some(assembled_pkt) =
            PacketBuf::from_raw_copy(&assembled.data[..assembled.len as usize])
        else {
            klog_debug!(
                "ipv4: failed to allocate packet for reassembled datagram len={}",
                assembled.len
            );
            return;
        };

        dispatch_l4(
            assembled.protocol,
            src_ip,
            dst_ip,
            &assembled_pkt,
            checksum_rx,
        );
        return;
    }

    // Trim to IP total_length so L4 handlers never see Ethernet padding.
    pkt.trim(ip_total_len);

    pkt.set_l4(pkt.head() + ihl as u16);

    if pkt.pull_header(ihl).is_err() {
        return;
    }

    dispatch_l4(proto, src_ip, dst_ip, &pkt, checksum_rx);
}

/// Whether an IPv4 packet `handle` received — head at the IP header — is the
/// host's to receive. Loopback takes everything. Elsewhere RFC 1122 §3.2.1.3
/// keeps loopback, broadcast and multicast sources off the wire, and any
/// interface's address will do as the destination (the weak host model of
/// RFC 1122 §3.3.4.2). A header too short to name both addresses is left for
/// [`handle_rx`] to reject.
pub fn admits(handle: &DeviceHandle, pkt: &PacketBuf) -> bool {
    if handle.kind().is_loopback() {
        return true;
    }
    let ip = pkt.payload();
    let Some(addrs) = ip.get(12..20) else {
        return true;
    };
    let src = Ipv4Addr([addrs[0], addrs[1], addrs[2], addrs[3]]);
    let dst = Ipv4Addr([addrs[4], addrs[5], addrs[6], addrs[7]]);
    let martian = src.is_loopback()
        || src.is_broadcast()
        || src.is_multicast()
        || src.is_unspecified()
        || dst.is_loopback();
    let admitted = !martian
        && (dst.is_broadcast()
            || dst.is_multicast()
            || iface::is_our_addr(dst)
            || iface::is_directed_broadcast(handle.index(), dst)
            || dhcp_before_lease(handle, ip));
    if !admitted {
        klog_debug!(
            "ipv4: dropping {} -> {} on dev {}",
            src,
            dst,
            handle.index()
        );
    }
    admitted
}

/// A datagram to the DHCP client port on a device that holds no address yet:
/// RFC 2131 §4.1 lets a server that ignores the broadcast flag unicast its
/// OFFER and ACK to the address it offers. Unfragmented only: a later fragment
/// carries payload where the port is read, so a first fragment admitted alone
/// could never complete. `ip` holds at least 20 bytes.
fn dhcp_before_lease(handle: &DeviceHandle, ip: &[u8]) -> bool {
    let ihl = usize::from(ip[0] & 0x0f) * 4;
    let fragment = u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff;
    ihl >= net::IPV4_HEADER_LEN
        && fragment == 0
        && ip[9] == IpProtocol::Udp as u8
        && ip.get(ihl + 2..ihl + 4) == Some(&super::dhcp::UDP_PORT_CLIENT.to_be_bytes()[..])
        && iface::our_ip(handle.index()).is_none()
}

fn dispatch_l4(proto: u8, src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf, checksum_rx: bool) {
    match IpProtocol::from_u8(proto) {
        Some(IpProtocol::Tcp) => dispatch_tcp(src_ip, dst_ip, pkt, checksum_rx),
        Some(IpProtocol::Udp) => dispatch_udp(src_ip, dst_ip, pkt),
        Some(IpProtocol::Icmp) => super::icmp::handle_rx(src_ip, dst_ip, pkt),
        None => {
            klog_debug!("ipv4: unknown protocol {}, dropping", proto);
        }
    }
}

fn dispatch_tcp(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf, checksum_rx: bool) {
    let ip_payload = pkt.payload();

    let Some(hdr) = tcp::parse_header(ip_payload) else {
        return;
    };
    let hdr_len = hdr.header_len();
    if hdr_len < tcp::TCP_HEADER_LEN || ip_payload.len() < hdr_len {
        return;
    }

    if !checksum_rx && !tcp::verify_checksum(src_ip, dst_ip, ip_payload) {
        klog_debug!("tcp: bad checksum, dropping segment");
        return;
    }

    let options = &ip_payload[tcp::TCP_HEADER_LEN..hdr_len];
    let payload = &ip_payload[hdr_len..];
    let now_ms = slopos_kernel_services::clock::uptime_ms();

    let actions = tcp::input(src_ip, dst_ip, &hdr, options, payload, now_ms);

    for seg in actions.segments() {
        let _ = socket::socket_send_tcp_segment(seg, &[]);
    }
    socket::socket_notify_tcp_activity(&actions);
    // Unsent, Nagle-held and lost bytes all wait on an acknowledgement or a
    // window update, which is what input carries.
    if let Some(id) = actions.conn_id
        && tcp::has_pending_output(id)
    {
        let _ = socket::tcp_drain_segments(id);
    }
}

fn dispatch_udp(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    super::udp::handle_rx(src_ip, dst_ip, pkt);
}

/// Route `pkt` — an Ethernet frame whose head is the Ethernet header — and send
/// it out the route's device, which also supplies the source MAC. A 127/8
/// source never leaves through a device other than loopback (RFC 1122
/// §3.2.1.3): that is `InvalidArgument`.
pub fn send(dst_ip: super::types::Ipv4Addr, mut pkt: PacketBuf) -> Result<(), NetError> {
    use super::netdev::DEVICE_REGISTRY;
    use super::route::ROUTE_TABLE;

    let (dev, next_hop) = ROUTE_TABLE.lookup(dst_ip).ok_or_else(|| {
        klog_debug!("ipv4::send: no route to {}", dst_ip);
        NetError::NetworkUnreachable
    })?;

    let device = DEVICE_REGISTRY
        .device_at(dev)
        .ok_or(NetError::NetworkUnreachable)?;
    if !device.kind().is_loopback() && source_of(&pkt).is_some_and(|src| src.is_loopback()) {
        klog_debug!("ipv4::send: loopback source to {} out dev {}", dst_ip, dev);
        return Err(NetError::InvalidArgument);
    }
    super::arp::set_src_mac_in_eth_header(&mut pkt, device.mac());

    if next_hop.is_loopback()
        || dst_ip.is_loopback()
        || dst_ip.is_broadcast()
        || dst_ip.is_multicast()
    {
        return device.tx(pkt);
    }

    resolve_neighbor_and_send(&*device, dev, next_hop, pkt)
}

fn source_of(frame: &PacketBuf) -> Option<Ipv4Addr> {
    let at = net::ETH_HEADER_LEN + 12;
    let src = frame.payload().get(at..at + 4)?;
    Some(Ipv4Addr([src[0], src[1], src[2], src[3]]))
}

fn resolve_neighbor_and_send(
    device: &(dyn NetDevice + Send + Sync),
    dev: DevIndex,
    next_hop: super::types::Ipv4Addr,
    pkt: PacketBuf,
) -> Result<(), NetError> {
    use super::arp;
    use super::neighbor::{NEIGHBOR_CACHE, ResolveOutcome};

    match NEIGHBOR_CACHE.resolve(dev, next_hop, pkt) {
        ResolveOutcome::Resolved {
            mac,
            mut pkt,
            action,
        } => {
            arp::set_dst_mac_in_eth_header(&mut pkt, mac);
            if let Some(act) = action {
                arp::execute_neighbor_action(act);
            }
            device.tx(pkt)
        }
        ResolveOutcome::Queued => Ok(()),
        ResolveOutcome::ArpNeeded(action) => {
            arp::execute_neighbor_action(action);
            Ok(())
        }
        ResolveOutcome::Failed(e) => {
            klog_debug!(
                "ipv4::send: neighbor resolution failed for {}: {}",
                next_hop,
                e
            );
            Err(e)
        }
    }
}
