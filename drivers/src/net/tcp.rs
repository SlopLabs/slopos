//! TCP (Transmission Control Protocol) implementation — RFC 793 + RFC 7413.
//!
//! Provides TCP header parsing/construction, one's-complement checksum with
//! IPv4 pseudo-header, a full TCP state machine, connection table, three-way
//! handshake (active and passive open), and connection teardown.
//!
//! This module is purely protocol logic — it does **not** drive the NIC
//! directly.  Higher layers (Phase 5B+) wire it into the VirtIO net driver
//! for actual packet I/O.

use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use slopos_lib::{IrqMutex, klog_debug};

// =============================================================================
// Constants
// =============================================================================

/// Minimum TCP header length (no options).
pub const TCP_HEADER_LEN: usize = 20;

/// Maximum TCP header length (with 40 bytes of options).
pub const TCP_HEADER_MAX_LEN: usize = 60;

/// Maximum number of simultaneous TCP connections.
pub const MAX_CONNECTIONS: usize = 64;

/// Default Maximum Segment Size (Ethernet MTU 1500 − IP 20 − TCP 20).
pub const DEFAULT_MSS: u16 = 1460;

/// Default receive window size (16 KiB).
pub const DEFAULT_WINDOW_SIZE: u16 = 16384;

/// Initial retransmission timeout in milliseconds (RFC 6298 recommends 1s).
pub const INITIAL_RTO_MS: u32 = 1000;

/// Maximum retransmission timeout in milliseconds.
pub const MAX_RTO_MS: u32 = 60_000;

/// TIME_WAIT duration in milliseconds (2 × MSL, MSL = 30s).
pub const TIME_WAIT_MS: u64 = 60_000;

/// Maximum retransmission attempts before giving up.
pub const MAX_RETRANSMITS: u8 = 8;

// ---------------------------------------------------------------------------
// TCP flag bits (in the flags byte of the header)
// ---------------------------------------------------------------------------

pub const TCP_FLAG_FIN: u8 = 0x01;
pub const TCP_FLAG_SYN: u8 = 0x02;
pub const TCP_FLAG_RST: u8 = 0x04;
pub const TCP_FLAG_PSH: u8 = 0x08;
pub const TCP_FLAG_ACK: u8 = 0x10;
pub const TCP_FLAG_URG: u8 = 0x20;

// ---------------------------------------------------------------------------
// TCP option kinds
// ---------------------------------------------------------------------------

pub const TCP_OPT_END: u8 = 0;
pub const TCP_OPT_NOP: u8 = 1;
pub const TCP_OPT_MSS: u8 = 2;
pub const TCP_OPT_MSS_LEN: u8 = 4;

// =============================================================================
// TCP Header
// =============================================================================

/// Parsed TCP header.
///
/// All multi-byte fields are stored in **host** byte order after parsing.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq_num: u32,
    pub ack_num: u32,
    /// Data offset in 32-bit words (5–15).
    pub data_offset: u8,
    pub flags: u8,
    pub window_size: u16,
    pub checksum: u16,
    pub urgent_ptr: u16,
}

impl TcpHeader {
    /// Header length in bytes (data_offset × 4).
    #[inline]
    pub const fn header_len(&self) -> usize {
        (self.data_offset as usize) * 4
    }

    /// Options length in bytes (header_len − 20).
    #[inline]
    pub const fn options_len(&self) -> usize {
        self.header_len().saturating_sub(TCP_HEADER_LEN)
    }

    // --- Flag helpers -------------------------------------------------------

    #[inline]
    pub const fn is_syn(&self) -> bool {
        (self.flags & TCP_FLAG_SYN) != 0
    }

    #[inline]
    pub const fn is_ack(&self) -> bool {
        (self.flags & TCP_FLAG_ACK) != 0
    }

    #[inline]
    pub const fn is_fin(&self) -> bool {
        (self.flags & TCP_FLAG_FIN) != 0
    }

    #[inline]
    pub const fn is_rst(&self) -> bool {
        (self.flags & TCP_FLAG_RST) != 0
    }

    #[inline]
    pub const fn is_psh(&self) -> bool {
        (self.flags & TCP_FLAG_PSH) != 0
    }

    #[inline]
    pub const fn is_urg(&self) -> bool {
        (self.flags & TCP_FLAG_URG) != 0
    }

    #[inline]
    pub const fn is_syn_ack(&self) -> bool {
        self.is_syn() && self.is_ack()
    }

    #[inline]
    pub const fn is_fin_ack(&self) -> bool {
        self.is_fin() && self.is_ack()
    }
}

// =============================================================================
// Parsing
// =============================================================================

/// Parse a TCP header from a byte slice.
///
/// Returns `None` if the slice is too short or the data offset is invalid.
pub fn parse_header(data: &[u8]) -> Option<TcpHeader> {
    if data.len() < TCP_HEADER_LEN {
        return None;
    }

    let src_port = u16::from_be_bytes([data[0], data[1]]);
    let dst_port = u16::from_be_bytes([data[2], data[3]]);
    let seq_num = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ack_num = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    let data_offset = (data[12] >> 4) & 0x0F;
    let flags = data[13] & 0x3F; // low 6 bits of byte 13

    let window_size = u16::from_be_bytes([data[14], data[15]]);
    let checksum = u16::from_be_bytes([data[16], data[17]]);
    let urgent_ptr = u16::from_be_bytes([data[18], data[19]]);

    // Data offset must be at least 5 (20 bytes) and at most 15 (60 bytes).
    if data_offset < 5 || data_offset > 15 {
        return None;
    }

    let header_len = (data_offset as usize) * 4;
    if data.len() < header_len {
        return None;
    }

    Some(TcpHeader {
        src_port,
        dst_port,
        seq_num,
        ack_num,
        data_offset,
        flags,
        window_size,
        checksum,
        urgent_ptr,
    })
}

