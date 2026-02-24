use core::cmp;
use core::ffi::c_int;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::net::{
    USER_NET_MEMBER_FLAG_ARP, USER_NET_MEMBER_FLAG_IPV4, UserNetInfo, UserNetMember,
};
use slopos_lib::{InitFlag, IrqMutex, klog_debug, klog_info};

use crate::net::{arp, dhcp, ethernet, ipv4};
use crate::pci::{PciDeviceInfo, PciDriver, pci_register_driver};
use crate::virtio::{
    self, VIRTQ_DESC_F_WRITE, VirtioMmioCaps,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        set_driver_ok,
    },
    queue::{self, DEFAULT_QUEUE_SIZE, VirtqDesc, Virtqueue},
};

use slopos_mm::page_alloc::OwnedPageFrame;

pub const VIRTIO_NET_DEVICE_ID_LEGACY: u16 = 0x1000;
pub const VIRTIO_NET_DEVICE_ID_MODERN: u16 = 0x1041;

const VIRTIO_NET_QUEUE_RX: u16 = 0;
const VIRTIO_NET_QUEUE_TX: u16 = 1;

const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_F_MTU: u64 = 1 << 3;

const VIRTIO_NET_S_LINK_UP: u16 = 1;

const DEV_CFG_MAC_OFFSET: usize = 0x00;
const DEV_CFG_STATUS_OFFSET: usize = 0x06;
const DEV_CFG_MTU_OFFSET: usize = 0x0A;

const REQUEST_TIMEOUT_SPINS: u32 = 1_000_000;
const DEFAULT_MTU: u16 = 1500;
const PACKET_BUFFER_SIZE: usize = 2048;
const MAX_NET_MEMBERS: usize = 32;

const UDP_HEADER_LEN: usize = 8;

const DHCP_RX_MAX_POLLS: usize = 64;

