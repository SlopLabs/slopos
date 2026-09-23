//! `Data` state: active data transfer and the closing-phase chain.
//!
//! Covers every RFC 793 substate that shares a send window, receive
//! window, RTT estimator, and congestion controller — that is,
//! `ESTABLISHED`, `FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSE_WAIT`, `CLOSING`,
//! and `LAST_ACK`.  Which of those six labels currently applies is
//! captured by the [`ClosePhase`] sub-enum on [`DataState`].

use core::mem;

use slopos_ostd::{AllocError, Init, Initialised, SlotPtr, init_struct_with, write_field};

use super::super::actions::{Actions, SocketNotify, TimerOp};
use super::super::buffer::{TcpBufferPair, TcpSendState};
use super::super::challenge_ack;
use super::super::cong::{CcAlgo, CongestionControl};
use super::super::header::{DEFAULT_MSS, TcpHeader};
use super::super::rtt::RttEstimator;
use super::super::segment::{SegmentBuilder, TcpOutSegment};
use super::super::seq::{SeqNum, seq_ge, seq_gt, seq_le, seq_lt};
use super::super::tuple::TcpTuple;
use super::time_wait::{TIME_WAIT_MS, TimeWaitState};
use super::{Pcb, PcbState};
use crate::timer::{TimerKind, TimerToken};

/// Which RFC 793 closing substate the [`DataState`] is currently in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePhase {
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
}

/// How a sub-method signals a variant change or slot release back to the top
/// dispatcher, so it can keep operating on `&mut DataState` without owning the
/// enum swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextTransition {
    StayInData,
    ToTimeWait,
    ReleaseNow,
}

/// State-specific payload for the Data variant.
#[derive(Debug, slopos_ostd::SlotFields)]
pub struct DataState {
    pub iss: SeqNum,
    pub irs: SeqNum,
    pub snd_una: SeqNum,
    pub snd_nxt: SeqNum,
    pub snd_wnd: u32,
    /// SND.WL1 and SND.WL2 (RFC 9293 §3.10.7.4): the segment that last set
    /// `snd_wnd`, so a late older one cannot put an old window back.
    pub snd_wl1: SeqNum,
    pub snd_wl2: SeqNum,
    pub rcv_nxt: SeqNum,
    /// Bytes from `rcv_nxt` to the right edge last advertised; arrivals use it
    /// up, and an advertisement never moves the edge left (RFC 7323 §2.4).
    pub rcv_wnd: u32,

    pub peer_mss: u16,
    pub rcv_wscale: u8,
    pub snd_wscale: u8,
    pub wscale_enabled: bool,
    pub sack_permitted: bool,
    pub nagle_enabled: bool,

    pub close_phase: ClosePhase,
    /// Our FIN waits for the bytes ahead of it to be sent; `close_phase`
    /// moves when it goes.
    pub fin_queued: bool,

    pub rtt: RttEstimator,
    pub cc: CcAlgo,

    /// The retransmission timer, which also paces zero-window probes.
    pub retransmit_token: Option<TimerToken>,
    /// Zero-window probes since the peer last answered one.
    pub persist_probes: u8,
    /// Zero-window probes since the window last opened: their interval
    /// doubles with each, answered or not.
    pub persist_backoff: u8,
    /// Zero-window probes sent while no socket owns the connection. Never reset:
    /// a peer answering each with a shut window would pin the send ring forever.
    pub orphan_probes: u8,

    pub keepalive_token: Option<TimerToken>,
    pub keepalive_probes_sent: u8,
    pub last_activity_tick: u64,

    pub fin_wait2_token: Option<TimerToken>,

    pub ts_enabled: bool,
    pub ts_recent: u32,
    pub last_ack_sent: u32,

    pub reset_received: bool,
    pub peer_closed: bool,

    pub challenge_budget: challenge_ack::ChallengeBudget,
}

