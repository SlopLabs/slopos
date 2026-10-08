//! The request engine every block transport runs on: a fixed set of request
//! slots, each owning the DMA pages a request is staged through, and what
//! happens when the device is slow — a request that times out is quarantined
//! with its pages until the device hands it back, and a write it abandoned
//! fences every later request that could reorder the medium behind it.
//!
//! A transport supplies only [`QueueOps`]: how a staged request becomes a
//! device submission, and how completions come back.

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_fs::blockdev::{BlockDeviceError, stats};
use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::WaitAbort;
use slopos_ostd::sync::wait_queue::current_task_is_killed;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock, WaitQueue};
use slopos_ostd::{
    KBox, KVec, klog_info, lock_class, write_array_field, write_field, write_init_field,
};

use crate::hpet::poll_wait;

pub const PAGE_SIZE: usize = 4096;
/// Bounce pages per slot: 32 × 4 KiB = 128 KiB of payload behind one
/// submission and one completion — a whole writeback run of 4 KiB blocks.
pub const MAX_DATA_PAGES: usize = 32;
pub const MAX_XFER: usize = MAX_DATA_PAGES * PAGE_SIZE;
pub const MAX_SLOTS: usize = 8;
pub const QUARANTINE_SLOTS: usize = 2;
/// How long a request the device owns is waited out with kills ignored: past
/// it a kill abandons the request to the quarantine, whose fence keeps what
/// the device may still write in order.
const UNINTERRUPTIBLE_MS: u64 = slopos_ostd::sync::wait_queue::UNINTERRUPTIBLE_MAX_MS;
/// How long a requester watches for its completion before it sleeps.
const COMPLETION_POLL_NS: u64 = 50_000;
/// How long a request waits for a free slot before it is answered `Busy`,
/// for a transport whose requests complete in milliseconds.
pub const SLOT_WAIT_MS: u64 = 250;
/// Attempts per logical request, including the first.
const REQUEST_ATTEMPTS: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    Flush,
}

/// What one queue's requests address: a namespace and the logical block its
/// medium is counted in. A virtio disk is namespace 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Namespace {
    pub nsid: u32,
    pub block_shift: u8,
}

/// One request as the transport submits it: `len` bytes at byte `offset` of
/// `ns`, both whole logical blocks.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    pub op: Op,
    pub ns: Namespace,
    pub offset: u64,
    pub len: usize,
}

/// Why one request failed, before it is mapped onto [`BlockDeviceError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkError {
    NotReady,
    OutOfBounds,
    BadRequest,
    Busy,
    Timeout,
    Interrupted,
    Abandoned,
    /// `retry` is set when the failure may not recur: a lost or truncated
    /// completion, or one the device marked retryable.
    DeviceFault {
        retry: bool,
    },
    Unsupported,
    OutOfMemory,
}

impl BlkError {
    pub(crate) fn retryable(self) -> bool {
        matches!(self, BlkError::Busy | BlkError::DeviceFault { retry: true })
    }
}

impl From<BlkError> for BlockDeviceError {
    fn from(err: BlkError) -> Self {
        match err {
            BlkError::OutOfBounds => BlockDeviceError::OutOfBounds,
            BlkError::BadRequest => BlockDeviceError::InvalidBuffer,
            BlkError::Busy => BlockDeviceError::Busy,
            BlkError::Timeout => BlockDeviceError::Timeout,
            BlkError::Interrupted => BlockDeviceError::Interrupted,
            BlkError::Abandoned => BlockDeviceError::Abandoned,
            BlkError::NotReady | BlkError::DeviceFault { .. } => BlockDeviceError::DeviceFault,
            BlkError::Unsupported => BlockDeviceError::Unsupported,
            BlkError::OutOfMemory => BlockDeviceError::OutOfMemory,
        }
    }
}

/// Why a waiter stopped waiting for the device to answer its request.
#[derive(Clone, Copy)]
enum GaveUp {
    Deadline,
    Killed,
}

impl GaveUp {
    /// A killed read changes nothing wherever it lands, so it is only
    /// interrupted; a killed write may land yet.
    fn error(self, writes: bool) -> BlkError {
        match self {
            GaveUp::Deadline => BlkError::Timeout,
            GaveUp::Killed if writes => BlkError::Abandoned,
            GaveUp::Killed => BlkError::Interrupted,
        }
    }

    fn name(self) -> &'static str {
        match self {
            GaveUp::Deadline => "timeout",
            GaveUp::Killed => "abandoned to a kill",
        }
    }
}

/// One slot's DMA memory: the transport's own page — virtio's request header
/// and status, NVMe's PRP list — and the bounce pages a payload is staged
/// through. Allocated at probe, so no steady-state request allocates.
pub struct RequestPages {
    pub aux: OwnedPageFrame,
    pub data: KVec<OwnedPageFrame>,
}

impl RequestPages {
    pub(crate) fn allocate(data_pages: usize) -> Option<Self> {
        let aux = OwnedPageFrame::alloc_zeroed()?;
        let mut data = KVec::with_capacity(data_pages).ok()?;
        for _ in 0..data_pages {
            data.push(OwnedPageFrame::alloc_zeroed()?).ok()?;
        }
        Some(Self { aux, data })
    }

    /// The pages a transfer of `len` bytes occupies.
    pub fn span(&self, len: usize) -> &[OwnedPageFrame] {
        &self.data[..len.div_ceil(PAGE_SIZE)]
    }

