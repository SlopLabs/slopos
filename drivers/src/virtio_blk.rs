use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_ostd::lock_class;

use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, BlockDeviceIndex, stats};
use slopos_fs::partition::{PartitionDevice, SharedBlockDevice, probe};
use slopos_ostd::KArc;
use slopos_ostd::KBox;
use slopos_ostd::KVec;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::WaitAbort;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, SpinLock, WaitQueue};
use slopos_ostd::{klog_debug, klog_info, write_array_field, write_field, write_init_field};

use crate::pci::BoundDevice;
use crate::pci::{PciMatch, PciProbeError, ProbeOutcome};
use crate::virtio::{
    self, VIRTIO_MSI_NO_VECTOR, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE, VirtioMmioCaps,
    VirtioMsixState,
    pci::{
        PCI_VENDOR_ID_VIRTIO, enable_bus_master, negotiate_features, parse_capabilities,
        set_driver_ok, setup_interrupts,
    },
    queue::{self, DEFAULT_QUEUE_SIZE, VirtqDesc, Virtqueue},
};

use slopos_mm::page_alloc::OwnedPageFrame;

pub const VIRTIO_BLK_DEVICE_ID_LEGACY: u16 = 0x1001;
pub const VIRTIO_BLK_DEVICE_ID_MODERN: u16 = 0x1042;

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
/// Valid only once `VIRTIO_BLK_F_FLUSH` has been negotiated.
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;
/// Sentinel written before submission: reading back as this means the device
/// never wrote a status byte at all.
const STATUS_PENDING: u8 = 0xFF;

/// Device feature bit 9: the device has a write-back cache and honours
/// `VIRTIO_BLK_T_FLUSH`.
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const SECTOR_SIZE: u64 = 512;
const PAGE_SIZE: usize = 4096;
const REQUEST_TIMEOUT_MS: u32 = 5000;
const SLOT_WAIT_MS: u64 = 250;
/// Attempts per logical request, including the first.
const REQUEST_ATTEMPTS: u32 = 3;

/// Bounce pages per chain: 8 × 4 KiB = 32 KiB of payload behind one
/// submission and one completion.
const MAX_DATA_PAGES: usize = 8;
const MAX_XFER: usize = MAX_DATA_PAGES * PAGE_SIZE;
/// header + payload + status.
const MAX_CHAIN_DESCS: usize = 2 + MAX_DATA_PAGES;
const STATUS_OFFSET: usize = size_of::<VirtioBlkReqHeader>();

/// Ring budget: `DEFAULT_QUEUE_SIZE` is 64 and shared with virtio-net and
/// virtio-gpu, so it is not raised here. A maximal chain is
/// `1 + MAX_DATA_PAGES + 1` = 10 descriptors, and `NUM_REQUEST_SLOTS * 10 = 40`
/// of 64 leaves 24 — two more full chains — for chains a timeout quarantined
/// and the device has not yet returned.
const NUM_REQUEST_SLOTS: usize = 4;
const QUARANTINE_SLOTS: usize = 2;

const _: () = assert!(
    NUM_REQUEST_SLOTS * MAX_CHAIN_DESCS + QUARANTINE_SLOTS * MAX_CHAIN_DESCS
        <= DEFAULT_QUEUE_SIZE as usize
);

#[repr(C)]
#[derive(Clone, Copy, slopos_ostd::Pod)]
struct VirtioBlkReqHeader {
    type_: u32,
    reserved: u32,
    sector: u64,
}

/// Why one request failed, before it is mapped onto [`BlockDeviceError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlkError {
    NotReady,
    OutOfBounds,
    BadRequest,
    Busy,
    Timeout,
    /// `retry` is set when the device left the status byte untouched — a lost
    /// or truncated completion, unlike a reported media error.
    DeviceFault {
        retry: bool,
    },
    Unsupported,
    OutOfMemory,
}

impl BlkError {
    fn retryable(self) -> bool {
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
            BlkError::NotReady | BlkError::DeviceFault { .. } => BlockDeviceError::DeviceFault,
            BlkError::Unsupported => BlockDeviceError::Unsupported,
            BlkError::OutOfMemory => BlockDeviceError::OutOfMemory,
        }
    }
}

fn status_error(status: u8) -> BlkError {
    match status {
        VIRTIO_BLK_S_UNSUPP => BlkError::Unsupported,
        STATUS_PENDING => BlkError::DeviceFault { retry: true },
        _ => BlkError::DeviceFault { retry: false },
    }
}

/// One chain's DMA memory: the request page (header at 0, status byte at
/// [`STATUS_OFFSET`]) and the bounce pages its payload is staged through.
/// Allocated once per slot at probe, so no steady-state request allocates.
struct RequestPages {
    req: OwnedPageFrame,
    data: KVec<OwnedPageFrame>,
}

impl RequestPages {
    fn allocate() -> Option<Self> {
        let req = OwnedPageFrame::alloc_zeroed()?;
        let mut data = KVec::with_capacity(MAX_DATA_PAGES).ok()?;
        for _ in 0..MAX_DATA_PAGES {
            data.push(OwnedPageFrame::alloc_zeroed()?).ok()?;
        }
        Some(Self { req, data })
    }

    fn write_header(&self, type_: u32, sector: u64) -> bool {
        let header = VirtioBlkReqHeader {
            type_,
            reserved: 0,
            sector,
        };
        self.req.write_at::<VirtioBlkReqHeader>(0, &header)
            && self
                .req
                .write_volatile_at::<u8>(STATUS_OFFSET, STATUS_PENDING)
    }

    fn status(&self) -> u8 {
        self.req
            .read_volatile_at::<u8>(STATUS_OFFSET)
            .unwrap_or(STATUS_PENDING)
    }
}

/// Lifecycle of one request slot.
///
/// `Held` means a caller owns the slot's pages and the device holds no
/// descriptor pointing at them. `Quarantined` is the fallback for a timeout
/// that could not move the chain into the quarantine list: the device may
/// still write into those pages, so neither they nor the descriptors may be
/// reused.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Held,
    InFlight,
    Complete,
    Quarantined,
    QuarantineDone,
}

struct RequestSlot {
    state: SlotState,
    /// Head descriptor index — the `id` the device echoes in its used-ring
    /// entry.
    head: u16,
    descs: [u16; MAX_CHAIN_DESCS],
    desc_count: u8,
    /// `None` exactly while a caller holds them, or while a post-timeout
    /// replacement set has not landed yet.
    pages: Option<RequestPages>,
}

impl RequestSlot {
    const EMPTY: RequestSlot = RequestSlot {
        state: SlotState::Free,
        head: 0,
        descs: [0; MAX_CHAIN_DESCS],
        desc_count: 0,
        pages: None,
    };

    fn available(&self) -> bool {
        self.state == SlotState::Free && self.pages.is_some()
    }
}

/// A chain a timeout gave up on, moved off its slot so the slot keeps serving.
/// The device still owns `descs` and may still write into `pages`, so both are
/// held until its completion is harvested.
struct Quarantined {
    head: u16,
    descs: [u16; MAX_CHAIN_DESCS],
    desc_count: u8,
    done: bool,
    pages: RequestPages,
}