static DHCP_XID_COUNTER: AtomicU32 = AtomicU32::new(0x534c_4f50);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioNetHdrV1 {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VirtioNetDevice {
    rx_queue: Virtqueue,
    tx_queue: Virtqueue,
    negotiated_features: u64,
    mac: [u8; 6],
    mtu: u16,
    ready: bool,
}

impl VirtioNetDevice {
    const fn new() -> Self {
        Self {
            rx_queue: Virtqueue::new(),
            tx_queue: Virtqueue::new(),
            negotiated_features: 0,
            mac: [0; 6],
            mtu: DEFAULT_MTU,
            ready: false,
        }
    }
}

struct VirtioNetState {
    device: VirtioNetDevice,
    caps: VirtioMmioCaps,
    ipv4_addr: [u8; 4],
    subnet_mask: [u8; 4],
    router: [u8; 4],
    dns: [u8; 4],
    members: [UserNetMember; MAX_NET_MEMBERS],
    member_count: usize,
}

impl VirtioNetState {
    const fn new() -> Self {
        Self {
            device: VirtioNetDevice::new(),
            caps: VirtioMmioCaps::empty(),
            ipv4_addr: [0; 4],
            subnet_mask: [0; 4],
            router: [0; 4],
            dns: [0; 4],
            members: [UserNetMember {
                ipv4: [0; 4],
                mac: [0; 6],
                flags: 0,
            }; MAX_NET_MEMBERS],
            member_count: 0,
        }
    }
}

static DEVICE_CLAIMED: InitFlag = InitFlag::new();
static VIRTIO_NET_STATE: IrqMutex<VirtioNetState> = IrqMutex::new(VirtioNetState::new());

fn virtio_net_match(info: *const PciDeviceInfo, _context: *mut core::ffi::c_void) -> bool {
    if info.is_null() {
        return false;
    }

    let info = unsafe { &*info };
    if info.vendor_id != PCI_VENDOR_ID_VIRTIO {
        return false;
    }

    info.device_id == VIRTIO_NET_DEVICE_ID_LEGACY || info.device_id == VIRTIO_NET_DEVICE_ID_MODERN
}

fn read_mac(caps: &VirtioMmioCaps, negotiated_features: u64) -> [u8; 6] {
    if (negotiated_features & VIRTIO_NET_F_MAC) == 0
        || !caps.has_device_cfg()
        || caps.device_cfg_len < 6
    {
        return [0; 6];
    }

    let mut mac = [0u8; 6];
    let mut i = 0usize;
    while i < mac.len() {
        mac[i] = caps.device_cfg.read::<u8>(DEV_CFG_MAC_OFFSET + i);
        i += 1;
    }
    mac
}

fn read_mtu(caps: &VirtioMmioCaps, negotiated_features: u64) -> u16 {
    if (negotiated_features & VIRTIO_NET_F_MTU) == 0
        || !caps.has_device_cfg()
        || caps.device_cfg_len < (DEV_CFG_MTU_OFFSET as u32 + 2)
    {
        return DEFAULT_MTU;
    }
    caps.device_cfg.read::<u16>(DEV_CFG_MTU_OFFSET)
}

fn link_is_up(state: &VirtioNetState) -> bool {
    if !state.device.ready {
        return false;
    }

    if (state.device.negotiated_features & VIRTIO_NET_F_STATUS) == 0
        || !state.caps.has_device_cfg()
        || state.caps.device_cfg_len < (DEV_CFG_STATUS_OFFSET as u32 + 2)
    {
        return true;
    }

    (state.caps.device_cfg.read::<u16>(DEV_CFG_STATUS_OFFSET) & VIRTIO_NET_S_LINK_UP) != 0
}

fn add_or_update_member(state: &mut VirtioNetState, mac: [u8; 6], ipv4: [u8; 4], flag: u16) {
    if mac == [0; 6] {
        return;
    }

    let mut idx = 0usize;
    while idx < state.member_count {
        if state.members[idx].mac == mac || (ipv4 != [0; 4] && state.members[idx].ipv4 == ipv4) {
            state.members[idx].mac = mac;
            if ipv4 != [0; 4] {
                state.members[idx].ipv4 = ipv4;
            }
            state.members[idx].flags |= flag;
            return;
        }
        idx += 1;
    }

    if state.member_count < state.members.len() {
        state.members[state.member_count] = UserNetMember {
            ipv4,
            mac,
            flags: flag,
        };
        state.member_count += 1;
    }
}

fn sniff_frame_for_members(state: &mut VirtioNetState, frame: &[u8]) {
    if frame.len() < ethernet::ETH_HEADER_LEN {
        return;
    }

    let src_mac = [frame[6], frame[7], frame[8], frame[9], frame[10], frame[11]];
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    if ethertype == ethernet::ETHERTYPE_ARP {
        if frame.len() < ethernet::ETH_HEADER_LEN + arp::ARP_HEADER_LEN {
            return;
        }
        let arp = &frame[ethernet::ETH_HEADER_LEN..ethernet::ETH_HEADER_LEN + arp::ARP_HEADER_LEN];
        let htype = u16::from_be_bytes([arp[0], arp[1]]);
        let ptype = u16::from_be_bytes([arp[2], arp[3]]);
        let hlen = arp[4];
        let plen = arp[5];
        if htype != arp::ARP_HTYPE_ETHERNET
            || ptype != arp::ARP_PTYPE_IPV4
            || hlen != arp::ARP_HLEN_ETHERNET
            || plen != arp::ARP_PLEN_IPV4
        {
            return;
        }
        let sender_mac = [arp[8], arp[9], arp[10], arp[11], arp[12], arp[13]];
        let sender_ip = [arp[14], arp[15], arp[16], arp[17]];
        add_or_update_member(state, sender_mac, sender_ip, USER_NET_MEMBER_FLAG_ARP);
        return;
    }

    if ethertype == ethernet::ETHERTYPE_IPV4 {
        if frame.len() < ethernet::ETH_HEADER_LEN + ipv4::IPV4_HEADER_LEN {
            return;
        }
        let src_ip = [frame[26], frame[27], frame[28], frame[29]];
        add_or_update_member(state, src_mac, src_ip, USER_NET_MEMBER_FLAG_IPV4);
    }
}

fn transmit_arp_request(state: &mut VirtioNetState, target_ip: [u8; 4]) -> bool {
    if !state.device.ready || !state.device.tx_queue.is_ready() {
        return false;
    }

    let tx_page = match OwnedPageFrame::alloc_zeroed() {
        Some(p) => p,
        None => return false,
    };

    let tx_virt = tx_page.as_mut_ptr::<u8>();
    let tx_phys = tx_page.phys_u64();

    let hdr_len = size_of::<VirtioNetHdrV1>();
    let frame_len = ethernet::ETH_HEADER_LEN + arp::ARP_HEADER_LEN;
    let total_len = hdr_len + frame_len;

    if total_len > PACKET_BUFFER_SIZE {
        return false;
    }

    unsafe {
        *(tx_virt as *mut VirtioNetHdrV1) = VirtioNetHdrV1::default();

        let frame = core::slice::from_raw_parts_mut(tx_virt.add(hdr_len), frame_len);

        frame[0..ethernet::ETH_ADDR_LEN].copy_from_slice(&ethernet::ETH_BROADCAST);
        frame[ethernet::ETH_ADDR_LEN..ethernet::ETH_ADDR_LEN * 2]
            .copy_from_slice(&state.device.mac);
        frame[ethernet::ETH_ADDR_LEN * 2..ethernet::ETH_HEADER_LEN]
            .copy_from_slice(&ethernet::ETHERTYPE_ARP.to_be_bytes());

        frame[ethernet::ETH_HEADER_LEN..ethernet::ETH_HEADER_LEN + 2]
            .copy_from_slice(&arp::ARP_HTYPE_ETHERNET.to_be_bytes());
        frame[ethernet::ETH_HEADER_LEN + 2..ethernet::ETH_HEADER_LEN + 4]
            .copy_from_slice(&arp::ARP_PTYPE_IPV4.to_be_bytes());
        frame[ethernet::ETH_HEADER_LEN + 4] = arp::ARP_HLEN_ETHERNET;
        frame[ethernet::ETH_HEADER_LEN + 5] = arp::ARP_PLEN_IPV4;
        frame[ethernet::ETH_HEADER_LEN + 6..ethernet::ETH_HEADER_LEN + 8]
            .copy_from_slice(&arp::ARP_OPER_REQUEST.to_be_bytes());
        frame[ethernet::ETH_HEADER_LEN + 8..ethernet::ETH_HEADER_LEN + 14]
            .copy_from_slice(&state.device.mac);
        frame[ethernet::ETH_HEADER_LEN + 14..ethernet::ETH_HEADER_LEN + 18]
            .copy_from_slice(&state.ipv4_addr);
        frame[ethernet::ETH_HEADER_LEN + 18..ethernet::ETH_HEADER_LEN + 24]
            .copy_from_slice(&[0; ethernet::ETH_ADDR_LEN]);
        frame[ethernet::ETH_HEADER_LEN + 24..ethernet::ETH_HEADER_LEN + 28]
            .copy_from_slice(&target_ip);
    }

    state.device.tx_queue.write_desc(
        0,
        VirtqDesc {
            addr: tx_phys,
            len: total_len as u32,
            flags: 0,
            next: 0,
        },
    );

    state.device.tx_queue.submit(0);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.tx_queue,
        VIRTIO_NET_QUEUE_TX,
    );

    let sent = state
        .device
        .tx_queue
        .pop_used(REQUEST_TIMEOUT_SPINS)
        .is_some();
    if !sent {
        // Leak: device may still be DMA-ing; freeing would cause use-after-free
        let _ = tx_page.into_phys();
    }
    sent
}