impl DataState {
    /// Heap-direct initialiser for a freshly-established `DataState`.
    ///
    /// Returns an [`Init`] recipe rather than a `Self` rvalue so the struct
    /// never materialises on the caller's stack, and is hand-written
    /// rather than macro-expanded so the closure's own frame stays inside the
    /// stack-safety gate.
    #[allow(clippy::too_many_arguments)]
    pub fn init_new(
        iss: SeqNum,
        irs: SeqNum,
        snd_una: SeqNum,
        snd_nxt: SeqNum,
        rcv_nxt: SeqNum,
        snd_wnd: u32,
        rcv_wnd: u32,
        peer_mss: u16,
        snd_wscale: u8,
        rcv_wscale: u8,
        wscale_enabled: bool,
        ts_enabled: bool,
    ) -> impl Init<Self, AllocError> {
        let cc_mss = peer_mss.max(DEFAULT_MSS) as u32;
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, iss, iss);
                write_field!(slot, irs, irs);
                write_field!(slot, snd_una, snd_una);
                write_field!(slot, snd_nxt, snd_nxt);
                write_field!(slot, snd_wnd, snd_wnd);
                write_field!(slot, snd_wl1, irs);
                write_field!(slot, snd_wl2, snd_una);
                write_field!(slot, rcv_nxt, rcv_nxt);
                write_field!(slot, rcv_wnd, rcv_wnd);
                write_field!(slot, peer_mss, peer_mss);
                write_field!(slot, rcv_wscale, rcv_wscale);
                write_field!(slot, snd_wscale, snd_wscale);
                write_field!(slot, wscale_enabled, wscale_enabled);
                write_field!(slot, sack_permitted, false);
                write_field!(slot, nagle_enabled, true);
                write_field!(slot, close_phase, ClosePhase::Established);
                write_field!(slot, fin_queued, false);
                write_field!(slot, rtt, RttEstimator::new());
                write_field!(slot, cc, CcAlgo::cubic(cc_mss));
                write_field!(slot, retransmit_token, None);
                write_field!(slot, persist_probes, 0u8);
                write_field!(slot, persist_backoff, 0u8);
                write_field!(slot, orphan_probes, 0u8);
                write_field!(slot, keepalive_token, None);
                write_field!(slot, keepalive_probes_sent, 0u8);
                write_field!(slot, last_activity_tick, 0u64);
                write_field!(slot, fin_wait2_token, None);
                write_field!(slot, ts_enabled, ts_enabled);
                write_field!(slot, ts_recent, 0u32);
                write_field!(slot, last_ack_sent, 0u32);
                write_field!(slot, reset_received, false);
                write_field!(slot, peer_closed, false);
                write_field!(
                    slot,
                    challenge_budget,
                    challenge_ack::ChallengeBudget::new()
                );
                Ok(slot.finish())
            },
        )
    }

    /// Heap-direct initialiser for the `SYN_RECV → ESTABLISHED` transition;
    /// builds in place so the struct never lands on a caller's stack.
    pub fn init_from_syn_recv(
        s: &super::syn_recv::SynRecvState,
    ) -> impl Init<Self, AllocError> + '_ {
        let cc_mss = s.peer_mss.max(DEFAULT_MSS) as u32;
        let ts_recent = if s.ts_enabled { s.peer_tsval } else { 0 };
        let iss = s.iss;
        let irs = s.irs;
        let snd_una = s.snd_una;
        let snd_nxt = s.snd_nxt;
        let rcv_nxt = s.rcv_nxt;
        let snd_wnd = s.snd_wnd;
        let rcv_wnd = u32::from(s.rcv_wnd);
        let peer_mss = s.peer_mss;
        let rcv_wscale = s.our_wscale;
        let snd_wscale = s.snd_wscale;
        let wscale_enabled = s.wscale_enabled;
        let sack_permitted = s.sack_permitted;
        let ts_enabled = s.ts_enabled;
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, iss, iss);
                write_field!(slot, irs, irs);
                write_field!(slot, snd_una, snd_una);
                write_field!(slot, snd_nxt, snd_nxt);
                write_field!(slot, snd_wnd, snd_wnd);
                write_field!(slot, snd_wl1, irs);
                write_field!(slot, snd_wl2, snd_una);
                write_field!(slot, rcv_nxt, rcv_nxt);
                write_field!(slot, rcv_wnd, rcv_wnd);
                write_field!(slot, peer_mss, peer_mss);
                write_field!(slot, rcv_wscale, rcv_wscale);
                write_field!(slot, snd_wscale, snd_wscale);
                write_field!(slot, wscale_enabled, wscale_enabled);
                write_field!(slot, sack_permitted, sack_permitted);
                write_field!(slot, nagle_enabled, true);
                write_field!(slot, close_phase, ClosePhase::Established);
                write_field!(slot, fin_queued, false);
                write_field!(slot, rtt, RttEstimator::new());
                write_field!(slot, cc, CcAlgo::cubic(cc_mss));
                write_field!(slot, retransmit_token, None);
                write_field!(slot, persist_probes, 0u8);
                write_field!(slot, persist_backoff, 0u8);
                write_field!(slot, orphan_probes, 0u8);
                write_field!(slot, keepalive_token, None);
                write_field!(slot, keepalive_probes_sent, 0u8);
                write_field!(slot, last_activity_tick, 0u64);
                write_field!(slot, fin_wait2_token, None);
                write_field!(slot, ts_enabled, ts_enabled);
                write_field!(slot, ts_recent, ts_recent);
                write_field!(slot, last_ack_sent, 0u32);
                write_field!(slot, reset_received, false);
                write_field!(slot, peer_closed, false);
                write_field!(
                    slot,
                    challenge_budget,
                    challenge_ack::ChallengeBudget::new()
                );
                Ok(slot.finish())
            },
        )
    }

    /// Test-only by-value constructor. Materialises the `Self` rvalue on the
    /// caller's stack, so only a consumer that immediately heap-moves it may
    /// call it; production code uses [`init_new`] / [`init_from_syn_recv`].
    #[cfg(any(test, feature = "test-hooks"))]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        iss: SeqNum,
        irs: SeqNum,
        snd_una: SeqNum,
        snd_nxt: SeqNum,
        rcv_nxt: SeqNum,
        snd_wnd: u32,
        rcv_wnd: u32,
        peer_mss: u16,
        snd_wscale: u8,
        rcv_wscale: u8,
        wscale_enabled: bool,
        ts_enabled: bool,
    ) -> Self {
        Self {
            iss,
            irs,
            snd_una,
            snd_nxt,
            snd_wnd,
            snd_wl1: irs,
            snd_wl2: snd_una,
            rcv_nxt,
            rcv_wnd,
            peer_mss,
            rcv_wscale,
            snd_wscale,
            wscale_enabled,
            sack_permitted: false,
            nagle_enabled: true,
            close_phase: ClosePhase::Established,
            fin_queued: false,
            rtt: RttEstimator::new(),
            cc: CcAlgo::cubic(peer_mss.max(DEFAULT_MSS) as u32),
            retransmit_token: None,
            persist_probes: 0,
            persist_backoff: 0,
            orphan_probes: 0,
            keepalive_token: None,
            keepalive_probes_sent: 0,
            last_activity_tick: 0,
            fin_wait2_token: None,
            ts_enabled,
            ts_recent: 0,
            last_ack_sent: 0,
            reset_received: false,
            peer_closed: false,
            challenge_budget: challenge_ack::ChallengeBudget::new(),
        }
    }

    fn window_unit(&self) -> u32 {
        if self.wscale_enabled {
            1 << self.rcv_wscale
        } else {
            1
        }
    }

    /// The header field for `rcv_wnd`, rounded up.
    pub fn advertised(&self) -> u16 {
        self.rcv_wnd
            .div_ceil(self.window_unit())
            .min(u32::from(u16::MAX)) as u16
    }

    /// Bytes past `rcv_nxt` the peer may believe it can send, counting the unit
    /// an earlier field rounded up (RFC 7323 §2.4).
    pub fn granted(&self) -> u32 {
        let unit = self.window_unit();
        u32::from(self.advertised()) * unit + (unit - 1)
    }

    /// Offer room for `bytes`, rounded down so ACKs cannot creep the edge past
    /// the buffer, and never pulling it back; returns the header field.
    pub fn advertise(&mut self, bytes: u32) -> u16 {
        let unit = self.window_unit();
        let offered = (bytes / unit).min(u32::from(u16::MAX)) * unit;
        if offered > self.rcv_wnd {
            self.rcv_wnd = offered;
        }
        self.advertised()
    }

    /// Timestamp option for outgoing segments, `None` if not negotiated.
    #[inline]
    pub fn ts_option(&self, now_ms: u64) -> Option<(u32, u32)> {
        if self.ts_enabled {
            Some((now_ms as u32, self.ts_recent))
        } else {
            None
        }
    }

    /// Our timestamp on `seg`, whose ACK becomes the last one sent (RFC 7323
    /// §4.3).
    pub fn stamp(&mut self, seg: &mut TcpOutSegment, now_ms: u64) {
        seg.timestamp = self.ts_option(now_ms);
        self.last_ack_sent = seg.ack_num;
    }

    /// Apply an incoming segment to a Data PCB.
    pub fn on_segment(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        options: &[u8],
        payload: &[u8],
        now_ms: u64,
    ) -> Actions {
        if hdr.is_rst() {
            return handle_data_rst(pcb, hdr, now_ms);
        }

        let mut actions = Actions::new();
        let tuple = pcb.tuple;

        // RFC 5961 §4: a SYN in a synchronized state is answered with a
        // challenge ACK irrespective of its sequence number, never a RST.
        if hdr.is_syn() {
            return Self::on_unexpected_syn(pcb, now_ms, actions);
        }

        // RFC 793 §3.9: everything below this point may only act on a segment
        // that falls in the receive window.
        if !data_segment_acceptable(pcb, hdr, payload.len()) {
            return unacceptable_segment_ack(pcb, now_ms, actions);
        }

        if data_paws_should_drop(pcb, options) {
            return paws_drop_ack(pcb, now_ms);
        }

        if !hdr.is_ack() {
            return actions;
        }

        // RFC 7323 §4.3: update ts_recent only when SEG.SEQ <= Last.ACK.sent.
        {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            if data.ts_enabled {
                let parsed = super::super::header::parse_tcp_options(options);
                if let Some((tsval, _)) = parsed.timestamp {
                    if seq_le(hdr.seq_num, data.last_ack_sent) || data.last_ack_sent == 0 {
                        data.ts_recent = tsval;
                    }
                }
            }
        }

        let acked = {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            data.process_ack(tuple, hdr, options, now_ms, &mut bufs.send, &mut actions)
        };
        if acked > 0 {
            bufs.send.process_ack(acked as usize);
        }

        let was_fin_wait_1 =
            matches!(&pcb.state, PcbState::Data(d) if d.close_phase == ClosePhase::FinWait1);
        let transition =
            Self::process_payload_fin_and_ack(pcb, bufs, hdr, payload, now_ms, &mut actions);
        if was_fin_wait_1
            && transition == NextTransition::StayInData
            && matches!(&pcb.state, PcbState::Data(d) if d.close_phase == ClosePhase::FinWait2)
        {
            actions.push_timer(TimerOp::Schedule {
                kind: TimerKind::TcpFinWait2,
                key: 0,
                delay_ms: super::super::FIN_WAIT2_TIMEOUT_MS,
            });
        }

        match transition {
            NextTransition::StayInData => {}
            NextTransition::ReleaseNow if pcb.socket_id.is_none() || bufs.recv.available() == 0 => {
                actions.release = true;
            }
            // A LAST_ACK socket still owed bytes waits in TIME_WAIT, which
            // keeps the rings until they are read.
            NextTransition::ToTimeWait | NextTransition::ReleaseNow => {
                let PcbState::Data(data) = &pcb.state else {
                    unreachable!()
                };
                let tw = TimeWaitState::new(
                    data.rcv_nxt,
                    data.snd_nxt,
                    data.granted(),
                    data.advertised(),
                    now_ms,
                );
                // Deferred through `Actions` so the glue layer installs the
                // 2×MSL timer after the lock drops; `key: 0` is a sentinel it
                // replaces with the real slot index.
                actions.push_timer(TimerOp::Schedule {
                    kind: TimerKind::TcpTimeWait,
                    key: 0,
                    delay_ms: TIME_WAIT_MS,
                });
                let _old = mem::replace(&mut pcb.state, PcbState::TimeWait(tw));
            }
        }

        actions
    }

    fn on_rst(pcb: &mut Pcb, mut actions: Actions) -> Actions {
        let PcbState::Data(data) = &mut pcb.state else {
            unreachable!()
        };
        data.reset_received = true;
        if let Some(token) = data.retransmit_token.take() {
            actions.push_timer(TimerOp::Cancel { token });
        }
        if let Some(token) = data.keepalive_token.take() {
            actions.push_timer(TimerOp::Cancel { token });
        }
        actions.release = true;
        actions.notify |= SocketNotify::RESET_RECEIVED | SocketNotify::RECV_WAKE;
        actions
    }

    fn on_unexpected_syn(pcb: &mut Pcb, now_ms: u64, mut actions: Actions) -> Actions {
        let tuple = pcb.tuple;
        let PcbState::Data(data) = &mut pcb.state else {
            unreachable!()
        };
        let snd_nxt = data.snd_nxt.raw();
        let rcv_nxt = data.rcv_nxt.raw();
        let window = data.advertised();
        let ts_opt = data.ts_option(now_ms);
        if data.challenge_budget.try_consume(now_ms) {
            let mut ack = SegmentBuilder::ack(tuple, snd_nxt, rcv_nxt, window);
            ack.timestamp = ts_opt;
            actions.push_segment(ack);
        }
        actions
    }

    /// Returns the number of bytes newly acknowledged, 0 if the ACK did not
    /// advance `snd_una`.
    fn process_ack(
        &mut self,
        _tuple: super::super::tuple::TcpTuple,
        hdr: &TcpHeader,
        options: &[u8],
        now_ms: u64,
        send: &mut TcpSendState,
        actions: &mut Actions,
    ) -> u32 {
        let old_snd_una = self.snd_una;
        let ack = hdr.ack_num;

        let parsed = if (!options.is_empty() && self.sack_permitted) || self.ts_enabled {
            Some(super::super::header::parse_tcp_options(options))
        } else {
            None
        };

        let (sack_blocks, sack_count) = if self.sack_permitted {
            if let Some(ref p) = parsed {
                (p.sack_blocks, p.sack_block_count)
            } else {
                ([(0, 0); 4], 0)
            }
        } else {
            ([(0, 0); 4], 0)
        };

        // RFC 9293 §3.10.7.4: a window rides any acceptable ACK, duplicates
        // included, unless an older segment than the one that set it.
        let acceptable = seq_ge(ack, old_snd_una.raw()) && seq_le(ack, self.snd_nxt.raw());
        if acceptable {
            actions.notify |= SocketNotify::PEER_HEARD;
            self.persist_probes = 0;
            let newer = seq_lt(self.snd_wl1.raw(), hdr.seq_num)
                || (self.snd_wl1.raw() == hdr.seq_num && seq_le(self.snd_wl2.raw(), ack));
            if newer {
                let wnd = if self.wscale_enabled {
                    (hdr.window_size as u32) << self.snd_wscale
                } else {
                    hdr.window_size as u32
                };
                // The timer running with nothing in flight is the persist
                // timer; new data arms a fresh RTO once it is gone.
                if self.snd_wnd == 0 && wnd > 0 && self.snd_una == self.snd_nxt {
                    if let Some(token) = self.retransmit_token.take() {
                        actions.push_timer(TimerOp::Cancel { token });
                    }
                    send.rto_deadline_ms = 0;
                }
                if wnd > 0 {
                    self.persist_backoff = 0;
                }
                self.snd_wnd = wnd;
                self.snd_wl1 = SeqNum::new(hdr.seq_num);
                self.snd_wl2 = SeqNum::new(ack);
            }
        }

        let acked = if acceptable && ack != old_snd_una.raw() {
            self.snd_una = SeqNum::new(ack);
            let acked = ack.wrapping_sub(old_snd_una.raw());
            let outcome = send.sendmap.on_cumulative_ack(self.snd_una);

            // RTT measurement: prefer RTTM (timestamps) over Karn.
            let rtt_sample = if self.ts_enabled {
                parsed
                    .as_ref()
                    .and_then(|p| p.timestamp)
                    .and_then(|(_, tsecr)| {
                        if tsecr != 0 {
                            Some((now_ms as u32).wrapping_sub(tsecr))
                        } else {
                            None
                        }
                    })
            } else {
                outcome
                    .rtt_sample_origin_ms
                    .map(|origin| now_ms.saturating_sub(origin) as u32)
            };
            if let Some(rtt_ms) = rtt_sample {
                self.rtt.sample(rtt_ms);
            }
            // RFC 1122 §4.2.3.5 counts retransmissions of one segment, so
            // progress restarts the count even where Karn's rule takes no sample.
            self.rtt.consecutive_timeouts = 0;
            self.cc.on_ack(
                outcome.bytes_freed,
                rtt_sample,
                self.snd_una.raw(),
                self.snd_nxt.raw(),
                now_ms,
            );
            if let Some(token) = self.retransmit_token.take() {
                actions.push_timer(TimerOp::Cancel { token });
            }
            send.rto_deadline_ms = 0;
            // Armed while anything is unacknowledged, our FIN included.
            if self.snd_una != self.snd_nxt {
                let delay_ms = (self.rtt.rto_ms() as u64).max(1);
                send.rto_deadline_ms = now_ms.saturating_add(delay_ms);
                actions.push_timer(TimerOp::Schedule {
                    kind: TimerKind::TcpRetransmit,
                    key: 0,
                    delay_ms,
                });
            }
            actions.notify |= SocketNotify::SEND_WAKE;
            acked
        } else {
            0
        };

        // SACK blocks feed RFC 6675 loss detection on duplicate ACKs too.
        if sack_count > 0 {
            let new_losses = send
                .sendmap
                .apply_sack_blocks(&sack_blocks[..sack_count as usize], sack_count);
            if new_losses && !self.cc.in_recovery() {
                self.cc
                    .on_fast_retransmit(send.sendmap.pipe(), self.snd_nxt.raw());
            }
        }

        acked
    }

    #[inline(never)]
    fn push_ack(&mut self, tuple: TcpTuple, free: u32, now_ms: u64, actions: &mut Actions) {
        let window = self.advertise(free);
        let mut ack = SegmentBuilder::ack(tuple, self.snd_nxt.raw(), self.rcv_nxt.raw(), window);
        self.stamp(&mut ack, now_ms);
        actions.push_segment(ack);
    }

    /// Buffers the segment and ACKs at once, so the peer retransmits the gap.
    #[inline(never)]
    fn queue_out_of_order(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        payload: &[u8],
        now_ms: u64,
        actions: &mut Actions,
    ) {
        let tuple = pcb.tuple;
        let PcbState::Data(d) = &mut pcb.state else {
            unreachable!()
        };
        let expected_seq = d.rcv_nxt;
        if seq_gt(hdr.seq_num, expected_seq.raw()) {
            let offset = hdr.seq_num.wrapping_sub(expected_seq.raw()) as usize;
            let wrote = bufs.recv.buf.write_at(offset, payload, &mut bufs.spares);
            if wrote > 0 {
                bufs.ooo.insert(hdr.seq_num, wrote);
            }
        }
        let window = d.advertise(bufs.recv.window());
        let mut ack = SegmentBuilder::ack(tuple, d.snd_nxt.raw(), expected_seq.raw(), window);
        if d.sack_permitted {
            let (ooo_blocks, ooo_count) = bufs.ooo.sack_blocks();
            let seg_end = hdr.seq_num.wrapping_add(payload.len() as u32);

            // DSACK (RFC 2883): a duplicate range goes in the first SACK block
            // so the peer can detect spurious retransmits.
            if seq_lt(hdr.seq_num, expected_seq.raw()) {
                let dsack_right = if seq_gt(seg_end, expected_seq.raw()) {
                    expected_seq.raw()
                } else {
                    seg_end
                };
                ack.sack_blocks[0] = (hdr.seq_num, dsack_right);
                let more = (ooo_count as usize).min(ack.sack_blocks.len() - 1);
                ack.sack_blocks[1..=more].copy_from_slice(&ooo_blocks[..more]);
                ack.sack_block_count = 1 + more as u8;
            } else {
                ack.sack_blocks = ooo_blocks;
                ack.sack_block_count = ooo_count;
            }
        }
        d.stamp(&mut ack, now_ms);
        actions.push_segment(ack);
    }

    /// Payload, FIN and post-ACK transitions share mutable access to both
    /// `bufs` and `pcb.state`, so splitting them further would mean juggling
    /// borrows.
    fn process_payload_fin_and_ack(
        pcb: &mut Pcb,
        bufs: &mut TcpBufferPair,
        hdr: &TcpHeader,
        payload: &[u8],
        now_ms: u64,
        actions: &mut Actions,
    ) -> NextTransition {
        let tuple = pcb.tuple;

        let PcbState::Data(d) = &pcb.state else {
            unreachable!()
        };
        let data_is_open = matches!(
            d.close_phase,
            ClosePhase::Established
                | ClosePhase::CloseWait
                | ClosePhase::FinWait1
                | ClosePhase::FinWait2
        );

        let PcbState::Data(data) = &mut pcb.state else {
            unreachable!()
        };
        let fin_acked = hdr.ack_num == data.snd_nxt.raw();
        match data.close_phase {
            ClosePhase::FinWait1 if fin_acked => data.close_phase = ClosePhase::FinWait2,
            ClosePhase::Closing if fin_acked => return NextTransition::ToTimeWait,
            ClosePhase::LastAck if fin_acked => return NextTransition::ReleaseNow,
            _ => {}
        }

        if !payload.is_empty() && data_is_open {
            let PcbState::Data(d) = &pcb.state else {
                unreachable!()
            };
            let expected_seq = d.rcv_nxt;
            // A retransmission overlapping bytes already taken carries new
            // ones past them (RFC 9293 §3.10.7.4 trims it to the window).
            let taken = expected_seq.raw().wrapping_sub(hdr.seq_num) as usize;
            let (seq, payload) = if seq_lt(hdr.seq_num, expected_seq.raw()) && taken < payload.len()
            {
                (expected_seq.raw(), &payload[taken..])
            } else {
                (hdr.seq_num, payload)
            };
            if seq != expected_seq.raw() {
                Self::queue_out_of_order(pcb, bufs, hdr, payload, now_ms, actions);
                return NextTransition::StayInData;
            }
            let wrote = bufs.recv.enqueue(payload, &mut bufs.spares, now_ms);
            // Bytes with nowhere to go are answered at once, so the peer
            // learns the window rather than waiting out its RTO.
            let dropped = wrote < payload.len();
            let mut accepted_len = wrote;
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            data.rcv_nxt = data.rcv_nxt.wrapping_add(wrote as u32);
            if !bufs.ooo.is_empty() {
                let PcbState::Data(d) = &pcb.state else {
                    unreachable!()
                };
                let rcv_nxt = d.rcv_nxt;
                let drained = bufs.ooo.drain_contiguous(rcv_nxt.raw());
                let drained = bufs.recv.buf.advance(drained);
                if drained > 0 {
                    bufs.recv.ack_pending = true;
                    bufs.recv.segments_since_ack = bufs.recv.segments_since_ack.saturating_add(1);
                    if bufs.recv.segments_since_ack == 1 {
                        bufs.recv.delayed_ack_deadline_ms =
                            now_ms.saturating_add(super::super::buffer::DELAYED_ACK_MS);
                    }
                    accepted_len += drained;
                    let PcbState::Data(data) = &mut pcb.state else {
                        unreachable!()
                    };
                    data.rcv_nxt = data.rcv_nxt.wrapping_add(drained as u32);
                }
            }
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            data.rcv_wnd = data.rcv_wnd.saturating_sub(accepted_len as u32);
            if accepted_len > 0 {
                actions.notify |= SocketNotify::RECV_WAKE;
            }
            if dropped || bufs.recv.should_ack_now(now_ms) {
                let PcbState::Data(data) = &mut pcb.state else {
                    unreachable!()
                };
                data.push_ack(tuple, bufs.recv.window(), now_ms, actions);
                bufs.recv.ack_sent();
                if !hdr.is_fin() {
                    return NextTransition::StayInData;
                }
            } else if !hdr.is_fin() {
                return NextTransition::StayInData;
            }
        }

        if hdr.is_fin() {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            // The FIN counts only where the stream ends: not after a segment
            // cut short, nor before out-of-order bytes that joined past it.
            let fin_seq = hdr.seq_num.wrapping_add(payload.len() as u32);
            if fin_seq != data.rcv_nxt.raw() {
                data.push_ack(tuple, bufs.recv.window(), now_ms, actions);
                return NextTransition::StayInData;
            }
            data.rcv_nxt = data.rcv_nxt.wrapping_add(1);
            data.peer_closed = true;
            let new_phase = match data.close_phase {
                ClosePhase::Established => ClosePhase::CloseWait,
                ClosePhase::FinWait1 => ClosePhase::Closing,
                ClosePhase::FinWait2 => {
                    if let Some(token) = data.fin_wait2_token.take() {
                        actions.push_timer(TimerOp::Cancel { token });
                    }
                    data.close_phase = ClosePhase::Closing;
                    data.push_ack(tuple, bufs.recv.window(), now_ms, actions);
                    return NextTransition::ToTimeWait;
                }
                other => other,
            };
            data.close_phase = new_phase;
            actions.notify |= SocketNotify::PEER_CLOSED | SocketNotify::RECV_WAKE;
            data.push_ack(tuple, bufs.recv.window(), now_ms, actions);
        }

        NextTransition::StayInData
    }

    /// The idle delay in milliseconds the caller should schedule, or `None` if
    /// keepalive is disabled or a timer is already active.
    pub fn schedule_initial_keepalive(&mut self, keepalive_enabled: bool) -> Option<u64> {
        if keepalive_enabled && self.keepalive_token.is_none() {
            Some(super::super::TCP_KEEPALIVE_IDLE_MS)
        } else {
            None
        }
    }

    /// The old token to cancel and the idle delay in milliseconds to
    /// reschedule, or `None` if keepalive was not active.
    pub fn reset_keepalive_on_activity(&mut self) -> Option<(TimerToken, u64)> {
        if let Some(token) = self.keepalive_token.take() {
            self.keepalive_probes_sent = 0;
            Some((token, super::super::TCP_KEEPALIVE_IDLE_MS))
        } else {
            None
        }
    }

    /// The ACK segment if a delayed ACK is now due, marking it sent on the
    /// receive buffer.
    pub fn check_delayed_ack(
        &mut self,
        tuple: super::super::tuple::TcpTuple,
        bufs: &mut TcpBufferPair,
        now_ms: u64,
    ) -> Option<super::super::segment::TcpOutSegment> {
        if bufs.recv.should_ack_now(now_ms) {
            let window = self.advertise(bufs.recv.window());
            let mut seg =
                SegmentBuilder::ack(tuple, self.snd_nxt.raw(), self.rcv_nxt.raw(), window);
            self.stamp(&mut seg, now_ms);
            bufs.recv.ack_sent();
            Some(seg)
        } else {
            None
        }
    }

    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_invariants(&self, _pcb: &Pcb) {
        debug_assert!(
            self.snd_una <= self.snd_nxt,
            "Data: snd_una ({}) > snd_nxt ({})",
            self.snd_una.raw(),
            self.snd_nxt.raw()
        );
        match self.close_phase {
            ClosePhase::Established => {}
            ClosePhase::FinWait1 | ClosePhase::FinWait2 => {
                debug_assert!(self.snd_nxt >= self.snd_una, "FinWait: snd_nxt >= snd_una");
            }
            ClosePhase::CloseWait => {
                debug_assert!(self.peer_closed, "CloseWait implies peer_closed");
            }
            ClosePhase::Closing | ClosePhase::LastAck => {
                debug_assert!(self.peer_closed, "Closing/LastAck implies peer_closed");
            }
        }
    }

    /// The send map covers exactly the data bytes in flight.
    #[cfg(debug_assertions)]
    pub fn debug_assert_sendmap(&self, sendmap: &super::super::retx::SendMap) {
        // FIN consumes one sequence byte but the send map covers data
        // segments only.
        let fin_offset = match self.close_phase {
            ClosePhase::FinWait1 | ClosePhase::LastAck | ClosePhase::Closing => 1u32,
            _ => 0,
        };
        // Our SYN occupies `iss` and is likewise not in the send map. It is
        // still outstanding whenever `snd_una` has not moved past it, which is
        // what a close out of `SYN_RECEIVED` leaves behind.
        let syn_offset = if self.snd_una.raw() == self.iss.raw() {
            1u32
        } else {
            0
        };
        let expected = self
            .snd_una
            .distance_to(self.snd_nxt)
            .saturating_sub(fin_offset)
            .saturating_sub(syn_offset);
        debug_assert_eq!(
            sendmap.total_bytes(),
            expected,
            "sendmap total_bytes ({}) != snd_nxt - snd_una - fin_offset ({})",
            sendmap.total_bytes(),
            expected,
        );
    }
}

