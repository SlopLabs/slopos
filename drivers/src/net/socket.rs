use core::cmp;

use slopos_abi::net::{AF_INET, MAX_SOCKETS, SOCK_DGRAM, SOCK_STREAM};
use slopos_abi::syscall::{
    ERRNO_EADDRINUSE, ERRNO_EAFNOSUPPORT, ERRNO_EAGAIN, ERRNO_ECONNREFUSED, ERRNO_EDESTADDRREQ,
    ERRNO_EFAULT, ERRNO_EINVAL, ERRNO_EISCONN, ERRNO_ENETUNREACH, ERRNO_ENOMEM, ERRNO_ENOTCONN,
    ERRNO_ENOTSOCK, ERRNO_EPROTONOSUPPORT, POLLERR, POLLHUP, POLLIN, POLLOUT,
};
use slopos_lib::{IrqMutex, WaitQueue};

use crate::net;
use crate::net::tcp::{
    self, MAX_CONNECTIONS, TCP_HEADER_LEN, TcpConnection, TcpError, TcpOutSegment, TcpState,
};
use crate::virtio_net;

const TCP_TX_MAX: usize = 1460;
pub const UDP_DGRAM_MAX_PAYLOAD: usize = 1472;
pub const UDP_RX_QUEUE_SIZE: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SocketState {
    Unbound,
    Bound,
    Listening,
    Connecting,
    Connected,
    Closed,
}

#[derive(Clone, Copy)]
pub struct UdpDatagram {
    pub src_ip: [u8; 4],
    pub src_port: u16,
    pub len: u16,
    pub data: [u8; UDP_DGRAM_MAX_PAYLOAD],
}

impl UdpDatagram {
    pub const fn empty() -> Self {
        Self {
            src_ip: [0; 4],
            src_port: 0,
            len: 0,
            data: [0; UDP_DGRAM_MAX_PAYLOAD],
        }
    }
}

pub struct UdpReceiveQueue {
    slots: [UdpDatagram; UDP_RX_QUEUE_SIZE],
    head: usize,
    len: usize,
}

