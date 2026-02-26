use core::ffi::c_int;
use core::mem::size_of;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::net::{
    USER_NET_MEMBER_FLAG_ARP, USER_NET_MEMBER_FLAG_IPV4, UserNetInfo, UserNetMember,
};
use slopos_lib::{InitFlag, IrqMutex, klog_debug, klog_info};

use crate::net::{self, dhcp};
use crate::pci::{PciDeviceInfo, PciDriver, pci_register_driver};
use crate::virtio::{
    self, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_WRITE, VirtioMmioCaps, VirtioMsixState,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        register_irq_handlers, set_driver_ok, setup_interrupts,
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
    msix_state: Option<VirtioMsixState>,
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
            msix_state: None,
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

// =============================================================================
// Device configuration helpers
// =============================================================================

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
    for (i, byte) in mac.iter_mut().enumerate() {
        *byte = caps.device_cfg.read::<u8>(DEV_CFG_MAC_OFFSET + i);
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

// =============================================================================
// Network member tracking
// =============================================================================

fn add_or_update_member(state: &mut VirtioNetState, mac: [u8; 6], ipv4: [u8; 4], flag: u16) {
    if mac == [0; 6] {
        return;
    }

    for entry in &mut state.members[..state.member_count] {
        if entry.mac == mac || (ipv4 != [0; 4] && entry.ipv4 == ipv4) {
            entry.mac = mac;
            if ipv4 != [0; 4] {
                entry.ipv4 = ipv4;
            }
            entry.flags |= flag;
            return;
        }
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

/// Inspect an incoming Ethernet frame and record any new MAC/IP associations.
fn sniff_frame_for_members(state: &mut VirtioNetState, frame: &[u8]) {
    if frame.len() < net::ETH_HEADER_LEN {
        return;
    }

    let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);

    if ethertype == net::ETHERTYPE_ARP {
        if frame.len() < net::ETH_HEADER_LEN + net::ARP_HEADER_LEN {
            return;
        }
        let arp = &frame[net::ETH_HEADER_LEN..net::ETH_HEADER_LEN + net::ARP_HEADER_LEN];
        let htype = u16::from_be_bytes([arp[0], arp[1]]);
        let ptype = u16::from_be_bytes([arp[2], arp[3]]);
        if htype != net::ARP_HTYPE_ETHERNET
            || ptype != net::ARP_PTYPE_IPV4
            || arp[4] != net::ARP_HLEN_ETHERNET
            || arp[5] != net::ARP_PLEN_IPV4
        {
            return;
        }
        let sender_mac: [u8; 6] = arp[8..14].try_into().unwrap();
        let sender_ip: [u8; 4] = arp[14..18].try_into().unwrap();
        add_or_update_member(state, sender_mac, sender_ip, USER_NET_MEMBER_FLAG_ARP);
        return;
    }

    if ethertype == net::ETHERTYPE_IPV4 {
        if frame.len() < net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN {
            return;
        }
        let src_ip: [u8; 4] = frame[26..30].try_into().unwrap();
        add_or_update_member(state, src_mac, src_ip, USER_NET_MEMBER_FLAG_IPV4);
    }
}

// =============================================================================
// Virtqueue I/O helpers
// =============================================================================

/// Submit a filled TX buffer to the device and wait for completion.
///
/// On timeout the backing page is intentionally leaked — the device may still
/// be DMA-ing into it, so freeing it would cause a use-after-free.
fn submit_tx(state: &mut VirtioNetState, page: OwnedPageFrame, total_len: u32) -> bool {
    state.device.tx_queue.write_desc(
        0,
        VirtqDesc {
            addr: page.phys_u64(),
            len: total_len,
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
        let _ = page.into_phys();
    }
    sent
}

/// Allocate a page and write the virtio-net header at the start.
/// Returns `(page, buffer_start)` where `buffer_start` points just past the header.
fn alloc_tx_page() -> Option<OwnedPageFrame> {
    let page = OwnedPageFrame::alloc_zeroed()?;
    unsafe {
        *(page.as_mut_ptr::<VirtioNetHdrV1>()) = VirtioNetHdrV1::default();
    }
    Some(page)
}

// =============================================================================
// ARP
// =============================================================================

fn transmit_arp_request(state: &mut VirtioNetState, target_ip: [u8; 4]) -> bool {
    if !state.device.ready || !state.device.tx_queue.is_ready() {
        return false;
    }

    let Some(tx_page) = alloc_tx_page() else {
        return false;
    };

    let hdr_len = size_of::<VirtioNetHdrV1>();
    let frame_len = net::ETH_HEADER_LEN + net::ARP_HEADER_LEN;
    let total_len = hdr_len + frame_len;

    if total_len > PACKET_BUFFER_SIZE {
        return false;
    }

    unsafe {
        let frame =
            core::slice::from_raw_parts_mut(tx_page.as_mut_ptr::<u8>().add(hdr_len), frame_len);

        // Ethernet header
        frame[0..net::ETH_ADDR_LEN].copy_from_slice(&net::ETH_BROADCAST);
        frame[net::ETH_ADDR_LEN..net::ETH_ADDR_LEN * 2].copy_from_slice(&state.device.mac);
        frame[net::ETH_ADDR_LEN * 2..net::ETH_HEADER_LEN]
            .copy_from_slice(&net::ETHERTYPE_ARP.to_be_bytes());

        // ARP payload
        let a = net::ETH_HEADER_LEN;
        frame[a..a + 2].copy_from_slice(&net::ARP_HTYPE_ETHERNET.to_be_bytes());
        frame[a + 2..a + 4].copy_from_slice(&net::ARP_PTYPE_IPV4.to_be_bytes());
        frame[a + 4] = net::ARP_HLEN_ETHERNET;
        frame[a + 5] = net::ARP_PLEN_IPV4;
        frame[a + 6..a + 8].copy_from_slice(&net::ARP_OPER_REQUEST.to_be_bytes());
        frame[a + 8..a + 14].copy_from_slice(&state.device.mac);
        frame[a + 14..a + 18].copy_from_slice(&state.ipv4_addr);
        frame[a + 18..a + 24].copy_from_slice(&[0; net::ETH_ADDR_LEN]);
        frame[a + 24..a + 28].copy_from_slice(&target_ip);
    }

    submit_tx(state, tx_page, total_len as u32)
}

// =============================================================================
// Receive path
// =============================================================================

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
            // Intentional leak: device may still be DMA-ing
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
        let copy_len = payload_len.min(dst.len());
        dst[..copy_len].copy_from_slice(&frame[..copy_len]);
        Some(copy_len)
    } else {
        Some(payload_len)
    }
}

// =============================================================================
// DHCP client
// =============================================================================

fn transmit_dhcp_packet(state: &mut VirtioNetState, payload: &[u8]) -> bool {
    if !state.device.ready || !state.device.tx_queue.is_ready() {
        return false;
    }

    let Some(tx_page) = alloc_tx_page() else {
        return false;
    };

    let hdr_len = size_of::<VirtioNetHdrV1>();
    let frame_len = net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();
    let total_len = hdr_len + frame_len;
    if total_len > PACKET_BUFFER_SIZE {
        return false;
    }

    unsafe {
        let frame =
            core::slice::from_raw_parts_mut(tx_page.as_mut_ptr::<u8>().add(hdr_len), frame_len);

        // Ethernet header
        frame[0..net::ETH_ADDR_LEN].copy_from_slice(&net::ETH_BROADCAST);
        frame[net::ETH_ADDR_LEN..net::ETH_ADDR_LEN * 2].copy_from_slice(&state.device.mac);
        frame[net::ETH_ADDR_LEN * 2..net::ETH_HEADER_LEN]
            .copy_from_slice(&net::ETHERTYPE_IPV4.to_be_bytes());

        // IPv4 header
        let ip = net::ETH_HEADER_LEN;
        let ip_total = net::IPV4_HEADER_LEN + UDP_HEADER_LEN + payload.len();
        frame[ip] = 0x45; // version=4, IHL=5
        frame[ip + 1] = 0; // DSCP/ECN
        frame[ip + 2..ip + 4].copy_from_slice(&(ip_total as u16).to_be_bytes());
        frame[ip + 4..ip + 6].copy_from_slice(&0u16.to_be_bytes()); // identification
        frame[ip + 6..ip + 8].copy_from_slice(&0u16.to_be_bytes()); // flags/fragment
        frame[ip + 8] = 64; // TTL
        frame[ip + 9] = net::IPPROTO_UDP;
        frame[ip + 10..ip + 12].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
        frame[ip + 12..ip + 16].copy_from_slice(&[0; 4]); // src: 0.0.0.0
        frame[ip + 16..ip + 20].copy_from_slice(&net::IPV4_BROADCAST);
        let csum = net::ipv4_header_checksum(&frame[ip..ip + net::IPV4_HEADER_LEN]);
        frame[ip + 10..ip + 12].copy_from_slice(&csum.to_be_bytes());

        // UDP header
        let udp = ip + net::IPV4_HEADER_LEN;
        let udp_total = UDP_HEADER_LEN + payload.len();
        frame[udp..udp + 2].copy_from_slice(&dhcp::UDP_PORT_CLIENT.to_be_bytes());
        frame[udp + 2..udp + 4].copy_from_slice(&dhcp::UDP_PORT_SERVER.to_be_bytes());
        frame[udp + 4..udp + 6].copy_from_slice(&(udp_total as u16).to_be_bytes());
        frame[udp + 6..udp + 8].copy_from_slice(&0u16.to_be_bytes()); // checksum (optional)

        // DHCP payload
        frame[udp + UDP_HEADER_LEN..udp + UDP_HEADER_LEN + payload.len()].copy_from_slice(payload);
    }

    submit_tx(state, tx_page, total_len as u32)
}

fn parse_dhcp_reply(frame: &[u8], xid: u32, expected_type: u8) -> Option<dhcp::DhcpOffer> {
    let min_len =
        net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN;
    if frame.len() < min_len {
        return None;
    }
    if u16::from_be_bytes([frame[12], frame[13]]) != net::ETHERTYPE_IPV4 {
        return None;
    }

    let ip_off = net::ETH_HEADER_LEN;
    let ihl = ((frame[ip_off] & 0x0f) as usize) * 4;
    if ihl < net::IPV4_HEADER_LEN
        || frame.len() < ip_off + ihl + UDP_HEADER_LEN + dhcp::BOOTP_HEADER_LEN
    {
        return None;
    }
    if frame[ip_off + 9] != net::IPPROTO_UDP {
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
    for _ in 0..DHCP_RX_MAX_POLLS {
        let len = poll_one_rx_frame(state, Some(&mut frame))?;
        if len > 0
            && let Some(reply) = parse_dhcp_reply(&frame[..len], xid, expected_type)
        {
            return Some(reply);
        }
    }
    None
}

/// Use the preferred value unless it's zeroed, in which case fall back.
fn or_fallback(preferred: [u8; 4], fallback: [u8; 4]) -> [u8; 4] {
    if preferred != [0; 4] {
        preferred
    } else {
        fallback
    }
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

    let lease = dhcp::DhcpLease {
        ipv4: ack.yiaddr,
        subnet_mask: or_fallback(ack.subnet_mask, offer.subnet_mask),
        router: or_fallback(ack.router, offer.router),
        dns: or_fallback(ack.dns, offer.dns),
    };
    if lease.is_valid() { Some(lease) } else { None }
}

// =============================================================================
// PCI probe
// =============================================================================

/// MSI-X / MSI interrupt handler for virtio-net.
///
/// The `ctx` pointer encodes the queue index (0 = RX, 1 = TX).  Completion is
/// detected by the polling loop, so this handler is intentionally minimal.
extern "C" fn virtio_net_irq_handler(
    _vector: u8,
    _frame: *mut slopos_lib::InterruptFrame,
    _ctx: *mut core::ffi::c_void,
) {
    // Intentional NOP — the polling path detects completion.
    // Future: signal a condvar / event to wake blocked RX/TX waiters.
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

    // --- MSI-X / MSI interrupt setup ---
    // Request 2 vectors: one for RX (queue 0), one for TX (queue 1).
    let (irq_mode, msix_state) = setup_interrupts(info, &caps, 2).unwrap_or_else(|msg| {
        panic!(
            "virtio-net: {}:{}.{} {}",
            info.bus, info.device, info.function, msg
        )
    });
    let rx_msix_entry = msix_state.as_ref().map_or(VIRTIO_MSI_NO_VECTOR, |s| {
        s.queue_msix_entry(VIRTIO_NET_QUEUE_RX)
    });
    let tx_msix_entry = msix_state.as_ref().map_or(VIRTIO_MSI_NO_VECTOR, |s| {
        s.queue_msix_entry(VIRTIO_NET_QUEUE_TX)
    });

    let Some(rx_queue) = queue::setup_queue(
        &caps.common_cfg,
        VIRTIO_NET_QUEUE_RX,
        DEFAULT_QUEUE_SIZE,
        rx_msix_entry,
    ) else {
        klog_info!("virtio-net: rx queue setup failed");
        DEVICE_CLAIMED.reset();
        return -1;
    };

    let Some(tx_queue) = queue::setup_queue(
        &caps.common_cfg,
        VIRTIO_NET_QUEUE_TX,
        DEFAULT_QUEUE_SIZE,
        tx_msix_entry,
    ) else {
        klog_info!("virtio-net: tx queue setup failed");
        DEVICE_CLAIMED.reset();
        return -1;
    };

    // Register MSI-X/MSI handlers (NOPs — completion detected by polling).
    let device_bdf =
        ((info.bus as u32) << 16) | ((info.device as u32) << 8) | (info.function as u32);
    register_irq_handlers(
        &irq_mode,
        msix_state.as_ref(),
        virtio_net_irq_handler,
        device_bdf,
    );

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
        state.msix_state = msix_state;
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
        "virtio-net: ready mtu={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} irq {:?}",
        mtu,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        irq_mode,
    );

    0
}
// =============================================================================
// Driver registration & public API
// =============================================================================

static VIRTIO_NET_DRIVER: PciDriver = PciDriver {
    name: b"virtio-net\0".as_ptr(),
    match_fn: Some(virtio_net_match),
    probe: Some(virtio_net_probe),
    context: core::ptr::null_mut(),
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

        // Probe .2 and .3 in the local subnet as simple neighbor discovery
        for last_octet in [2u8, 3] {
            if state.ipv4_addr != [0; 4] && target_count < targets.len() {
                let mut t = state.ipv4_addr;
                t[3] = last_octet;
                targets[target_count] = t;
                target_count += 1;
            }
        }

        for target in &targets[..target_count] {
            let _ = transmit_arp_request(&mut state, *target);
            let _ = poll_one_rx_frame(&mut state, None);
        }

        // Drain any remaining rx frames from the above probes
        for _ in 0..8 {
            if poll_one_rx_frame(&mut state, None).is_none() {
                break;
            }
        }
    }

    let copy_count = max.min(state.member_count);
    unsafe {
        core::ptr::copy_nonoverlapping(state.members.as_ptr(), out, copy_count);
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
    out.nic_ready = u8::from(state.device.ready);
    out.link_up = u8::from(state.device.ready && link_is_up(&state));
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
    if !state.device.ready || !link_is_up(&state) {
        return false;
    }

    let hdr_len = size_of::<VirtioNetHdrV1>();
    if packet.len() + hdr_len > PACKET_BUFFER_SIZE {
        return false;
    }

    let Some(tx_page) = alloc_tx_page() else {
        return false;
    };

    unsafe {
        core::ptr::copy_nonoverlapping(
            packet.as_ptr(),
            tx_page.as_mut_ptr::<u8>().add(hdr_len),
            packet.len(),
        );
    }

    submit_tx(&mut state, tx_page, (hdr_len + packet.len()) as u32)
}

pub fn virtio_net_receive(buffer: &mut [u8]) -> Option<usize> {
    if buffer.is_empty() {
        return Some(0);
    }

    let mut state = VIRTIO_NET_STATE.lock();
    if !state.device.ready || !link_is_up(&state) {
        return None;
    }

    poll_one_rx_frame(&mut state, Some(buffer))
}

// =============================================================================
// Test-only accessors
// =============================================================================

/// Return a snapshot of the MSI-X state for the claimed VirtIO-net device.
///
/// Only available in test builds (`itests` feature).  Returns `None` if the
/// device was not probed or MSI-X was not configured (i.e. MSI fallback).
#[cfg(feature = "itests")]
pub fn virtio_net_msix_state() -> Option<VirtioMsixState> {
    VIRTIO_NET_STATE.lock().msix_state
}
