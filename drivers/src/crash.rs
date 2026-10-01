//! The crash store: the crash partition on the disk the kernel was loaded
//! from, held under a write claim for the boot's life. A fatal panic writes
//! its record there through the NVMe controller's panic queue, polled, with
//! nothing allocated and no lock waited on. The next boot's `/dev/crash`
//! serves the records, and erasing one goes through the same claim.
//!
//! What each slot holds is read at boot and kept in memory, so the panic path
//! reads nothing from the disk to choose one. A slot being written or erased
//! is busy and never chosen, which keeps an erase on the I/O queue and a
//! panic's write on the panic queue off the same slot; one whose read or
//! write failed stays busy for the boot, since a request the device never
//! answered may still land.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use slopos_boot_core::crash::{
    self, HEADER_BYTES, Header, LOG_HEADING, SEQUENCE_MAX, SLOT_BYTES, SlotState, Summary, TEXT_MAX,
};
use slopos_boot_core::{Guid, layout};
use slopos_fs::blockdev::BlockDevice;
use slopos_fs::devfs::{CrashRecord, CrashStoreOps};
use slopos_fs::vfs::{VfsError, VfsResult};
use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, OnceLock, SpinLock, SpinLockGuard};
use slopos_ostd::{KArc, KBox, KVec, klog_info, lock_class};

use crate::block::engine::{BlkError, Namespace};
use crate::block::{self, ClaimError, DiskName, LocateError, Located};
use crate::nvme::{self, Controller};

/// The panic report takes at most this much of a record, so the kernel log's
/// tail always has room.
pub(crate) const REPORT_MAX: usize = TEXT_MAX / 2;

const EMPTY: u64 = 0;
const BUSY: u64 = u64::MAX;