/// What one harvest pass changed, so the wakes can happen with the state lock
/// released.
#[derive(Clone, Copy)]
struct Harvest {
    /// Bit *i* set: slot *i* moved to `Complete`.
    completed: u32,
    reapable: bool,
}

/// What the timeout epilogue did with the slot it gave up waiting on.
enum SlotOutcome {
    /// Chain moved into the quarantine list, slot took the replacement page
    /// set and serves again.
    Recycled,
    /// Chain stays on its slot, which serves nothing until the device returns
    /// it.
    Withheld,
    /// The device completed the chain while the replacement was allocated with
    /// the lock released, so it is the caller's after all.
    Completed(RequestPages),
}

#[derive(slopos_ostd::SlotFields)]
struct VirtioBlkState {
    queue: Virtqueue,
    caps: VirtioMmioCaps,
    msix_state: Option<VirtioMsixState>,
    slots: [RequestSlot; NUM_REQUEST_SLOTS],
    quarantine: [Option<Quarantined>; QUARANTINE_SLOTS],
}

impl VirtioBlkState {
    /// Written field by field into the heap slot so the aggregate never lands
    /// on the prober's stack (the 2 KiB frame gate).
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, queue, Virtqueue::new());
                write_field!(slot, caps, VirtioMmioCaps::empty());
                write_field!(slot, msix_state, None);
                write_array_field!(slot, slots, NUM_REQUEST_SLOTS, |_| RequestSlot::EMPTY);
                write_array_field!(slot, quarantine, QUARANTINE_SLOTS, |_| None);
                Ok(slot.finish())
            },
        )
    }

    /// Match each pending used-ring entry to its chain by head id; an unknown
    /// id is dropped rather than attributed to whatever request is waiting.
    /// Runs in IRQ context under the state `SpinLock`.
    fn harvest_used(&mut self) -> Harvest {
        let mut out = Harvest {
            completed: 0,
            reapable: false,
        };
        while let Some(elem) = self.queue.try_pop_used() {
            let head = elem.id as u16;
            let slot = self.slots.iter().position(|s| {
                s.head == head && matches!(s.state, SlotState::InFlight | SlotState::Quarantined)
            });
            if let Some(i) = slot {
                if self.slots[i].state == SlotState::InFlight {
                    self.slots[i].state = SlotState::Complete;
                    out.completed |= 1 << i;
                } else {
                    self.slots[i].state = SlotState::QuarantineDone;
                    out.reapable = true;
                }
                continue;
            }
            if let Some(q) = self
                .quarantine
                .iter_mut()
                .flatten()
                .find(|q| q.head == head)
            {
                q.done = true;
                out.reapable = true;
            }
        }
        out
    }

    fn has_available_slot(&self) -> bool {
        self.slots.iter().any(RequestSlot::available)
    }

    fn take_available_slot(&mut self) -> Option<(usize, RequestPages)> {
        let i = self.slots.iter().position(RequestSlot::available)?;
        let pages = self.slots[i].pages.take()?;
        self.slots[i].state = SlotState::Held;
        Some((i, pages))
    }

    /// Reserve a slot whose post-timeout replacement allocation has not landed
    /// yet, so the (unlocked) allocation cannot race a second caller.
    fn reserve_pageless_slot(&mut self) -> Option<usize> {
        let i = self
            .slots
            .iter()
            .position(|s| s.state == SlotState::Free && s.pages.is_none())?;
        self.slots[i].state = SlotState::Held;
        Some(i)
    }

    /// All-or-nothing: a partial reservation is handed back, so a failure
    /// leaks no descriptors.
    fn alloc_chain(&mut self, count: usize) -> Option<[u16; MAX_CHAIN_DESCS]> {
        let mut descs = [0u16; MAX_CHAIN_DESCS];
        for i in 0..count {
            match self.queue.alloc_desc() {
                Some(desc) => descs[i] = desc,
                None => {
                    for &desc in &descs[..i] {
                        self.queue.free_desc(desc);
                    }
                    return None;
                }
            }
        }
        Some(descs)
    }

    /// `pages` moves into a field that is already `None`, so no frame is
    /// dropped under the lock.
    fn put_slot(&mut self, idx: usize, pages: Option<RequestPages>) {
        self.slots[idx].pages = pages;
        self.slots[idx].state = SlotState::Free;
    }

    fn take_complete(&mut self, idx: usize) -> Option<RequestPages> {
        if self.slots[idx].state != SlotState::Complete {
            return None;
        }
        self.free_slot_descs(idx);
        let pages = self.slots[idx].pages.take();
        self.slots[idx].state = SlotState::Held;
        pages
    }

    fn free_slot_descs(&mut self, idx: usize) {
        for i in 0..self.slots[idx].desc_count as usize {
            let desc = self.slots[idx].descs[i];
            self.queue.free_desc(desc);
        }
        self.slots[idx].desc_count = 0;
    }

    /// Move a timed-out chain off its slot. `replacement` is the fresh page
    /// set that puts the slot back in service; the one it could not use comes
    /// back for the caller to drop with the lock released, since a buddy free
    /// must never run under it.
    ///
    /// `replacement` is allocated with the lock released, so an IRQ harvest
    /// can flip the slot to `Complete` in that window. That completion is
    /// taken here: a slot quarantined out of `Complete` is one neither
    /// [`RequestSlot::available`] nor [`Self::recycle_quarantined_slots`] ever
    /// looks at again.
    fn quarantine_slot(
        &mut self,
        idx: usize,
        replacement: Option<RequestPages>,
    ) -> (SlotOutcome, Option<RequestPages>) {
        let slot_state = self.slots[idx].state;
        match slot_state {
            SlotState::Complete => match self.take_complete(idx) {
                Some(pages) => (SlotOutcome::Completed(pages), replacement),
                // `Complete` is only entered from `InFlight`, which leaves the
                // pages on the slot, so there is nothing to hand over.
                None => {
                    self.put_slot(idx, replacement);
                    (SlotOutcome::Recycled, None)
                }
            },
            SlotState::InFlight => {
                let free = self.quarantine.iter().position(Option::is_none);
                match (free, replacement) {
                    (Some(free), Some(replacement)) => {
                        let Some(pages) = self.slots[idx].pages.take() else {
                            self.slots[idx].state = SlotState::Quarantined;
                            return (SlotOutcome::Withheld, Some(replacement));
                        };
                        self.quarantine[free] = Some(Quarantined {
                            head: self.slots[idx].head,
                            descs: self.slots[idx].descs,
                            desc_count: self.slots[idx].desc_count,
                            done: false,
                            pages,
                        });
                        self.slots[idx].desc_count = 0;
                        self.slots[idx].pages = Some(replacement);
                        self.slots[idx].state = SlotState::Free;
                        (SlotOutcome::Recycled, None)
                    }
                    (_, unused) => {
                        self.slots[idx].state = SlotState::Quarantined;
                        (SlotOutcome::Withheld, unused)
                    }
                }
            }
            // The epilogue only runs on the caller's own chain, which is
            // `InFlight` or `Complete`; no other state owns a chain to move.
            SlotState::Free
            | SlotState::Held
            | SlotState::Quarantined
            | SlotState::QuarantineDone => (SlotOutcome::Withheld, replacement),
        }
    }

    /// Frees the chain's descriptors; the pages go out for the caller to drop
    /// once the lock is released.
    fn take_reaped(&mut self) -> Option<RequestPages> {
        let i = self
            .quarantine
            .iter()
            .position(|q| q.as_ref().is_some_and(|q| q.done))?;
        let entry = self.quarantine[i].take()?;
        for &desc in &entry.descs[..entry.desc_count as usize] {
            self.queue.free_desc(desc);
        }
        Some(entry.pages)
    }

    /// Put slots the device has finally released back in service. Their pages
    /// were never handed out, so nothing is freed here.
    fn recycle_quarantined_slots(&mut self) -> bool {
        let mut any = false;
        for idx in 0..NUM_REQUEST_SLOTS {
            if self.slots[idx].state != SlotState::QuarantineDone {
                continue;
            }
            self.free_slot_descs(idx);
            self.slots[idx].state = SlotState::Free;
            any = true;
        }
        any
    }

    fn quarantine_count(&self) -> usize {
        self.quarantine.iter().flatten().count()
            + self
                .slots
                .iter()
                .filter(|s| matches!(s.state, SlotState::Quarantined | SlotState::QuarantineDone))
                .count()
    }

    fn all_slots_quarantined(&self) -> bool {
        self.slots
            .iter()
            .all(|s| matches!(s.state, SlotState::Quarantined | SlotState::QuarantineDone))
    }
}