impl UdpReceiveQueue {
    pub const fn new() -> Self {
        Self {
            slots: [UdpDatagram::empty(); UDP_RX_QUEUE_SIZE],
            head: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, dgram: &UdpDatagram) {
        if self.len == UDP_RX_QUEUE_SIZE {
            self.slots[self.head] = *dgram;
            self.head = (self.head + 1) % UDP_RX_QUEUE_SIZE;
            return;
        }

        let tail = (self.head + self.len) % UDP_RX_QUEUE_SIZE;
        self.slots[tail] = *dgram;
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<UdpDatagram> {
        if self.len == 0 {
            return None;
        }

        let dgram = self.slots[self.head];
        self.head = (self.head + 1) % UDP_RX_QUEUE_SIZE;
        self.len -= 1;
        Some(dgram)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

#[derive(Clone, Copy)]
pub struct KernelSocket {
    pub active: bool,
    pub domain: u16,
    pub sock_type: u16,
    pub protocol: u16,
    pub state: SocketState,
    pub local_ip: [u8; 4],
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
    pub tcp_idx: Option<usize>,
    pub process_id: u32,
    pub recv_wq_idx: u8,
    pub accept_wq_idx: u8,
    pub send_wq_idx: u8,
    pub recv_timeout_ms: u64,
    pub send_timeout_ms: u64,
    pub nonblocking: bool,
}

impl KernelSocket {
    const fn empty() -> Self {
        Self {
            active: false,
            domain: 0,
            sock_type: 0,
            protocol: 0,
            state: SocketState::Closed,
            local_ip: [0; 4],
            local_port: 0,
            remote_ip: [0; 4],
            remote_port: 0,
            tcp_idx: None,
            process_id: 0,
            recv_wq_idx: 0,
            accept_wq_idx: 0,
            send_wq_idx: 0,
            recv_timeout_ms: 0,
            send_timeout_ms: 0,
            nonblocking: true,
        }
    }
}

struct SocketTable {
    sockets: [KernelSocket; MAX_SOCKETS],
}

impl SocketTable {
    const fn new() -> Self {
        Self {
            sockets: [KernelSocket::empty(); MAX_SOCKETS],
        }
    }

    fn alloc_slot(&mut self) -> Option<u32> {
        for (idx, sock) in self.sockets.iter_mut().enumerate() {
            if !sock.active {
                *sock = KernelSocket::empty();
                sock.active = true;
                sock.state = SocketState::Unbound;
                sock.recv_wq_idx = idx as u8;
                sock.accept_wq_idx = idx as u8;
                sock.send_wq_idx = idx as u8;
                return Some(idx as u32);
            }
        }
        None
    }

    fn get(&self, idx: u32) -> Option<&KernelSocket> {
        self.sockets.get(idx as usize).filter(|s| s.active)
    }

    fn get_mut(&mut self, idx: u32) -> Option<&mut KernelSocket> {
        self.sockets.get_mut(idx as usize).filter(|s| s.active)
    }

    fn release(&mut self, idx: u32) {
        if let Some(sock) = self.sockets.get_mut(idx as usize) {
            *sock = KernelSocket::empty();
        }
    }

    fn tcp_idx_is_bound(&self, tcp_idx: usize) -> bool {
        self.sockets
            .iter()
            .any(|s| s.active && s.tcp_idx == Some(tcp_idx) && s.state != SocketState::Closed)
    }

    fn find_udp_socket(&self, dst_ip: [u8; 4], dst_port: u16) -> Option<u32> {
        for (idx, sock) in self.sockets.iter().enumerate() {
            if !sock.active
                || sock.sock_type != SOCK_DGRAM
                || !matches!(sock.state, SocketState::Bound | SocketState::Connected)
                || sock.local_port != dst_port
            {
                continue;
            }

            if sock.local_ip == dst_ip {
                return Some(idx as u32);
            }
        }

        for (idx, sock) in self.sockets.iter().enumerate() {
            if !sock.active
                || sock.sock_type != SOCK_DGRAM
                || !matches!(sock.state, SocketState::Bound | SocketState::Connected)
                || sock.local_port != dst_port
            {
                continue;
            }

            if sock.local_ip == [0; 4] {
                return Some(idx as u32);
            }
        }

        None
    }
}

static SOCKET_TABLE: IrqMutex<SocketTable> = IrqMutex::new(SocketTable::new());

static RECV_WQS: [WaitQueue; MAX_SOCKETS] = {
    const WAIT_QUEUE: WaitQueue = WaitQueue::new();
    [WAIT_QUEUE; MAX_SOCKETS]
};
static ACCEPT_WQS: [WaitQueue; MAX_SOCKETS] = {
    const WAIT_QUEUE: WaitQueue = WaitQueue::new();
    [WAIT_QUEUE; MAX_SOCKETS]
};
static SEND_WQS: [WaitQueue; MAX_SOCKETS] = {
    const WAIT_QUEUE: WaitQueue = WaitQueue::new();
    [WAIT_QUEUE; MAX_SOCKETS]
};
pub static UDP_RX_QUEUES: [IrqMutex<UdpReceiveQueue>; MAX_SOCKETS] =
    [const { IrqMutex::new(UdpReceiveQueue::new()) }; MAX_SOCKETS];

fn errno_i32(errno: u64) -> i32 {
    errno as i64 as i32
}

fn map_tcp_err(err: TcpError) -> i32 {
    match err {
        TcpError::NotFound => errno_i32(ERRNO_ENOTSOCK),
        TcpError::InvalidState => errno_i32(ERRNO_ENOTCONN),
        TcpError::AddrInUse => errno_i32(ERRNO_EADDRINUSE),
        TcpError::TableFull => errno_i32(ERRNO_ENOMEM),
        TcpError::ConnectionRefused => errno_i32(ERRNO_ECONNREFUSED),
        TcpError::ConnectionReset => errno_i32(ERRNO_ECONNREFUSED),
        TcpError::TimedOut => errno_i32(ERRNO_EAGAIN),
        TcpError::InvalidSegment => errno_i32(ERRNO_EINVAL),
    }
}

fn map_tcp_err_i64(err: TcpError) -> i64 {
    map_tcp_err(err) as i64
}

fn alloc_ephemeral_port(table: &SocketTable) -> Option<u16> {
    for port in 49152u16..=65535u16 {
        let used = table
            .sockets
            .iter()
            .any(|s| s.active && s.local_port == port && s.state != SocketState::Closed);
        if !used {
            return Some(port);
        }
    }
    None
}

fn be_port(port: u16) -> [u8; 2] {
    port.to_be_bytes()
}

fn write_tcp_segment(seg: &TcpOutSegment, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let opt_len = if seg.mss != 0 { 4usize } else { 0usize };
    let data_offset_words = ((TCP_HEADER_LEN + opt_len) / 4) as u8;
    let tcp_len = TCP_HEADER_LEN + opt_len + payload.len();
    if out.len() < tcp_len {
        return None;
    }

    let hdr = tcp::build_header(
        seg.tuple.local_port,
        seg.tuple.remote_port,
        seg.seq_num,
        seg.ack_num,
        seg.flags,
        seg.window_size,
        data_offset_words,
    );
    let hdr_len = tcp::write_header(&hdr, out)?;

    if seg.mss != 0 {
        let opt_start = TCP_HEADER_LEN;
        out[opt_start] = tcp::TCP_OPT_MSS;
        out[opt_start + 1] = tcp::TCP_OPT_MSS_LEN;
        out[opt_start + 2..opt_start + 4].copy_from_slice(&seg.mss.to_be_bytes());
    }

    out[hdr_len..hdr_len + payload.len()].copy_from_slice(payload);

    let checksum = tcp::tcp_checksum(seg.tuple.local_ip, seg.tuple.remote_ip, &out[..tcp_len]);
    out[16..18].copy_from_slice(&checksum.to_be_bytes());
    Some(tcp_len)
}

pub(crate) fn socket_send_tcp_segment(seg: &TcpOutSegment, payload: &[u8]) -> i32 {
    let src_mac = virtio_net::virtio_net_mac().unwrap_or([0; 6]);
    let dst_mac = [0xff; 6];

    let ip_total_len =
        net::IPV4_HEADER_LEN + TCP_HEADER_LEN + if seg.mss != 0 { 4 } else { 0 } + payload.len();
    let frame_len = net::ETH_HEADER_LEN + ip_total_len;
    let mut frame = [0u8; 1600];
    if frame_len > frame.len() {
        return errno_i32(ERRNO_EINVAL);
    }

    frame[0..6].copy_from_slice(&dst_mac);
    frame[6..12].copy_from_slice(&src_mac);
    frame[12..14].copy_from_slice(&net::ETHERTYPE_IPV4.to_be_bytes());

    let ip = net::ETH_HEADER_LEN;
    frame[ip] = 0x45;
    frame[ip + 1] = 0;
    frame[ip + 2..ip + 4].copy_from_slice(&(ip_total_len as u16).to_be_bytes());
    frame[ip + 4..ip + 6].copy_from_slice(&0u16.to_be_bytes());
    frame[ip + 6..ip + 8].copy_from_slice(&0u16.to_be_bytes());
    frame[ip + 8] = 64;
    frame[ip + 9] = net::IPPROTO_TCP;
    frame[ip + 10..ip + 12].copy_from_slice(&0u16.to_be_bytes());
    frame[ip + 12..ip + 16].copy_from_slice(&seg.tuple.local_ip);
    frame[ip + 16..ip + 20].copy_from_slice(&seg.tuple.remote_ip);
    let ip_csum = net::ipv4_header_checksum(&frame[ip..ip + net::IPV4_HEADER_LEN]);
    frame[ip + 10..ip + 12].copy_from_slice(&ip_csum.to_be_bytes());

    let tcp_start = ip + net::IPV4_HEADER_LEN;
    let tcp_len = match write_tcp_segment(seg, payload, &mut frame[tcp_start..]) {
        Some(n) => n,
        None => return errno_i32(ERRNO_EINVAL),
    };

    let total = net::ETH_HEADER_LEN + net::IPV4_HEADER_LEN + tcp_len;
    if !virtio_net::virtio_net_is_ready() {
        return 0;
    }

    if virtio_net::virtio_net_transmit(&frame[..total]) {
        0
    } else {
        0
    }
}

fn socket_wake_recv(sock_idx: u32) {
    RECV_WQS[sock_idx as usize].wake_all();
}

fn make_udp_datagram(src_ip: [u8; 4], src_port: u16, payload: &[u8]) -> UdpDatagram {
    let copy_len = cmp::min(payload.len(), UDP_DGRAM_MAX_PAYLOAD);
    let mut dgram = UdpDatagram::empty();
    dgram.src_ip = src_ip;
    dgram.src_port = src_port;
    dgram.len = copy_len as u16;
    dgram.data[..copy_len].copy_from_slice(&payload[..copy_len]);
    dgram
}

pub fn socket_deliver_udp(sock_idx: u32, src_ip: [u8; 4], src_port: u16, payload: &[u8]) {
    if (sock_idx as usize) >= MAX_SOCKETS {
        return;
    }

    let dgram = make_udp_datagram(src_ip, src_port, payload);
    UDP_RX_QUEUES[sock_idx as usize].lock().push(&dgram);
    socket_wake_recv(sock_idx);
}

pub fn socket_deliver_udp_from_dispatch(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) {
    let sock_idx = {
        let table = SOCKET_TABLE.lock();
        table.find_udp_socket(dst_ip, dst_port)
    };

    if let Some(sock_idx) = sock_idx {
        socket_deliver_udp(sock_idx, src_ip, src_port, payload);
    }
}

fn socket_wake_send(sock_idx: u32) {
    SEND_WQS[sock_idx as usize].wake_all();
}

fn socket_wake_accept(sock_idx: u32) {
    ACCEPT_WQS[sock_idx as usize].wake_all();
}

fn socket_notify_tcp_idx_waiters(tcp_idx: usize) {
    let table = SOCKET_TABLE.lock();
    for (sock_idx, sock) in table.sockets.iter().enumerate() {
        if !sock.active {
            continue;
        }
        if sock.tcp_idx == Some(tcp_idx) {
            if tcp::tcp_recv_available(tcp_idx) > 0 {
                RECV_WQS[sock_idx].wake_all();
            }
            if tcp::tcp_send_buffer_space(tcp_idx) > 0 {
                SEND_WQS[sock_idx].wake_all();
            }
            if !matches!(
                tcp::tcp_get_state(tcp_idx),
                Some(TcpState::Established | TcpState::CloseWait)
            ) {
                RECV_WQS[sock_idx].wake_all();
                SEND_WQS[sock_idx].wake_all();
            }
        }
    }
}

fn socket_notify_accept_waiters() {
    let table = SOCKET_TABLE.lock();
    for (sock_idx, sock) in table.sockets.iter().enumerate() {
        if !sock.active || sock.state != SocketState::Listening {
            continue;
        }
        if find_established_child(sock, &table).is_some() {
            ACCEPT_WQS[sock_idx].wake_all();
        }
    }
}

pub fn socket_notify_tcp_activity(result: &tcp::TcpInputResult) {
    if let Some(tcp_idx) = result.conn_idx {
        socket_notify_tcp_idx_waiters(tcp_idx);
    }
    if result.accepted_idx.is_some() || result.new_state == Some(TcpState::Established) {
        socket_notify_accept_waiters();
    }
}

fn sync_socket_state(sock: &mut KernelSocket) {
    if let Some(tcp_idx) = sock.tcp_idx
        && let Some(state) = tcp::tcp_get_state(tcp_idx)
    {
        if state == TcpState::Established {
            sock.state = SocketState::Connected;
        }
        if matches!(
            state,
            TcpState::Closed
                | TcpState::TimeWait
                | TcpState::Closing
                | TcpState::LastAck
                | TcpState::FinWait1
                | TcpState::FinWait2
        ) {
            sock.state = SocketState::Closed;
        }
    }
}

fn find_established_child(listening: &KernelSocket, table: &SocketTable) -> Option<usize> {
    for tcp_idx in 0..MAX_CONNECTIONS {
        let Some(conn): Option<TcpConnection> = tcp::tcp_get_connection(tcp_idx) else {
            continue;
        };
        if conn.state != TcpState::Established {
            continue;
        }
        if conn.tuple.local_port != listening.local_port {
            continue;
        }
        if listening.local_ip != [0; 4] && conn.tuple.local_ip != listening.local_ip {
            continue;
        }
        if table.tcp_idx_is_bound(tcp_idx) {
            continue;
        }
        return Some(tcp_idx);
    }
    None
}

pub fn socket_create(domain: u16, sock_type: u16, protocol: u16) -> i32 {
    if domain != AF_INET {
        return errno_i32(ERRNO_EAFNOSUPPORT);
    }
    if sock_type != SOCK_STREAM && sock_type != SOCK_DGRAM {
        return errno_i32(ERRNO_EPROTONOSUPPORT);
    }

    let mut table = SOCKET_TABLE.lock();
    let Some(idx) = table.alloc_slot() else {
        return errno_i32(ERRNO_ENOMEM);
    };

    let sock = table
        .get_mut(idx)
        .expect("allocated socket slot must exist");
    sock.domain = domain;
    sock.sock_type = sock_type;
    sock.protocol = protocol;
    sock.state = SocketState::Unbound;
    sock.process_id = 0;
    UDP_RX_QUEUES[idx as usize].lock().clear();
    idx as i32
}

pub fn socket_sendto(
    sock_idx: u32,
    data: *const u8,
    len: usize,
    dst_ip: [u8; 4],
    dst_port: u16,
) -> i64 {
    if data.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }
    if dst_port == 0 {
        return errno_i32(ERRNO_EDESTADDRREQ) as i64;
    }
    if len > UDP_DGRAM_MAX_PAYLOAD {
        return errno_i32(ERRNO_EINVAL) as i64;
    }

    let (local_ip, local_port) = {
        let mut table = SOCKET_TABLE.lock();
        let idx = sock_idx as usize;
        if idx >= MAX_SOCKETS || !table.sockets[idx].active {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        }

        if table.sockets[idx].sock_type != SOCK_DGRAM {
            return errno_i32(ERRNO_EPROTONOSUPPORT) as i64;
        }

        if table.sockets[idx].local_port == 0 {
            let Some(port) = alloc_ephemeral_port(&table) else {
                return errno_i32(ERRNO_ENOMEM) as i64;
            };
            let local_ip = crate::net::netstack::NET_STACK
                .first_ipv4()
                .map(|ip| ip.0)
                .unwrap_or([0; 4]);

            let sock = &mut table.sockets[idx];
            sock.local_ip = local_ip;
            sock.local_port = port;
            if sock.state == SocketState::Unbound {
                sock.state = SocketState::Bound;
            }
        }

        let sock = &table.sockets[idx];
        (sock.local_ip, sock.local_port)
    };

    let payload = if len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(data, len) }
    };

    if !virtio_net::virtio_net_is_ready() {
        return len as i64;
    }

    if virtio_net::transmit_udp_packet(local_ip, dst_ip, local_port, dst_port, payload) {
        len as i64
    } else {
        errno_i32(ERRNO_ENETUNREACH) as i64
    }
}

pub fn socket_recvfrom(
    sock_idx: u32,
    buf: *mut u8,
    len: usize,
    src_ip: *mut [u8; 4],
    src_port: *mut u16,
) -> i64 {
    if buf.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }

