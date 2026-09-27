//! Pending signals and the `siginfo` each pending instance carries to
//! delivery.
//!
//! A [`PendingSignals`] is one pending set: a thread's own, for signals sent to
//! that thread, or its thread group's, shared by every member, for signals sent
//! to the process. Its word is what the lock-free "anything to deliver?"
//! probes read; its [`SigQueue`] says what each set bit carries. Standard
//! signals coalesce — one record per signal, the first sender's — and realtime
//! ones queue every instance in arrival order, up to [`SIGQUEUE_MAX`], as
//! POSIX has them. A bit set with no record (the kernel raised it, the store
//! was never needed, or a `kill` overflowed the queue) delivers as
//! [`SigInfo::KERNEL`].

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::signal::{SIGQUEUE_MAX, SIGRTMIN, SigInfo, SigSet, sig_bit, sig_is_realtime};

use crate::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use crate::task::ops::SignalPost;
use crate::{AllocError, KArc, KBox, Zeroable};

#[derive(Clone, Copy, Zeroable)]
#[repr(C)]
struct RtEntry {
    signo: u8,
    _pad: [u8; 7],
    info: SigInfo,
}

const STANDARD: usize = (SIGRTMIN - 1) as usize;

/// One slot past the send limit, so an instance a delivery took and has to put
/// back still fits after a sender refilled the queue behind it.
const RT_CAPACITY: usize = SIGQUEUE_MAX + 1;

/// Allocated zeroed on the first signal that carries a record; all-zero is
/// the empty store.
#[derive(Zeroable)]
#[repr(C)]
pub struct SigQueue {
    /// Bit `n - 1`: standard signal `n` has a record in `standard`.
    recorded: u32,
    rt_len: u32,
    standard: [SigInfo; STANDARD],
    rt: [RtEntry; RT_CAPACITY],
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

    /// Queue an instance of realtime `signo` behind every earlier one. `false`
    /// when [`SIGQUEUE_MAX`] are already queued.
    pub(crate) fn push(&mut self, signo: u8, info: SigInfo) -> bool {
        let len = self.rt_len as usize;
        if len >= SIGQUEUE_MAX {
            return false;
        }
        self.rt[len] = RtEntry {
            signo,
            _pad: [0; 7],
            info,
        };
        self.rt_len += 1;
        true
    }