/// Owned per-device state, heap-resident inside a [`KArc`] so its address is
/// stable: the per-device IRQ closure and the registry each hold a clone.
#[derive(slopos_ostd::SlotFields)]
struct VirtioBlkInner {
    /// The `SpinLock` disables IRQs while held, so the IRQ-side harvest never
    /// interleaves with a task-side submit/collect. Nothing that sleeps, frees
    /// a frame or emits a klog line runs under it.
    state: SpinLock<VirtioBlkState>,
    /// One queue per slot: a completion wakes the single caller waiting on
    /// that chain instead of every in-flight requester.
    slot_waiters: [WaitQueue; NUM_REQUEST_SLOTS],
    free_waiters: WaitQueue,
    /// Flagged by the harvest, consumed by the next task-context reap: the IRQ
    /// side must not free frames, and the steady state must not pay for the
    /// scan.
    quarantine_dirty: AtomicBool,
    /// Immutable after probe, so a bounds check costs no lock.
    capacity: AtomicU64,
    ready: AtomicBool,
    flush_supported: AtomicBool,
}

impl VirtioBlkInner {
    /// Built via [`KArc::try_init`], so nothing materialises on the caller's
    /// stack.
    fn init_empty() -> impl Init<Self, AllocError> {
        init_struct_with(
            |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_init_field!(
                    slot,
                    state,
                    SpinLock::init_with(
                        lock_class!("VirtioBlk.state", LOCK_LEVEL_RESOURCE),
                        VirtioBlkState::init_empty()
                    )
                )?;
                write_array_field!(slot, slot_waiters, NUM_REQUEST_SLOTS, |_| WaitQueue::new(
                    lock_class!("VirtioBlk.slot_waiters", LOCK_LEVEL_RESOURCE)
                ));
                write_field!(
                    slot,
                    free_waiters,
                    WaitQueue::new(lock_class!("VirtioBlk.free_waiters", LOCK_LEVEL_RESOURCE))
                );
                write_field!(slot, quarantine_dirty, AtomicBool::new(false));
                write_field!(slot, capacity, AtomicU64::new(0));
                write_field!(slot, ready, AtomicBool::new(false));
                write_field!(slot, flush_supported, AtomicBool::new(false));
                Ok(slot.finish())
            },
        )
    }

    fn handle_queue_irq(&self) {
        let harvest = {
            let mut state = self.state.lock();
            state.harvest_used()
        };
        self.publish_harvest(harvest, NUM_REQUEST_SLOTS);
    }

    /// Wake each slot the harvest completed except `skip` (the caller's own,
    /// which returns through its own predicate). Runs with the state lock
    /// released: a wait queue's lock must never nest inside it.
    fn publish_harvest(&self, harvest: Harvest, skip: usize) {
        for i in 0..NUM_REQUEST_SLOTS {
            if i != skip && harvest.completed & (1 << i) != 0 {
                let _ = self.slot_waiters[i].wake_one();
            }
        }
        if harvest.reapable {
            self.quarantine_dirty.store(true, Ordering::Release);
            let _ = self.free_waiters.wake_one();
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    fn check_span(&self, offset: u64, len: usize) -> Result<(), BlkError> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.capacity_bytes() => Ok(()),
            _ => Err(BlkError::OutOfBounds),
        }
    }

    #[cfg(feature = "test-hooks")]
    fn msix_state(&self) -> Option<VirtioMsixState> {
        self.state.lock().msix_state.clone()
    }

    /// Reclaim the descriptors and DMA pages of quarantined chains the device
    /// has finished with. The pages are dropped with the lock released — a
    /// buddy free must not run under the IRQ-off state spinlock.
    fn reap_quarantine(&self) {
        if !self.quarantine_dirty.swap(false, Ordering::Acquire) {
            return;
        }

        let recycled = {
            let mut state = self.state.lock();
            state.recycle_quarantined_slots()
        };
        loop {
            let reaped = {
                let mut state = self.state.lock();
                state.take_reaped()
            };
            if reaped.is_none() {
                break;
            }
        }
        if recycled {
            let _ = self.free_waiters.wake_one();
        }
    }

    /// Give a slot whose post-timeout replacement allocation failed another
    /// chance, with no lock held. `None`: no slot needed one.
    fn refill_slot(&self) -> Option<bool> {
        let idx = self.state.lock().reserve_pageless_slot()?;
        let pages = RequestPages::allocate();
        let refilled = pages.is_some();
        self.state.lock().put_slot(idx, pages);
        Some(refilled)
    }

    fn acquire_slot(&self) -> Result<(usize, RequestPages), BlkError> {
        if let Some(slot) = self.state.lock().take_available_slot() {
            return Ok(slot);
        }
        match self.refill_slot() {
            Some(false) => return Err(BlkError::OutOfMemory),
            Some(true) => {
                if let Some(slot) = self.state.lock().take_available_slot() {
                    return Ok(slot);
                }
            }
            None => {}
        }
        match self
            .free_waiters
            .wait_event_timeout(|| self.state.lock().has_available_slot(), SLOT_WAIT_MS)
        {
            Ok(()) => {}
            // Pre-scheduler context (probe / early boot): poll the release
            // under the HPET deadline.
            Err(WaitAbort::NoRuntime) => {
                virtio::hpet_poll_wait(
                    &|| self.state.lock().has_available_slot(),
                    SLOT_WAIT_MS as u32,
                );
            }
            Err(_) => return Err(BlkError::Busy),
        }
        self.state
            .lock()
            .take_available_slot()
            .ok_or(BlkError::Busy)
    }

    fn release_slot(&self, idx: usize, pages: RequestPages) {
        self.state.lock().put_slot(idx, Some(pages));
        let _ = self.free_waiters.wake_one();
    }

    /// Build and submit one `1 + N + 1` descriptor chain. On failure the pages
    /// come back, so the caller can release the slot.
    fn submit_chain(
        &self,
        slot_idx: usize,
        pages: RequestPages,
        type_: u32,
        len: usize,
    ) -> Result<(), (BlkError, RequestPages)> {
        let data_pages = len.div_ceil(PAGE_SIZE);
        let desc_count = 2 + data_pages;
        let device_writes = type_ == VIRTIO_BLK_T_IN;
        let req_phys = pages.req.phys_u64();
        let status_phys = req_phys + STATUS_OFFSET as u64;

        let mut state = self.state.lock();
        if !state.queue.is_ready() {
            drop(state);
            return Err((BlkError::NotReady, pages));
        }

        let Some(descs) = state.alloc_chain(desc_count) else {
            let held = state.quarantine_count();
            drop(state);
            klog_info!(
                "virtio-blk: descriptor ring exhausted, {} chain(s) quarantined",
                held
            );
            return Err((BlkError::Busy, pages));
        };

        state.queue.write_desc(
            descs[0],
            VirtqDesc {
                addr: req_phys,
                len: size_of::<VirtioBlkReqHeader>() as u32,
                flags: VIRTQ_DESC_F_NEXT,
                next: descs[1],
            },
        );
        for i in 0..data_pages {
            state.queue.write_desc(
                descs[1 + i],
                VirtqDesc {
                    addr: pages.data[i].phys_u64(),
                    len: (len - i * PAGE_SIZE).min(PAGE_SIZE) as u32,
                    flags: if device_writes {
                        VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
                    } else {
                        VIRTQ_DESC_F_NEXT
                    },
                    next: descs[2 + i],
                },
            );
        }
        state.queue.write_desc(
            descs[desc_count - 1],
            VirtqDesc {
                addr: status_phys,
                len: 1,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );

        state.slots[slot_idx].state = SlotState::InFlight;
        state.slots[slot_idx].head = descs[0];
        state.slots[slot_idx].descs = descs;
        state.slots[slot_idx].desc_count = desc_count as u8;
        state.slots[slot_idx].pages = Some(pages);

        state.queue.submit(descs[0]);
        queue::notify_queue(
            &state.caps.notify_cfg,
            state.caps.notify_off_multiplier,
            &state.queue,
            0,
        );
        Ok(())
    }

    /// Park on this slot's queue until its chain completes. The predicate
    /// re-harvests, so a lost interrupt is recovered on any wake.
    fn wait_for_completion(&self, slot_idx: usize) -> Result<RequestPages, BlkError> {
        let collect = || {
            let (pages, harvest) = {
                let mut state = self.state.lock();
                let harvest = state.harvest_used();
                (state.take_complete(slot_idx), harvest)
            };
            self.publish_harvest(harvest, slot_idx);
            pages
        };

        match self.slot_waiters[slot_idx]
            .wait_event_timeout_until(collect, REQUEST_TIMEOUT_MS as u64)
        {
            Ok(pages) => Ok(pages),
            // Pre-scheduler context (probe / early boot): poll the used ring
            // under the HPET deadline.
            Err(WaitAbort::NoRuntime) => {
                virtio::hpet_poll_wait(
                    &|| {
                        let mut state = self.state.lock();
                        state.harvest_used();
                        state.slots[slot_idx].state == SlotState::Complete
                    },
                    REQUEST_TIMEOUT_MS,
                );
                self.finish_or_quarantine(slot_idx)
            }
            // A killed or signalled requester must not free a chain the device
            // may still be writing into; quarantine it as a timeout does.
            Err(_) => self.finish_or_quarantine(slot_idx),
        }
    }

    /// Timeout epilogue: one final harvest (which recovers a completion whose
    /// interrupt was lost), else move the chain out of the slot's way.
    #[inline(never)]
    fn finish_or_quarantine(&self, slot_idx: usize) -> Result<RequestPages, BlkError> {
        if let Some(pages) = self.final_harvest(slot_idx) {
            return Ok(pages);
        }
        self.quarantine_after_timeout(slot_idx)
    }

    fn final_harvest(&self, slot_idx: usize) -> Option<RequestPages> {
        let (pages, harvest) = {
            let mut state = self.state.lock();
            let harvest = state.harvest_used();
            (state.take_complete(slot_idx), harvest)
        };
        self.publish_harvest(harvest, slot_idx);
        pages
    }

    #[inline(never)]
    fn quarantine_after_timeout(&self, slot_idx: usize) -> Result<RequestPages, BlkError> {
        // Allocated before the lock is taken: the slot's own pages stay with
        // the device, and a frame allocation must not run under the lock.
        let replacement = RequestPages::allocate();
        let had_replacement = replacement.is_some();

        let (outcome, unused, head, wedged) = {
            let mut state = self.state.lock();
            let (outcome, unused) = state.quarantine_slot(slot_idx, replacement);
            let head = state.slots[slot_idx].head;
            let wedged = state.all_slots_quarantined();
            (outcome, unused, head, wedged)
        };
        drop(unused);

        let recycled = match outcome {
            // The device answered inside the allocation window: an ordinary
            // completion, handed over as the wait's harvest would have.
            SlotOutcome::Completed(pages) => return Ok(pages),
            SlotOutcome::Recycled => true,
            SlotOutcome::Withheld => false,
        };

        klog_info!(
            "virtio-blk: request timeout, chain head {} quarantined, slot {} {}",
            head,
            slot_idx,
            if recycled { "recycled" } else { "withheld" }
        );
        if wedged {
            klog_info!(
                "virtio-blk: all {} request slots quarantined — the device is not completing requests",
                NUM_REQUEST_SLOTS
            );
        }

        if recycled {
            let _ = self.free_waiters.wake_one();
        }
        // A quarantine the replacement allocation could not cover costs the
        // slot until the device returns it, so the caller is told to retry
        // rather than that its request timed out.
        if had_replacement {
            Err(BlkError::Timeout)
        } else {
            Err(BlkError::Busy)
        }
    }

    /// One attempt at one chain. `fill` and `drain` run with the pages owned
    /// by this caller and no lock held.
    #[inline(never)]
    fn attempt(
        &self,
        sector: u64,
        type_: u32,
        len: usize,
        fill: &mut dyn FnMut(&RequestPages) -> bool,
        drain: &mut dyn FnMut(&RequestPages) -> bool,
    ) -> Result<(), BlkError> {
        self.reap_quarantine();

        let (idx, pages) = self.acquire_slot()?;

        if !pages.write_header(type_, sector) || !fill(&pages) {
            self.release_slot(idx, pages);
            return Err(BlkError::BadRequest);
        }

        if let Err((err, pages)) = self.submit_chain(idx, pages, type_, len) {
            self.release_slot(idx, pages);
            return Err(err);
        }

        let pages = self.wait_for_completion(idx)?;
        let status = pages.status();
        let drained = status == VIRTIO_BLK_S_OK && drain(&pages);
        self.release_slot(idx, pages);

        if status != VIRTIO_BLK_S_OK {
            return Err(status_error(status));
        }
        if !drained {
            return Err(BlkError::BadRequest);
        }
        Ok(())
    }

    fn run_request(
        &self,
        sector: u64,
        type_: u32,
        len: usize,
        fill: &mut dyn FnMut(&RequestPages) -> bool,
        drain: &mut dyn FnMut(&RequestPages) -> bool,
    ) -> Result<(), BlkError> {
        let mut last = BlkError::Busy;
        for _ in 0..REQUEST_ATTEMPTS {
            match self.attempt(sector, type_, len, fill, drain) {
                Ok(()) => return Ok(()),
                Err(err) if err.retryable() => last = err,
                Err(err) => return Err(err),
            }
        }
        Err(last)
    }

    /// The counter sits here rather than at the [`BlockDevice`] boundary: one
    /// filesystem call is `ceil(len / MAX_XFER)` chains plus a read+write pair
    /// per partial sector, and it is that traffic a regression multiplies.
    fn request_read(&self, sector: u64, dst: &mut [u8]) -> Result<(), BlkError> {
        let len = dst.len();
        stats::note_read(len);
        let mut fill = |_: &RequestPages| true;
        let mut drain = |pages: &RequestPages| unstage_read(pages, dst);
        self.run_request(sector, VIRTIO_BLK_T_IN, len, &mut fill, &mut drain)
    }

    fn request_write(
        &self,
        sector: u64,
        cur: &mut SegCursor<'_>,
        len: usize,
    ) -> Result<(), BlkError> {
        stats::note_write(len);
        // Snapshotted so a retry re-gathers exactly the same bytes.
        let start = *cur;
        let mut fill = |pages: &RequestPages| {
            *cur = start;
            stage_write(pages, cur, len)
        };
        let mut drain = |_: &RequestPages| true;
        self.run_request(sector, VIRTIO_BLK_T_OUT, len, &mut fill, &mut drain)
    }

    /// Partial head/tail sectors go through a stack staging buffer; the
    /// aligned middle transfers straight into `buffer` in [`MAX_XFER`] chains.
    fn read_span(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlkError> {
        if buffer.is_empty() {
            return Ok(());
        }
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        self.check_span(offset, buffer.len())?;

        let mut pos = 0usize;
        let mut at = offset;

        let head_within = (at % SECTOR_SIZE) as usize;
        if head_within != 0 {
            let n = (SECTOR_SIZE as usize - head_within).min(buffer.len());
            self.read_partial_sector(at / SECTOR_SIZE, head_within, &mut buffer[..n])?;
            pos += n;
            at += n as u64;
        }

        while buffer.len() - pos >= SECTOR_SIZE as usize {
            let whole = (buffer.len() - pos) / SECTOR_SIZE as usize * SECTOR_SIZE as usize;
            let n = whole.min(MAX_XFER);
            self.request_read(at / SECTOR_SIZE, &mut buffer[pos..pos + n])?;
            pos += n;
            at += n as u64;
        }

        if pos < buffer.len() {
            let end = buffer.len();
            self.read_partial_sector(at / SECTOR_SIZE, 0, &mut buffer[pos..end])?;
        }
        Ok(())
    }

    #[inline(never)]
    fn read_partial_sector(
        &self,
        sector: u64,
        within: usize,
        dst: &mut [u8],
    ) -> Result<(), BlkError> {
        let mut sector_buf = [0u8; SECTOR_SIZE as usize];
        self.request_read(sector, &mut sector_buf)?;
        dst.copy_from_slice(&sector_buf[within..within + dst.len()]);
        Ok(())
    }

    /// A partial head or tail sector is read-modify-written, so bytes outside
    /// the span are never clobbered; the aligned middle is gathered across
    /// segment boundaries into chains of up to [`MAX_XFER`] bytes.
    fn write_span(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlkError> {
        let total = total_seg_len(segs)?;
        if total == 0 {
            return Ok(());
        }
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        self.check_span(offset, total)?;

        let mut cur = SegCursor::new(segs);
        let mut at = offset;
        let mut left = total;

        let head_within = (at % SECTOR_SIZE) as usize;
        if head_within != 0 {
            let n = (SECTOR_SIZE as usize - head_within).min(left);
            self.rmw_sector(at / SECTOR_SIZE, head_within, n, &mut cur)?;
            at += n as u64;
            left -= n;
        }

        while left >= SECTOR_SIZE as usize {
            let n = (left - left % SECTOR_SIZE as usize).min(MAX_XFER);
            self.request_write(at / SECTOR_SIZE, &mut cur, n)?;
            at += n as u64;
            left -= n;
        }

        if left > 0 {
            self.rmw_sector(at / SECTOR_SIZE, 0, left, &mut cur)?;
        }
        Ok(())
    }

    #[inline(never)]
    fn rmw_sector(
        &self,
        sector: u64,
        within: usize,
        n: usize,
        cur: &mut SegCursor<'_>,
    ) -> Result<(), BlkError> {
        let mut sector_buf = [0u8; SECTOR_SIZE as usize];
        self.request_read(sector, &mut sector_buf)?;
        if !cur.copy_out(&mut sector_buf[within..within + n]) {
            return Err(BlkError::BadRequest);
        }
        let whole: [&[u8]; 1] = [&sector_buf];
        let mut back = SegCursor::new(&whole);
        self.request_write(sector, &mut back, SECTOR_SIZE as usize)
    }

    /// Block until the device acknowledges a `VIRTIO_BLK_T_FLUSH`. Without
    /// `VIRTIO_BLK_F_FLUSH` there is no volatile cache, so this is a
    /// successful no-op.
    fn do_flush(&self) -> Result<(), BlkError> {
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        if !self.flush_supported.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut fill = |_: &RequestPages| true;
        let mut drain = |_: &RequestPages| true;
        // The flush sector field must be zero per the virtio spec.
        self.run_request(0, VIRTIO_BLK_T_FLUSH, 0, &mut fill, &mut drain)
    }

    /// Submit a read and return its slot without parking, so one task can
    /// hold several chains in flight. Paired with
    /// [`complete_read`](Self::complete_read).
    #[cfg(feature = "test-hooks")]
    fn submit_read(&self, sector: u64, len: usize) -> Result<usize, BlkError> {
        if !self.is_ready() {
            return Err(BlkError::NotReady);
        }
        if len == 0 || len > MAX_XFER || !len.is_multiple_of(SECTOR_SIZE as usize) {
            return Err(BlkError::BadRequest);
        }
        self.check_span(sector * SECTOR_SIZE, len)?;

        let (idx, pages) = self.acquire_slot()?;
        if !pages.write_header(VIRTIO_BLK_T_IN, sector) {
            self.release_slot(idx, pages);
            return Err(BlkError::BadRequest);
        }
        if let Err((err, pages)) = self.submit_chain(idx, pages, VIRTIO_BLK_T_IN, len) {
            self.release_slot(idx, pages);
            return Err(err);
        }
        Ok(idx)
    }

    #[cfg(feature = "test-hooks")]
    fn complete_read(&self, idx: usize, dst: &mut [u8]) -> Result<(), BlkError> {
        let pages = self.wait_for_completion(idx)?;
        let status = pages.status();
        let drained = status == VIRTIO_BLK_S_OK && unstage_read(&pages, dst);
        self.release_slot(idx, pages);

        if status != VIRTIO_BLK_S_OK {
            return Err(status_error(status));
        }
        if !drained {
            return Err(BlkError::BadRequest);
        }
        Ok(())
    }

    /// Enter the timeout epilogue's second critical section with the device's
    /// completion already harvested — the state an IRQ landing inside the
    /// replacement-allocation window leaves behind. A real
    /// `REQUEST_TIMEOUT_MS` race cannot be staged against a device that
    /// answers in microseconds, so this enters the epilogue where the race
    /// leaves it, with a chain the device really finished.
    #[cfg(feature = "test-hooks")]
    fn read_completing_in_replacement_window(
        &self,
        sector: u64,
        dst: &mut [u8],
    ) -> Result<(), BlkError> {
        let idx = self.submit_read(sector, dst.len())?;
        let harvested = || {
            let (done, harvest) = {
                let mut state = self.state.lock();
                let harvest = state.harvest_used();
                (state.slots[idx].state == SlotState::Complete, harvest)
            };
            self.publish_harvest(harvest, idx);
            done
        };
        let completed =
            match self.slot_waiters[idx].wait_event_timeout(harvested, REQUEST_TIMEOUT_MS as u64) {
                Ok(()) => true,
                Err(WaitAbort::NoRuntime) => virtio::hpet_poll_wait(&harvested, REQUEST_TIMEOUT_MS),
                Err(_) => false,
            };
        if !completed {
            return Err(BlkError::Timeout);
        }

        let pages = self.quarantine_after_timeout(idx)?;
        let status = pages.status();
        let drained = status == VIRTIO_BLK_S_OK && unstage_read(&pages, dst);
        self.release_slot(idx, pages);

        if status != VIRTIO_BLK_S_OK {
            return Err(status_error(status));
        }
        if !drained {
            return Err(BlkError::BadRequest);
        }
        Ok(())
    }

    /// Slots ready to take a new chain; one the epilogue stranded is not.
    #[cfg(feature = "test-hooks")]
    fn available_slots(&self) -> usize {
        self.state
            .lock()
            .slots
            .iter()
            .filter(|s| s.available())
            .count()
    }
}

/// Cursor over a vectored write's segments, so one chain can gather bytes that
/// cross a segment boundary.
#[derive(Clone, Copy)]
struct SegCursor<'a> {
    segs: &'a [&'a [u8]],
    idx: usize,
    off: usize,
}