    let (sock_type, nonblocking, timeout_ms) = {
        let table = SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        (sock.sock_type, sock.nonblocking, sock.recv_timeout_ms)
    };

    if sock_type != SOCK_DGRAM {
        return errno_i32(ERRNO_EPROTONOSUPPORT) as i64;
    }

    let out = if len == 0 {
        &mut [][..]
    } else {
        unsafe { core::slice::from_raw_parts_mut(buf, len) }
    };

    loop {
        if let Some(dgram) = UDP_RX_QUEUES[sock_idx as usize].lock().pop() {
            let copy_len = cmp::min(out.len(), dgram.len as usize);
            out[..copy_len].copy_from_slice(&dgram.data[..copy_len]);

            if !src_ip.is_null() {
                unsafe {
                    *src_ip = dgram.src_ip;
                }
            }
            if !src_port.is_null() {
                unsafe {
                    *src_port = dgram.src_port;
                }
            }

            return copy_len as i64;
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN) as i64;
        }

        let wait_ok = if timeout_ms > 0 {
            RECV_WQS[sock_idx as usize].wait_event_timeout(
                || !UDP_RX_QUEUES[sock_idx as usize].lock().is_empty(),
                timeout_ms,
            )
        } else {
            RECV_WQS[sock_idx as usize]
                .wait_event(|| !UDP_RX_QUEUES[sock_idx as usize].lock().is_empty())
        };

