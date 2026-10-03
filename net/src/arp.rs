//! ARP protocol handler — request/reply processing and frame construction.
//!
//! RFC 826 ARP for Ethernet/IPv4, feeding the
//! [`NeighborCache`](super::neighbor::NEIGHBOR_CACHE).

use slopos_ostd::klog_debug;

use super::neighbor::{NEIGHBOR_CACHE, NeighborAction};
use super::netdev::{DEVICE_REGISTRY, DeviceHandle};
use super::packetbuf::PacketBuf;
use super::types::{DevIndex, EtherType, Ipv4Addr, MacAddr};
use super::{ETH_ADDR_LEN, ETH_HEADER_LEN};

const ARP_HTYPE_ETHERNET: u16 = 1;
const ARP_PTYPE_IPV4: u16 = EtherType::Ipv4.as_u16();
const ARP_HLEN_ETHERNET: u8 = 6;
const ARP_PLEN_IPV4: u8 = 4;
const ARP_OPER_REQUEST: u16 = 1;
const ARP_OPER_REPLY: u16 = 2;
const ARP_HEADER_LEN: usize = 28;

/// Handle an incoming ARP frame; `pkt`'s head is at the ARP header, the
/// Ethernet header having been consumed by the ingress pipeline.
pub fn handle_rx(handle: &DeviceHandle, pkt: PacketBuf) {
    let data = pkt.payload();

    if data.len() < ARP_HEADER_LEN {
        klog_debug!("arp: frame too short ({} < {})", data.len(), ARP_HEADER_LEN);
        return;
    }

    let htype = u16::from_be_bytes([data[0], data[1]]);
    let ptype = u16::from_be_bytes([data[2], data[3]]);
    let hlen = data[4];
    let plen = data[5];
    let oper = u16::from_be_bytes([data[6], data[7]]);

    if htype != ARP_HTYPE_ETHERNET
        || ptype != ARP_PTYPE_IPV4
        || hlen != ARP_HLEN_ETHERNET
        || plen != ARP_PLEN_IPV4
    {
        klog_debug!(
            "arp: malformed header (htype={}, ptype=0x{:04x}, hlen={}, plen={})",
            htype,
            ptype,
            hlen,
            plen
        );
        return;
    }

    let sender_mac = MacAddr([data[8], data[9], data[10], data[11], data[12], data[13]]);
    let sender_ip = Ipv4Addr([data[14], data[15], data[16], data[17]]);
    let target_ip = Ipv4Addr([data[24], data[25], data[26], data[27]]);

    let dev = handle.index();
    let our_ip = dev_ip(dev);
    let for_us = !our_ip.is_unspecified() && target_ip == our_ip;

    // RFC 826 merge rule: refresh any known sender, learn a new one only from
    // an ARP addressed to us.
    let current_ms = slopos_kernel_services::clock::monotonic_ns() / 1_000_000;
    let action = match NEIGHBOR_CACHE.update(dev, sender_ip, sender_mac, current_ms) {
        Some(action) => action,
        None if for_us => NEIGHBOR_CACHE.insert_or_update(dev, sender_ip, sender_mac, current_ms),
        None => NeighborAction::None,
    };
    execute_neighbor_action(action);

    match oper {
        ARP_OPER_REPLY => {
            klog_debug!(
                "arp: reply from {} ({}) on dev {}",
                sender_ip,
                sender_mac,
                dev
            );
        }
        ARP_OPER_REQUEST if for_us => {
            klog_debug!(
                "arp: request for our IP {} from {} ({}), sending reply",
                target_ip,
                sender_ip,
                sender_mac
            );
            send_reply(handle, sender_ip, sender_mac);
        }
        ARP_OPER_REQUEST => {}
        _ => {
            klog_debug!("arp: unknown opcode {}", oper);
        }
    }
}