impl<'a> SegCursor<'a> {
    fn new(segs: &'a [&'a [u8]]) -> Self {
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

    /// Copy the next `dst.len()` bytes out; `false` if the cursor runs dry.
    fn copy_out(&mut self, dst: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < dst.len() {
            let frag = self.peek();
            if frag.is_empty() {
                return false;
            }
            let n = frag.len().min(dst.len() - done);
            dst[done..done + n].copy_from_slice(&frag[..n]);
            self.advance(n);
            done += n;
        }
        true
    }
}

fn total_seg_len(segs: &[&[u8]]) -> Result<usize, BlkError> {
    let mut total = 0usize;
    for seg in segs {
        total = total.checked_add(seg.len()).ok_or(BlkError::OutOfBounds)?;
    }
    Ok(total)
}

fn stage_write(pages: &RequestPages, cur: &mut SegCursor<'_>, len: usize) -> bool {
    let mut pos = 0usize;
    while pos < len {
        let frag = cur.peek();
        if frag.is_empty() {
            return false;
        }
        let within = pos % PAGE_SIZE;
        let n = frag.len().min(PAGE_SIZE - within).min(len - pos);
        if !pages.data[pos / PAGE_SIZE].write_slice(within, &frag[..n]) {
            return false;
        }
        cur.advance(n);
        pos += n;
    }
    true
}