        if !wait_ok {
            return errno_i32(ERRNO_EAGAIN) as i64;
        }
    }
}

pub fn socket_bind(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    let mut table = SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };

    if sock.state != SocketState::Unbound {
        return errno_i32(ERRNO_EINVAL);
    }

    sock.local_ip = addr;
    sock.local_port = port;
    sock.state = SocketState::Bound;
    0
}

pub fn socket_listen(sock_idx: u32, _backlog: u32) -> i32 {
    let mut table = SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };

    if sock.sock_type != SOCK_STREAM {
        return errno_i32(ERRNO_EPROTONOSUPPORT);
    }

    if sock.state != SocketState::Bound {
        return errno_i32(ERRNO_EINVAL);
    }

    match tcp::tcp_listen(sock.local_ip, sock.local_port) {
        Ok(tcp_idx) => {
            sock.tcp_idx = Some(tcp_idx);
            sock.state = SocketState::Listening;
            0
        }
        Err(e) => map_tcp_err(e),
    }
}

pub fn socket_accept(sock_idx: u32, peer_addr: *mut [u8; 4], peer_port: *mut u16) -> i32 {
    loop {
        let (listen_sock, nonblocking, recv_timeout_ms, send_timeout_ms) = {
            let table = SOCKET_TABLE.lock();
            let Some(sock) = table.get(sock_idx).copied() else {
                return errno_i32(ERRNO_ENOTSOCK);
            };
            if sock.state != SocketState::Listening {
                return errno_i32(ERRNO_EINVAL);
            }
            (
                sock,
                sock.nonblocking,
                sock.recv_timeout_ms,
                sock.send_timeout_ms,
            )
        };

        {
            let mut table = SOCKET_TABLE.lock();
            if let Some(tcp_idx) = find_established_child(&listen_sock, &table)
                && let Some(conn) = tcp::tcp_get_connection(tcp_idx)
            {
                let Some(new_idx) = table.alloc_slot() else {
                    return errno_i32(ERRNO_ENOMEM);
                };
                let sock = table
                    .get_mut(new_idx)
                    .expect("allocated accepted socket slot must exist");
                sock.domain = AF_INET;
                sock.sock_type = SOCK_STREAM;
                sock.protocol = 0;
                sock.state = SocketState::Connected;
                sock.local_ip = conn.tuple.local_ip;
                sock.local_port = conn.tuple.local_port;
                sock.remote_ip = conn.tuple.remote_ip;
                sock.remote_port = conn.tuple.remote_port;
                sock.tcp_idx = Some(tcp_idx);
                sock.nonblocking = nonblocking;
                sock.recv_timeout_ms = recv_timeout_ms;
                sock.send_timeout_ms = send_timeout_ms;

                if !peer_addr.is_null() {
                    unsafe {
                        *peer_addr = conn.tuple.remote_ip;
                    }
                }
                if !peer_port.is_null() {
                    unsafe {
                        *peer_port = conn.tuple.remote_port;
                    }
                }
                return new_idx as i32;
            }
        }

        if nonblocking {
            return errno_i32(ERRNO_EAGAIN);
        }

        let wait_ok = if recv_timeout_ms > 0 {
            ACCEPT_WQS[sock_idx as usize].wait_event_timeout(
                || {
                    let table = SOCKET_TABLE.lock();
                    let Some(sock) = table.get(sock_idx) else {
                        return true;
                    };
                    find_established_child(sock, &table).is_some()
                },
                recv_timeout_ms,
            )
        } else {
            ACCEPT_WQS[sock_idx as usize].wait_event(|| {
                let table = SOCKET_TABLE.lock();
                let Some(sock) = table.get(sock_idx) else {
                    return true;
                };
                find_established_child(sock, &table).is_some()
            })
        };

        if !wait_ok {
            return errno_i32(ERRNO_EAGAIN);
        }
    }
}

