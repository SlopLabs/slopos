//! Per-connection send and receive buffers.

use slopos_ostd::mm::uframe::{KeepaliveFrames, copy_out_frames};
use slopos_ostd::{
    AllocError, Init, Initialised, KBox, KVecDeque, SlotPtr, ZcNotifToken, init_struct_with,
    write_field,
};

use super::chunk::{self, ChunkRing, Spares};
use super::retx::SendMap;

/// Allocated up front and never grown, since the piece queue is pushed to
/// under the PCB lock.
const SEND_PIECES: usize = 64;

/// Delayed ACK timeout in milliseconds (RFC 1122 §4.2.3.2).
pub const DELAYED_ACK_MS: u64 = 200;

/// Send ACK after this many unacknowledged data segments.
pub const DELAYED_ACK_SEGMENTS: u8 = 2;

/// One segment of the send byte-stream, in stream order.
///
/// The concatenation, in queue order, of all `Inline` pieces' bytes *is* the
/// ring's contents. `Zerocopy` data lives in pinned user pages the NIC DMAs
/// straight from (TCP `MSG_ZEROCOPY`): the piece holds an owning ref on every
/// backing page until cumulative ACK, and a refcounted [`ZcNotifToken`] gating
/// the buffer-reusable notification.
enum SendPiece {
    Inline {
        len: u32,
    },
    Zerocopy {
        keepalive: KeepaliveFrames,
        /// In-page byte offset within `keepalive[0]`; advances as the piece is
        /// partially acked.
        base_off: usize,
        len: u32,
        token: ZcNotifToken,
    },
}

impl SendPiece {
    fn len(&self) -> usize {
        match self {
            SendPiece::Inline { len } | SendPiece::Zerocopy { len, .. } => *len as usize,
        }
    }
}

/// The pinned-page source of a zero-copy outgoing segment, handed to the
/// transmit leaf to DMA from (or copy-fall-back from on a cold neighbor).
/// Carries an **independent** keepalive clone, held by the driver TX slot until
/// the descriptor is reclaimed. Not `Copy` — it owns page refs — so it rides
/// beside the `Copy` [`TcpOutSegment`] rather than inside it.
pub struct ZcSource {
    pub keepalive: KeepaliveFrames,
    /// Absolute byte offset of this segment within `keepalive`.
    pub byte_start: usize,
    pub len: usize,
    pub token: ZcNotifToken,
}

/// Where the bytes of one outgoing segment come from — the result of locating a
/// stream offset in the piece queue (see [`TcpSendState::segment_source`]).
pub(crate) enum SegmentSource {
    /// No data buffered at the requested offset.
    Empty,
    /// `len` bytes copied from the inline ring (the caller peeks them).
    Inline { len: usize },
    /// `len` bytes the NIC can DMA straight from the pinned pages, with an
    /// **independent** keepalive clone.
    Zerocopy {
        keepalive: KeepaliveFrames,
        /// Absolute byte offset of this segment within `keepalive`.
        byte_start: usize,
        len: usize,
        token: ZcNotifToken,
    },
}

pub struct TcpSendState {
    /// Inline (copied) bytes, FIFO in stream order; zero-copy pieces reference
    /// pinned pages, not this ring.
    pub(crate) ring: ChunkRing,
    /// Pieces in send order, boxed so `TcpSendState` stays within its size
    /// tripwire.
    pieces: KBox<KVecDeque<SendPiece>>,
    /// Here rather than in the PCB state: this pair is allocated outside the
    /// PCB lock, and the map is a page allocation.
    pub(crate) sendmap: KBox<SendMap>,
    /// Bytes sent but not yet acked — a stream offset measured from the queue
    /// head (`snd_una`).
    pub(crate) inflight: usize,
    /// Total stream bytes buffered (sum of all piece lengths).
    buffered: usize,
    pub(crate) rto_deadline_ms: u64,
    /// SO_SNDBUF, which counts zero-copy bytes as well as the ring's.
    pub(crate) effective_capacity: usize,
}

