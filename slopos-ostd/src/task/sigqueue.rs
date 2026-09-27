//! A task's pending-signal records: the `siginfo` each pending instance
//! carries to delivery.
//!
//! The pending set itself stays the task's `signal_pending` word, which the
//! lock-free "anything to deliver?" probes read; this store says what each
//! set bit carries. Standard signals coalesce — one record per signal, the
//! first sender's — and realtime ones queue every instance in arrival order,
//! up to [`SIGQUEUE_MAX`], as POSIX has them. A bit set with no record (the
//! kernel raised it, or the store was never needed) delivers as
//! [`SigInfo::KERNEL`].

use slopos_abi::signal::{SIGQUEUE_MAX, SIGRTMIN, SigInfo, SigSet, sig_bit, sig_is_realtime};

use crate::Zeroable;

#[derive(Clone, Copy, Zeroable)]
#[repr(C)]
struct RtEntry {
    signo: u8,
    _pad: [u8; 7],
    info: SigInfo,
}

const STANDARD: usize = (SIGRTMIN - 1) as usize;

/// Allocated zeroed on a task's first signal that carries a record; all-zero
/// is the empty store.
#[derive(Zeroable)]
#[repr(C)]
pub struct SigQueue {
    /// Bit `n - 1`: standard signal `n` has a record in `standard`.
    recorded: u32,
    rt_len: u32,
    standard: [SigInfo; STANDARD],
    rt: [RtEntry; SIGQUEUE_MAX],
}

impl SigQueue {
    /// Keep `info` for standard signal `signo`, replacing any record.
    pub(crate) fn record(&mut self, signo: u8, info: SigInfo) {
        let idx = signo as usize - 1;
        if let Some(slot) = self.standard.get_mut(idx) {
            *slot = info;
            self.recorded |= 1 << idx;
        }
    }

    /// Queue an instance of realtime `signo` behind every earlier one, or at
    /// the head when `front`. `false` when the queue is full.
    pub(crate) fn push(&mut self, signo: u8, info: SigInfo, front: bool) -> bool {
        let len = self.rt_len as usize;
        if len >= SIGQUEUE_MAX {
            return false;
        }
        let entry = RtEntry {
            signo,
            _pad: [0; 7],
            info,
        };
        if front {
            self.rt.copy_within(0..len, 1);
            self.rt[0] = entry;
        } else {
            self.rt[len] = entry;
        }
        self.rt_len += 1;
        true
    }

    /// The record the next delivery of `signo` carries, removed, and whether
    /// another instance of it stays queued.
    pub(crate) fn take(&mut self, signo: u8) -> (SigInfo, bool) {
        if !sig_is_realtime(signo) {
            let idx = signo as usize - 1;
            let bit = 1u32.checked_shl(idx as u32).unwrap_or(0);
            if self.recorded & bit == 0 {
                return (SigInfo::KERNEL, false);
            }
            self.recorded &= !bit;
            return (self.standard[idx], false);
        }
        let len = self.rt_len as usize;
        let Some(at) = self.rt[..len].iter().position(|e| e.signo == signo) else {
            return (SigInfo::KERNEL, false);
        };
        let info = self.rt[at].info;
        self.rt.copy_within(at + 1..len, at);
        self.rt_len -= 1;
        let more = self.rt[..len - 1].iter().any(|e| e.signo == signo);
        (info, more)
    }

    /// Drop every record for the signals in `mask`.
    pub(crate) fn forget(&mut self, mask: SigSet) {
        self.recorded &= !(mask as u32);
        let len = self.rt_len as usize;
        let mut kept = 0;
        for i in 0..len {
            if mask & sig_bit(self.rt[i].signo) == 0 {
                self.rt[kept] = self.rt[i];
                kept += 1;
            }
        }
        self.rt_len = kept as u32;
    }

    /// Realtime instances queued, of every signal.
    pub fn queued(&self) -> usize {
        self.rt_len as usize
    }
}