pub fn socket_connect(sock_idx: u32, addr: [u8; 4], port: u16) -> i32 {
    let mut table = SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };

    match sock.sock_type {
        SOCK_STREAM => {
            if matches!(sock.state, SocketState::Connected | SocketState::Connecting) {
                return errno_i32(ERRNO_EISCONN);
            }

            let local_ip = if sock.local_ip != [0; 4] {
                sock.local_ip
            } else {
                crate::net::netstack::NET_STACK
                    .first_ipv4()
                    .map(|ip| ip.0)
                    .unwrap_or([0; 4])
            };

            match tcp::tcp_connect(local_ip, addr, port) {
                Ok((tcp_idx, syn)) => {
                    let send_rc = socket_send_tcp_segment(&syn, &[]);
                    if send_rc != 0 {
                        let _ = tcp::tcp_abort(tcp_idx);
                        return send_rc;
                    }

                    sock.local_ip = syn.tuple.local_ip;
                    sock.local_port = syn.tuple.local_port;
                    sock.remote_ip = addr;
                    sock.remote_port = port;
                    sock.tcp_idx = Some(tcp_idx);
                    sock.state = SocketState::Connecting;
                    0
                }
                Err(e) => map_tcp_err(e),
            }
        }
        SOCK_DGRAM => {
            sock.remote_ip = addr;
            sock.remote_port = port;
            sock.state = SocketState::Connected;
            0
        }
        _ => errno_i32(ERRNO_EPROTONOSUPPORT),
    }
}