impl TcpSendState {
    pub(crate) fn new(max: usize, cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            ring: ChunkRing::new(max, max)?,
            pieces: KBox::try_new(KVecDeque::with_capacity(SEND_PIECES)?)?,
            sendmap: SendMap::boxed()?,
            inflight: 0,
            buffered: 0,
            rto_deadline_ms: 0,
            effective_capacity: cap.min(max),
        })
    }

    pub(crate) fn set_capacity(&mut self, cap: usize) {
        self.effective_capacity = cap.min(self.ring.max_capacity());
    }

    /// Ensure the tail piece is `Inline`; `false` if a new one is needed but
    /// cannot be allocated.
    fn ensure_inline_tail(&mut self) -> bool {
        if matches!(self.pieces.back(), Some(SendPiece::Inline { .. })) {
            return true;
        }
        self.pieces.len() < SEND_PIECES
            && self.pieces.push_back(SendPiece::Inline { len: 0 }).is_ok()
    }

    fn extend_inline_tail(&mut self, n: usize) {
        if let Some(SendPiece::Inline { len }) = self.pieces.back_mut() {
            *len += n as u32;
        }
        self.buffered += n;
    }

    pub fn enqueue(&mut self, data: &[u8], spares: &mut Spares) -> usize {
        let avail = self.free_space();
        let n = core::cmp::min(data.len(), avail);
        if n == 0 || !self.ensure_inline_tail() {
            return 0;
        }
        let wrote = self.ring.write(&data[..n], spares);
        self.extend_inline_tail(wrote);
        wrote
    }

    /// Single-direct-copy [`enqueue`](Self::enqueue): up to
    /// `min(free_space, reader.remain())` bytes straight from the pinned user
    /// pages into the send ring, with no kernel scratch. Returns the number of
    /// bytes buffered.
    pub fn enqueue_from(
        &mut self,
        reader: &mut slopos_ostd::mm::VmReader<'_>,
        spares: &mut Spares,
    ) -> usize {
        let avail = self.free_space();
        if avail == 0 || !self.ensure_inline_tail() {
            return 0;
        }
        let wrote = self.ring.write_from(reader, avail, spares);
        self.extend_inline_tail(wrote);
        wrote
    }

    /// Append a zero-copy piece: the NIC will DMA `len` bytes straight from the
    /// pinned pages `keepalive` (whose data begins at `base_off`), held until
    /// the bytes are cumulatively ACKed. Returns `false` if the piece queue
    /// is full. Does **not** check `free_space`; the caller
    /// (`socket_send_zerocopy`) gates against SO_SNDBUF.
    pub(crate) fn enqueue_zerocopy(
        &mut self,
        keepalive: KeepaliveFrames,
        base_off: usize,
        len: u32,
        token: ZcNotifToken,
    ) -> bool {
        if self.pieces.len() == SEND_PIECES
            || self
                .pieces
                .push_back(SendPiece::Zerocopy {
                    keepalive,
                    base_off,
                    len,
                    token,
                })
                .is_err()
        {
            return false;
        }
        self.buffered += len as usize;
        true
    }

    pub fn unsent_len(&self) -> usize {
        self.buffered.saturating_sub(self.inflight)
    }

    pub fn buffered_len(&self) -> usize {
        self.buffered
    }

    pub fn free_space(&self) -> usize {
        // The SO_SNDBUF headroom counts in-flight zero-copy bytes too.
        let raw = self.ring.free_space();
        let cap_limit = self.effective_capacity.saturating_sub(self.buffered);
        core::cmp::min(raw, cap_limit)
    }

    /// What a copying write can take now: the free space, as far as chunks
    /// and a piece exist to hold it.
    pub fn writable(&self) -> usize {
        let inline_tail_available = matches!(self.pieces.back(), Some(SendPiece::Inline { .. }))
            || self.pieces.len() < SEND_PIECES;
        if !inline_tail_available {
            return 0;
        }
        self.free_space()
            .min(self.ring.tail_room() + self.ring.backed_room())
    }

    pub fn chunks_held(&self) -> usize {
        self.ring.chunks_held()
    }

    /// SO_SNDBUF headroom for a zero-copy enqueue (no ring involved).
    pub(crate) fn zc_free_space(&self) -> usize {
        self.effective_capacity.saturating_sub(self.buffered)
    }

    /// Copy up to `out.len()` bytes of the send stream from stream offset `off`
    /// into `out`, clamped to the covering piece's boundary. Zero-copy pieces
    /// are read volatilely from the pinned pages. Returns the count.
    fn peek_at_stream(&self, off: usize, out: &mut [u8]) -> usize {
        let mut acc = 0usize;
        let mut ring_acc = 0usize;
        for piece in self.pieces.iter() {
            let len = piece.len();
            if off < acc + len {
                let intra = off - acc;
                let n = core::cmp::min(out.len(), len - intra);
                if n == 0 {
                    return 0;
                }
                return match piece {
                    SendPiece::Inline { .. } => self.ring.peek_at(ring_acc + intra, &mut out[..n]),
                    SendPiece::Zerocopy {
                        keepalive,
                        base_off,
                        ..
                    } => {
                        match copy_out_frames(keepalive.as_slice(), base_off + intra, &mut out[..n])
                        {
                            Ok(()) => n,
                            Err(_) => 0,
                        }
                    }
                };
            }
            acc += len;
            if let SendPiece::Inline { .. } = piece {
                ring_acc += len;
            }
        }
        0
    }

    pub fn peek_unsent(&self, out: &mut [u8]) -> usize {
        self.peek_at_stream(self.inflight, out)
    }

    /// Read data at an arbitrary offset within the buffered (unacked) range.
    pub fn peek_retransmit(&self, offset: usize, out: &mut [u8]) -> usize {
        self.peek_at_stream(offset, out)
    }

    /// Resolve the source of one outgoing segment at stream offset `off`, taking
    /// at most `max_len` bytes and never crossing a piece boundary.
    pub(crate) fn segment_source(&self, off: usize, max_len: usize) -> SegmentSource {
        let mut acc = 0usize;
        for piece in self.pieces.iter() {
            let len = piece.len();
            if off < acc + len {
                let intra = off - acc;
                let n = core::cmp::min(max_len, len - intra);
                if n == 0 {
                    return SegmentSource::Empty;
                }
                return match piece {
                    SendPiece::Inline { .. } => SegmentSource::Inline { len: n },
                    SendPiece::Zerocopy {
                        keepalive,
                        base_off,
                        token,
                        ..
                        // A retransmit is a second in-flight DMA of the same
                        // pages, so it takes and pays for its own keepalive.
                    } => match keepalive.redup() {
                        // Cloning the page refs failed (OOM or the pin ceiling):
                        // fall back to a copy via `peek_at_stream`.
                        None => SegmentSource::Inline { len: n },
                        Some(ka) => SegmentSource::Zerocopy {
                            keepalive: ka,
                            byte_start: base_off + intra,
                            len: n,
                            token: token.clone(),
                        },
                    },
                };
            }
            acc += len;
        }
        SegmentSource::Empty
    }

    pub fn mark_sent(&mut self, n: usize) {
        let unsent = self.unsent_len();
        let sent = core::cmp::min(n, unsent);
        self.inflight += sent;
    }

    pub fn process_ack(&mut self, acked: usize) {
        if acked == 0 {
            return;
        }
        let consumed = core::cmp::min(acked, self.buffered);
        let mut left = consumed;
        while left > 0 {
            let Some(front) = self.pieces.front_mut() else {
                break;
            };
            let clen = front.len();
            if clen <= left {
                left -= clen;
                match self.pieces.pop_front() {
                    Some(SendPiece::Inline { len }) => self.ring.consume(len as usize),
                    // Whole piece acked: retire its token reference; the buffer
                    // becomes reusable once every in-flight DMA is reclaimed.
                    Some(SendPiece::Zerocopy { token, .. }) => token.mark_acked_and_release(),
                    None => break,
                }
            } else {
                match front {
                    SendPiece::Inline { len } => {
                        self.ring.consume(left);
                        *len -= left as u32;
                    }
                    SendPiece::Zerocopy { base_off, len, .. } => {
                        *base_off += left;
                        *len -= left as u32;
                    }
                }
                left = 0;
            }
        }
        self.buffered -= consumed;
        self.inflight = self.inflight.saturating_sub(consumed);
        if self.inflight == 0 {
            self.rto_deadline_ms = 0;
        }
    }

    pub fn clear(&mut self) {
        // Retire in-flight zero-copy pieces so their notification tokens make
        // progress on teardown; the driver's independent keepalive keeps any
        // in-flight DMA's pages alive.
        while let Some(piece) = self.pieces.pop_front() {
            if let SendPiece::Zerocopy { token, .. } = piece {
                token.mark_acked_and_release();
            }
        }
        self.ring.reset();
        self.sendmap.clear();
        self.inflight = 0;
        self.buffered = 0;
        self.rto_deadline_ms = 0;
    }

    pub fn inflight(&self) -> usize {
        self.inflight
    }

    pub fn rto_deadline_ms(&self) -> u64 {
        self.rto_deadline_ms
    }

    pub fn set_rto_deadline_ms(&mut self, deadline: u64) {
        self.rto_deadline_ms = deadline;
    }

    pub fn effective_capacity(&self) -> usize {
        self.effective_capacity
    }
}