fn unstage_read(pages: &RequestPages, dst: &mut [u8]) -> bool {
    let mut pos = 0usize;
    while pos < dst.len() {
        let within = pos % PAGE_SIZE;
        let n = (PAGE_SIZE - within).min(dst.len() - pos);
        if !pages.data[pos / PAGE_SIZE].read_slice(within, &mut dst[pos..pos + n]) {
            return false;
        }
        pos += n;
    }
    true
}

const MAX_BLK_DEVICES: usize = 8;

/// Generation-checked handle over the registry's [`DevState`] slots: a stale
/// handle fails validation instead of aliasing a different device.
pub type DevHandle = Handle<DevState>;

/// Error from acquiring an exclusive write capability on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkClaimError {
    /// The handle does not refer to a live device (freed/never existed).
    Stale,
    AlreadyClaimed,
}

pub struct DevState {
    inner: KArc<VirtioBlkInner>,
    /// Stable probe-order index (disk0 = first probed); devices are never
    /// removed, so it equals the registration position.
    index: u16,
    write_claimed: bool,
}

static BLK_REGISTRY: SpinLock<Option<HandleTable<DevState>>> =
    SpinLock::new(None, lock_class!("BLK_REGISTRY", LOCK_LEVEL_REGISTRY));

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<DevState>) -> R) -> R {
    let mut guard = BLK_REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_BLK_DEVICES).expect("blk registry alloc")
    });
    f(table)
}