pub fn socket_send(sock_idx: u32, data: *const u8, len: usize) -> i64 {
    if data.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }

    let sock_type = {
        let table = SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        sock.sock_type
    };

    if sock_type == SOCK_DGRAM {
        if len > UDP_DGRAM_MAX_PAYLOAD {
            return errno_i32(ERRNO_EINVAL) as i64;
        }

        let (state, remote_ip, remote_port, local_ip, local_port) = {
            let mut table = SOCKET_TABLE.lock();
            let idx = sock_idx as usize;
            if idx >= MAX_SOCKETS || !table.sockets[idx].active {
                return errno_i32(ERRNO_ENOTSOCK) as i64;
            }

            if table.sockets[idx].state == SocketState::Unbound
                || table.sockets[idx].local_port == 0
            {
                let Some(port) = alloc_ephemeral_port(&table) else {
                    return errno_i32(ERRNO_ENOMEM) as i64;
                };
                let local_ip = crate::net::netstack::NET_STACK
                    .first_ipv4()
                    .map(|ip| ip.0)
                    .unwrap_or([0; 4]);
                let sock = &mut table.sockets[idx];
                sock.local_ip = local_ip;
                sock.local_port = port;
                if sock.state == SocketState::Unbound {
                    sock.state = SocketState::Bound;
                }
            }

            let sock = &table.sockets[idx];
            (
                sock.state,
                sock.remote_ip,
                sock.remote_port,
                sock.local_ip,
                sock.local_port,
            )
        };

        if state != SocketState::Connected || remote_port == 0 {
            return errno_i32(ERRNO_ENOTCONN) as i64;
        }

        let payload = if len == 0 {
            &[][..]
        } else {
            unsafe { core::slice::from_raw_parts(data, len) }
        };

        if !virtio_net::virtio_net_is_ready() {
            return len as i64;
        }

        if virtio_net::transmit_udp_packet(local_ip, remote_ip, local_port, remote_port, payload) {
            return len as i64;
        }
        return errno_i32(ERRNO_ENETUNREACH) as i64;
    }

    let (tcp_idx, state, nonblocking, timeout_ms) = {
        let mut table = SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        sync_socket_state(sock);
        (
            sock.tcp_idx,
            sock.state,
            sock.nonblocking,
            sock.send_timeout_ms,
        )
    };

    if !matches!(state, SocketState::Connected) {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    }
    let Some(tcp_idx) = tcp_idx else {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    };

    let payload = if len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(data, len) }
    };

    let mut total_wrote = 0usize;
    while total_wrote < payload.len() {
        let space = tcp::tcp_send_buffer_space(tcp_idx);
        if space == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            let wait_ok = if timeout_ms > 0 {
                SEND_WQS[sock_idx as usize]
                    .wait_event_timeout(|| tcp::tcp_send_buffer_space(tcp_idx) > 0, timeout_ms)
            } else {
                SEND_WQS[sock_idx as usize].wait_event(|| tcp::tcp_send_buffer_space(tcp_idx) > 0)
            };
            if !wait_ok {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            continue;
        }

        let remaining = payload.len() - total_wrote;
        let chunk_len = cmp::min(space, remaining);
        let chunk = &payload[total_wrote..total_wrote + chunk_len];
        let wrote = match tcp::tcp_send(tcp_idx, chunk) {
            Ok(n) => n,
            Err(e) => {
                if total_wrote > 0 {
                    break;
                }
                return map_tcp_err_i64(e);
            }
        };
        if wrote == 0 {
            if total_wrote > 0 {
                break;
            }
            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            let wait_ok = if timeout_ms > 0 {
                SEND_WQS[sock_idx as usize]
                    .wait_event_timeout(|| tcp::tcp_send_buffer_space(tcp_idx) > 0, timeout_ms)
            } else {
                SEND_WQS[sock_idx as usize].wait_event(|| tcp::tcp_send_buffer_space(tcp_idx) > 0)
            };
            if !wait_ok {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
            continue;
        }
        total_wrote += wrote;
    }

    let mut tx_payload = [0u8; TCP_TX_MAX];
    let now_ms = slopos_lib::clock::uptime_ms();
    loop {
        let Some((seg, n)) = tcp::tcp_poll_transmit(tcp_idx, &mut tx_payload, now_ms) else {
            break;
        };
        let rc = socket_send_tcp_segment(&seg, &tx_payload[..n]);
        if rc != 0 {
            return rc as i64;
        }
    }

    total_wrote as i64
}

