//! ARP protocol handler — stub module for Phase 1.
//!
//! Phase 2 will implement the full neighbor cache state machine, dynamic ARP
//! request/reply handling, and timer-driven aging.  For now, this module
//! establishes the module boundary so that the ingress pipeline can dispatch
//! ARP frames here instead of handling them inline in the VirtIO driver.

use slopos_lib::klog_debug;

use super::packetbuf::PacketBuf;
use super::types::{DevIndex, Ipv4Addr};

/// Handle an incoming ARP frame.
///
/// **Phase 1 stub**: logs and drops.  Phase 2 will parse the ARP header,
/// update the neighbor cache, and reply to requests for our IP.
pub fn handle_rx(dev: DevIndex, pkt: PacketBuf) {
    klog_debug!(
        "arp: rx on dev {} ({} bytes) — stub, dropping",
        dev,
        pkt.len()
    );
    // PacketBuf is dropped automatically, returning its slot to the pool.
}

/// Send an ARP request for `target_ip` on device `dev`.
///
/// **Phase 1 stub**: no-op.  Phase 2 will build the ARP request frame,
/// allocate a [`PacketBuf`], and transmit via [`DeviceHandle::tx`].
pub fn send_request(_dev: DevIndex, _target_ip: Ipv4Addr) {
    klog_debug!("arp: send_request stub called");
}