/// Parse MSS option from TCP header options region.
///
/// Returns the MSS value if found, otherwise `None`.
pub fn parse_mss_option(options: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i < options.len() {
        match options[i] {
            TCP_OPT_END => break,
            TCP_OPT_NOP => {
                i += 1;
            }
            TCP_OPT_MSS => {
                if i + 3 < options.len() && options[i + 1] == TCP_OPT_MSS_LEN {
                    return Some(u16::from_be_bytes([options[i + 2], options[i + 3]]));
                }
                break;
            }
            _ => {
                // Unknown option: skip using length byte.
                if i + 1 >= options.len() {
                    break;
                }
                let opt_len = options[i + 1] as usize;
                if opt_len < 2 || i + opt_len > options.len() {
                    break;
                }
                i += opt_len;
            }
        }
    }
    None
}

// =============================================================================
// Construction
// =============================================================================

/// Write a TCP header into `out[..TCP_HEADER_LEN]`.
///
/// Returns `TCP_HEADER_LEN` on success, `None` if `out` is too short.
/// The checksum field is written as 0 — the caller must compute and patch it
/// afterwards using [`tcp_checksum`].
pub fn write_header(hdr: &TcpHeader, out: &mut [u8]) -> Option<usize> {
    let header_len = hdr.header_len();
    if out.len() < header_len || header_len < TCP_HEADER_LEN {
        return None;
    }

    out[0..2].copy_from_slice(&hdr.src_port.to_be_bytes());
    out[2..4].copy_from_slice(&hdr.dst_port.to_be_bytes());
    out[4..8].copy_from_slice(&hdr.seq_num.to_be_bytes());
    out[8..12].copy_from_slice(&hdr.ack_num.to_be_bytes());
    out[12] = (hdr.data_offset << 4) & 0xF0;
    out[13] = hdr.flags & 0x3F;
    out[14..16].copy_from_slice(&hdr.window_size.to_be_bytes());
    out[16..18].copy_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    out[18..20].copy_from_slice(&hdr.urgent_ptr.to_be_bytes());

    // Zero any options area beyond the minimum header.
    if header_len > TCP_HEADER_LEN {
        out[TCP_HEADER_LEN..header_len].fill(0);
    }

    Some(header_len)
}

/// Build a minimal TCP header with the given parameters.
pub fn build_header(
    src_port: u16,
    dst_port: u16,
    seq_num: u32,
    ack_num: u32,
    flags: u8,
    window_size: u16,
    data_offset: u8,
) -> TcpHeader {
    TcpHeader {
        src_port,
        dst_port,
        seq_num,
        ack_num,
        data_offset,
        flags,
        window_size,
        checksum: 0,
        urgent_ptr: 0,
    }
}

/// Write MSS option into `out` buffer.  Returns bytes written (4) or `None`.
pub fn write_mss_option(mss: u16, out: &mut [u8]) -> Option<usize> {
    if out.len() < 4 {
        return None;
    }
    out[0] = TCP_OPT_MSS;
    out[1] = TCP_OPT_MSS_LEN;
    out[2..4].copy_from_slice(&mss.to_be_bytes());
    Some(4)
}

// =============================================================================
// Checksum
// =============================================================================

/// Compute the one's-complement sum over a byte slice (for checksum accumulation).
fn ones_complement_sum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0usize;
    while i + 1 < data.len() {
        sum = sum.wrapping_add(u16::from_be_bytes([data[i], data[i + 1]]) as u32);
        i += 2;
    }
    // Handle trailing odd byte.
    if i < data.len() {
        sum = sum.wrapping_add((data[i] as u32) << 8);
    }
    sum
}

/// Fold a 32-bit accumulator into a 16-bit one's-complement value.
fn fold_checksum(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Compute the TCP checksum over a pseudo-header + TCP header + payload.
///
/// The pseudo-header includes `src_ip`, `dst_ip`, protocol (6 = TCP), and the
/// TCP segment length (header + payload).
///
/// `tcp_segment` must contain the full TCP segment (header + payload) with the
/// checksum field set to 0.
pub fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> u16 {
    let tcp_len = tcp_segment.len() as u16;

    // Pseudo-header: src_ip (4) + dst_ip (4) + zero (1) + proto (1) + tcp_len (2) = 12 bytes
    let mut sum = 0u32;
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[0], src_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[2], src_ip[3]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[0], dst_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[2], dst_ip[3]]) as u32);
    sum = sum.wrapping_add(6u32); // Protocol = TCP
    sum = sum.wrapping_add(tcp_len as u32);

    // Add TCP segment bytes.
    sum = sum.wrapping_add(ones_complement_sum(tcp_segment));

    fold_checksum(sum)
}

/// Verify a received TCP segment's checksum.
///
/// Returns `true` if the checksum is valid (folds to 0).
pub fn verify_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_segment: &[u8]) -> bool {
    let tcp_len = tcp_segment.len() as u16;

    let mut sum = 0u32;
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[0], src_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([src_ip[2], src_ip[3]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[0], dst_ip[1]]) as u32);
    sum = sum.wrapping_add(u16::from_be_bytes([dst_ip[2], dst_ip[3]]) as u32);
    sum = sum.wrapping_add(6u32);
    sum = sum.wrapping_add(tcp_len as u32);
    sum = sum.wrapping_add(ones_complement_sum(tcp_segment));

    fold_checksum(sum) == 0
}