fn poll_one_rx_frame(state: &mut VirtioNetState, out_payload: Option<&mut [u8]>) -> Option<usize> {
    let rx_page = OwnedPageFrame::alloc_zeroed()?;
    let rx_virt = rx_page.as_mut_ptr::<u8>();
    let rx_phys = rx_page.phys_u64();

    state.device.rx_queue.write_desc(
        0,
        VirtqDesc {
            addr: rx_phys,
            len: PACKET_BUFFER_SIZE as u32,
            flags: VIRTQ_DESC_F_WRITE,
            next: 0,
        },
    );

    state.device.rx_queue.submit(0);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.rx_queue,
        VIRTIO_NET_QUEUE_RX,
    );

    let used = match state.device.rx_queue.pop_used(REQUEST_TIMEOUT_SPINS) {
        Some(u) => u,
        None => {
            // Leak: device may still be DMA-ing; freeing would cause use-after-free
            let _ = rx_page.into_phys();
            return None;
        }
    };
    let hdr_len = size_of::<VirtioNetHdrV1>();
    if (used.len as usize) <= hdr_len {
        return Some(0);
    }

    let payload_len = (used.len as usize) - hdr_len;
    let frame = unsafe { core::slice::from_raw_parts(rx_virt.add(hdr_len), payload_len) };
    sniff_frame_for_members(state, frame);

    if let Some(dst) = out_payload {
        let copy_len = cmp::min(payload_len, dst.len());
        unsafe {
            ptr::copy_nonoverlapping(frame.as_ptr(), dst.as_mut_ptr(), copy_len);
        }
        Some(copy_len)
    } else {
        Some(payload_len)
    }
}

