//! SACK-aware send map for in-flight TCP segments (RFC 6675).
//!
//! Per-segment state drives selective retransmission: SACK coverage confirms
//! entries, DupThresh confirmations past a hole declare it `Lost`, and an RTO
//! marks everything `Lost` rather than rewinding `snd_nxt`.
//!
//! One entry per segment in flight, so the capacity bounds the send window
//! (1024 full-sized segments, ~1.5 MB). Running totals answer every transmit's
//! pipe query without a scan; only loss recovery walks the entries.

use slopos_ostd::{AllocError, KBox};

use crate::tcp::seq::SeqNum;

/// Maximum number of in-flight entries tracked per connection.
pub const SENDMAP_CAPACITY: usize = 1024;

/// DupThresh: number of SACKed entries past a hole to declare it lost.
const DUP_THRESH: usize = 3;

/// Lifecycle state of a single in-flight segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, slopos_ostd::Zeroable)]
#[repr(u8)]
pub enum SegmentState {
    /// Sent, not yet acknowledged or SACKed by the peer. **Discriminant 0**: a
    /// zero byte must be a valid `InFlight`, which [`SendMap`]'s zero-fill
    /// relies on.
    InFlight = 0,
    /// Covered by a SACK block from the peer.
    SackConfirmed = 1,
    /// Declared lost: ≥ DupThresh SACKed entries exist past this hole, or an
    /// RTO fired.
    Lost = 2,
    /// Was `Lost`, has been retransmitted, now in flight again.
    Retransmitted = 3,
}

impl SegmentState {
    fn in_pipe(self) -> bool {
        matches!(self, Self::InFlight | Self::Retransmitted)
    }
}

impl Default for SegmentState {
    fn default() -> Self {
        Self::InFlight
    }
}

/// One in-flight segment's metadata; the payload lives in the send ring buffer.
#[derive(Clone, Copy, Debug, slopos_ostd::Zeroable)]
pub struct SendMapEntry {
    /// First sequence number covered by this entry.
    pub seq: SeqNum,
    /// Byte length of the segment (data only; SYN/FIN tracked separately).
    pub len: u32,
    /// Timestamp (`now_ms`) when this segment was first transmitted.
    /// Never updated on retransmit — needed for RTT sampling.
    pub first_send_ms: u64,
    pub state: SegmentState,
}

/// Per-connection send map tracking every unacknowledged segment, in emit
/// order (== sequence order) from `head`.
#[derive(Debug, slopos_ostd::Zeroable)]
pub struct SendMap {
    entries: [SendMapEntry; SENDMAP_CAPACITY],
    head: u16,
    len: u16,
    total: u32,
    pipe: u32,
    lost: u16,
}

impl SendMap {
    pub fn boxed() -> Result<KBox<Self>, AllocError> {
        KBox::zeroed()
    }

    fn at(&self, i: usize) -> &SendMapEntry {
        &self.entries[(self.head as usize + i) % SENDMAP_CAPACITY]
    }

    fn at_mut(&mut self, i: usize) -> &mut SendMapEntry {
        &mut self.entries[(self.head as usize + i) % SENDMAP_CAPACITY]
    }

    fn count(&mut self, e: SendMapEntry) {
        if e.state.in_pipe() {
            self.pipe += e.len;
        }
        if e.state == SegmentState::Lost {
            self.lost += 1;
        }
    }

    fn uncount(&mut self, e: SendMapEntry) {
        if e.state.in_pipe() {
            self.pipe -= e.len;
        }
        if e.state == SegmentState::Lost {
            self.lost -= 1;
        }
    }

