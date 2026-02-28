//! IPv4 ingress handler — validates IP headers and dispatches to L4 protocols.
//!
//! This module is the single entry point for all received IPv4 packets after
//! Ethernet demux.  It validates the IP header (version, length, checksum, TTL),
//! sets the L4 layer offset on the [`PacketBuf`], and dispatches to the
//! appropriate protocol handler (TCP, UDP, ICMP).
//!
//! # Phase 1D scope
//!
//! - Full IPv4 header validation
//! - Protocol dispatch to existing TCP/UDP handlers via the socket layer
//! - DNS response interception for the in-kernel resolver
//! - ICMP stub (logs and drops)

use slopos_lib::klog_debug;

use super::socket;
use super::tcp;
use super::types::{DevIndex, IpProtocol};
use crate::net::{self as net, packetbuf::PacketBuf};

/// Handle an incoming IPv4 packet.
///
/// Called from [`super::ingress::net_rx`] after Ethernet demux.  The packet's
/// `head` points at the first byte of the IP header (Ethernet header has been
/// consumed via [`PacketBuf::pull_header`]).
///
/// # Validation
///
/// 1. IP version must be 4
/// 2. IHL ≥ 5 (header length ≥ 20 bytes)
/// 3. Total length ≤ packet size
/// 4. Header checksum must verify (unless device has `CHECKSUM_RX`)
/// 5. TTL > 0 (we don't forward, so TTL=0 is always dropped)
///
/// Packets failing any check are silently dropped with a debug log.
pub fn handle_rx(dev: DevIndex, mut pkt: PacketBuf, checksum_rx: bool) {
    // Extract all fields we need while borrowing the payload immutably.
    // We must drop this borrow before calling pkt.set_l4() / pkt.pull_header().
    let (proto, src_ip, dst_ip, ihl) = {
        let ip_data = pkt.payload();
        if ip_data.len() < net::IPV4_HEADER_LEN {
            klog_debug!(
                "ipv4: packet too short ({} < {})",
                ip_data.len(),
                net::IPV4_HEADER_LEN
            );
            return;
        }

        // Version must be 4.
        let version = (ip_data[0] >> 4) & 0x0F;
        if version != 4 {
            klog_debug!("ipv4: bad version {}", version);
            return;
        }

        // Internet Header Length (in 32-bit words).
        let ihl = ((ip_data[0] & 0x0F) as usize) * 4;
        if ihl < net::IPV4_HEADER_LEN || ip_data.len() < ihl {
            klog_debug!("ipv4: bad IHL {} (packet len {})", ihl, ip_data.len());
            return;
        }

        // Total length sanity check.
        let total_len = u16::from_be_bytes([ip_data[2], ip_data[3]]) as usize;
        if total_len > ip_data.len() {
            klog_debug!(
                "ipv4: total_len {} > packet len {}",
                total_len,
                ip_data.len()
            );
            return;
        }

        // Header checksum verification (skip if device already verified).
        if !checksum_rx && net::ipv4_header_checksum(&ip_data[..ihl]) != 0 {
            klog_debug!("ipv4: bad header checksum");
            return;
        }

        // TTL check — we don't forward, so TTL=0 is always invalid.
        let ttl = ip_data[8];
        if ttl == 0 {
            klog_debug!("ipv4: TTL=0, dropping");
            return;
        }

        let proto = ip_data[9];
        let src_ip: [u8; 4] = ip_data[12..16].try_into().unwrap_or([0; 4]);
        let dst_ip: [u8; 4] = ip_data[16..20].try_into().unwrap_or([0; 4]);

        (proto, src_ip, dst_ip, ihl)
    };
    // Immutable borrow of pkt dropped here.

    // Set L4 offset (absolute position: current head + IHL).
    pkt.set_l4(pkt.head() + ihl as u16);

    // Pull the IP header so payload() now points at the L4 data.
    if pkt.pull_header(ihl).is_err() {
        return;
    }

    // Dispatch to L4 protocol handler.
    match IpProtocol::from_u8(proto) {
        Some(IpProtocol::Tcp) => dispatch_tcp(src_ip, dst_ip, &pkt),
        Some(IpProtocol::Udp) => dispatch_udp(src_ip, dst_ip, &pkt),
        Some(IpProtocol::Icmp) => {
            klog_debug!(
                "ipv4: ICMP from {}.{}.{}.{} — stub, dropping",
                src_ip[0],
                src_ip[1],
                src_ip[2],
                src_ip[3]
            );
        }
        None => {
            klog_debug!("ipv4: unknown protocol {}, dropping", proto);
        }
    }

    let _ = dev; // Will be used in Phase 2 for per-device neighbor cache
}

// =============================================================================
// L4 dispatch helpers
// =============================================================================

/// Dispatch a TCP segment to the TCP state machine and socket layer.
///
/// Mirrors the logic previously in `dispatch_rx_frame()` in `virtio_net.rs`.
fn dispatch_tcp(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    let ip_payload = pkt.payload();

    let Some(hdr) = tcp::parse_header(ip_payload) else {
        return;
    };
    let hdr_len = hdr.header_len();
    if hdr_len < tcp::TCP_HEADER_LEN || ip_payload.len() < hdr_len {
        return;
    }
    let options = &ip_payload[tcp::TCP_HEADER_LEN..hdr_len];
    let payload = &ip_payload[hdr_len..];
    let now_ms = slopos_lib::clock::uptime_ms();

    let result = tcp::tcp_input(src_ip, dst_ip, &hdr, options, payload, now_ms);

    if let Some(seg) = result.response {
        let _ = socket::socket_send_tcp_segment(&seg, &[]);
    }
    socket::socket_notify_tcp_activity(&result);
}

/// Dispatch a UDP datagram to the socket layer, with DNS interception.
///
/// Mirrors the logic previously in `dispatch_rx_frame()` in `virtio_net.rs`.
fn dispatch_udp(src_ip: [u8; 4], dst_ip: [u8; 4], pkt: &PacketBuf) {
    let ip_payload = pkt.payload();

    let Some((src_port, dst_port, udp_payload)) = net::parse_udp_header(ip_payload) else {
        return;
    };

    // Intercept DNS responses (src port 53) for the in-kernel resolver.
    if src_port == net::dns::DNS_PORT {
        crate::virtio_net::dns_intercept_response(udp_payload);
    }

    // Always deliver to the socket table — userland might have a UDP socket
    // bound to port 53 (or any other port) for its own purposes.
    socket::socket_deliver_udp_from_dispatch(src_ip, dst_ip, src_port, dst_port, udp_payload);
}