fn transmit_dhcp_packet(state: &mut VirtioNetState, payload: &[u8]) -> bool {
    if !state.device.ready || !state.device.tx_queue.is_ready() {
        return false;
    }

    let tx_page = match OwnedPageFrame::alloc_zeroed() {
        Some(p) => p,
        None => return false,
    };

    let tx_virt = tx_page.as_mut_ptr::<u8>();
    let tx_phys = tx_page.phys_u64();

    let hdr_len = size_of::<VirtioNetHdrV1>();
    let eth_len = ethernet::ETH_HEADER_LEN;
    let ip_len = ipv4::IPV4_HEADER_LEN;
    let udp_len = UDP_HEADER_LEN;
    let frame_len = eth_len + ip_len + udp_len + payload.len();
    let total_len = hdr_len + frame_len;
    if total_len > PACKET_BUFFER_SIZE {
        return false;
    }

    unsafe {
        *(tx_virt as *mut VirtioNetHdrV1) = VirtioNetHdrV1::default();
        let frame = core::slice::from_raw_parts_mut(tx_virt.add(hdr_len), frame_len);

        frame[0..ethernet::ETH_ADDR_LEN].copy_from_slice(&ethernet::ETH_BROADCAST);
        frame[ethernet::ETH_ADDR_LEN..ethernet::ETH_ADDR_LEN * 2]
            .copy_from_slice(&state.device.mac);
        frame[ethernet::ETH_ADDR_LEN * 2..ethernet::ETH_HEADER_LEN]
            .copy_from_slice(&ethernet::ETHERTYPE_IPV4.to_be_bytes());

        let ip_off = ethernet::ETH_HEADER_LEN;
        frame[ip_off] = 0x45;
        frame[ip_off + 1] = 0;
        frame[ip_off + 2..ip_off + 4]
            .copy_from_slice(&((ip_len + udp_len + payload.len()) as u16).to_be_bytes());
        frame[ip_off + 4..ip_off + 6].copy_from_slice(&0u16.to_be_bytes());
        frame[ip_off + 6..ip_off + 8].copy_from_slice(&0u16.to_be_bytes());
        frame[ip_off + 8] = 64;
        frame[ip_off + 9] = ipv4::IPPROTO_UDP;
        frame[ip_off + 10..ip_off + 12].copy_from_slice(&0u16.to_be_bytes());
        frame[ip_off + 12..ip_off + 16].copy_from_slice(&[0; 4]);
        frame[ip_off + 16..ip_off + 20].copy_from_slice(&ipv4::IPV4_BROADCAST);
        let csum = ipv4::header_checksum(&frame[ip_off..ip_off + ip_len]);
        frame[ip_off + 10..ip_off + 12].copy_from_slice(&csum.to_be_bytes());

        let udp_off = ip_off + ip_len;
        frame[udp_off..udp_off + 2].copy_from_slice(&dhcp::UDP_PORT_CLIENT.to_be_bytes());
        frame[udp_off + 2..udp_off + 4].copy_from_slice(&dhcp::UDP_PORT_SERVER.to_be_bytes());
        frame[udp_off + 4..udp_off + 6]
            .copy_from_slice(&((udp_len + payload.len()) as u16).to_be_bytes());
        frame[udp_off + 6..udp_off + 8].copy_from_slice(&0u16.to_be_bytes());

        frame[udp_off + udp_len..udp_off + udp_len + payload.len()].copy_from_slice(payload);
    }

    state.device.tx_queue.write_desc(
        0,
        VirtqDesc {
            addr: tx_phys,
            len: total_len as u32,
            flags: 0,
            next: 0,
        },
    );

    state.device.tx_queue.submit(0);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.tx_queue,
        VIRTIO_NET_QUEUE_TX,
    );

    let sent = state
        .device
        .tx_queue
        .pop_used(REQUEST_TIMEOUT_SPINS)
        .is_some();
    if !sent {
        // Leak: device may still be DMA-ing; freeing would cause use-after-free
        let _ = tx_page.into_phys();
    }
    sent
}