/// Handle an incoming RST on a Data PCB (RFC 5961 classification).
///
/// `#[inline(never)]` here and on the PAWS helpers below keeps their
/// `SegmentBuilder` scratch and 400 B `Actions` slots out of the `on_segment`
/// dispatcher's frame, which the stack-safety gate bounds.
#[inline(never)]
fn handle_data_rst(pcb: &mut Pcb, hdr: &TcpHeader, now_ms: u64) -> Actions {
    let tuple = pcb.tuple;
    let mut actions = Actions::new();
    let PcbState::Data(data) = &pcb.state else {
        unreachable!()
    };
    let effective_wnd = data.granted();
    let rcv_nxt = data.rcv_nxt.raw();
    let snd_nxt = data.snd_nxt.raw();
    let rcv_wnd = data.advertised();
    let ts_opt = data.ts_option(now_ms);
    match challenge_ack::classify_rst(hdr.seq_num, rcv_nxt, effective_wnd) {
        challenge_ack::RstAction::Accept => DataState::on_rst(pcb, actions),
        challenge_ack::RstAction::ChallengeAck => {
            let PcbState::Data(data) = &mut pcb.state else {
                unreachable!()
            };
            if data.challenge_budget.try_consume(now_ms) {
                let mut ack = SegmentBuilder::ack(tuple, snd_nxt, rcv_nxt, rcv_wnd);
                ack.timestamp = ts_opt;
                actions.push_segment(ack);
            }
            actions
        }
        challenge_ack::RstAction::Drop => actions,
    }
}