    /// Put an instance back at the head. Never refused: the reserve slot takes
    /// it, and should that be spoken for too, the newest queued record makes
    /// room, so an older instance is never lost to a later one.
    pub(crate) fn push_front(&mut self, signo: u8, info: SigInfo) {
        let len = (self.rt_len as usize).min(RT_CAPACITY - 1);
        self.rt.copy_within(0..len, 1);
        self.rt[0] = RtEntry {
            signo,
            _pad: [0; 7],
            info,
        };
        self.rt_len = (len + 1) as u32;
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

/// One instance a thread took for delivery, and which set it came from so a
/// delivery that cannot use it puts it back there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DequeuedSignal {
    pub signum: u8,
    pub info: SigInfo,
    pub(crate) shared: bool,
}

/// The one lock class of every pending set. A leaf: nothing is acquired under
/// it, and a post publishes its wake only after releasing it.
const SIGQUEUE_CLASS: &crate::sync::lock_tracking::LockClassKey =
    crate::lock_class!("Task.sigqueue", LOCK_LEVEL_RESOURCE);

/// One pending set: bit `n - 1` for signal `n`, and the records behind it.
pub struct PendingSignals {
    /// Every writer holds `store`'s lock; readers probe it lock-free.
    bits: AtomicU64,
    store: SpinLock<Option<KBox<SigQueue>>>,
}

impl PendingSignals {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU64::new(0),
            store: SpinLock::new(None, SIGQUEUE_CLASS),
        }
    }

    /// A thread group's shared set.
    pub fn try_new_shared() -> Result<KArc<Self>, AllocError> {
        KArc::try_new(Self::new())
    }

    /// Complete an empty set at `slot`, whose bytes the caller zeroed, without
    /// building one on the stack: the zero word and `None` store are already
    /// right, and only the lock needs its class.
    ///
    /// # Safety
    /// `slot` is valid for writes, aligned, and all zero.
    pub(crate) unsafe fn init_zeroed_in_place(slot: *mut Self) {
        // SAFETY: the caller guarantees `slot` is writable and aligned.
        unsafe { addr_of_mut!((*slot).store).write(SpinLock::new(None, SIGQUEUE_CLASS)) };
    }

    #[inline]
    pub fn bits(&self) -> SigSet {
        self.bits.load(Ordering::Acquire)
    }

    /// Realtime instances queued, of every signal.
    pub fn queued(&self) -> usize {
        self.store.lock().as_deref().map_or(0, SigQueue::queued)
    }

    /// Overwrite the set, dropping the records of every signal it clears.
    pub(crate) fn set(&self, value: SigSet) {
        let mut store = self.store.lock();
        if let Some(queue) = store.as_deref_mut() {
            queue.forget(!value);
        }
        self.bits.store(value, Ordering::Release);
    }

    /// Clear `bits` with every record they carried, returning the previous set.
    pub(crate) fn clear(&self, bits: SigSet) -> SigSet {
        let mut store = self.store.lock();
        if let Some(queue) = store.as_deref_mut() {
            queue.forget(bits);
        }
        self.bits.fetch_and(!bits, Ordering::AcqRel)
    }

    /// Raise `bits` with no record. Returns the previous set.
    pub(crate) fn raise(&self, bits: SigSet) -> SigSet {
        let _store = self.store.lock();
        self.bits.fetch_or(bits, Ordering::AcqRel)
    }

    pub(crate) fn has_store(&self) -> bool {
        self.store.lock().is_some()
    }

    pub(crate) fn take_store(&mut self) -> Option<KBox<SigQueue>> {
        self.store.get_mut().take()
    }

    /// Make one instance of `signum` pending with `info`, installing `spare` as
    /// the record store if there is none yet. Hands back what it did and the
    /// spare if unused, for the caller to drop outside the lock.
    ///
    /// A realtime instance past the limit is refused unless its sender is one
    /// POSIX lets overflow ([`SigInfo::survives_queue_overflow`]); that one pends
    /// without its record.
    pub(crate) fn enqueue(
        &self,
        signum: u8,
        info: SigInfo,
        spare: Option<KBox<SigQueue>>,
    ) -> (SignalPost, Option<KBox<SigQueue>>) {
        let bit = sig_bit(signum);
        let mut store = self.store.lock();
        let mut spare = spare;
        if store.is_none() {
            *store = spare.take();
        }
        if !sig_is_realtime(signum) {
            // Standard signals coalesce: the first sender's record stands.
            if self.bits.load(Ordering::Acquire) & bit != 0 {
                return (SignalPost::Dropped, spare);
            }
            if let Some(queue) = store.as_deref_mut() {
                queue.record(signum, info);
            }
        } else {
            let queued = store
                .as_deref_mut()
                .is_some_and(|queue| queue.push(signum, info));
            if !queued && !info.survives_queue_overflow() {
                return (SignalPost::QueueFull, spare);
            }
        }
        self.bits.fetch_or(bit, Ordering::AcqRel);
        (SignalPost::Pending, spare)
    }

    /// Take the lowest-numbered pending signal in `mask` with the record the
    /// instance carries. Its bit clears unless another instance stays queued.
    pub(crate) fn dequeue(&self, mask: SigSet) -> Option<(u8, SigInfo)> {
        let mut store = self.store.lock();
        let pending = self.bits.load(Ordering::Acquire) & mask;
        if pending == 0 {
            return None;
        }
        let signum = (pending.trailing_zeros() + 1) as u8;
        let (info, more) = match store.as_deref_mut() {
            Some(queue) => queue.take(signum),
            None => (SigInfo::KERNEL, false),
        };
        if !more {
            self.bits.fetch_and(!sig_bit(signum), Ordering::AcqRel);
        }
        Some((signum, info))
    }

    /// Put back an instance [`dequeue`](Self::dequeue) took, ahead of any later
    /// one of its signal.
    pub(crate) fn requeue(&self, signum: u8, info: SigInfo) {
        let mut store = self.store.lock();
        if let Some(queue) = store.as_deref_mut() {
            if sig_is_realtime(signum) {
                queue.push_front(signum, info);
            } else {
                queue.record(signum, info);
            }
        }
        self.bits.fetch_or(sig_bit(signum), Ordering::AcqRel);
    }
}

impl Default for PendingSignals {
    fn default() -> Self {
        Self::new()
    }
}