pub fn socket_recv(sock_idx: u32, buf: *mut u8, len: usize) -> i64 {
    if buf.is_null() && len != 0 {
        return errno_i32(ERRNO_EFAULT) as i64;
    }

    let sock_type = {
        let table = SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        sock.sock_type
    };

    if sock_type == SOCK_DGRAM {
        let (nonblocking, timeout_ms, peer_filter) = {
            let table = SOCKET_TABLE.lock();
            let Some(sock) = table.get(sock_idx) else {
                return errno_i32(ERRNO_ENOTSOCK) as i64;
            };
            let filter = if sock.state == SocketState::Connected {
                Some((sock.remote_ip, sock.remote_port))
            } else {
                None
            };
            (sock.nonblocking, sock.recv_timeout_ms, filter)
        };

        let out = if len == 0 {
            &mut [][..]
        } else {
            unsafe { core::slice::from_raw_parts_mut(buf, len) }
        };

        loop {
            let maybe_dgram = {
                let mut queue = UDP_RX_QUEUES[sock_idx as usize].lock();
                let mut found = None;
                while let Some(dgram) = queue.pop() {
                    if let Some((peer_ip, peer_port)) = peer_filter
                        && (dgram.src_ip != peer_ip || dgram.src_port != peer_port)
                    {
                        continue;
                    }
                    found = Some(dgram);
                    break;
                }
                found
            };

            if let Some(dgram) = maybe_dgram {
                let copy_len = cmp::min(out.len(), dgram.len as usize);
                out[..copy_len].copy_from_slice(&dgram.data[..copy_len]);
                return copy_len as i64;
            }

            if nonblocking {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }

            let wait_ok = if timeout_ms > 0 {
                RECV_WQS[sock_idx as usize].wait_event_timeout(
                    || !UDP_RX_QUEUES[sock_idx as usize].lock().is_empty(),
                    timeout_ms,
                )
            } else {
                RECV_WQS[sock_idx as usize]
                    .wait_event(|| !UDP_RX_QUEUES[sock_idx as usize].lock().is_empty())
            };

            if !wait_ok {
                return errno_i32(ERRNO_EAGAIN) as i64;
            }
        }
    }

    let (tcp_idx, state, nonblocking, timeout_ms) = {
        let mut table = SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx) else {
            return errno_i32(ERRNO_ENOTSOCK) as i64;
        };
        sync_socket_state(sock);
        (
            sock.tcp_idx,
            sock.state,
            sock.nonblocking,
            sock.recv_timeout_ms,
        )
    };

    if !matches!(state, SocketState::Connected | SocketState::Connecting) {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    }

    let Some(tcp_idx) = tcp_idx else {
        return errno_i32(ERRNO_ENOTCONN) as i64;
    };

    let out = if len == 0 {
        &mut [][..]
    } else {
        unsafe { core::slice::from_raw_parts_mut(buf, len) }
    };

    loop {
        match tcp::tcp_recv(tcp_idx, out) {
            Ok(n) => {
                if n > 0 {
                    return n as i64;
                }

                if !matches!(
                    tcp::tcp_get_state(tcp_idx),
                    Some(TcpState::Established | TcpState::CloseWait)
                ) {
                    return 0;
                }

                if nonblocking {
                    return errno_i32(ERRNO_EAGAIN) as i64;
                }

                let wait_ok = if timeout_ms > 0 {
                    RECV_WQS[sock_idx as usize].wait_event_timeout(
                        || {
                            tcp::tcp_recv_available(tcp_idx) > 0
                                || !matches!(
                                    tcp::tcp_get_state(tcp_idx),
                                    Some(TcpState::Established | TcpState::CloseWait)
                                )
                        },
                        timeout_ms,
                    )
                } else {
                    RECV_WQS[sock_idx as usize].wait_event(|| {
                        tcp::tcp_recv_available(tcp_idx) > 0
                            || !matches!(
                                tcp::tcp_get_state(tcp_idx),
                                Some(TcpState::Established | TcpState::CloseWait)
                            )
                    })
                };

                if !wait_ok {
                    return errno_i32(ERRNO_EAGAIN) as i64;
                }
            }
            Err(e) => return map_tcp_err_i64(e),
        }
    }
}

pub fn socket_close(sock_idx: u32) -> i32 {
    let tcp_idx = {
        let mut table = SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx).copied() else {
            return errno_i32(ERRNO_ENOTSOCK);
        };
        let tcp_idx = sock.tcp_idx;
        table.release(sock_idx);
        tcp_idx
    };

    socket_wake_recv(sock_idx);
    socket_wake_send(sock_idx);
    socket_wake_accept(sock_idx);
    UDP_RX_QUEUES[sock_idx as usize].lock().clear();

    if let Some(tcp_idx) = tcp_idx {
        match tcp::tcp_close(tcp_idx) {
            Ok(Some(seg)) => {
                let _ = socket_send_tcp_segment(&seg, &[]);
                socket_notify_tcp_idx_waiters(tcp_idx);
                0
            }
            Ok(None) => 0,
            Err(e) => map_tcp_err(e),
        }
    } else {
        0
    }
}

pub fn socket_poll_readable(sock_idx: u32) -> u32 {
    let (sock, tcp_idx) = {
        let mut table = SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx) else {
            return 0;
        };
        sync_socket_state(sock);
        (*sock, sock.tcp_idx)
    };

    if sock.state == SocketState::Listening {
        let table = SOCKET_TABLE.lock();
        if find_established_child(&sock, &table).is_some() {
            return POLLIN as u32;
        }
        return 0;
    }

    if sock.sock_type == SOCK_DGRAM {
        if !UDP_RX_QUEUES[sock_idx as usize].lock().is_empty() {
            return POLLIN as u32;
        }
        return 0;
    }

    let Some(tcp_idx) = tcp_idx else {
        return 0;
    };

    let mut flags = 0u32;
    if tcp::tcp_recv_available(tcp_idx) > 0 {
        flags |= POLLIN as u32;
    }

    match tcp::tcp_get_state(tcp_idx) {
        Some(TcpState::Established | TcpState::CloseWait) => {}
        Some(
            TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::LastAck
            | TcpState::TimeWait,
        ) => {
            flags |= POLLHUP as u32;
        }
        Some(TcpState::Closed) | None => {
            flags |= (POLLERR | POLLHUP) as u32;
        }
        _ => {}
    }

    flags
}