/// RFC 793 §3.9 acceptability for a Data-state segment, with the receive
/// window scaled as negotiated.
#[inline(never)]
fn data_segment_acceptable(pcb: &Pcb, hdr: &TcpHeader, payload_len: usize) -> bool {
    let PcbState::Data(data) = &pcb.state else {
        return false;
    };
    let seg_len = payload_len as u32 + u32::from(hdr.is_fin());
    challenge_ack::segment_acceptable(hdr.seq_num, seg_len, data.rcv_nxt.raw(), data.granted())
}

/// RFC 793 §3.9: an unacceptable segment is dropped, and unless it carried a
/// RST an ACK is sent back so the peer resynchronises.
#[inline(never)]
fn unacceptable_segment_ack(pcb: &Pcb, now_ms: u64, mut actions: Actions) -> Actions {
    let PcbState::Data(data) = &pcb.state else {
        return actions;
    };
    let mut ack = SegmentBuilder::ack(
        pcb.tuple,
        data.snd_nxt.raw(),
        data.rcv_nxt.raw(),
        data.advertised(),
    );
    ack.timestamp = data.ts_option(now_ms);
    actions.push_segment(ack);
    actions
}

/// Does the incoming segment's timestamp option trip PAWS?
#[inline(never)]
fn data_paws_should_drop(pcb: &Pcb, options: &[u8]) -> bool {
    let PcbState::Data(data) = &pcb.state else {
        return false;
    };
    if !data.ts_enabled || data.ts_recent == 0 {
        return false;
    }
    let parsed = super::super::header::parse_tcp_options(options);
    let Some((tsval, _)) = parsed.timestamp else {
        return false;
    };
    super::super::header::ts_less_than(tsval, data.ts_recent)
}

/// Build the drop-ACK [`Actions`] for a PAWS-rejected segment.
#[inline(never)]
fn paws_drop_ack(pcb: &Pcb, now_ms: u64) -> Actions {
    let mut actions = Actions::new();
    let PcbState::Data(data) = &pcb.state else {
        return actions;
    };
    let mut ack = SegmentBuilder::ack(
        pcb.tuple,
        data.snd_nxt.raw(),
        data.rcv_nxt.raw(),
        data.advertised(),
    );
    ack.timestamp = data.ts_option(now_ms);
    actions.push_segment(ack);
    actions
}