// =============================================================================
// TCP State Machine (RFC 793)
// =============================================================================

/// TCP connection state per RFC 793 §3.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

impl TcpState {
    /// Human-readable name for logging.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::Listen => "LISTEN",
            Self::SynSent => "SYN_SENT",
            Self::SynReceived => "SYN_RECEIVED",
            Self::Established => "ESTABLISHED",
            Self::FinWait1 => "FIN_WAIT_1",
            Self::FinWait2 => "FIN_WAIT_2",
            Self::CloseWait => "CLOSE_WAIT",
            Self::Closing => "CLOSING",
            Self::LastAck => "LAST_ACK",
            Self::TimeWait => "TIME_WAIT",
        }
    }

    /// Is this state "open" (capable of data transfer or about to be)?
    pub const fn is_open(self) -> bool {
        matches!(
            self,
            Self::Established | Self::FinWait1 | Self::FinWait2 | Self::CloseWait
        )
    }

    /// Is this state a closing/teardown state?
    pub const fn is_closing(self) -> bool {
        matches!(
            self,
            Self::FinWait1
                | Self::FinWait2
                | Self::CloseWait
                | Self::Closing
                | Self::LastAck
                | Self::TimeWait
        )
    }
}

// =============================================================================
// TCP Connection
// =============================================================================

/// Four-tuple identifying a TCP connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpTuple {
    pub local_ip: [u8; 4],
    pub local_port: u16,
    pub remote_ip: [u8; 4],
    pub remote_port: u16,
}

impl TcpTuple {
    pub const ZERO: Self = Self {
        local_ip: [0; 4],
        local_port: 0,
        remote_ip: [0; 4],
        remote_port: 0,
    };

    /// Check if this tuple matches a specific remote endpoint (for listen sockets,
    /// `remote_ip`/`remote_port` may be zero = wildcard).
    pub fn matches(&self, other: &TcpTuple) -> bool {
        self.local_ip == other.local_ip
            && self.local_port == other.local_port
            && (self.remote_ip == [0; 4] || self.remote_ip == other.remote_ip)
            && (self.remote_port == 0 || self.remote_port == other.remote_port)
    }
}

/// Error type for TCP operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TcpError {
    /// Connection table is full.
    TableFull,
    /// No connection found for the given tuple.
    NotFound,
    /// Connection is in wrong state for the requested operation.
    InvalidState,
    /// Port already in use.
    AddrInUse,
    /// Connection was reset by peer.
    ConnectionReset,
    /// Connection timed out.
    TimedOut,
    /// Connection refused by peer (RST received in SYN_SENT).
    ConnectionRefused,
    /// Invalid segment or parameter.
    InvalidSegment,
}

/// Outgoing segment generated by the state machine.
///
/// The TCP layer produces these; the network layer converts them to
/// wire-format packets and hands them to the NIC.
#[derive(Clone, Copy, Debug)]
pub struct TcpOutSegment {
    pub tuple: TcpTuple,
    pub seq_num: u32,
    pub ack_num: u32,
    pub flags: u8,
    pub window_size: u16,
    /// MSS option to include (0 = no MSS option).
    pub mss: u16,
}

/// Per-connection state.
#[derive(Clone, Copy, Debug)]
pub struct TcpConnection {
    pub tuple: TcpTuple,
    pub state: TcpState,

    // --- Send sequence variables (RFC 793 §3.2) ---
    /// Send unacknowledged.
    pub snd_una: u32,
    /// Send next.
    pub snd_nxt: u32,
    /// Send window.
    pub snd_wnd: u16,
    /// Initial send sequence number.
    pub iss: u32,

    // --- Receive sequence variables ---
    /// Receive next.
    pub rcv_nxt: u32,
    /// Receive window.
    pub rcv_wnd: u16,
    /// Initial receive sequence number.
    pub irs: u32,

    /// Peer's advertised MSS (or DEFAULT_MSS if not specified).
    pub peer_mss: u16,

    /// Retransmission timeout (ms).
    pub rto_ms: u32,
    /// Retransmit counter.
    pub retransmits: u8,

    /// Timestamp (ms) when TIME_WAIT entered (for 2×MSL expiry).
    pub time_wait_start_ms: u64,

    /// Whether the connection slot is in use.
    pub active: bool,
}

impl TcpConnection {
    pub const fn empty() -> Self {
        Self {
            tuple: TcpTuple::ZERO,
            state: TcpState::Closed,
            snd_una: 0,
            snd_nxt: 0,
            snd_wnd: 0,
            iss: 0,
            rcv_nxt: 0,
            rcv_wnd: DEFAULT_WINDOW_SIZE,
            irs: 0,
            peer_mss: DEFAULT_MSS,
            rto_ms: INITIAL_RTO_MS,
            retransmits: 0,
            time_wait_start_ms: 0,
            active: false,
        }
    }
}

// =============================================================================
// Sequence number arithmetic (RFC 793 §3.3)
// =============================================================================

/// `a` is before `b` in sequence space (wrapping comparison).
#[inline]
pub fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// `a` is before or equal to `b` in sequence space.
#[inline]
pub fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// `a` is after `b` in sequence space.
#[inline]
pub fn seq_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// `a` is after or equal to `b` in sequence space.
#[inline]
pub fn seq_ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

// =============================================================================
// ISN (Initial Sequence Number) generator
// =============================================================================

/// Simple monotonic ISN counter — incremented by 64000 per connection (RFC 6528
/// recommends a clock-based or hash-based ISN; this is a minimal starting point).
static ISN_COUNTER: AtomicU32 = AtomicU32::new(0x4F50_534C); // "OPSL"