pub fn socket_poll_writable(sock_idx: u32) -> u32 {
    let (sock_type, tcp_idx, state) = {
        let mut table = SOCKET_TABLE.lock();
        let Some(sock) = table.get_mut(sock_idx) else {
            return 0;
        };
        sync_socket_state(sock);
        (sock.sock_type, sock.tcp_idx, sock.state)
    };

    if sock_type == SOCK_DGRAM {
        return POLLOUT as u32;
    }

    let Some(tcp_idx) = tcp_idx else {
        return 0;
    };

    let mut flags = 0u32;
    if matches!(state, SocketState::Connected) && tcp::tcp_send_buffer_space(tcp_idx) > 0 {
        flags |= POLLOUT as u32;
    }

    match tcp::tcp_get_state(tcp_idx) {
        Some(TcpState::Established | TcpState::CloseWait) => {}
        Some(TcpState::Closed) | None => {
            flags |= (POLLERR | POLLHUP) as u32;
        }
        Some(
            TcpState::FinWait1
            | TcpState::FinWait2
            | TcpState::Closing
            | TcpState::LastAck
            | TcpState::TimeWait,
        ) => {
            flags |= POLLHUP as u32;
        }
        _ => {}
    }

    flags
}

pub fn socket_get_state(sock_idx: u32) -> Option<SocketState> {
    SOCKET_TABLE.lock().get(sock_idx).map(|s| s.state)
}

pub fn socket_set_nonblocking(sock_idx: u32, nonblocking: bool) -> i32 {
    let mut table = SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };
    sock.nonblocking = nonblocking;
    0
}

pub fn socket_set_timeouts(sock_idx: u32, recv_timeout_ms: u64, send_timeout_ms: u64) -> i32 {
    let mut table = SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };
    sock.recv_timeout_ms = recv_timeout_ms;
    sock.send_timeout_ms = send_timeout_ms;
    0
}

pub fn socket_reset_all() {
    let mut table = SOCKET_TABLE.lock();
    for idx in 0..MAX_SOCKETS {
        RECV_WQS[idx].wake_all();
        ACCEPT_WQS[idx].wake_all();
        SEND_WQS[idx].wake_all();
        UDP_RX_QUEUES[idx].lock().clear();
        table.sockets[idx] = KernelSocket::empty();
    }
    tcp::tcp_reset_all();
}

pub fn socket_snapshot(sock_idx: u32) -> Option<KernelSocket> {
    SOCKET_TABLE.lock().get(sock_idx).copied()
}

pub fn socket_lookup_tcp_idx(sock_idx: u32) -> Option<usize> {
    SOCKET_TABLE.lock().get(sock_idx).and_then(|s| s.tcp_idx)
}

pub fn socket_count_active() -> usize {
    SOCKET_TABLE
        .lock()
        .sockets
        .iter()
        .filter(|s| s.active)
        .count()
}

pub fn socket_send_queued(sock_idx: u32) -> i32 {
    let tcp_idx = {
        let table = SOCKET_TABLE.lock();
        let Some(sock) = table.get(sock_idx) else {
            return errno_i32(ERRNO_ENOTSOCK);
        };
        match sock.tcp_idx {
            Some(i) => i,
            None => return errno_i32(ERRNO_ENOTCONN),
        }
    };

    let mut tx_payload = [0u8; TCP_TX_MAX];
    let now_ms = slopos_lib::clock::uptime_ms();
    loop {
        let Some((seg, n)) = tcp::tcp_poll_transmit(tcp_idx, &mut tx_payload, now_ms) else {
            break;
        };
        let rc = socket_send_tcp_segment(&seg, &tx_payload[..n]);
        if rc != 0 {
            return rc;
        }
    }
    0
}

pub fn socket_process_timers() {
    let now_ms = slopos_lib::clock::uptime_ms();
    if let Some(idx) = tcp::tcp_retransmit_check(now_ms)
        && let Some(sock_idx) = socket_from_tcp_idx(idx)
    {
        let _ = socket_send_queued(sock_idx);
    }

    if let Some((_idx, seg)) = tcp::tcp_delayed_ack_check(now_ms) {
        let _ = socket_send_tcp_segment(&seg, &[]);
    }
}

fn socket_from_tcp_idx(tcp_idx: usize) -> Option<u32> {
    let table = SOCKET_TABLE.lock();
    for (idx, sock) in table.sockets.iter().enumerate() {
        if sock.active && sock.tcp_idx == Some(tcp_idx) {
            return Some(idx as u32);
        }
    }
    None
}

pub fn socket_debug_set_connected(sock_idx: u32, remote_ip: [u8; 4], remote_port: u16) -> i32 {
    let mut table = SOCKET_TABLE.lock();
    let Some(sock) = table.get_mut(sock_idx) else {
        return errno_i32(ERRNO_ENOTSOCK);
    };
    let Some(tcp_idx) = sock.tcp_idx else {
        return errno_i32(ERRNO_ENOTCONN);
    };
    if let Some(conn) = tcp::tcp_get_connection(tcp_idx)
        && conn.state == TcpState::Established
    {
        sock.state = SocketState::Connected;
        sock.remote_ip = remote_ip;
        sock.remote_port = remote_port;
        return 0;
    }
    errno_i32(ERRNO_ENOTCONN)
}

pub fn socket_host_to_be_port(port: u16) -> u16 {
    u16::from_be_bytes(be_port(port))
}

pub fn socket_be_to_host_port(port: u16) -> u16 {
    u16::from_be(port)
}

pub fn socket_max_send_probe(sock_idx: u32, max_len: usize) -> i32 {
    let Some(tcp_idx) = socket_lookup_tcp_idx(sock_idx) else {
        return errno_i32(ERRNO_ENOTCONN);
    };
    let space = tcp::tcp_send_buffer_space(tcp_idx);
    cmp::min(space, max_len) as i32
}