/// Assigns the next probe-order index — devices are never removed, so the live
/// count is the next index. Called with the PCI `ENUM_STATE` lock held; the only
/// nesting is `ENUM_STATE -> BLK_REGISTRY`, never the reverse.
fn register_device(inner: KArc<VirtioBlkInner>) -> Option<BlockDeviceIndex> {
    with_registry(|t| {
        let index = t.len() as u16;
        t.insert(DevState {
            inner,
            index,
            write_claimed: false,
        })
        .ok()
        .map(|_| BlockDeviceIndex(index))
    })
}

/// Clones the state out from under the registry lock, so the (potentially
/// blocking) I/O path never runs while holding the registry.
fn clone_inner(handle: DevHandle) -> Option<KArc<VirtioBlkInner>> {
    with_registry(|t| t.get(handle).map(|s| s.inner.clone()).ok())
}

pub fn blk_device_count() -> usize {
    with_registry(|t| t.len())
}

pub fn blk_device_by_index(index: BlockDeviceIndex) -> Option<DevHandle> {
    with_registry(|t| t.iter().find(|(_, s)| s.index == index.0).map(|(h, _)| h))
}

pub fn blk_read(handle: DevHandle, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
    let Some(inner) = clone_inner(handle) else {
        return Err(BlockDeviceError::DeviceFault);
    };
    inner.read_span(offset, buffer).map_err(Into::into)
}