struct Slot {
    /// [`EMPTY`], [`BUSY`] or the sequence number of the record held.
    state: AtomicU64,
    text_len: AtomicU32,
    intact: AtomicBool,
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(EMPTY),
            text_len: AtomicU32::new(0),
            intact: AtomicBool::new(false),
        }
    }

    fn state(&self) -> SlotState {
        match self.state.load(Ordering::Acquire) {
            EMPTY => SlotState::Empty,
            BUSY => SlotState::Busy,
            sequence => SlotState::Holds(sequence),
        }
    }

    /// Make the slot busy if it is still as `found`.
    fn take(&self, found: SlotState) -> bool {
        let found = match found {
            SlotState::Empty => EMPTY,
            SlotState::Holds(sequence) if (1..=SEQUENCE_MAX).contains(&sequence) => sequence,
            SlotState::Holds(_) | SlotState::Busy => return false,
        };
        self.state
            .compare_exchange(found, BUSY, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn hold(&self, sequence: u64, text_len: usize, intact: bool) {
        self.text_len.store(text_len as u32, Ordering::Relaxed);
        self.intact.store(intact, Ordering::Relaxed);
        self.state.store(sequence, Ordering::Release);
    }

    fn release(&self) {
        self.state.store(EMPTY, Ordering::Release);
    }

    fn retire(&self) {
        self.state.store(BUSY, Ordering::Release);
    }

    fn record(&self) -> Option<CrashRecord> {
        let SlotState::Holds(sequence) = self.state() else {
            return None;
        };
        Some(CrashRecord {
            sequence,
            text_len: u64::from(self.text_len.load(Ordering::Relaxed)),
            intact: self.intact.load(Ordering::Relaxed),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    Locate(LocateError),
    /// The partition's disk is no NVMe namespace.
    NotNvme,
    /// Its controller kept no queue pair for the panic path.
    NoPanicQueue,
    Claim(ClaimError),
    /// A table re-read moved the partition between finding and claiming it.
    Moved,
    /// Smaller than one slot.
    TooSmall,
    NoMemory,
    AlreadyArmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanicWriteError {
    /// The controller has shut down, its queue is held, or a command it was
    /// given went unanswered.
    NoQueue,
    /// Every slot is busy.
    NoSlot,
    TooLong,
    Device(BlkError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub slot: usize,
    pub sequence: u64,
}

pub struct Store {
    partition: DiskName,
    /// Every read and erase goes through it, and while it is held no table
    /// re-read can move the window the panic path writes.
    device: KBox<dyn BlockDevice + Send + Sync>,
    controller: KArc<Controller>,
    ns: Namespace,
    partition_start: u64,
    slots: KVec<Slot>,
    /// Past every sequence number a readable slot held at boot, torn records'
    /// included, so a new record never takes the name of one still held.
    next_sequence: AtomicU64,
    kernel: KVec<u8>,
    cmdline: KVec<u8>,
    build: &'static str,
}

/// The crash partition on the disk whose GPT names it `disk`, under a write
/// claim, and the NVMe namespace behind it.
struct Claimed {
    located: Located,
    device: KBox<dyn BlockDevice + Send + Sync>,
    controller: KArc<Controller>,
    nsid: u32,
}

impl Claimed {
    #[inline(never)]
    fn take(disk: Guid) -> Result<Claimed, OpenError> {
        let located =
            block::locate_partition(disk, layout::CRASH_TYPE).map_err(OpenError::Locate)?;
        let (controller, nsid) =
            nvme::controller_of(located.disk.as_bytes()).ok_or(OpenError::NotNvme)?;
        if controller.panic_queue().is_none() {
            return Err(OpenError::NoPanicQueue);
        }
        let device = block::claim(located.partition.as_bytes()).map_err(OpenError::Claim)?;
        if block::locate_partition(disk, layout::CRASH_TYPE) != Ok(located) {
            return Err(OpenError::Moved);
        }
        Ok(Claimed {
            located,
            device,
            controller,
            nsid,
        })
    }
}

/// What a record names the boot by.
pub struct Booted<'a> {
    /// The path the loader read the kernel from.
    pub kernel: &'a str,
    pub cmdline: &'a str,
    pub build: &'static str,
}

/// A copy, since the loader's memory is not the kernel's to keep.
fn copied(text: &str) -> Result<KVec<u8>, OpenError> {
    let mut copy = KVec::new();
    copy.extend_from_slice(text.as_bytes())
        .map_err(|_| OpenError::NoMemory)?;
    Ok(copy)
}

fn utf8(copy: &[u8]) -> &str {
    core::str::from_utf8(copy).unwrap_or_default()
}

impl Store {
    /// The crash partition on the disk whose GPT names it `disk`, claimed,
    /// and what its slots hold.
    pub(crate) fn open(disk: Guid, booted: &Booted<'_>) -> Result<Store, OpenError> {
        let claimed = Claimed::take(disk)?;
        let store = Store::from_claim(claimed, booted)?;
        store.scan()?;
        Ok(store)
    }

    #[inline(never)]
    fn from_claim(claimed: Claimed, booted: &Booted<'_>) -> Result<Store, OpenError> {
        let Claimed {
            located,
            device,
            controller,
            nsid,
        } = claimed;
        let count = crash::slot_count(located.len);
        if count == 0 {
            return Err(OpenError::TooSmall);
        }
        let mut slots = KVec::with_capacity(count).map_err(|_| OpenError::NoMemory)?;
        for _ in 0..count {
            slots.push(Slot::new()).map_err(|_| OpenError::NoMemory)?;
        }
        let ns = Namespace {
            nsid,
            block_shift: device.logical_block_size().trailing_zeros() as u8,
        };
        Ok(Store {
            partition: located.partition,
            device,
            controller,
            ns,
            partition_start: located.start,
            slots,
            next_sequence: AtomicU64::new(1),
            kernel: copied(booted.kernel)?,
            cmdline: copied(booted.cmdline)?,
            build: booted.build,
        })
    }

    fn scan(&self) -> Result<(), OpenError> {
        let mut record = KVec::<u8>::zeroed(SLOT_BYTES).map_err(|_| OpenError::NoMemory)?;
        let block = self.device.logical_block_size() as usize;
        for (index, slot) in self.slots.iter().enumerate() {
            let at = slot_offset(index);
            let Ok(()) = self.device.read_at(at, &mut record[..block]) else {
                klog_info!("CRASH: {} slot {} unreadable", self.partition, index);
                slot.retire();
                continue;
            };
            let Some(header) = Header::parse(&record) else {
                continue;
            };
            self.next_sequence
                .fetch_max(header.sequence.saturating_add(1), Ordering::Relaxed);
            let text = &mut record[HEADER_BYTES..HEADER_BYTES + header.text_len];
            match self.device.read_at(at + HEADER_BYTES as u64, text) {
                Ok(()) => slot.hold(header.sequence, header.text_len, header.seals(text)),
                Err(_) => {
                    klog_info!("CRASH: {} slot {} unreadable", self.partition, index);
                    slot.retire();
                }
            }
        }
        Ok(())
    }

    pub fn partition(&self) -> DiskName {
        self.partition
    }

    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    pub fn record(&self, slot: usize) -> Option<CrashRecord> {
        self.slots.get(slot)?.record()
    }

    fn held(&self, slot: usize, sequence: u64) -> Option<CrashRecord> {
        self.record(slot).filter(|r| r.sequence == sequence)
    }

    /// Bytes of record `sequence`'s text from `offset`; 0 past its end.
    pub(crate) fn read(
        &self,
        slot: usize,
        sequence: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> VfsResult<usize> {
        let record = self.held(slot, sequence).ok_or(VfsError::NotFound)?;
        let left = record.text_len.saturating_sub(offset);
        let n = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
        if n == 0 {
            return Ok(0);
        }
        let at = slot_offset(slot) + HEADER_BYTES as u64 + offset;
        self.device
            .read_at(at, &mut buf[..n])
            .map_err(|_| VfsError::IoError)?;
        Ok(n)
    }

    /// Erase record `sequence` by zeroing its header, which is all that makes
    /// the slot hold it.
    pub(crate) fn erase(&self, slot: usize, sequence: u64) -> VfsResult<()> {
        let found = self.slots.get(slot).ok_or(VfsError::NotFound)?;
        if !found.take(SlotState::Holds(sequence)) {
            return Err(VfsError::NotFound);
        }
        let zeroed = self
            .device
            .write_at(slot_offset(slot), &[0u8; HEADER_BYTES])
            .and_then(|()| self.device.flush());
        if zeroed.is_err() {
            return Err(VfsError::IoError);
        }
        found.release();
        Ok(())
    }

    /// Write the record whose text, `text_len` bytes, follows the header room
    /// at the front of `record`, and flush it. Polled through the panic queue;
    /// allocates nothing.
    pub(crate) fn write_panic(
        &self,
        record: &mut [u8; SLOT_BYTES],
        text_len: usize,
    ) -> Result<Written, PanicWriteError> {
        self.write_in_chunks(record, text_len, SLOT_BYTES)
    }

    /// [`Store::write_panic`] in commands of at most `chunk` bytes, the one
    /// holding the header last.
    pub(crate) fn write_in_chunks(
        &self,
        record: &mut [u8; SLOT_BYTES],
        text_len: usize,
        chunk: usize,
    ) -> Result<Written, PanicWriteError> {
        let queue = self
            .controller
            .panic_queue()
            .ok_or(PanicWriteError::NoQueue)?;
        let mut session = queue.take().ok_or(PanicWriteError::NoQueue)?;
        let sequence =
            match self
                .next_sequence
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < SEQUENCE_MAX).then_some(n + 1)
                }) {
                Ok(n) | Err(n) => n.min(SEQUENCE_MAX),
            };
        let len = crash::seal(record, text_len, sequence).ok_or(PanicWriteError::TooLong)?;
        let slot = self.take_slot()?;
        let block = 1usize << self.ns.block_shift;
        let end = len.next_multiple_of(block);
        record[len..end].fill(0);
        let chunk = chunk.min(queue.max_transfer()).max(block) / block * block;
        let at = self.partition_start + slot_offset(slot);
        let written = (chunk..end)
            .step_by(chunk)
            .chain(core::iter::once(0))
            .try_for_each(|from| {
                let to = (from + chunk).min(end);
                session.write(self.ns, at + from as u64, &record[from..to])
            })
            .and_then(|()| session.flush(self.ns));
        written.map_err(PanicWriteError::Device)?;
        self.slots[slot].hold(sequence, text_len, true);
        Ok(Written { slot, sequence })
    }

    fn take_slot(&self) -> Result<usize, PanicWriteError> {
        for _ in 0..=self.slots.len() {
            let slot = crash::place(self.slots.len(), |i| self.slots[i].state())
                .ok_or(PanicWriteError::NoSlot)?;
            if self.slots[slot].take(self.slots[slot].state()) {
                return Ok(slot);
            }
        }
        Err(PanicWriteError::NoSlot)
    }
}

fn slot_offset(slot: usize) -> u64 {
    (slot * SLOT_BYTES) as u64
}

static ARMED: OnceLock<Store> = OnceLock::new();

/// Hold the crash partition on the disk `disk` names for the panic path and
/// `/dev/crash`.
pub fn arm(disk: Guid, booted: &Booted<'_>) -> Result<&'static Store, OpenError> {
    if ARMED.is_completed() {
        return Err(OpenError::AlreadyArmed);
    }
    let store = Store::open(disk, booted)?;
    let mut ours = false;
    ARMED.call_once(|| {
        ours = true;
        store
    });
    match ARMED.get() {
        Some(store) if ours => Ok(store),
        _ => Err(OpenError::AlreadyArmed),
    }
}

pub fn armed() -> Option<&'static Store> {
    ARMED.get()
}

fn ops_slots() -> usize {
    armed().map_or(0, Store::slots)
}

fn ops_record(slot: usize) -> Option<CrashRecord> {
    armed()?.record(slot)
}

fn ops_read(slot: usize, sequence: u64, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
    armed()
        .ok_or(VfsError::NotFound)?
        .read(slot, sequence, offset, buf)
}

fn ops_erase(slot: usize, sequence: u64) -> VfsResult<()> {
    armed().ok_or(VfsError::NotFound)?.erase(slot, sequence)
}

pub static DEVFS_OPS: CrashStoreOps = CrashStoreOps {
    slots: ops_slots,
    record: ops_record,
    read: ops_read,
    erase: ops_erase,
};

struct Buffer {
    record: [u8; SLOT_BYTES],
    text_len: usize,
}

/// Static, so the panic path allocates nothing.
static BUFFER: SpinLock<Buffer> = SpinLock::new(
    Buffer {
        record: [0; SLOT_BYTES],
        text_len: 0,
    },
    lock_class!("crash.BUFFER", LOCK_LEVEL_UNORDERED),
);

impl Buffer {
    /// Append what fits of `bytes` below `limit` bytes of text.
    fn push(&mut self, bytes: &[u8], limit: usize) {
        let from = HEADER_BYTES + self.text_len;
        let n = bytes.len().min(limit.saturating_sub(self.text_len));
        self.record[from..from + n].copy_from_slice(&bytes[..n]);
        self.text_len += n;
    }
}

/// A fatal panic's record, written into as the report is: a [`Summary`],
/// then whatever the report writes, at most [`REPORT_MAX`] of it.
pub struct PanicRecord<'a> {
    store: &'a Store,
    buffer: SpinLockGuard<'static, Buffer>,
}

/// The record for a fatal panic `panic` describes, when a store is armed and
/// nobody else is writing one.
pub fn begin_panic_record(panic: &str) -> Option<PanicRecord<'static>> {
    PanicRecord::begin(armed()?, panic)
}

impl Write for PanicRecord<'_> {
    /// A report past [`REPORT_MAX`] is cut short on a character boundary.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut fits = s.len().min(REPORT_MAX.saturating_sub(self.buffer.text_len));
        while !s.is_char_boundary(fits) {
            fits -= 1;
        }
        self.buffer.push(&s.as_bytes()[..fits], REPORT_MAX);
        Ok(())
    }
}