fn parse_dhcp_reply(frame: &[u8], xid: u32, expected_type: u8) -> Option<dhcp::DhcpOffer> {
    if frame.len()
        < ethernet::ETH_HEADER_LEN + ipv4::IPV4_HEADER_LEN + UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN
    {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != ethernet::ETHERTYPE_IPV4 {
        return None;
    }

    let ip_off = ethernet::ETH_HEADER_LEN;
    let ihl = ((frame[ip_off] & 0x0f) as usize) * 4;
    if ihl < ipv4::IPV4_HEADER_LEN
        || frame.len() < ip_off + ihl + UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN
    {
        return None;
    }
    if frame[ip_off + 9] != ipv4::IPPROTO_UDP {
        return None;
    }

    let udp_off = ip_off + ihl;
    let src_port = u16::from_be_bytes([frame[udp_off], frame[udp_off + 1]]);
    let dst_port = u16::from_be_bytes([frame[udp_off + 2], frame[udp_off + 3]]);
    if src_port != dhcp::UDP_PORT_SERVER || dst_port != dhcp::UDP_PORT_CLIENT {
        return None;
    }

    let udp_len = u16::from_be_bytes([frame[udp_off + 4], frame[udp_off + 5]]) as usize;
    if udp_len < UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN || frame.len() < udp_off + udp_len {
        return None;
    }

    let payload = &frame[udp_off + UDP_HEADER_LEN..udp_off + udp_len];
    dhcp::parse_bootp_reply(payload, xid, expected_type)
}

fn wait_for_dhcp_reply(
    state: &mut VirtioNetState,
    xid: u32,
    expected_type: u8,
) -> Option<dhcp::DhcpOffer> {
    let mut frame = [0u8; PACKET_BUFFER_SIZE];
    let mut polls = 0usize;
    while polls < DHCP_RX_MAX_POLLS {
        let len = poll_one_rx_frame(state, Some(&mut frame))?;
        if len > 0
            && let Some(reply) = parse_dhcp_reply(&frame[..len], xid, expected_type)
        {
            return Some(reply);
        }
        polls += 1;
    }
    None
}

fn dhcp_acquire_lease(state: &mut VirtioNetState) -> Option<dhcp::DhcpLease> {
    let xid = DHCP_XID_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let mut packet = [0u8; 320];

    let discover_len = dhcp::build_discover(state.device.mac, xid, &mut packet);
    if !transmit_dhcp_packet(state, &packet[..discover_len]) {
        return None;
    }

    let offer = wait_for_dhcp_reply(state, xid, dhcp::MSG_OFFER)?;
    let request_len = dhcp::build_request(state.device.mac, xid, offer, &mut packet);
    if !transmit_dhcp_packet(state, &packet[..request_len]) {
        return None;
    }

    let ack = wait_for_dhcp_reply(state, xid, dhcp::MSG_ACK)?;
    let router = if ack.router == [0; 4] {
        offer.router
    } else {
        ack.router
    };
    let dns = if ack.dns == [0; 4] {
        offer.dns
    } else {
        ack.dns
    };
    let subnet_mask = if ack.subnet_mask == [0; 4] {
        offer.subnet_mask
    } else {
        ack.subnet_mask
    };

    let lease = dhcp::DhcpLease {
        ipv4: ack.yiaddr,
        subnet_mask,
        router,
        dns,
    };
    if lease.is_valid() { Some(lease) } else { None }
}

fn virtio_net_probe(info: *const PciDeviceInfo, _context: *mut core::ffi::c_void) -> c_int {
    if !DEVICE_CLAIMED.claim() {
        klog_debug!("virtio-net: already claimed");
        return -1;
    }

    let info = unsafe { &*info };
    klog_info!(
        "virtio-net: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(info);

    let caps = parse_capabilities(info);
    klog_debug!(
        "virtio-net: caps common={} notify={} isr={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.isr_cfg.is_mapped(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-net: missing common cfg");
        DEVICE_CLAIMED.reset();
        return -1;
    }

    if !caps.has_notify_cfg() {
        klog_info!("virtio-net: missing notify cfg");
        DEVICE_CLAIMED.reset();
        return -1;
    }

    let required_features = virtio::VIRTIO_F_VERSION_1;
    let optional_features =
        VIRTIO_NET_F_CSUM | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_MTU;
    let feat_result = negotiate_features(&caps, required_features, optional_features);
    if !feat_result.success {
        klog_info!("virtio-net: features negotiation failed");
        DEVICE_CLAIMED.reset();
        return -1;
    }

    let rx_queue =
        match queue::setup_queue(&caps.common_cfg, VIRTIO_NET_QUEUE_RX, DEFAULT_QUEUE_SIZE) {
            Some(q) => q,
            None => {
                klog_info!("virtio-net: rx queue setup failed");
                DEVICE_CLAIMED.reset();
                return -1;
            }
        };

    let tx_queue =
        match queue::setup_queue(&caps.common_cfg, VIRTIO_NET_QUEUE_TX, DEFAULT_QUEUE_SIZE) {
            Some(q) => q,
            None => {
                klog_info!("virtio-net: tx queue setup failed");
                DEVICE_CLAIMED.reset();
                return -1;
            }
        };

    let negotiated_features = feat_result.driver_features;
    let mac = read_mac(&caps, negotiated_features);
    let mtu = read_mtu(&caps, negotiated_features);

    set_driver_ok(&caps);

    {
        let mut state = VIRTIO_NET_STATE.lock();
        state.device = VirtioNetDevice {
            rx_queue,
            tx_queue,
            negotiated_features,
            mac,
            mtu,
            ready: true,
        };
        state.caps = caps;
        state.ipv4_addr = [0; 4];
        state.subnet_mask = [0; 4];
        state.router = [0; 4];
        state.dns = [0; 4];

        if let Some(lease) = dhcp_acquire_lease(&mut state) {
            state.ipv4_addr = lease.ipv4;
            state.subnet_mask = lease.subnet_mask;
            state.router = lease.router;
            state.dns = lease.dns;
            klog_info!(
                "virtio-net: DHCP lease ip={}.{}.{}.{} gw={}.{}.{}.{} dns={}.{}.{}.{}",
                lease.ipv4[0],
                lease.ipv4[1],
                lease.ipv4[2],
                lease.ipv4[3],
                lease.router[0],
                lease.router[1],
                lease.router[2],
                lease.router[3],
                lease.dns[0],
                lease.dns[1],
                lease.dns[2],
                lease.dns[3]
            );
        } else {
            klog_info!("virtio-net: DHCP lease unavailable");
        }
    }

    klog_info!(
        "virtio-net: ready mtu={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mtu,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    0
}

static VIRTIO_NET_DRIVER: PciDriver = PciDriver {
    name: b"virtio-net\0".as_ptr(),
    match_fn: Some(virtio_net_match),
    probe: Some(virtio_net_probe),
    context: ptr::null_mut(),
};

pub fn virtio_net_register_driver() {
    if pci_register_driver(&VIRTIO_NET_DRIVER) != 0 {
        klog_info!("virtio-net: driver registration failed");
    }
}

pub fn virtio_net_is_ready() -> bool {
    VIRTIO_NET_STATE.lock().device.ready
}

pub fn virtio_net_link_up() -> bool {
    let state = VIRTIO_NET_STATE.lock();
    link_is_up(&state)
}

pub fn virtio_net_scan_members(out: *mut UserNetMember, max: usize, active_probe: bool) -> usize {
    if out.is_null() || max == 0 {
        return 0;
    }

    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || !link_is_up(&state) {
        return 0;
    }

    let self_mac = state.device.mac;
    let self_ipv4 = state.ipv4_addr;
    if self_ipv4 != [0; 4] {
        add_or_update_member(&mut state, self_mac, self_ipv4, USER_NET_MEMBER_FLAG_IPV4);
    }

    if active_probe {
        let mut targets = [[0u8; 4]; 3];
        let mut target_count = 0usize;

        if state.router != [0; 4] {
            targets[target_count] = state.router;
            target_count += 1;
        }

        if state.ipv4_addr != [0; 4] && target_count < targets.len() {
            let mut t = state.ipv4_addr;
            t[3] = 2;
            targets[target_count] = t;
            target_count += 1;
        }

        if state.ipv4_addr != [0; 4] && target_count < targets.len() {
            let mut t = state.ipv4_addr;
            t[3] = 3;
            targets[target_count] = t;
            target_count += 1;
        }

        let mut i = 0usize;
        while i < target_count {
            let _ = transmit_arp_request(&mut state, targets[i]);
            let _ = poll_one_rx_frame(&mut state, None);
            i += 1;
        }

        let mut drain = 0usize;
        while drain < 8 {
            if poll_one_rx_frame(&mut state, None).is_none() {
                break;
            }
            drain += 1;
        }
    }

    let copy_count = cmp::min(max, state.member_count);
    unsafe {
        ptr::copy_nonoverlapping(state.members.as_ptr(), out, copy_count);
    }
    copy_count
}

pub fn virtio_net_queue_sizes() -> Option<(u16, u16)> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    Some((state.device.rx_queue.size, state.device.tx_queue.size))
}

pub fn virtio_net_mac() -> Option<[u8; 6]> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    Some(state.device.mac)
}