fn generate_isn() -> u32 {
    ISN_COUNTER.fetch_add(64000, Ordering::Relaxed)
}

// =============================================================================
// Ephemeral port allocator
// =============================================================================

static EPHEMERAL_PORT: AtomicU16 = AtomicU16::new(49152);

/// Allocate the next ephemeral port (49152–65535).
pub fn alloc_ephemeral_port() -> u16 {
    loop {
        let port = EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed);
        if port >= 49152 {
            return port;
        }
        // Wrapped around — reset.
        EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
    }
}

// =============================================================================
// Connection Table
// =============================================================================

/// Global TCP connection table.
static TCP_TABLE: IrqMutex<TcpConnectionTable> = IrqMutex::new(TcpConnectionTable::new());

pub struct TcpConnectionTable {
    connections: [TcpConnection; MAX_CONNECTIONS],
}

impl TcpConnectionTable {
    pub const fn new() -> Self {
        Self {
            connections: [TcpConnection::empty(); MAX_CONNECTIONS],
        }
    }

    /// Find a connection matching the given tuple.  Exact match first, then
    /// wildcard listen sockets.
    pub fn find(&self, tuple: &TcpTuple) -> Option<usize> {
        // First pass: exact match.
        for (i, conn) in self.connections.iter().enumerate() {
            if conn.active
                && conn.tuple.local_ip == tuple.local_ip
                && conn.tuple.local_port == tuple.local_port
                && conn.tuple.remote_ip == tuple.remote_ip
                && conn.tuple.remote_port == tuple.remote_port
            {
                return Some(i);
            }
        }
        // Second pass: wildcard listen sockets (remote = 0).
        for (i, conn) in self.connections.iter().enumerate() {
            if conn.active
                && conn.state == TcpState::Listen
                && conn.tuple.local_port == tuple.local_port
                && (conn.tuple.local_ip == [0; 4] || conn.tuple.local_ip == tuple.local_ip)
            {
                return Some(i);
            }
        }
        None
    }

    /// Find a free slot in the table.
    fn alloc_slot(&self) -> Option<usize> {
        for (i, conn) in self.connections.iter().enumerate() {
            if !conn.active {
                return Some(i);
            }
        }
        None
    }

    /// Count of active connections.
    pub fn active_count(&self) -> usize {
        self.connections.iter().filter(|c| c.active).count()
    }

    /// Check if a local port is already bound.
    pub fn port_in_use(&self, local_ip: [u8; 4], local_port: u16) -> bool {
        self.connections.iter().any(|c| {
            c.active
                && c.tuple.local_port == local_port
                && (c.tuple.local_ip == [0; 4]
                    || local_ip == [0; 4]
                    || c.tuple.local_ip == local_ip)
        })
    }

    /// Get a reference to a connection by index.
    pub fn get(&self, idx: usize) -> Option<&TcpConnection> {
        self.connections.get(idx).filter(|c| c.active)
    }

    /// Get a mutable reference to a connection by index.
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut TcpConnection> {
        self.connections.get_mut(idx).filter(|c| c.active)
    }

    /// Release a connection slot.
    pub fn release(&mut self, idx: usize) {
        if let Some(conn) = self.connections.get_mut(idx) {
            *conn = TcpConnection::empty();
        }
    }
}

// =============================================================================
// Public API — connection lifecycle
// =============================================================================

/// Open an active connection (client: SYN → SYN_SENT).
///
/// Returns `(connection_index, outgoing_SYN_segment)`.
pub fn tcp_connect(
    local_ip: [u8; 4],
    remote_ip: [u8; 4],
    remote_port: u16,
) -> Result<(usize, TcpOutSegment), TcpError> {
    let local_port = alloc_ephemeral_port();
    let iss = generate_isn();

    let mut table = TCP_TABLE.lock();

    let idx = table.alloc_slot().ok_or(TcpError::TableFull)?;

    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip,
        remote_port,
    };

    let conn = &mut table.connections[idx];
    conn.tuple = tuple;
    conn.state = TcpState::SynSent;
    conn.iss = iss;
    conn.snd_una = iss;
    conn.snd_nxt = iss.wrapping_add(1); // SYN consumes one sequence number
    conn.snd_wnd = 0;
    conn.rcv_wnd = DEFAULT_WINDOW_SIZE;
    conn.peer_mss = DEFAULT_MSS;
    conn.rto_ms = INITIAL_RTO_MS;
    conn.retransmits = 0;
    conn.active = true;

    klog_debug!(
        "tcp: CONNECT {}:{} -> {}:{} ISS={} idx={}",
        local_ip[0],
        local_ip[1],
        local_port,
        remote_ip[0],
        remote_port,
        idx
    );

    let seg = TcpOutSegment {
        tuple,
        seq_num: iss,
        ack_num: 0,
        flags: TCP_FLAG_SYN,
        window_size: DEFAULT_WINDOW_SIZE,
        mss: DEFAULT_MSS,
    };

    Ok((idx, seg))
}

/// Open a passive connection (server: → LISTEN).
///
/// Binds to `local_ip:local_port` and waits for incoming SYNs.
pub fn tcp_listen(local_ip: [u8; 4], local_port: u16) -> Result<usize, TcpError> {
    let mut table = TCP_TABLE.lock();

    if table.port_in_use(local_ip, local_port) {
        return Err(TcpError::AddrInUse);
    }

    let idx = table.alloc_slot().ok_or(TcpError::TableFull)?;

    let conn = &mut table.connections[idx];
    conn.tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    conn.state = TcpState::Listen;
    conn.rcv_wnd = DEFAULT_WINDOW_SIZE;
    conn.active = true;

    klog_debug!("tcp: LISTEN on port {} idx={}", local_port, idx);
    Ok(idx)
}