fn send_reply(handle: &DeviceHandle, target_ip: Ipv4Addr, target_mac: MacAddr) {
    let our_mac = handle.mac();
    let our_ip = dev_ip(handle.index());

    let Some(mut pkt) = PacketBuf::alloc() else {
        klog_debug!("arp: send_reply — pool exhausted");
        return;
    };

    let eth = match pkt.push_header(ETH_HEADER_LEN) {
        Ok(h) => h,
        Err(_) => return,
    };
    eth[0..ETH_ADDR_LEN].copy_from_slice(&target_mac.0);
    eth[ETH_ADDR_LEN..ETH_ADDR_LEN * 2].copy_from_slice(&our_mac.0);
    eth[ETH_ADDR_LEN * 2..ETH_HEADER_LEN].copy_from_slice(&EtherType::Arp.to_be_bytes());

    let mut arp_data = [0u8; ARP_HEADER_LEN];
    arp_data[0..2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    arp_data[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    arp_data[4] = ARP_HLEN_ETHERNET;
    arp_data[5] = ARP_PLEN_IPV4;
    arp_data[6..8].copy_from_slice(&ARP_OPER_REPLY.to_be_bytes());
    arp_data[8..14].copy_from_slice(&our_mac.0);
    arp_data[14..18].copy_from_slice(&our_ip.0);
    arp_data[18..24].copy_from_slice(&target_mac.0);
    arp_data[24..28].copy_from_slice(&target_ip.0);

    if pkt.append(&arp_data).is_err() {
        return;
    }

    klog_debug!(
        "arp: sending reply to {} ({}) on dev {}",
        target_ip,
        target_mac,
        handle.index()
    );
    if let Err(e) = handle.tx(pkt) {
        klog_debug!("arp: send_reply tx failed: {}", e);
    }
}

/// Perform the I/O the neighbor cache deferred so TX never runs under its lock,
/// on the device the neighbour belongs to.
pub fn execute_neighbor_action(action: NeighborAction) {
    match action {
        NeighborAction::SendArpRequest { dev, target_ip } => {
            send_request(dev, target_ip);
        }
        NeighborAction::FlushPending {
            packets,
            dst_mac,
            dev,
        } => {
            for mut pkt in packets {
                set_dst_mac_in_eth_header(&mut pkt, dst_mac);
                if let Err(e) = DEVICE_REGISTRY.tx_by_index(dev, pkt) {
                    klog_debug!("arp: flush tx failed: {}", e);
                }
            }
        }
        NeighborAction::None => {}
    }
}

/// Set the destination MAC, with the head at the start of the Ethernet header.
pub fn set_dst_mac_in_eth_header(pkt: &mut PacketBuf, mac: MacAddr) {
    let data = pkt.payload_mut();
    if data.len() >= ETH_ADDR_LEN {
        data[..ETH_ADDR_LEN].copy_from_slice(&mac.0);
    }
}

/// Set the source MAC, with the head at the start of the Ethernet header.
pub fn set_src_mac_in_eth_header(pkt: &mut PacketBuf, mac: MacAddr) {
    let data = pkt.payload_mut();
    if data.len() >= ETH_ADDR_LEN * 2 {
        data[ETH_ADDR_LEN..ETH_ADDR_LEN * 2].copy_from_slice(&mac.0);
    }
}

/// `dev`'s IPv4 address, or `UNSPECIFIED` while it has none; callers check
/// before claiming it in a reply.
fn dev_ip(dev: DevIndex) -> Ipv4Addr {
    super::iface::our_ip(dev).unwrap_or(Ipv4Addr::UNSPECIFIED)
}

/// Send an ARP request for `target_ip` out `dev`.
pub fn send_request(dev: DevIndex, target_ip: Ipv4Addr) {
    let our_mac = match DEVICE_REGISTRY.mac_by_index(dev) {
        Some(mac) => mac,
        None => {
            klog_debug!("arp: send_request — no device {}", dev);
            return;
        }
    };
    let our_ip = dev_ip(dev);

    let Some(mut pkt) = PacketBuf::alloc() else {
        klog_debug!("arp: send_request — pool exhausted");
        return;
    };

    let eth = match pkt.push_header(ETH_HEADER_LEN) {
        Ok(h) => h,
        Err(_) => {
            klog_debug!("arp: send_request — insufficient headroom");
            return;
        }
    };
    eth[0..ETH_ADDR_LEN].copy_from_slice(&MacAddr::BROADCAST.0);
    eth[ETH_ADDR_LEN..ETH_ADDR_LEN * 2].copy_from_slice(&our_mac.0);
    eth[ETH_ADDR_LEN * 2..ETH_HEADER_LEN].copy_from_slice(&EtherType::Arp.to_be_bytes());

    let mut arp_data = [0u8; ARP_HEADER_LEN];
    arp_data[0..2].copy_from_slice(&ARP_HTYPE_ETHERNET.to_be_bytes());
    arp_data[2..4].copy_from_slice(&ARP_PTYPE_IPV4.to_be_bytes());
    arp_data[4] = ARP_HLEN_ETHERNET;
    arp_data[5] = ARP_PLEN_IPV4;
    arp_data[6..8].copy_from_slice(&ARP_OPER_REQUEST.to_be_bytes());
    arp_data[8..14].copy_from_slice(&our_mac.0);
    arp_data[14..18].copy_from_slice(&our_ip.0);
    arp_data[18..24].copy_from_slice(&MacAddr::ZERO.0);
    arp_data[24..28].copy_from_slice(&target_ip.0);

    if pkt.append(&arp_data).is_err() {
        klog_debug!("arp: send_request — append failed");
        return;
    }

    klog_debug!("arp: sending request for {} on dev {}", target_ip, dev);
    if let Err(e) = DEVICE_REGISTRY.tx_by_index(dev, pkt) {
        klog_debug!("arp: send_request tx failed: {}", e);
    }
}