pub fn blk_is_ready(handle: DevHandle) -> bool {
    clone_inner(handle).is_some_and(|inner| inner.is_ready())
}

/// Device capacity in bytes (0 if the handle is stale).
pub fn blk_capacity(handle: DevHandle) -> u64 {
    clone_inner(handle).map_or(0, |inner| inner.capacity_bytes())
}

#[cfg(feature = "test-hooks")]
pub fn blk_msix_state(handle: DevHandle) -> Option<VirtioMsixState> {
    clone_inner(handle).and_then(|inner| inner.msix_state())
}

/// A read the device is still working on, occupying one request slot. The
/// production path submits and parks in one call, so only a test needs to
/// name a slot.
#[cfg(feature = "test-hooks")]
pub struct InFlightRead {
    inner: KArc<VirtioBlkInner>,
    slot: usize,
}

#[cfg(feature = "test-hooks")]
impl InFlightRead {
    /// Park until this chain completes and copy its payload into `dst`, whose
    /// length must be the `len` the read was submitted with.
    pub fn complete(self, dst: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.complete_read(self.slot, dst).map_err(Into::into)
    }
}

/// Submit a sector-aligned read of `len` bytes without waiting for it.
#[cfg(feature = "test-hooks")]
pub fn blk_submit_read(
    handle: DevHandle,
    sector: u64,
    len: usize,
) -> Result<InFlightRead, BlockDeviceError> {
    let Some(inner) = clone_inner(handle) else {
        return Err(BlockDeviceError::DeviceFault);
    };
    let slot = inner.submit_read(sector, len)?;
    Ok(InFlightRead { inner, slot })
}

/// Read `dst.len()` bytes from `sector`, driving the completion into the
/// epilogue's replacement-allocation window and leaving the slot as the
/// epilogue left it.
#[cfg(feature = "test-hooks")]
pub fn blk_read_completing_in_replacement_window(
    handle: DevHandle,
    sector: u64,
    dst: &mut [u8],
) -> Result<(), BlockDeviceError> {
    let Some(inner) = clone_inner(handle) else {
        return Err(BlockDeviceError::DeviceFault);
    };
    inner
        .read_completing_in_replacement_window(sector, dst)
        .map_err(Into::into)
}

/// How many of the device's request slots can take a new chain.
#[cfg(feature = "test-hooks")]
pub fn blk_available_slots(handle: DevHandle) -> usize {
    clone_inner(handle).map_or(0, |inner| inner.available_slots())
}

/// Acquire the exclusive write capability. A second `open_writer` on the same
/// device returns [`BlkClaimError::AlreadyClaimed`] until the first token is
/// dropped.
pub fn open_writer(handle: DevHandle) -> Result<BlockWriteToken, BlkClaimError> {
    with_registry(|t| match t.get_mut(handle) {
        Ok(s) if s.write_claimed => Err(BlkClaimError::AlreadyClaimed),
        Ok(s) => {
            let inner = s.inner.clone();
            s.write_claimed = true;
            Ok(BlockWriteToken { handle, inner })
        }
        Err(_) => Err(BlkClaimError::Stale),
    })
}

/// Owned, exclusive read+write capability; dropping it releases the claim.
pub struct BlockWriteToken {
    handle: DevHandle,
    inner: KArc<VirtioBlkInner>,
}

impl BlockDevice for BlockWriteToken {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_span(offset, buffer).map_err(Into::into)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.inner.write_span(offset, &[buffer]).map_err(Into::into)
    }

    /// Gathered into one chain, so a run of contiguous kernel buffers costs
    /// one device request per 32 KiB rather than one per buffer.
    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.inner.write_span(offset, segs).map_err(Into::into)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity_bytes()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        stats::note_flush();
        self.inner.do_flush().map_err(Into::into)
    }
}

impl Drop for BlockWriteToken {
    fn drop(&mut self) {
        with_registry(|t| {
            if let Ok(s) = t.get_mut(self.handle) {
                s.write_claimed = false;
            }
        });
    }
}

/// Everything a mount needs from one named device.
pub struct ClaimedBlockDevice {
    /// The whole-device write claim, shared so a `/dev` node can name the same
    /// claim rather than opening a second view of it.
    pub whole: KArc<dyn BlockDevice + Send + Sync>,
    /// The window a filesystem mounts: the whole device, or the partition the
    /// name's suffix selected. Holds its own reference to `whole`, so the
    /// claim outlives nothing but this handle.
    pub window: KBox<dyn BlockDevice + Send + Sync>,
}

/// Why a named device could not be opened for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlkOpenError {
    /// Not a `vd<letter>[<partition>]` name.
    BadName,
    /// No virtio-blk device at that probe-order index.
    NoDevice,
    NotReady,
    Claim(BlkClaimError),
    NoMemory,
    /// The device's partition table could not be read.
    PartitionTable,
    /// The table has no such partition, or its window is unusable.
    NoSuchPartition,
}

/// `vd<letter>[<partition>]`, with or without a leading `/dev/`. Answers the
/// probe-order index the letter names (`vda` = disk0) and a 1-based
/// partition, 0 for the whole device.
pub fn parse_block_device_name(name: &[u8]) -> Option<(BlockDeviceIndex, u8)> {
    let bare = name.strip_prefix(b"/dev/").unwrap_or(name);
    let rest = bare.strip_prefix(b"vd")?;
    let (&letter, digits) = rest.split_first()?;
    if !letter.is_ascii_lowercase() {
        return None;
    }
    let index = BlockDeviceIndex(u16::from(letter - b'a'));
    if digits.is_empty() {
        return Some((index, 0));
    }
    let mut partition: u32 = 0;
    for &digit in digits {
        if !digit.is_ascii_digit() {
            return None;
        }
        partition = partition * 10 + u32::from(digit - b'0');
        if partition > u32::from(u8::MAX) {
            return None;
        }
    }
    // `vda0` is not a spelling of anything: partition numbers are 1-based.
    if partition == 0 {
        return None;
    }
    Some((index, partition as u8))
}

/// Claim the exclusive write capability on the device `name` selects, windowed
/// to the partition its suffix asks for. The claim releases when the returned
/// window is dropped.
pub fn claim_writer_by_name(name: &[u8]) -> Result<ClaimedBlockDevice, BlkOpenError> {
    let (index, partition) = parse_block_device_name(name).ok_or(BlkOpenError::BadName)?;
    claim_writer_at(index, partition)
}

/// [`claim_writer_by_name`] against an already-parsed index and partition —
/// the `root=` knob, say.
pub fn claim_writer_at(
    index: BlockDeviceIndex,
    partition: u8,
) -> Result<ClaimedBlockDevice, BlkOpenError> {
    let handle = blk_device_by_index(index).ok_or(BlkOpenError::NoDevice)?;
    if !blk_is_ready(handle) {
        return Err(BlkOpenError::NotReady);
    }
    let token = open_writer(handle).map_err(BlkOpenError::Claim)?;
    let owned = KArc::try_new(token).map_err(|_| BlkOpenError::NoMemory)?;
    let whole: KArc<dyn BlockDevice + Send + Sync> = owned;
    let window = partition_window(&whole, partition)?;
    Ok(ClaimedBlockDevice { whole, window })
}

