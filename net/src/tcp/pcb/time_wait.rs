//! `TimeWait` state: connection fully closed, waiting out `2 × MSL`
//! (RFC 793 §3.5).
//!
//! A FIN is re-ACKed, a RST at exactly `rcv_nxt` releases the slot early and
//! one elsewhere in the window is challenged; everything else is dropped.

use super::super::actions::{Actions, SocketNotify};
use super::super::challenge_ack;
use super::super::header::TcpHeader;
use super::super::segment::SegmentBuilder;
use super::super::seq::SeqNum;
use super::{Pcb, PcbState};
use crate::timer::TimerToken;

/// `2 × MSL` in milliseconds.  MSL = 30 s per RFC 793 §3.3.
pub const TIME_WAIT_MS: u64 = 60_000;

#[derive(Debug)]
pub struct TimeWaitState {
    pub last_rcv_nxt: SeqNum,
    pub last_snd_nxt: SeqNum,
    /// In bytes; `last_wnd_field` is the same window as last sent in the header.
    pub last_rcv_wnd: u32,
    pub last_wnd_field: u16,
    pub entry_ms: u64,
    pub expire_token: Option<TimerToken>,
    pub challenge_budget: challenge_ack::ChallengeBudget,
}

impl TimeWaitState {
    pub const fn new(
        last_rcv_nxt: SeqNum,
        last_snd_nxt: SeqNum,
        last_rcv_wnd: u32,
        last_wnd_field: u16,
        entry_ms: u64,
    ) -> Self {
        Self {
            last_rcv_nxt,
            last_snd_nxt,
            last_rcv_wnd,
            last_wnd_field,
            entry_ms,
            expire_token: None,
            challenge_budget: challenge_ack::ChallengeBudget::new(),
        }
    }

    pub fn on_segment(pcb: &mut Pcb, hdr: &TcpHeader, payload_len: usize, now_ms: u64) -> Actions {
        let mut actions = Actions::new();

        let tuple = pcb.tuple;
        let PcbState::TimeWait(s) = &mut pcb.state else {
            unreachable!("TimeWaitState::on_segment called with non-TimeWait state");
        };

        // RFC 5961 §3.2: only a RST at exactly `rcv_nxt` releases the slot,
        // which may still hold bytes its reader has not taken.
        if hdr.is_rst() {
            match challenge_ack::classify_rst(hdr.seq_num, s.last_rcv_nxt.raw(), s.last_rcv_wnd) {
                challenge_ack::RstAction::Accept => {
                    actions.release = true;
                    actions.notify |= SocketNotify::RESET_RECEIVED;
                }
                challenge_ack::RstAction::ChallengeAck
                    if s.challenge_budget.try_consume(now_ms) =>
                {
                    actions.push_segment(SegmentBuilder::ack(
                        tuple,
                        s.last_snd_nxt.raw(),
                        s.last_rcv_nxt.raw(),
                        s.last_wnd_field,
                    ));
                }
                challenge_ack::RstAction::ChallengeAck | challenge_ack::RstAction::Drop => {}
            }
            return actions;
        }

        if hdr.is_fin() {
            actions.push_segment(SegmentBuilder::ack(
                tuple,
                s.last_snd_nxt.raw(),
                s.last_rcv_nxt.raw(),
                s.last_wnd_field,
            ));
            // Only the peer's own FIN, sent again, restarts 2MSL (RFC 9293
            // §3.10.7.4); the expiry timer measures from `entry_ms`.
            let fin_seq = hdr.seq_num.wrapping_add(payload_len as u32);
            if fin_seq.wrapping_add(1) == s.last_rcv_nxt.raw() {
                s.entry_ms = now_ms;
            }
        }

        actions
    }
}
