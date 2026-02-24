//! Network protocol constants and helpers.
//!
//! Protocol-level definitions shared across network drivers. DHCP client
//! logic lives in the [`dhcp`] submodule.

pub mod dhcp;

// =============================================================================
// Ethernet
// =============================================================================

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETH_HEADER_LEN: usize = 14;
pub const ETH_ADDR_LEN: usize = 6;
pub const ETH_BROADCAST: [u8; 6] = [0xff; 6];

// =============================================================================
// ARP (Ethernet + IPv4 only)
// =============================================================================

pub const ARP_HTYPE_ETHERNET: u16 = 1;
pub const ARP_PTYPE_IPV4: u16 = ETHERTYPE_IPV4;
pub const ARP_HLEN_ETHERNET: u8 = 6;
pub const ARP_PLEN_IPV4: u8 = 4;
pub const ARP_OPER_REQUEST: u16 = 1;
pub const ARP_OPER_REPLY: u16 = 2;
pub const ARP_HEADER_LEN: usize = 28;

// =============================================================================
// IPv4
// =============================================================================

pub const IPV4_HEADER_LEN: usize = 20;
pub const IPV4_BROADCAST: [u8; 4] = [255, 255, 255, 255];
pub const IPPROTO_UDP: u8 = 17;
pub const IPPROTO_ICMP: u8 = 1;

/// Compute the one's-complement checksum for an IPv4 header.
pub fn ipv4_header_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < header.len() {
        let word = u16::from_be_bytes([header[i], header[i + 1]]) as u32;
        sum = sum.wrapping_add(word);
        i += 2;
    }

    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }

    !(sum as u16)
}