fn partition_window(
    whole: &KArc<dyn BlockDevice + Send + Sync>,
    partition: u8,
) -> Result<KBox<dyn BlockDevice + Send + Sync>, BlkOpenError> {
    if partition == 0 {
        let boxed =
            KBox::try_new(SharedBlockDevice(whole.clone())).map_err(|_| BlkOpenError::NoMemory)?;
        return Ok(boxed);
    }
    let table = probe(whole.as_ref()).map_err(|_| BlkOpenError::PartitionTable)?;
    let entry = *table.find(partition).ok_or(BlkOpenError::NoSuchPartition)?;
    let window = PartitionDevice::try_new(whole.clone(), entry.start, entry.len)
        .map_err(|_| BlkOpenError::NoSuchPartition)?;
    let boxed = KBox::try_new(window).map_err(|_| BlkOpenError::NoMemory)?;
    Ok(boxed)
}

/// Read-only view of a registered device: it takes no write claim, so it can
/// back a `/dev/vd*` node without competing with a mount's
/// [`BlockWriteToken`].
pub struct BlockReader {
    handle: DevHandle,
}

impl BlockReader {
    pub const fn new(handle: DevHandle) -> Self {
        Self { handle }
    }
}

impl BlockDevice for BlockReader {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        blk_read(self.handle, offset, buffer)
    }

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::WriteProtected)
    }

    fn capacity(&self) -> u64 {
        blk_capacity(self.handle)
    }

    fn write_protected(&self) -> bool {
        true
    }
}

fn read_capacity(caps: &VirtioMmioCaps) -> u64 {
    if !caps.has_device_cfg() {
        return 0;
    }
    let lo = caps.device_cfg.read::<u32>(0) as u64;
    let hi = caps.device_cfg.read::<u32>(4) as u64;
    lo | (hi << 32)
}

/// Preallocate every slot's request page and bounce pages, so the steady-state
/// request path never touches the frame allocator.
fn prime_request_slots(inner: &VirtioBlkInner) -> bool {
    for idx in 0..NUM_REQUEST_SLOTS {
        let Some(pages) = RequestPages::allocate() else {
            return false;
        };
        inner.state.lock().put_slot(idx, Some(pages));
    }
    true
}

fn virtio_blk_probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    klog_info!(
        "virtio-blk: probing {:04x}:{:04x} at {:02x}:{:02x}.{}",
        info.vendor_id,
        info.device_id,
        info.bus,
        info.device,
        info.function
    );

    enable_bus_master(&info);

    let caps = parse_capabilities(&info);

    klog_debug!(
        "virtio-blk: caps common={} notify={} device={}",
        caps.has_common_cfg(),
        caps.has_notify_cfg(),
        caps.has_device_cfg()
    );

    if !caps.has_common_cfg() {
        klog_info!("virtio-blk: missing common cfg");
        return Err(PciProbeError::Unsupported);
    }

    let feat_result = negotiate_features(&caps, virtio::VIRTIO_F_VERSION_1, VIRTIO_BLK_F_FLUSH);
    if !feat_result.success {
        klog_info!("virtio-blk: features negotiation failed");
        return Err(PciProbeError::DeviceFault);
    }
    let flush_supported = feat_result.driver_features & VIRTIO_BLK_F_FLUSH != 0;

    // Allocated up front so the IRQ closure can capture a clone of it.
    let inner = match KArc::try_init(VirtioBlkInner::init_empty()) {
        Ok(i) => i,
        Err(_) => return Err(PciProbeError::OutOfMemory),
    };

    // VirtIO modern on q35 always has MSI-X; MSI is the minimum fallback.
    let inner_for_irq = inner.clone();
    let (irq_mode, msix_state) = setup_interrupts(bound, &caps, 1, move |_q: u8| {
        inner_for_irq.handle_queue_irq();
    })
    .unwrap_or_else(|msg| {
        panic!(
            "virtio-blk: {}:{}.{} {}",
            info.bus, info.device, info.function, msg
        )
    });
    let q0_msix_entry = msix_state
        .as_ref()
        .map_or(VIRTIO_MSI_NO_VECTOR, |s| s.queue_msix_entry(0));

    if !prime_request_slots(&inner) {
        klog_info!("virtio-blk: could not preallocate request slot DMA pages");
        return Err(PciProbeError::OutOfMemory);
    }

    let capacity_sectors;
    let ring_size;
    {
        // Set up in place so the ~200-byte `Virtqueue` never lands on this
        // probe's stack frame (2 KiB gate).
        let mut state = inner.state.lock();
        if !queue::setup_queue_into(
            &caps.common_cfg,
            0,
            DEFAULT_QUEUE_SIZE,
            q0_msix_entry,
            &mut state.queue,
        ) {
            klog_info!("virtio-blk: queue setup failed");
            return Err(PciProbeError::OutOfMemory);
        }

        set_driver_ok(&caps);

        ring_size = state.queue.free_count();
        capacity_sectors = read_capacity(&caps);
        state.caps = caps;
        state.msix_state = msix_state;
    }

    // The const assert covers `DEFAULT_QUEUE_SIZE`, but the device may
    // negotiate down; say so once here rather than per exhausted request.
    if (ring_size as usize) < NUM_REQUEST_SLOTS * MAX_CHAIN_DESCS {
        klog_info!(
            "virtio-blk: virtqueue negotiated down to {} descriptors, below the {} the {} request slots want — expect Busy under load",
            ring_size,
            NUM_REQUEST_SLOTS * MAX_CHAIN_DESCS,
            NUM_REQUEST_SLOTS
        );
    }

    // Published after the queue is live: `is_ready` is the only gate the I/O
    // path checks before it submits.
    inner
        .capacity
        .store(capacity_sectors * SECTOR_SIZE, Ordering::Relaxed);
    inner
        .flush_supported
        .store(flush_supported, Ordering::Relaxed);
    inner.ready.store(true, Ordering::Release);

    let index = match register_device(inner) {
        Some(i) => i,
        None => {
            klog_info!("virtio-blk: device registry full");
            return Err(PciProbeError::OutOfMemory);
        }
    };

    klog_info!(
        "virtio-blk: disk{} ready, capacity {} sectors ({} MB), flush={}, irq {:?}",
        index.0,
        capacity_sectors,
        (capacity_sectors * SECTOR_SIZE) / (1024 * 1024),
        flush_supported,
        irq_mode,
    );

    Ok(ProbeOutcome::Bound)
}

crate::pci_driver! {
    pub static VIRTIO_BLK_DRIVER = {
        name: "virtio-blk",
        match_table: &[
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_BLK_DEVICE_ID_LEGACY,
            },
            PciMatch::VendorDevice {
                vendor: PCI_VENDOR_ID_VIRTIO,
                device: VIRTIO_BLK_DEVICE_ID_MODERN,
            },
        ],
        probe: virtio_blk_probe,
    };
}