/// Close a connection (initiate graceful teardown).
///
/// Returns the outgoing FIN segment if one should be sent.
pub fn tcp_close(idx: usize) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = TCP_TABLE.lock();
    let conn = table.get_mut(idx).ok_or(TcpError::NotFound)?;

    match conn.state {
        TcpState::Closed => Err(TcpError::InvalidState),
        TcpState::Listen | TcpState::SynSent => {
            // No connection established — just release.
            let state = conn.state;
            table.release(idx);
            klog_debug!("tcp: CLOSE idx={} from {} — released", idx, state.name());
            Ok(None)
        }
        TcpState::SynReceived | TcpState::Established => {
            // Send FIN, move to FIN_WAIT_1.
            let seq = conn.snd_nxt;
            conn.snd_nxt = seq.wrapping_add(1); // FIN consumes one sequence number
            let prev = conn.state;
            conn.state = TcpState::FinWait1;

            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: seq,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
            };

            klog_debug!(
                "tcp: CLOSE idx={} {} -> FIN_WAIT_1, FIN seq={}",
                idx,
                prev.name(),
                seq
            );
            Ok(Some(seg))
        }
        TcpState::CloseWait => {
            // Peer already sent FIN — send our FIN, move to LAST_ACK.
            let seq = conn.snd_nxt;
            conn.snd_nxt = seq.wrapping_add(1);
            conn.state = TcpState::LastAck;

            let seg = TcpOutSegment {
                tuple: conn.tuple,
                seq_num: seq,
                ack_num: conn.rcv_nxt,
                flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                window_size: conn.rcv_wnd,
                mss: 0,
            };

            klog_debug!(
                "tcp: CLOSE idx={} CLOSE_WAIT -> LAST_ACK, FIN seq={}",
                idx,
                seq
            );
            Ok(Some(seg))
        }
        // Already closing — ignore.
        TcpState::FinWait1
        | TcpState::FinWait2
        | TcpState::Closing
        | TcpState::LastAck
        | TcpState::TimeWait => {
            klog_debug!(
                "tcp: CLOSE idx={} already closing ({})",
                idx,
                conn.state.name()
            );
            Ok(None)
        }
    }
}

/// Abort a connection (send RST, release immediately).
pub fn tcp_abort(idx: usize) -> Result<Option<TcpOutSegment>, TcpError> {
    let mut table = TCP_TABLE.lock();
    let conn = table.get_mut(idx).ok_or(TcpError::NotFound)?;

    let seg = if conn.state != TcpState::Listen && conn.state != TcpState::Closed {
        Some(TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: 0,
            flags: TCP_FLAG_RST,
            window_size: 0,
            mss: 0,
        })
    } else {
        None
    };

    klog_debug!("tcp: ABORT idx={} from {}", idx, conn.state.name());
    table.release(idx);
    Ok(seg)
}

// =============================================================================
// Incoming segment processing
// =============================================================================

/// Result of processing an incoming TCP segment.
#[derive(Clone, Debug)]
pub struct TcpInputResult {
    /// Outgoing segment(s) to send in response (ACK, SYN+ACK, RST, etc.).
    pub response: Option<TcpOutSegment>,
    /// Index of the connection this segment was processed against.
    pub conn_idx: Option<usize>,
    /// New state after processing.
    pub new_state: Option<TcpState>,
    /// If a new connection was accepted from a listen socket, its index.
    pub accepted_idx: Option<usize>,
    /// If the connection was reset.
    pub reset: bool,
}

impl TcpInputResult {
    const fn empty() -> Self {
        Self {
            response: None,
            conn_idx: None,
            new_state: None,
            accepted_idx: None,
            reset: false,
        }
    }
}

/// Build a RST segment in response to an unexpected incoming segment.
fn build_rst_for(hdr: &TcpHeader, local_ip: [u8; 4], remote_ip: [u8; 4]) -> TcpOutSegment {
    let (seq, ack, flags) = if hdr.is_ack() {
        // RST with seq = incoming ACK number.
        (hdr.ack_num, 0u32, TCP_FLAG_RST)
    } else {
        // RST+ACK with ack = incoming SEQ + segment length.
        let seg_len = if hdr.is_syn() { 1u32 } else { 0u32 };
        (
            0u32,
            hdr.seq_num.wrapping_add(seg_len),
            TCP_FLAG_RST | TCP_FLAG_ACK,
        )
    };

    TcpOutSegment {
        tuple: TcpTuple {
            local_ip,
            local_port: hdr.dst_port,
            remote_ip,
            remote_port: hdr.src_port,
        },
        seq_num: seq,
        ack_num: ack,
        flags,
        window_size: 0,
        mss: 0,
    }
}