    fn set_state(&mut self, i: usize, state: SegmentState) {
        let before = *self.at(i);
        self.uncount(before);
        self.at_mut(i).state = state;
        self.count(SendMapEntry { state, ..before });
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn capacity_remaining(&self) -> usize {
        SENDMAP_CAPACITY - self.len as usize
    }

    /// Sum of ALL entry lengths regardless of state.
    /// Invariant target: `total_bytes() == snd_nxt - snd_una - fin_offset`.
    pub fn total_bytes(&self) -> u32 {
        self.total
    }

    /// RFC 6675 "pipe" estimate: bytes believed to be in the network, the
    /// `InFlight` and `Retransmitted` entries.
    pub fn pipe(&self) -> u32 {
        self.pipe
    }

    pub fn has_lost(&self) -> bool {
        self.lost > 0
    }

    pub fn next_lost(&self) -> Option<&SendMapEntry> {
        if self.lost == 0 {
            return None;
        }
        (0..self.len())
            .map(|i| self.at(i))
            .find(|e| e.state == SegmentState::Lost)
    }

    /// Record a segment put on the wire. Returns `Err(())` if the map is full.
    pub fn push_sent(&mut self, seq: SeqNum, len: u32, now_ms: u64) -> Result<(), ()> {
        if self.len as usize >= SENDMAP_CAPACITY {
            return Err(());
        }
        let i = self.len as usize;
        *self.at_mut(i) = SendMapEntry {
            seq,
            len,
            first_send_ms: now_ms,
            state: SegmentState::InFlight,
        };
        self.len += 1;
        self.total += len;
        self.pipe += len;
        Ok(())
    }

    /// Process a cumulative ACK covering everything up to (but not including)
    /// `up_to`, removing fully-acked entries from the head.
    ///
    /// RTT samples are only taken from `InFlight` entries — retransmitted,
    /// SACK-confirmed and lost entries are ineligible.
    pub fn on_cumulative_ack(&mut self, up_to: SeqNum) -> AckOutcome {
        let mut outcome = AckOutcome::default();
        while self.len > 0 {
            let e = *self.at(0);
            let end = e.seq + e.len;
            if up_to >= end {
                if outcome.entries_removed == 0 && e.state == SegmentState::InFlight {
                    outcome.rtt_sample_origin_ms = Some(e.first_send_ms);
                }
                self.uncount(e);
                self.total -= e.len;
                self.head = ((self.head as usize + 1) % SENDMAP_CAPACITY) as u16;
                self.len -= 1;
                outcome.bytes_freed = outcome.bytes_freed.saturating_add(e.len);
                outcome.entries_removed += 1;
            } else {
                if up_to > e.seq {
                    let delta = up_to - e.seq;
                    if e.state.in_pipe() {
                        self.pipe -= delta;
                    }
                    self.total -= delta;
                    let head = self.at_mut(0);
                    head.seq = head.seq + delta;
                    head.len -= delta;
                    outcome.bytes_freed = outcome.bytes_freed.saturating_add(delta);
                }
                break;
            }
        }
        outcome
    }

    /// Apply SACK blocks from the peer, then run loss detection.
    ///
    /// Only entries **fully** covered by a block are marked `SackConfirmed`.
    /// An `InFlight` entry with ≥ `DUP_THRESH` `SackConfirmed` entries after it
    /// is marked `Lost` (RFC 6675 §4). Returns `true` on a new loss.
    pub fn apply_sack_blocks(&mut self, blocks: &[(u32, u32)], count: u8) -> bool {
        let n = core::cmp::min(count as usize, blocks.len());
        if n == 0 || self.len == 0 {
            return false;
        }

        for &(left, right) in &blocks[..n] {
            if right <= left {
                continue;
            }
            for i in 0..self.len() {
                let e = *self.at(i);
                if e.state == SegmentState::SackConfirmed || e.state == SegmentState::Lost {
                    continue;
                }
                let e_end = (e.seq + e.len).raw();
                if seq_le(left, e.seq.raw()) && seq_ge(right, e_end) {
                    self.set_state(i, SegmentState::SackConfirmed);
                }
            }
        }

        let mut any_new_loss = false;
        let mut sacked_after = 0usize;
        for i in (0..self.len()).rev() {
            match self.at(i).state {
                SegmentState::SackConfirmed => sacked_after += 1,
                SegmentState::InFlight if sacked_after >= DUP_THRESH => {
                    self.set_state(i, SegmentState::Lost);
                    any_new_loss = true;
                }
                _ => {}
            }
        }

        any_new_loss
    }

    /// RTO path: mark every entry as `Lost` so the transmit loop re-sends them
    /// selectively instead of doing go-back-N.
    pub fn mark_all_lost(&mut self) {
        for i in 0..self.len() {
            self.at_mut(i).state = SegmentState::Lost;
        }
        self.pipe = 0;
        self.lost = self.len;
    }

    pub fn mark_retransmitted(&mut self, seq: SeqNum) {
        if let Some(i) = (0..self.len())
            .find(|&i| self.at(i).seq == seq && self.at(i).state == SegmentState::Lost)
        {
            self.set_state(i, SegmentState::Retransmitted);
        }
    }

    /// The running `(total, pipe, lost)`.
    #[cfg(feature = "test-hooks")]
    pub fn totals(&self) -> (u32, u32, u16) {
        (self.total_bytes(), self.pipe(), self.lost)
    }

    /// [`totals`](Self::totals) recomputed from the entries.
    #[cfg(feature = "test-hooks")]
    pub fn recount(&self) -> (u32, u32, u16) {
        let (mut total, mut pipe, mut lost) = (0u32, 0u32, 0u16);
        for e in (0..self.len()).map(|i| self.at(i)) {
            total += e.len;
            if e.state.in_pipe() {
                pipe += e.len;
            }
            if e.state == SegmentState::Lost {
                lost += 1;
            }
        }
        (total, pipe, lost)
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
        self.total = 0;
        self.pipe = 0;
        self.lost = 0;
    }
}

#[inline]
fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

#[inline]
fn seq_ge(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
}

/// Result of applying a cumulative ACK to the send map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AckOutcome {
    pub bytes_freed: u32,
    /// First transmission of the oldest-freed `InFlight` entry, or `None` if no
    /// eligible entry was freed.
    pub rtt_sample_origin_ms: Option<u64>,
    pub entries_removed: u16,
}