pub struct TcpRecvState {
    pub(crate) buf: ChunkRing,
    pub(crate) segments_since_ack: u8,
    pub(crate) ack_pending: bool,
    pub(crate) delayed_ack_deadline_ms: u64,
}

impl TcpRecvState {
    pub(crate) fn new(max: usize, cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            buf: ChunkRing::new(max, cap)?,
            segments_since_ack: 0,
            ack_pending: false,
            delayed_ack_deadline_ms: 0,
        })
    }

    pub(crate) fn set_capacity(&mut self, cap: usize) {
        self.buf.set_capacity(cap);
    }

    pub fn enqueue(&mut self, data: &[u8], spares: &mut Spares, now_ms: u64) -> usize {
        if data.is_empty() {
            return 0;
        }

        let wrote = self.buf.write(data, spares);
        if wrote > 0 {
            self.ack_pending = true;
            self.segments_since_ack = self.segments_since_ack.saturating_add(1);
            if self.segments_since_ack == 1 {
                self.delayed_ack_deadline_ms = now_ms.saturating_add(DELAYED_ACK_MS);
            }
        }
        wrote
    }

    pub fn dequeue(&mut self, out: &mut [u8]) -> usize {
        self.buf.read(out)
    }

    /// Single-direct-copy [`dequeue`](Self::dequeue): up to
    /// `min(available, writer.remain())` bytes straight from the recv ring into
    /// the pinned user pages, with no kernel scratch. Returns the number of
    /// bytes drained.
    pub fn dequeue_into(&mut self, writer: &mut slopos_ostd::mm::VmWriter<'_>) -> usize {
        self.buf.read_into(writer)
    }

    pub fn available(&self) -> usize {
        self.buf.len()
    }

    /// In bytes, unscaled. Bounded by the chunks that can still be found: a
    /// window the allocator cannot back invites bytes the receive path drops.
    pub fn window(&self) -> u32 {
        self.buf.free_space().min(self.buf.backed_room()) as u32
    }

    pub fn should_ack_now(&self, now_ms: u64) -> bool {
        self.ack_pending
            && (self.segments_since_ack >= DELAYED_ACK_SEGMENTS
                || (self.delayed_ack_deadline_ms != 0 && now_ms >= self.delayed_ack_deadline_ms))
    }

    pub fn ack_sent(&mut self) {
        self.segments_since_ack = 0;
        self.ack_pending = false;
        self.delayed_ack_deadline_ms = 0;
    }

    pub fn clear(&mut self) {
        self.buf.reset();
        self.segments_since_ack = 0;
        self.ack_pending = false;
        self.delayed_ack_deadline_ms = 0;
    }

    pub fn ack_pending(&self) -> bool {
        self.ack_pending
    }

    pub fn segments_since_ack(&self) -> u8 {
        self.segments_since_ack
    }

    pub fn delayed_ack_deadline_ms(&self) -> u64 {
        self.delayed_ack_deadline_ms
    }

    pub fn effective_capacity(&self) -> usize {
        self.buf.capacity()
    }
}