/// Process an incoming TCP segment.
///
/// `src_ip` / `dst_ip` are from the IPv4 header.
/// `tcp_data` is the raw TCP segment (header + payload).
/// `options` is the TCP options region (may be empty).
/// `now_ms` is the current monotonic time in milliseconds.
///
/// Returns instructions for the caller (segments to send, state changes, etc.).
pub fn tcp_input(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    hdr: &TcpHeader,
    options: &[u8],
    _payload_len: usize,
    now_ms: u64,
) -> TcpInputResult {
    let incoming_tuple = TcpTuple {
        local_ip: dst_ip,
        local_port: hdr.dst_port,
        remote_ip: src_ip,
        remote_port: hdr.src_port,
    };

    let mut table = TCP_TABLE.lock();

    let conn_idx = match table.find(&incoming_tuple) {
        Some(idx) => idx,
        None => {
            // No matching connection — send RST unless it's already a RST.
            if hdr.is_rst() {
                return TcpInputResult::empty();
            }
            return TcpInputResult {
                response: Some(build_rst_for(hdr, dst_ip, src_ip)),
                ..TcpInputResult::empty()
            };
        }
    };

    let conn_state = table.connections[conn_idx].state;

    match conn_state {
        TcpState::Closed => {
            // Should not happen (slot would be inactive).
            TcpInputResult::empty()
        }

        TcpState::Listen => process_listen(&mut table, conn_idx, hdr, options, &incoming_tuple),

        TcpState::SynSent => process_syn_sent(&mut table, conn_idx, hdr, options),

        TcpState::SynReceived => process_syn_received(&mut table, conn_idx, hdr),

        TcpState::Established
        | TcpState::FinWait1
        | TcpState::FinWait2
        | TcpState::CloseWait
        | TcpState::Closing
        | TcpState::LastAck => process_established_and_closing(&mut table, conn_idx, hdr, now_ms),

        TcpState::TimeWait => process_time_wait(&mut table, conn_idx, hdr),
    }
}

// =============================================================================
// Per-state processing
// =============================================================================

/// LISTEN state: expecting SYN.
fn process_listen(
    table: &mut TcpConnectionTable,
    listen_idx: usize,
    hdr: &TcpHeader,
    options: &[u8],
    incoming_tuple: &TcpTuple,
) -> TcpInputResult {
    // RST in LISTEN — ignore.
    if hdr.is_rst() {
        return TcpInputResult::empty();
    }

    // ACK to a LISTEN — send RST.
    if hdr.is_ack() {
        return TcpInputResult {
            response: Some(TcpOutSegment {
                tuple: TcpTuple {
                    local_ip: incoming_tuple.local_ip,
                    local_port: incoming_tuple.local_port,
                    remote_ip: incoming_tuple.remote_ip,
                    remote_port: incoming_tuple.remote_port,
                },
                seq_num: hdr.ack_num,
                ack_num: 0,
                flags: TCP_FLAG_RST,
                window_size: 0,
                mss: 0,
            }),
            conn_idx: Some(listen_idx),
            ..TcpInputResult::empty()
        };
    }

    // SYN — create a new connection in SYN_RECEIVED.
    if !hdr.is_syn() {
        return TcpInputResult::empty();
    }

    let new_idx = match table.alloc_slot() {
        Some(i) => i,
        None => return TcpInputResult::empty(), // Table full, drop silently.
    };

    let iss = generate_isn();
    let peer_mss = parse_mss_option(options).unwrap_or(DEFAULT_MSS);

    let child = &mut table.connections[new_idx];
    child.tuple = *incoming_tuple;
    child.state = TcpState::SynReceived;
    child.iss = iss;
    child.snd_una = iss;
    child.snd_nxt = iss.wrapping_add(1);
    child.irs = hdr.seq_num;
    child.rcv_nxt = hdr.seq_num.wrapping_add(1);
    child.snd_wnd = hdr.window_size;
    child.rcv_wnd = DEFAULT_WINDOW_SIZE;
    child.peer_mss = peer_mss;
    child.rto_ms = INITIAL_RTO_MS;
    child.retransmits = 0;
    child.active = true;

    klog_debug!(
        "tcp: LISTEN -> SYN_RECEIVED idx={} ISS={} IRS={}",
        new_idx,
        iss,
        hdr.seq_num
    );

    let seg = TcpOutSegment {
        tuple: child.tuple,
        seq_num: iss,
        ack_num: child.rcv_nxt,
        flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
        window_size: DEFAULT_WINDOW_SIZE,
        mss: DEFAULT_MSS,
    };

    TcpInputResult {
        response: Some(seg),
        conn_idx: Some(listen_idx),
        new_state: Some(TcpState::SynReceived),
        accepted_idx: Some(new_idx),
        reset: false,
    }
}

/// SYN_SENT state: expecting SYN+ACK (or simultaneous open SYN).
fn process_syn_sent(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
    options: &[u8],
) -> TcpInputResult {
    let conn = &table.connections[idx];
    let iss = conn.iss;

    // Step 1: Check ACK.
    if hdr.is_ack() {
        // ACK must acknowledge our SYN.
        if seq_le(hdr.ack_num, iss) || seq_gt(hdr.ack_num, conn.snd_nxt) {
            // Bad ACK — send RST unless incoming is RST.
            if hdr.is_rst() {
                return TcpInputResult::empty();
            }
            return TcpInputResult {
                response: Some(TcpOutSegment {
                    tuple: conn.tuple,
                    seq_num: hdr.ack_num,
                    ack_num: 0,
                    flags: TCP_FLAG_RST,
                    window_size: 0,
                    mss: 0,
                }),
                conn_idx: Some(idx),
                ..TcpInputResult::empty()
            };
        }
    }

    // Step 2: Check RST.
    if hdr.is_rst() {
        if hdr.is_ack() {
            // Valid RST — connection refused.
            klog_debug!(
                "tcp: SYN_SENT idx={} — RST received, connection refused",
                idx
            );
            table.release(idx);
            return TcpInputResult {
                conn_idx: Some(idx),
                new_state: Some(TcpState::Closed),
                reset: true,
                ..TcpInputResult::empty()
            };
        }
        return TcpInputResult::empty();
    }

    // Step 3: Check SYN.
    if !hdr.is_syn() {
        return TcpInputResult::empty();
    }

    let peer_mss = parse_mss_option(options).unwrap_or(DEFAULT_MSS);
    let conn = &mut table.connections[idx];
    conn.irs = hdr.seq_num;
    conn.rcv_nxt = hdr.seq_num.wrapping_add(1);
    conn.snd_wnd = hdr.window_size;
    conn.peer_mss = peer_mss;

    if hdr.is_ack() {
        // SYN+ACK — our SYN was acknowledged.
        conn.snd_una = hdr.ack_num;
    }

    if seq_gt(conn.snd_una, conn.iss) {
        // Our SYN has been ACKed → ESTABLISHED.
        conn.state = TcpState::Established;
        conn.retransmits = 0;

        klog_debug!("tcp: SYN_SENT -> ESTABLISHED idx={} IRS={}", idx, conn.irs);

        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: 0,
        };

        TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::Established),
            accepted_idx: None,
            reset: false,
        }
    } else {
        // Simultaneous open: SYN without ACK → SYN_RECEIVED.
        conn.state = TcpState::SynReceived;

        klog_debug!(
            "tcp: SYN_SENT -> SYN_RECEIVED idx={} (simultaneous open)",
            idx
        );

        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.iss,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: DEFAULT_MSS,
        };

        TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::SynReceived),
            accepted_idx: None,
            reset: false,
        }
    }
}