    pub(crate) fn copy_in(&self, at: usize, src: &[u8]) -> bool {
        let mut done = 0;
        while done < src.len() {
            let pos = at + done;
            let within = pos % PAGE_SIZE;
            let n = (PAGE_SIZE - within).min(src.len() - done);
            let Some(page) = self.data.get(pos / PAGE_SIZE) else {
                return false;
            };
            if !page.write_slice(within, &src[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    pub(crate) fn copy_out(&self, at: usize, dst: &mut [u8]) -> bool {
        let mut done = 0;
        while done < dst.len() {
            let pos = at + done;
            let within = pos % PAGE_SIZE;
            let n = (PAGE_SIZE - within).min(dst.len() - done);
            let Some(page) = self.data.get(pos / PAGE_SIZE) else {
                return false;
            };
            if !page.read_slice(within, &mut dst[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    fn gather(&self, at: usize, cur: &mut SegCursor<'_>, len: usize) -> bool {
        let mut done = 0;
        while done < len {
            let frag = cur.peek();
            if frag.is_empty() {
                return false;
            }
            let n = frag.len().min(len - done);
            if !self.copy_in(at + done, &frag[..n]) {
                return false;
            }
            cur.advance(n);
            done += n;
        }
        true
    }
}

/// What a transport does, called with the engine's state lock held and IRQs
/// off: nothing here may sleep, allocate or log.
pub trait QueueOps: Send {
    /// Hand the device `req`, staged in `pages`; the answer is the tag its
    /// completion will carry. `Busy` when the queue has no room.
    fn submit(&mut self, req: &Request, pages: &RequestPages) -> Result<u16, BlkError>;
    /// The next completion the device posted: its tag and transport status.
    fn pop(&mut self) -> Option<(u16, u32)>;
    /// Called once a harvest popped at least one completion.
    fn end_harvest(&mut self) {}
    /// The device no longer owns what `tag`'s submission gave it. A tag the
    /// transport never issued is ignored.
    fn retire(&mut self, tag: u16);
    /// What a completion's status means for its request.
    fn outcome(&self, pages: &RequestPages, status: u32) -> Result<(), BlkError>;
}

/// Lifecycle of one request slot.
///
/// `Held` means a caller owns the slot's pages and the device holds nothing
/// pointing at them. `Quarantined` is the fallback for a timeout that could
/// not move the request into the quarantine list: the device may still write
/// into those pages, so neither they nor the tag may be reused.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Held,
    InFlight,
    Complete,
    Quarantined,
}

struct RequestSlot {
    state: SlotState,
    tag: u16,
    status: u32,
    /// The namespace a write request would change.
    writes: Option<u32>,
    /// `None` exactly while a caller holds them.
    pages: Option<RequestPages>,
}

impl RequestSlot {
    const EMPTY: RequestSlot = RequestSlot {
        state: SlotState::Free,
        tag: 0,
        status: 0,
        writes: None,
        pages: None,
    };

    fn available(&self) -> bool {
        self.state == SlotState::Free && self.pages.is_some()
    }
}

/// A request a timeout gave up on, moved off its slot so the slot keeps
/// serving. The device still owns its tag and may still write into `pages`.
struct Quarantined {
    tag: u16,
    writes: Option<u32>,
    done: bool,
    pages: RequestPages,
}

/// What one harvest pass changed, so the wakes can happen with the state lock
/// released.
#[derive(Clone, Copy)]
struct Harvest {
    /// Bit *i* set: slot *i* moved to `Complete`.
    completed: u32,
    /// A quarantined slot's request came back, and the slot serves again.
    freed: bool,
    /// A quarantine-list entry's request came back, and its pages can go.
    reapable: bool,
}

/// What the timeout epilogue did with the slot it gave up waiting on.
enum SlotOutcome {
    /// Request moved into the quarantine list; the slot took the replacement
    /// page set and serves again.
    Recycled,
    /// Request stays on its slot, which serves nothing until the device
    /// returns it.
    Withheld,
    /// The device completed the request while the replacement was allocated
    /// with the lock released, so it is the caller's after all.
    Completed(RequestPages, u32),
}

#[derive(slopos_ostd::SlotFields)]
struct EngineState {
    queue: Option<KBox<dyn QueueOps>>,
    slots: [RequestSlot; MAX_SLOTS],
    quarantine: [Option<Quarantined>; QUARANTINE_SLOTS],
}

impl EngineState {
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, queue, None);
                write_array_field!(slot, slots, MAX_SLOTS, |_| RequestSlot::EMPTY);
                write_array_field!(slot, quarantine, QUARANTINE_SLOTS, |_| None);
                Ok(slot.finish())
            },
        )
    }

    /// Match each completion to its request by tag; an unknown tag is dropped
    /// rather than attributed to whatever request is waiting. Runs in IRQ
    /// context under the state lock.
    fn harvest(&mut self) -> Harvest {
        let mut out = Harvest {
            completed: 0,
            freed: false,
            reapable: false,
        };
        let Some(queue) = self.queue.as_mut() else {
            return out;
        };
        let mut popped = false;
        while let Some((tag, status)) = queue.pop() {
            popped = true;
            let slot = self.slots.iter().position(|s| {
                s.tag == tag && matches!(s.state, SlotState::InFlight | SlotState::Quarantined)
            });
            if let Some(i) = slot {
                self.slots[i].status = status;
                if self.slots[i].state == SlotState::InFlight {
                    self.slots[i].state = SlotState::Complete;
                    out.completed |= 1 << i;
                } else {
                    queue.retire(tag);
                    self.slots[i].state = SlotState::Free;
                    out.freed = true;
                }
                continue;
            }
            if let Some(q) = self
                .quarantine
                .iter_mut()
                .flatten()
                .find(|q| q.tag == tag && !q.done)
            {
                q.done = true;
                out.reapable = true;
            }
        }
        if popped {
            queue.end_harvest();
        }
        out
    }

    fn retire(&mut self, tag: u16) {
        if let Some(queue) = self.queue.as_mut() {
            queue.retire(tag);
        }
    }

    fn has_available_slot(&self, count: usize) -> bool {
        self.slots[..count].iter().any(RequestSlot::available)
    }

    fn take_available_slot(&mut self, count: usize) -> Option<(usize, RequestPages)> {
        let i = self.slots[..count]
            .iter()
            .position(RequestSlot::available)?;
        let pages = self.slots[i].pages.take()?;
        self.slots[i].state = SlotState::Held;
        Some((i, pages))
    }

    /// `pages` moves into a field that is already `None`, so no frame is
    /// dropped under the lock.
    fn put_slot(&mut self, idx: usize, pages: RequestPages) {
        self.slots[idx].pages = Some(pages);
        self.slots[idx].state = SlotState::Free;
    }

    fn take_complete(&mut self, idx: usize) -> Option<(RequestPages, u32)> {
        if self.slots[idx].state != SlotState::Complete {
            return None;
        }
        let pages = self.slots[idx].pages.take()?;
        let tag = self.slots[idx].tag;
        self.retire(tag);
        self.slots[idx].state = SlotState::Held;
        Some((pages, self.slots[idx].status))
    }

    /// Move a timed-out request off its slot. `replacement` is the fresh page
    /// set that puts the slot back in service; one it could not use comes
    /// back for the caller to drop with the lock released, since a buddy free
    /// must never run under it.
    ///
    /// `replacement` is allocated with the lock released, so an IRQ harvest
    /// can flip the slot to `Complete` in that window. That completion is
    /// taken here: a slot quarantined out of `Complete` is one nothing would
    /// ever look at again.
    fn quarantine_slot(
        &mut self,
        idx: usize,
        replacement: Option<RequestPages>,
    ) -> (SlotOutcome, Option<RequestPages>) {
        match self.slots[idx].state {
            SlotState::Complete => match self.take_complete(idx) {
                Some((pages, status)) => (SlotOutcome::Completed(pages, status), replacement),
                None => (SlotOutcome::Withheld, replacement),
            },
            SlotState::InFlight => {
                let free = self.quarantine.iter().position(Option::is_none);
                match (free, replacement, self.slots[idx].pages.take()) {
                    (Some(free), Some(replacement), Some(pages)) => {
                        self.quarantine[free] = Some(Quarantined {
                            tag: self.slots[idx].tag,
                            writes: self.slots[idx].writes,
                            done: false,
                            pages,
                        });
                        self.slots[idx].pages = Some(replacement);
                        self.slots[idx].state = SlotState::Free;
                        (SlotOutcome::Recycled, None)
                    }
                    (_, unused, pages) => {
                        self.slots[idx].pages = pages;
                        self.slots[idx].state = SlotState::Quarantined;
                        (SlotOutcome::Withheld, unused)
                    }
                }
            }
            // Only the caller's own request, `InFlight` or `Complete`, gets here.
            SlotState::Free | SlotState::Held | SlotState::Quarantined => {
                (SlotOutcome::Withheld, replacement)
            }
        }
    }

    /// Retires the request's tag; the pages go out for the caller to drop once
    /// the lock is released.
    fn take_reaped(&mut self) -> Option<RequestPages> {
        let i = self
            .quarantine
            .iter()
            .position(|q| q.as_ref().is_some_and(|q| q.done))?;
        let entry = self.quarantine[i].take()?;
        self.retire(entry.tag);
        Some(entry.pages)
    }

    fn quarantine_count(&self) -> usize {
        self.quarantine.iter().flatten().count()
            + self
                .slots
                .iter()
                .filter(|s| s.state == SlotState::Quarantined)
                .count()
    }

    fn all_slots_quarantined(&self, count: usize) -> bool {
        self.slots[..count]
            .iter()
            .all(|s| s.state == SlotState::Quarantined)
    }

    /// Whether a write the device may still perform is owed to namespace
    /// `nsid`, or to any namespace when `None`.
    fn owes_abandoned_write(&self, nsid: Option<u32>) -> bool {
        let owed = |writes: Option<u32>| writes.is_some() && (nsid.is_none() || writes == nsid);
        self.quarantine
            .iter()
            .flatten()
            .any(|q| owed(q.writes) && !q.done)
            || self
                .slots
                .iter()
                .any(|s| owed(s.writes) && s.state == SlotState::Quarantined)
    }
}

/// Per-queue request state, heap-resident inside a `KArc` so its address is
/// stable: the queue's interrupt handler and every disk it serves hold one.
#[derive(slopos_ostd::SlotFields)]
pub struct Engine {
    /// The `SpinLock` disables IRQs while held, so the IRQ-side harvest never
    /// interleaves with a task-side submit or collect. Nothing that sleeps,
    /// frees a frame or emits a klog line runs under it.
    state: SpinLock<EngineState>,
    /// One queue per slot: a completion wakes the single caller waiting on
    /// that request instead of every requester.
    slot_waiters: [WaitQueue; MAX_SLOTS],
    free_waiters: WaitQueue,
    /// Requests that could reorder the medium, waiting for the device to
    /// return an abandoned write.
    abandon_waiters: WaitQueue,
    /// Set with the state lock held whenever a write is quarantined, cleared
    /// under it once none is owed, so an unfenced request reads one atomic.
    write_abandoned: AtomicBool,
    /// Flagged by the harvest, consumed by the next task-context reap: the IRQ
    /// side must not free frames, and the steady state must not pay for the
    /// scan.
    quarantine_dirty: AtomicBool,
    ready: AtomicBool,
    slot_count: usize,
    max_transfer: usize,
    timeout_ms: u64,
    slot_wait_ms: u64,
    name: &'static str,
}

impl Engine {
    /// `slot_count` request slots of `max_transfer` bytes each, a request
    /// given up on after `timeout_ms` or [`UNINTERRUPTIBLE_MS`], whichever is
    /// longer, and a free slot waited for `slot_wait_ms` before the request
    /// is `Busy`. Built in place through `KArc::try_init`, so nothing
    /// materialises on the caller's stack; serves nothing until
    /// [`Self::start`].
    pub fn init(
        name: &'static str,
        slot_count: usize,
        max_transfer: usize,
        timeout_ms: u64,
        slot_wait_ms: u64,
    ) -> impl Init<Self, AllocError> {
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_init_field!(
                    slot,
                    state,
                    SpinLock::init_with(
                        lock_class!("BlockEngine.state", LOCK_LEVEL_RESOURCE),
                        EngineState::init_empty()
                    )
                )?;
                write_array_field!(slot, slot_waiters, MAX_SLOTS, |_| WaitQueue::new(
                    lock_class!("BlockEngine.slot_waiters", LOCK_LEVEL_RESOURCE)
                ));
                write_field!(
                    slot,
                    free_waiters,
                    WaitQueue::new(lock_class!("BlockEngine.free_waiters", LOCK_LEVEL_RESOURCE))
                );
                write_field!(
                    slot,
                    abandon_waiters,
                    WaitQueue::new(lock_class!(
                        "BlockEngine.abandon_waiters",
                        LOCK_LEVEL_RESOURCE
                    ))
                );
                write_field!(slot, write_abandoned, AtomicBool::new(false));
                write_field!(slot, quarantine_dirty, AtomicBool::new(false));
                write_field!(slot, ready, AtomicBool::new(false));
                write_field!(slot, slot_count, slot_count.clamp(1, MAX_SLOTS));
                write_field!(
                    slot,
                    max_transfer,
                    max_transfer.clamp(PAGE_SIZE, MAX_XFER) / PAGE_SIZE * PAGE_SIZE
                );
                write_field!(slot, timeout_ms, timeout_ms.max(UNINTERRUPTIBLE_MS));
                write_field!(slot, slot_wait_ms, slot_wait_ms);
                write_field!(slot, name, name);
                Ok(slot.finish())
            },
        )
    }

    /// Preallocate every slot's pages, so the steady-state request path never
    /// touches the frame allocator. Call before [`Self::start`].
    pub fn prime(&self) -> bool {
        for idx in 0..self.slot_count {
            let Some(pages) = RequestPages::allocate(self.max_transfer / PAGE_SIZE) else {
                return false;
            };
            self.state.lock().put_slot(idx, pages);
        }
        true
    }

    /// Install the transport's queue and begin serving.
    pub fn start(&self, queue: KBox<dyn QueueOps>) {
        self.state.lock().queue = Some(queue);
        self.ready.store(true, Ordering::Release);
    }

    /// Refuse every request from here on, as a controller about to be shut
    /// down needs.
    pub fn stop(&self) {
        self.ready.store(false, Ordering::Release);
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    /// Two in-flight writes per caller would leave nothing for anyone sharing
    /// the queue, so a caller gets half the slots.
    pub fn write_depth(&self) -> usize {
        (self.slot_count / 2).max(1)
    }

    /// The queue's completion interrupt.
    pub fn handle_irq(&self) {
        let harvest = self.state.lock().harvest();
        self.publish_harvest(harvest, MAX_SLOTS);
    }

    /// Whether the device holds nothing of the engine's: no request in
    /// flight, and every quarantined one returned. Harvests first, since a
    /// stopped engine takes no request that would.
    pub fn is_idle(&self) -> bool {
        let (idle, harvest) = {
            let mut state = self.state.lock();
            let harvest = state.harvest();
            let idle = state.slots[..self.slot_count]
                .iter()
                .all(|s| !matches!(s.state, SlotState::InFlight | SlotState::Quarantined))
                && state.quarantine.iter().flatten().all(|q| q.done);
            (idle, harvest)
        };
        self.publish_harvest(harvest, MAX_SLOTS);
        idle
    }

    /// Wake each slot the harvest completed except `skip` (the caller's own,
    /// which returns through its own predicate). Runs with the state lock
    /// released: a wait queue's lock must never nest inside it.
    fn publish_harvest(&self, harvest: Harvest, skip: usize) {
        for i in 0..self.slot_count {
            if i != skip && harvest.completed & (1 << i) != 0 {
                let _ = self.slot_waiters[i].wake_one();
            }
        }
        if harvest.freed {
            let _ = self.free_waiters.wake_all();
        }
        if harvest.reapable {
            self.quarantine_dirty.store(true, Ordering::Release);
        }
        if harvest.freed || harvest.reapable {
            let _ = self.abandon_waiters.wake_all();
        }
    }

    /// Reclaim the tags and DMA pages of quarantined requests the device has
    /// finished with. The pages are dropped with the lock released.
    fn reap_quarantine(&self) {
        if !self.quarantine_dirty.swap(false, Ordering::Acquire) {
            return;
        }
        loop {
            let reaped = self.state.lock().take_reaped();
            if reaped.is_none() {
                break;
            }
        }
    }

    /// A free slot, or `Busy` without waiting for one.
    fn acquire_slot_now(&self) -> Result<(usize, RequestPages), BlkError> {
        self.state
            .lock()
            .take_available_slot(self.slot_count)
            .ok_or(BlkError::Busy)
    }

    fn acquire_slot(&self) -> Result<(usize, RequestPages), BlkError> {
        match self.acquire_slot_now() {
            Err(BlkError::Busy) => {}
            acquired => return acquired,
        }
        let available = || self.state.lock().has_available_slot(self.slot_count);
        match self
            .free_waiters
            .wait_event_timeout(available, self.slot_wait_ms)
        {
            Ok(()) => {}
            Err(WaitAbort::NoRuntime) => {
                poll_wait(&available, self.slot_wait_ms as u32);
            }
            Err(WaitAbort::Killed) => return Err(BlkError::Interrupted),
            Err(_) => return Err(BlkError::Busy),
        }
        self.state
            .lock()
            .take_available_slot(self.slot_count)
            .ok_or(BlkError::Busy)
    }

    /// Hold a request that could reorder `nsid`'s medium behind a write a
    /// timeout abandoned: the device may still perform it, after anything sent
    /// since.
    fn await_abandoned_writes(&self, nsid: u32) -> Result<(), BlkError> {
        if !self.write_abandoned.load(Ordering::Acquire) {
            return Ok(());
        }
        let settled = || {
            let (owed, harvest) = {
                let mut state = self.state.lock();
                let harvest = state.harvest();
                if !state.owes_abandoned_write(None) {
                    self.write_abandoned.store(false, Ordering::Release);
                }
                (state.owes_abandoned_write(Some(nsid)), harvest)
            };
            self.publish_harvest(harvest, MAX_SLOTS);
            !owed
        };
        let settled = match self
            .abandon_waiters
            .wait_event_timeout(settled, self.timeout_ms)
        {
            Ok(()) => Ok(()),
            Err(WaitAbort::NoRuntime) if poll_wait(&settled, self.timeout_ms as u32) => Ok(()),
            Err(WaitAbort::Killed) => Err(BlkError::Interrupted),
            Err(_) => Err(BlkError::Timeout),
        };
        self.reap_quarantine();
        settled
    }

    fn release_slot(&self, idx: usize, pages: RequestPages) {
        self.state.lock().put_slot(idx, pages);
        let _ = self.free_waiters.wake_one();
    }

    /// Hand the slot's request to the transport. On failure the pages come
    /// back, so the caller can release the slot.
    fn submit(
        &self,
        idx: usize,
        pages: RequestPages,
        req: &Request,
    ) -> Result<(), (BlkError, RequestPages)> {
        let writes = (req.op == Op::Write).then_some(req.ns.nsid);
        let mut state = self.state.lock();
        if !self.is_ready() {
            drop(state);
            return Err((BlkError::NotReady, pages));
        }
        if req.op != Op::Read && state.owes_abandoned_write(Some(req.ns.nsid)) {
            drop(state);
            return Err((BlkError::Busy, pages));
        }
        let Some(queue) = state.queue.as_mut() else {
            drop(state);
            return Err((BlkError::NotReady, pages));
        };
        let tag = match queue.submit(req, &pages) {
            Ok(tag) => tag,
            Err(err) => {
                let held = state.quarantine_count();
                drop(state);
                if err == BlkError::Busy {
                    klog_info!(
                        "{}: submission queue full, {} request(s) quarantined",
                        self.name,
                        held
                    );
                }
                return Err((err, pages));
            }
        };
        let slot = &mut state.slots[idx];
        slot.state = SlotState::InFlight;
        slot.tag = tag;
        slot.writes = writes;
        slot.pages = Some(pages);
        Ok(())
    }

    /// Park on this slot's queue until its request completes. The predicate
    /// re-harvests, so a lost interrupt is recovered on any wake.
    fn wait_for_completion(&self, idx: usize) -> Result<(RequestPages, u32), BlkError> {
        let collect = || {
            let (done, harvest) = {
                let mut state = self.state.lock();
                let harvest = state.harvest();
                (state.take_complete(idx), harvest)
            };
            self.publish_harvest(harvest, idx);
            done
        };

        // Spin first: a device answering in microseconds costs less to watch
        // than a sleep and a wake, and the requester often holds a lock.
        let deadline =
            slopos_kernel_services::clock::monotonic_ns().saturating_add(COMPLETION_POLL_NS);
        loop {
            if let Some(done) = collect() {
                return Ok(done);
            }
            if slopos_kernel_services::clock::monotonic_ns() >= deadline {
                break;
            }
            for _ in 0..64 {
                core::hint::spin_loop();
            }
        }

        // Uninterruptible first: a request abandoned to a kill holds one of the
        // quarantine places until the device returns it, and a write it
        // abandons fences every later one to its namespace.
        let first = UNINTERRUPTIBLE_MS.min(self.timeout_ms);
        let waited = match self.slot_waiters[idx]
            .wait_event_uninterruptible_timeout_until(&collect, first)
        {
            Err(WaitAbort::Timeout) if self.timeout_ms > first => {
                self.slot_waiters[idx].wait_event_timeout_until(&collect, self.timeout_ms - first)
            }
            other => other,
        };
        match waited {
            Ok(done) => Ok(done),
            Err(WaitAbort::NoRuntime) => {
                poll_wait(
                    &|| {
                        let (complete, harvest) = {
                            let mut state = self.state.lock();
                            let harvest = state.harvest();
                            (state.slots[idx].state == SlotState::Complete, harvest)
                        };
                        self.publish_harvest(harvest, idx);
                        complete
                    },
                    self.timeout_ms as u32,
                );
                self.finish_or_quarantine(idx, GaveUp::Deadline)
            }
            Err(WaitAbort::Killed) => self.finish_or_quarantine(idx, GaveUp::Killed),
            Err(_) => self.finish_or_quarantine(idx, GaveUp::Deadline),
        }
    }

    /// What a waiter that stopped waiting does: one final harvest, which
    /// recovers a completion whose interrupt was lost, else move the request
    /// out of the slot's way.
    #[inline(never)]
    fn finish_or_quarantine(
        &self,
        idx: usize,
        why: GaveUp,
    ) -> Result<(RequestPages, u32), BlkError> {
        let (done, harvest) = {
            let mut state = self.state.lock();
            let harvest = state.harvest();
            (state.take_complete(idx), harvest)
        };
        self.publish_harvest(harvest, idx);
        if let Some(done) = done {
            return Ok(done);
        }
        self.quarantine(idx, why)
    }

    #[inline(never)]
    fn quarantine(&self, idx: usize, why: GaveUp) -> Result<(RequestPages, u32), BlkError> {
        // Allocated before the lock is taken: the slot's own pages stay with
        // the device, and a frame allocation must not run under the lock.
        let replacement = RequestPages::allocate(self.max_transfer / PAGE_SIZE);

        let (outcome, unused, tag, wedged, writes) = {
            let mut state = self.state.lock();
            let writes = state.slots[idx].writes.is_some();
            let (outcome, unused) = state.quarantine_slot(idx, replacement);
            if state.owes_abandoned_write(None) {
                self.write_abandoned.store(true, Ordering::Release);
            }
            let tag = state.slots[idx].tag;
            let wedged = state.all_slots_quarantined(self.slot_count);
            (outcome, unused, tag, wedged, writes)
        };
        drop(unused);

        let recycled = match outcome {
            SlotOutcome::Completed(pages, status) => return Ok((pages, status)),
            SlotOutcome::Recycled => true,
            SlotOutcome::Withheld => false,
        };

        klog_info!(
            "{}: request {}, tag {} quarantined, slot {} {}",
            self.name,
            why.name(),
            tag,
            idx,
            if recycled { "recycled" } else { "withheld" }
        );
        if wedged {
            klog_info!(
                "{}: all {} request slots quarantined — the device is not completing requests",
                self.name,
                self.slot_count
            );
        }
        if recycled {
            let _ = self.free_waiters.wake_all();
        }
        Err(why.error(writes))
    }

    /// Finish a request `wait_for_completion` handed back: its status, then
    /// `drain` with the pages, then the slot back.
    fn conclude(
        &self,
        idx: usize,
        (pages, status): (RequestPages, u32),
        drain: &mut dyn FnMut(&RequestPages) -> bool,
    ) -> Result<(), BlkError> {
        let outcome = self.outcome_of(&pages, status);
        let drained = outcome.is_ok() && drain(&pages);
        self.release_slot(idx, pages);
        outcome?;
        if !drained {
            return Err(BlkError::BadRequest);
        }
        Ok(())
    }

    fn outcome_of(&self, pages: &RequestPages, status: u32) -> Result<(), BlkError> {
        match self.state.lock().queue.as_ref() {
            Some(queue) => queue.outcome(pages, status),
            None => Err(BlkError::NotReady),
        }
    }

    fn mark_killed_after_submit(&self) {
        #[cfg(feature = "test-hooks")]
        hooks::kill_if_armed();
    }

    /// Acquire a slot and refuse a task already killed: it sends nothing.
    fn acquire_live_slot(&self) -> Result<(usize, RequestPages), BlkError> {
        let slot = self.acquire_slot()?;
        self.refuse_killed(slot)
    }

    /// [`Self::acquire_live_slot`] for a caller that may hold requests of its
    /// own: only it can release those, so it must never wait for a slot.
    fn acquire_live_slot_now(&self) -> Result<(usize, RequestPages), BlkError> {
        let slot = self.acquire_slot_now()?;
        self.refuse_killed(slot)
    }

    fn refuse_killed(
        &self,
        (idx, pages): (usize, RequestPages),
    ) -> Result<(usize, RequestPages), BlkError> {
        if current_task_is_killed() {
            self.release_slot(idx, pages);
            return Err(BlkError::Interrupted);
        }
        Ok((idx, pages))
    }

    /// One attempt at one request. `fill` and `drain` run with the pages owned
    /// by this caller and no lock held.
    #[inline(never)]
    fn attempt(
        &self,
        req: &Request,
        fill: &mut dyn FnMut(&RequestPages) -> bool,
        drain: &mut dyn FnMut(&RequestPages) -> bool,
    ) -> Result<(), BlkError> {
        self.reap_quarantine();
        if req.op != Op::Read {
            self.await_abandoned_writes(req.ns.nsid)?;
        }
        let (idx, pages) = self.acquire_live_slot()?;
        if !fill(&pages) {
            self.release_slot(idx, pages);
            return Err(BlkError::BadRequest);
        }
        if let Err((err, pages)) = self.submit(idx, pages, req) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        self.mark_killed_after_submit();
        let done = self.wait_for_completion(idx)?;
        self.conclude(idx, done, drain)
    }

    fn run(
        &self,
        req: &Request,
        fill: &mut dyn FnMut(&RequestPages) -> bool,
        drain: &mut dyn FnMut(&RequestPages) -> bool,
    ) -> Result<(), BlkError> {
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        if req.op != Op::Flush && !self.transfer_fits(req.ns, req.len) {
            return Err(BlkError::BadRequest);
        }
        let mut last = BlkError::Busy;
        let began = slopos_arch::tsc::rdtsc();
        for _ in 0..REQUEST_ATTEMPTS {
            match self.attempt(req, fill, drain) {
                Ok(()) => {
                    stats::note_request_cycles(
                        req.op == Op::Write,
                        slopos_arch::tsc::rdtsc().saturating_sub(began),
                    );
                    return Ok(());
                }
                Err(err) if err.retryable() => last = err,
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    /// Whether `len` bytes are whole blocks of `ns` a slot's pages can carry.
    fn transfer_fits(&self, ns: Namespace, len: usize) -> bool {
        len != 0 && len <= self.max_transfer && len.is_multiple_of(1 << ns.block_shift)
    }

    /// Read `dst.len()` bytes of whole blocks at `offset` straight into `dst`.
    /// The counter sits here rather than at the `BlockDevice` boundary: one
    /// filesystem call is several requests, and it is that traffic a
    /// regression multiplies.
    pub fn read(&self, ns: Namespace, offset: u64, dst: &mut [u8]) -> Result<(), BlkError> {
        stats::note_read(dst.len());
        let req = Request {
            op: Op::Read,
            ns,
            offset,
            len: dst.len(),
        };
        self.run(&req, &mut |_| true, &mut |pages| pages.copy_out(0, dst))
    }

    /// Read the one block at `offset` and copy `dst.len()` bytes from
    /// `within` it out, the rest of the block never leaving the slot.
    pub fn read_partial(
        &self,
        ns: Namespace,
        offset: u64,
        block: usize,
        within: usize,
        dst: &mut [u8],
    ) -> Result<(), BlkError> {
        stats::note_read(block);
        let req = Request {
            op: Op::Read,
            ns,
            offset,
            len: block,
        };
        self.run(&req, &mut |_| true, &mut |pages| {
            pages.copy_out(within, dst)
        })
    }

    /// Write `len` bytes of whole blocks at `offset`, gathered from `cur`.
    pub fn write(
        &self,
        ns: Namespace,
        offset: u64,
        cur: &mut SegCursor<'_>,
        len: usize,
    ) -> Result<(), BlkError> {
        stats::note_write(len);
        let start = *cur;
        let req = Request {
            op: Op::Write,
            ns,
            offset,
            len,
        };
        self.run(
            &req,
            &mut |pages| {
                *cur = start;
                pages.gather(0, cur, len)
            },
            &mut |_| true,
        )
    }

    /// Replace `n` bytes at `within` of the block at `offset` with the next
    /// `n` from `cur`: the block is read into a slot's pages, patched there
    /// and written back from them, so the bytes outside the span are never
    /// clobbered and no block-sized buffer exists anywhere else.
    pub fn read_modify_write(
        &self,
        ns: Namespace,
        offset: u64,
        block: usize,
        within: usize,
        n: usize,
        cur: &mut SegCursor<'_>,
    ) -> Result<(), BlkError> {
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        if !self.transfer_fits(ns, block) || within + n > block {
            return Err(BlkError::BadRequest);
        }
        let start = *cur;
        let mut last = BlkError::Busy;
        for _ in 0..REQUEST_ATTEMPTS {
            *cur = start;
            match self.rmw_attempt(ns, offset, block, within, n, cur) {
                Ok(()) => return Ok(()),
                Err(err) if err.retryable() => last = err,
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    #[inline(never)]
    fn rmw_attempt(
        &self,
        ns: Namespace,
        offset: u64,
        block: usize,
        within: usize,
        n: usize,
        cur: &mut SegCursor<'_>,
    ) -> Result<(), BlkError> {
        self.reap_quarantine();
        // The read is half of a write: taken past an abandoned write, it would
        // put the block's older bytes back once that write lands.
        self.await_abandoned_writes(ns.nsid)?;
        #[cfg(feature = "test-hooks")]
        hooks::note_rmw_read(self.state.lock().owes_abandoned_write(Some(ns.nsid)));
        let (idx, pages) = self.acquire_live_slot()?;
        let read = Request {
            op: Op::Read,
            ns,
            offset,
            len: block,
        };
        stats::note_read(block);
        if let Err((err, pages)) = self.submit(idx, pages, &read) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        let (pages, status) = self.wait_for_completion(idx)?;
        if let Err(err) = self.outcome_of(&pages, status) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        if !pages.gather(within, cur, n) {
            self.release_slot(idx, pages);
            return Err(BlkError::BadRequest);
        }
        if current_task_is_killed() {
            self.release_slot(idx, pages);
            return Err(BlkError::Interrupted);
        }
        stats::note_write(block);
        let write = Request {
            op: Op::Write,
            ..read
        };
        if let Err((err, pages)) = self.submit(idx, pages, &write) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        let done = self.wait_for_completion(idx)?;
        self.conclude(idx, done, &mut |_| true)
    }

    /// Stage and submit one whole-block write of at most
    /// [`Self::max_transfer`] without waiting for it, nor for a slot: `Busy`
    /// when none is free. The answer names the slot for [`Self::complete`].
    pub fn submit_write(
        &self,
        ns: Namespace,
        offset: u64,
        segs: &[&[u8]],
        len: usize,
    ) -> Result<usize, BlkError> {
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        if !self.transfer_fits(ns, len) {
            return Err(BlkError::BadRequest);
        }
        self.reap_quarantine();
        self.await_abandoned_writes(ns.nsid)?;
        let (idx, pages) = self.acquire_live_slot_now()?;
        let mut cur = SegCursor::new(segs);
        if !pages.gather(0, &mut cur, len) {
            self.release_slot(idx, pages);
            return Err(BlkError::BadRequest);
        }
        let req = Request {
            op: Op::Write,
            ns,
            offset,
            len,
        };
        if let Err((err, pages)) = self.submit(idx, pages, &req) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        stats::note_write(len);
        Ok(idx)
    }

    /// Submit a read and return its slot without parking, nor waiting for a
    /// slot, so one task can hold several requests in flight. Paired with
    /// [`Self::complete`].
    pub fn submit_read(&self, ns: Namespace, offset: u64, len: usize) -> Result<usize, BlkError> {
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        if !self.transfer_fits(ns, len) {
            return Err(BlkError::BadRequest);
        }
        self.reap_quarantine();
        let (idx, pages) = self.acquire_live_slot_now()?;
        let req = Request {
            op: Op::Read,
            ns,
            offset,
            len,
        };
        if let Err((err, pages)) = self.submit(idx, pages, &req) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        stats::note_read(len);
        Ok(idx)
    }

    /// Wait out a request [`Self::submit_read`] or [`Self::submit_write`]
    /// started; a read's payload lands in `dst`.
    pub fn complete(&self, idx: usize, dst: &mut [u8]) -> Result<(), BlkError> {
        let done = self.wait_for_completion(idx)?;
        self.conclude(idx, done, &mut |pages| pages.copy_out(0, dst))
    }

    /// Block until the device acknowledges a flush of `ns`.
    pub fn flush(&self, ns: Namespace) -> Result<(), BlkError> {
        let req = Request {
            op: Op::Flush,
            ns,
            offset: 0,
            len: 0,
        };
        self.run(&req, &mut |_| true, &mut |_| true)
    }
}

/// Cursor over a vectored write's segments, so one request can gather bytes
/// that cross a segment boundary.
#[derive(Clone, Copy)]
pub struct SegCursor<'a> {
    segs: &'a [&'a [u8]],
    idx: usize,
    off: usize,
}

impl<'a> SegCursor<'a> {
    pub fn new(segs: &'a [&'a [u8]]) -> Self {
        Self {
            segs,
            idx: 0,
            off: 0,
        }
    }

    /// The contiguous run available at the cursor; empty once exhausted.
    fn peek(&mut self) -> &'a [u8] {
        while self.idx < self.segs.len() && self.off == self.segs[self.idx].len() {
            self.idx += 1;
            self.off = 0;
        }
        if self.idx == self.segs.len() {
            return &[];
        }
        &self.segs[self.idx][self.off..]
    }

    fn advance(&mut self, n: usize) {
        self.off += n;
    }
}

/// Hooks that stage the device states a test cannot reach by timing alone.
#[cfg(feature = "test-hooks")]
pub mod hooks {
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::{BlkError, Engine, Namespace, Quarantined, RequestPages, SlotState};
    use slopos_abi::task::INVALID_TASK_ID;

    /// No transport issues a tag this high.
    const STAGED_TAG_BASE: u16 = 0xFFF0;

    static KILL_AFTER_SUBMIT: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);
    static RMW_READ_PAST_FENCE: AtomicBool = AtomicBool::new(false);

    /// Mark the calling task killed once its next request is in the device.
    pub fn kill_after_next_submit() {
        KILL_AFTER_SUBMIT.store(slopos_arch::pcr::current_task_id(), Ordering::Release);
    }

    pub(super) fn kill_if_armed() {
        let me = slopos_arch::pcr::current_task_id();
        if me != INVALID_TASK_ID
            && KILL_AFTER_SUBMIT
                .compare_exchange(me, INVALID_TASK_ID, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            slopos_core::tests::helpers::mark_current_killed(true);
        }
    }

    pub(super) fn note_rmw_read(past_fence: bool) {
        if past_fence {
            RMW_READ_PAST_FENCE.store(true, Ordering::Release);
        }
    }

    /// Whether a read-modify-write has read a block while an abandoned write
    /// was still owed to its namespace, since the last call.
    pub fn take_rmw_read_past_fence() -> bool {
        RMW_READ_PAST_FENCE.swap(false, Ordering::AcqRel)
    }

    impl Engine {
        /// Slots no quarantine withholds. Unlike the free count, other
        /// requests in flight do not move it.
        pub fn slots_in_service(&self) -> usize {
            self.state.lock().slots[..self.slot_count]
                .iter()
                .filter(|s| s.state != SlotState::Quarantined)
                .count()
        }

        pub fn quarantine_count(&self) -> usize {
            self.state.lock().quarantine_count()
        }

        /// Quarantine a write to `ns` the device never saw, the state a
        /// timeout leaves behind; the answer names it to
        /// [`Self::return_abandoned_write`].
        pub fn stage_abandoned_write(&self, ns: Namespace) -> Option<u16> {
            let pages = RequestPages::allocate(1)?;
            let mut state = self.state.lock();
            let Some(free) = state.quarantine.iter().position(Option::is_none) else {
                drop(state);
                drop(pages);
                return None;
            };
            state.quarantine[free] = Some(Quarantined {
                tag: STAGED_TAG_BASE + free as u16,
                writes: Some(ns.nsid),
                done: false,
                pages,
            });
            self.write_abandoned.store(true, Ordering::Release);
            Some(STAGED_TAG_BASE + free as u16)
        }

        /// The device returns the write [`Self::stage_abandoned_write`]
        /// staged.
        pub fn return_abandoned_write(&self, tag: u16) {
            if let Some(entry) = self
                .state
                .lock()
                .quarantine
                .iter_mut()
                .flatten()
                .find(|q| q.tag == tag)
            {
                entry.done = true;
            }
            self.quarantine_dirty.store(true, Ordering::Release);
            let _ = self.abandon_waiters.wake_all();
        }

        /// Read `dst.len()` bytes at `offset`, driving the completion into the
        /// timeout epilogue's replacement-allocation window and leaving the
        /// slot as the epilogue left it. A real timeout race cannot be staged
        /// against a device that answers in microseconds, so this enters the
        /// epilogue where the race leaves it, with a request the device really
        /// finished.
        pub fn read_completing_in_replacement_window(
            &self,
            ns: Namespace,
            offset: u64,
            dst: &mut [u8],
        ) -> Result<(), BlkError> {
            let idx = self.submit_read(ns, offset, dst.len())?;
            let harvested = || {
                let (done, harvest) = {
                    let mut state = self.state.lock();
                    let harvest = state.harvest();
                    (state.slots[idx].state == SlotState::Complete, harvest)
                };
                self.publish_harvest(harvest, idx);
                done
            };
            let completed =
                match self.slot_waiters[idx].wait_event_timeout(harvested, self.timeout_ms) {
                    Ok(()) => true,
                    Err(slopos_ostd::sync::WaitAbort::NoRuntime) => {
                        super::poll_wait(&harvested, self.timeout_ms as u32)
                    }
                    Err(_) => false,
                };
            if !completed {
                return Err(BlkError::Timeout);
            }
            let done = self.quarantine(idx, super::GaveUp::Deadline)?;
            self.conclude(idx, done, &mut |pages| pages.copy_out(0, dst))
        }
    }
}