pub fn virtio_net_mtu() -> Option<u16> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }
    Some(state.device.mtu)
}

pub fn virtio_net_ipv4_addr() -> Option<[u8; 4]> {
    let state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || state.ipv4_addr == [0; 4] {
        return None;
    }
    Some(state.ipv4_addr)
}

pub fn virtio_net_get_info(out: &mut UserNetInfo) {
    let state = VIRTIO_NET_STATE.lock();
    out.nic_ready = if state.device.ready { 1 } else { 0 };
    out.link_up = if state.device.ready && link_is_up(&state) {
        1
    } else {
        0
    };
    out.mac = state.device.mac;
    out.mtu = state.device.mtu;
    out.ipv4 = state.ipv4_addr;
    out.subnet_mask = state.subnet_mask;
    out.gateway = state.router;
    out.dns = state.dns;
}

pub fn virtio_net_transmit(packet: &[u8]) -> bool {
    if packet.is_empty() {
        return true;
    }

    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return false;
    }

    if !link_is_up(&state) {
        return false;
    }

    let hdr_len = size_of::<VirtioNetHdrV1>();
    if packet.len() + hdr_len > PACKET_BUFFER_SIZE {
        return false;
    }

    let tx_page = match OwnedPageFrame::alloc_zeroed() {
        Some(p) => p,
        None => return false,
    };

    let tx_virt = tx_page.as_mut_ptr::<u8>();
    let tx_phys = tx_page.phys_u64();

    let hdr_ptr = tx_virt as *mut VirtioNetHdrV1;
    unsafe {
        *hdr_ptr = VirtioNetHdrV1::default();
        ptr::copy_nonoverlapping(packet.as_ptr(), tx_virt.add(hdr_len), packet.len());
    }

    state.device.tx_queue.write_desc(
        0,
        VirtqDesc {
            addr: tx_phys,
            len: (hdr_len + packet.len()) as u32,
            flags: 0,
            next: 0,
        },
    );

    state.device.tx_queue.submit(0);
    queue::notify_queue(
        &state.caps.notify_cfg,
        state.caps.notify_off_multiplier,
        &state.device.tx_queue,
        VIRTIO_NET_QUEUE_TX,
    );

    let sent = state
        .device
        .tx_queue
        .pop_used(REQUEST_TIMEOUT_SPINS)
        .is_some();
    if !sent {
        // Leak: device may still be DMA-ing; freeing would cause use-after-free
        let _ = tx_page.into_phys();
    }
    sent
}

pub fn virtio_net_receive(buffer: &mut [u8]) -> Option<usize> {
    if buffer.is_empty() {
        return Some(0);
    }

    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready {
        return None;
    }

    if !link_is_up(&state) {
        return None;
    }

    poll_one_rx_frame(&mut state, Some(buffer))
}