/// SYN_RECEIVED state: expecting ACK to complete handshake.
fn process_syn_received(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
) -> TcpInputResult {
    let conn = &table.connections[idx];

    // RST — abort.
    if hdr.is_rst() {
        klog_debug!("tcp: SYN_RECEIVED idx={} — RST, closing", idx);
        table.release(idx);
        return TcpInputResult {
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            reset: true,
            ..TcpInputResult::empty()
        };
    }

    // Must have ACK.
    if !hdr.is_ack() {
        return TcpInputResult::empty();
    }

    // Validate ACK range.
    if seq_lt(hdr.ack_num, conn.snd_una) || seq_gt(hdr.ack_num, conn.snd_nxt) {
        // Bad ACK — send RST.
        return TcpInputResult {
            response: Some(TcpOutSegment {
                tuple: conn.tuple,
                seq_num: hdr.ack_num,
                ack_num: 0,
                flags: TCP_FLAG_RST,
                window_size: 0,
                mss: 0,
            }),
            conn_idx: Some(idx),
            ..TcpInputResult::empty()
        };
    }

    // Valid ACK → ESTABLISHED.
    let conn = &mut table.connections[idx];
    conn.snd_una = hdr.ack_num;
    conn.snd_wnd = hdr.window_size;
    conn.state = TcpState::Established;
    conn.retransmits = 0;

    klog_debug!("tcp: SYN_RECEIVED -> ESTABLISHED idx={}", idx);

    TcpInputResult {
        response: None,
        conn_idx: Some(idx),
        new_state: Some(TcpState::Established),
        accepted_idx: None,
        reset: false,
    }
}

/// ESTABLISHED and closing states: main segment processing.
fn process_established_and_closing(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
    now_ms: u64,
) -> TcpInputResult {
    let current_state = table.connections[idx].state;

    // Step 1: Check RST.
    if hdr.is_rst() {
        klog_debug!("tcp: {} idx={} — RST received", current_state.name(), idx);
        table.release(idx);
        return TcpInputResult {
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            reset: true,
            ..TcpInputResult::empty()
        };
    }

    // Step 2: Check SYN (unexpected in established+ states → RST).
    if hdr.is_syn() {
        let tuple = table.connections[idx].tuple;
        let snd_nxt = table.connections[idx].snd_nxt;
        klog_debug!(
            "tcp: {} idx={} — unexpected SYN, sending RST",
            current_state.name(),
            idx
        );
        table.release(idx);
        return TcpInputResult {
            response: Some(TcpOutSegment {
                tuple,
                seq_num: snd_nxt,
                ack_num: 0,
                flags: TCP_FLAG_RST,
                window_size: 0,
                mss: 0,
            }),
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            accepted_idx: None,
            reset: true,
        };
    }

    // Step 3: Check ACK.
    if !hdr.is_ack() {
        return TcpInputResult::empty();
    }

    // Update snd_una / snd_wnd from the ACK.
    let conn = &mut table.connections[idx];
    if seq_gt(hdr.ack_num, conn.snd_una) && seq_le(hdr.ack_num, conn.snd_nxt) {
        conn.snd_una = hdr.ack_num;
        conn.snd_wnd = hdr.window_size;
    }

    // State-specific ACK processing.
    match current_state {
        TcpState::FinWait1 => {
            // If our FIN is acknowledged.
            if hdr.ack_num == conn.snd_nxt {
                if hdr.is_fin() {
                    // Simultaneous close: FIN+ACK acks our FIN and carries theirs.
                    conn.rcv_nxt = hdr.seq_num.wrapping_add(1);
                    conn.state = TcpState::TimeWait;
                    conn.time_wait_start_ms = now_ms;
                    klog_debug!(
                        "tcp: FIN_WAIT_1 -> TIME_WAIT idx={} (simultaneous close)",
                        idx
                    );

                    let seg = TcpOutSegment {
                        tuple: conn.tuple,
                        seq_num: conn.snd_nxt,
                        ack_num: conn.rcv_nxt,
                        flags: TCP_FLAG_ACK,
                        window_size: conn.rcv_wnd,
                        mss: 0,
                    };
                    return TcpInputResult {
                        response: Some(seg),
                        conn_idx: Some(idx),
                        new_state: Some(TcpState::TimeWait),
                        accepted_idx: None,
                        reset: false,
                    };
                }
                conn.state = TcpState::FinWait2;
                klog_debug!("tcp: FIN_WAIT_1 -> FIN_WAIT_2 idx={}", idx);
            }
        }
        TcpState::Closing => {
            if hdr.ack_num == conn.snd_nxt {
                conn.state = TcpState::TimeWait;
                conn.time_wait_start_ms = now_ms;
                klog_debug!("tcp: CLOSING -> TIME_WAIT idx={}", idx);
                return TcpInputResult {
                    response: None,
                    conn_idx: Some(idx),
                    new_state: Some(TcpState::TimeWait),
                    accepted_idx: None,
                    reset: false,
                };
            }
        }
        TcpState::LastAck => {
            if hdr.ack_num == conn.snd_nxt {
                klog_debug!("tcp: LAST_ACK -> CLOSED idx={}", idx);
                table.release(idx);
                return TcpInputResult {
                    conn_idx: Some(idx),
                    new_state: Some(TcpState::Closed),
                    accepted_idx: None,
                    reset: false,
                    response: None,
                };
            }
        }
        _ => {}
    }

    // Step 4: Check FIN.
    if hdr.is_fin() {
        let conn = &mut table.connections[idx];
        conn.rcv_nxt = hdr.seq_num.wrapping_add(1);

        let new_state = match current_state {
            TcpState::Established => {
                conn.state = TcpState::CloseWait;
                klog_debug!("tcp: ESTABLISHED -> CLOSE_WAIT idx={}", idx);
                TcpState::CloseWait
            }
            TcpState::FinWait1 => {
                // Our FIN not yet acked + peer FIN → CLOSING.
                conn.state = TcpState::Closing;
                klog_debug!("tcp: FIN_WAIT_1 -> CLOSING idx={}", idx);
                TcpState::Closing
            }
            TcpState::FinWait2 => {
                conn.state = TcpState::TimeWait;
                conn.time_wait_start_ms = now_ms;
                klog_debug!("tcp: FIN_WAIT_2 -> TIME_WAIT idx={}", idx);
                TcpState::TimeWait
            }
            other => other, // FIN in other states — just ACK.
        };

        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: 0,
        };

        return TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(new_state),
            accepted_idx: None,
            reset: false,
        };
    }

    TcpInputResult {
        response: None,
        conn_idx: Some(idx),
        new_state: Some(table.connections[idx].state),
        accepted_idx: None,
        reset: false,
    }
}