#[derive(slopos_ostd::SlotFields)]
pub struct TcpBufferPair {
    pub(crate) send: TcpSendState,
    pub(crate) recv: TcpRecvState,
    pub(crate) ooo: super::reasm::Assembler,
    /// What the receive path may draw on under the PCB lock; filled before it
    /// is taken and emptied after.
    pub(crate) spares: Spares,
}

/// Connection-buffer allocations still owed a synthetic failure.
#[cfg(feature = "test-hooks")]
static INJECTED_ALLOC_FAILURES: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Fail the next `count` connection-buffer allocations.
///
/// The failure path runs under a cli-spinlock, and exhausting the real heap to
/// reach it is neither hermetic nor repeatable.
#[cfg(feature = "test-hooks")]
pub fn inject_buffer_alloc_failures(count: u32) {
    INJECTED_ALLOC_FAILURES.store(count, core::sync::atomic::Ordering::Relaxed);
}

#[cfg(feature = "test-hooks")]
fn take_injected_alloc_failure() -> bool {
    use core::sync::atomic::Ordering;
    INJECTED_ALLOC_FAILURES
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok()
}

impl TcpBufferPair {
    /// By value, for a test that drives the state machine with no table.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn new(rcv_cap: usize, snd_cap: usize) -> Result<Self, AllocError> {
        let max = chunk::buffer_max();
        Ok(Self {
            send: TcpSendState::new(max, snd_cap)?,
            recv: TcpRecvState::new(max, rcv_cap)?,
            ooo: super::reasm::Assembler::new(),
            spares: Spares::new(),
        })
    }

    /// Built field by field in its heap slot: the whole pair as one rvalue is
    /// past the 2 KiB stack gate once `Box` has copied it about.
    pub(crate) fn boxed(rcv_cap: usize, snd_cap: usize) -> Result<KBox<Self>, AllocError> {
        #[cfg(feature = "test-hooks")]
        if take_injected_alloc_failure() {
            return Err(AllocError);
        }
        KBox::try_init(Self::init(rcv_cap, snd_cap))
    }

    fn init(rcv_cap: usize, snd_cap: usize) -> impl Init<Self, AllocError> {
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                let max = chunk::buffer_max();
                // Both before either is written: a field already in the slot
                // is not dropped when a later one fails.
                let send = TcpSendState::new(max, snd_cap)?;
                let recv = TcpRecvState::new(max, rcv_cap)?;
                write_field!(slot, send, send);
                write_field!(slot, recv, recv);
                write_field!(slot, ooo, super::reasm::Assembler::new());
                write_field!(slot, spares, Spares::new());
                Ok(slot.finish())
            },
        )
    }

    pub fn clear(&mut self) {
        self.send.clear();
        self.recv.clear();
        self.ooo.clear();
    }

    pub fn send(&self) -> &TcpSendState {
        &self.send
    }

    pub fn send_mut(&mut self) -> &mut TcpSendState {
        &mut self.send
    }

    pub fn recv(&self) -> &TcpRecvState {
        &self.recv
    }

    pub fn recv_mut(&mut self) -> &mut TcpRecvState {
        &mut self.recv
    }

    pub fn ooo(&self) -> &super::reasm::Assembler {
        &self.ooo
    }

    pub fn ooo_mut(&mut self) -> &mut super::reasm::Assembler {
        &mut self.ooo
    }
}

// Size tripwires: these state types stay small so every function along the
// buffer-allocation chain keeps a tiny frame.
const _: () = assert!(core::mem::size_of::<TcpSendState>() <= 128);
const _: () = assert!(core::mem::size_of::<TcpRecvState>() <= 128);
const _: () = assert!(core::mem::size_of::<TcpBufferPair>() <= 512);