impl<'a> PanicRecord<'a> {
    pub(crate) fn begin(store: &'a Store, panic: &str) -> Option<Self> {
        let mut record = PanicRecord {
            store,
            buffer: BUFFER.try_lock()?,
        };
        record.buffer.text_len = 0;
        let summary = Summary {
            kernel: utf8(&store.kernel),
            cmdline: utf8(&store.cmdline),
            build: store.build,
            time: slopos_kernel_services::clock::realtime_ns().map(|ns| ns / 1_000_000_000),
            uptime_ms: slopos_kernel_services::clock::uptime_ms(),
            cpu: slopos_arch::get_current_cpu() as u32,
            panic,
        };
        let _ = summary.write(&mut record);
        Some(record)
    }

    pub fn partition(&self) -> DiskName {
        self.store.partition
    }

    /// Close the report with the kernel log's newest lines and write it.
    pub fn commit(mut self) -> Result<Written, PanicWriteError> {
        let buffer = &mut *self.buffer;
        for part in ["\n", LOG_HEADING, "\n"] {
            buffer.push(part.as_bytes(), TEXT_MAX);
        }
        let from = HEADER_BYTES + buffer.text_len;
        match slopos_ostd::klog::klog_tail(&mut buffer.record[from..]) {
            Some(tail) => buffer.text_len += tail,
            None => buffer.push(b"(its lock was held)\n", TEXT_MAX),
        }
        self.store.write_panic(&mut buffer.record, buffer.text_len)
    }
}