/// TIME_WAIT state: handle retransmitted FIN.
fn process_time_wait(
    table: &mut TcpConnectionTable,
    idx: usize,
    hdr: &TcpHeader,
) -> TcpInputResult {
    let conn = &table.connections[idx];

    if hdr.is_rst() {
        table.release(idx);
        return TcpInputResult {
            conn_idx: Some(idx),
            new_state: Some(TcpState::Closed),
            reset: true,
            ..TcpInputResult::empty()
        };
    }

    // Retransmitted FIN — re-ACK and restart timer.
    if hdr.is_fin() {
        let seg = TcpOutSegment {
            tuple: conn.tuple,
            seq_num: conn.snd_nxt,
            ack_num: conn.rcv_nxt,
            flags: TCP_FLAG_ACK,
            window_size: conn.rcv_wnd,
            mss: 0,
        };
        // Restart TIME_WAIT timer.
        let conn = &mut table.connections[idx];
        conn.time_wait_start_ms = 0; // Caller should set to current time.

        return TcpInputResult {
            response: Some(seg),
            conn_idx: Some(idx),
            new_state: Some(TcpState::TimeWait),
            accepted_idx: None,
            reset: false,
        };
    }

    TcpInputResult::empty()
}

// =============================================================================
// Timer-driven maintenance
// =============================================================================

/// Expire TIME_WAIT connections whose 2×MSL has elapsed.
///
/// Call periodically from a timer context.  Returns the number of connections
/// reaped.
pub fn tcp_timer_tick(now_ms: u64) -> usize {
    let mut table = TCP_TABLE.lock();
    let mut reaped = 0usize;
    for i in 0..MAX_CONNECTIONS {
        let conn = &table.connections[i];
        if conn.active
            && conn.state == TcpState::TimeWait
            && now_ms.saturating_sub(conn.time_wait_start_ms) >= TIME_WAIT_MS
        {
            klog_debug!("tcp: TIME_WAIT expired idx={}", i);
            table.release(i);
            reaped += 1;
        }
    }
    reaped
}

// =============================================================================
// Query helpers (for tests and upper layers)
// =============================================================================

/// Get a snapshot of a connection's state.
pub fn tcp_get_state(idx: usize) -> Option<TcpState> {
    TCP_TABLE.lock().get(idx).map(|c| c.state)
}

/// Get a snapshot of a connection.
pub fn tcp_get_connection(idx: usize) -> Option<TcpConnection> {
    TCP_TABLE.lock().get(idx).copied()
}

/// Get the number of active connections.
pub fn tcp_active_count() -> usize {
    TCP_TABLE.lock().active_count()
}

/// Find a connection index by tuple.
pub fn tcp_find(tuple: &TcpTuple) -> Option<usize> {
    TCP_TABLE.lock().find(tuple)
}

/// Release all connections (for testing).
pub fn tcp_reset_all() {
    let mut table = TCP_TABLE.lock();
    for i in 0..MAX_CONNECTIONS {
        table.connections[i] = TcpConnection::empty();
    }
    // Reset ISN counter and ephemeral port for deterministic tests.
    ISN_COUNTER.store(0x4F50_534C, Ordering::Relaxed);
    EPHEMERAL_PORT.store(49152, Ordering::Relaxed);
}
