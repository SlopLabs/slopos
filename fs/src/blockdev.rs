use slopos_ostd::KVec;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockDeviceError {
    OutOfBounds,
    InvalidBuffer,
    /// A block read back with contents that do not match its trusted
    /// build-time integrity hash (see [`crate::verity`]).
    IntegrityFailure,
    /// The device refuses every write: its contents are attested and a
    /// write would make them unverifiable (see [`crate::verity`]).
    WriteProtected,
    /// Every request slot or descriptor the device has is taken. Transient:
    /// the same request is expected to succeed once an in-flight one retires.
    Busy,
    /// The device did not answer in time. What it had already been handed
    /// may still land; a request that waited on an earlier one was never sent.
    Timeout,
    /// The requesting task was killed, and nothing it left with the device
    /// can change the medium: the request never reached it, or was a read or
    /// a flush. An earlier part of a request split across several may have
    /// landed.
    Interrupted,
    /// The requesting task was killed while the device held the request. It
    /// may still land, ahead of every later write to the same medium.
    Abandoned,
    /// The device completed the request but reported a failure.
    DeviceFault,
    Unsupported,
    OutOfMemory,
}

/// The logical block size of a device that is no disk, such as a memory image.
pub const DEFAULT_LOGICAL_BLOCK: u32 = 512;

pub(crate) fn total_seg_len(segs: &[&[u8]]) -> Result<usize, BlockDeviceError> {
    let mut total = 0usize;
    for seg in segs {
        total = total
            .checked_add(seg.len())
            .ok_or(BlockDeviceError::OutOfBounds)?;
    }
    Ok(total)
}

/// A write [`BlockDevice::submit_write`] started; hand it to
/// [`BlockDevice::complete_write`] exactly once.
#[must_use = "an uncompleted write holds a device request slot"]
#[derive(Debug)]
pub struct WriteTicket {
    tag: u64,
    began: u64,
}

impl WriteTicket {
    /// A write the device already finished before `submit_write` returned.
    pub const DONE: u64 = u64::MAX;

    pub fn new(tag: u64, began: u64) -> Self {
        Self { tag, began }
    }

    pub fn tag(&self) -> u64 {
        self.tag
    }

    /// When the device was handed the write, in TSC cycles.
    pub fn began(&self) -> u64 {
        self.began
    }
}

pub trait BlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError>;
    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError>;
    fn capacity(&self) -> u64;

    /// The unit the medium is addressed in: what a partition table counts in,
    /// and the span below which a write is a read-modify-write. A power of two
    /// of at least 512. No default: a wrapper that forgot to forward it would
    /// answer 512 over a 4096-byte disk and silence every check keyed on it.
    fn logical_block_size(&self) -> u32;

    /// Write `segs` back to back starting at `offset`.
    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        let mut at = offset;
        for seg in segs {
            self.write_at(at, seg)?;
            at = at
                .checked_add(seg.len() as u64)
                .ok_or(BlockDeviceError::OutOfBounds)?;
        }
        Ok(())
    }

    /// Start writing `segs` at `offset` and return before the device has
    /// finished, so a caller can keep [`Self::write_depth`] writes in flight.
    /// The bytes are copied before this returns. The default writes
    /// synchronously.
    ///
    /// Never waits for a request slot: `Busy` when none is free. Only the
    /// caller can complete the writes it holds, so it completes one before
    /// trying again, or writes synchronously when it holds none.
    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        self.write_vectored(offset, segs)?;
        Ok(WriteTicket::new(WriteTicket::DONE, 0))
    }

    /// Wait for a submitted write. A failure leaves nothing to retry here: the
    /// caller still holds the bytes and writes them again.
    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        let _ = ticket;
        Ok(())
    }

    /// How many submitted writes one caller may hold uncompleted at once.
    fn write_depth(&self) -> usize {
        1
    }

    /// `true` when every `write_at` will fail with
    /// [`BlockDeviceError::WriteProtected`]. A filesystem consults this at
    /// mount so it never dirties a block it cannot persist.
    fn write_protected(&self) -> bool {
        false
    }

    /// Force every previously-acknowledged write out of any volatile device
    /// cache onto non-volatile media: on a write-back device, `write_at`
    /// returning `Ok` only means the bytes reached that cache.
    ///
    /// The default is a no-op for devices that are inherently durable on write
    /// (e.g. [`MemoryBlockDevice`], or a disk without a volatile write
    /// cache).
    fn flush(&self) -> Result<(), BlockDeviceError> {
        Ok(())
    }

    /// Write out per-device metadata the filesystem's own blocks do not carry
    /// — today the verity attested bitmap (see [`crate::verity`]).
    ///
    /// Ordering: a filesystem MUST call this, and flush, *before* it marks
    /// itself clean. The other order can leave a bitmap attesting blocks a
    /// later write rewrote, which reads as an integrity failure next boot;
    /// this one leaves the image unclean, and an unclean image is treated as
    /// wholly unattested.
    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        Ok(())
    }
}

pub struct MemoryBlockDevice {
    buffer: slopos_ostd::sync::SpinLock<KVec<u8>>,
    block_size: u32,
}

impl MemoryBlockDevice {
    pub fn allocate(len: usize) -> Option<Self> {
        Self::allocate_with_block_size(len, DEFAULT_LOGICAL_BLOCK)
    }

    /// A device that reports `block_size` as its logical block size, as a
    /// 4K-native disk does. Byte access stays exact: nothing here
    /// read-modify-writes.
    pub fn allocate_with_block_size(len: usize, block_size: u32) -> Option<Self> {
        let mut buffer = KVec::with_capacity(len).ok()?;
        for _ in 0..len {
            buffer.push(0).ok()?;
        }
        Some(Self {
            block_size,
            buffer: slopos_ostd::sync::SpinLock::new(
                buffer,
                slopos_ostd::lock_class!(
                    "MemoryBlockDevice.data",
                    slopos_ostd::sync::LOCK_LEVEL_RESOURCE
                ),
            ),
        })
    }

    /// Return a mutable view of the backing buffer for in-place
    /// fixture construction (e.g. test images). Production paths
    /// should use [`BlockDevice::write_at`].
    pub fn with_buffer_mut<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let mut guard = self.buffer.lock();
        f(guard.as_mut_slice())
    }

    pub fn capacity_inner(&self) -> usize {
        self.buffer.lock().len()
    }
}

impl BlockDevice for MemoryBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if buffer.is_empty() {
            return Ok(());
        }
        stats::note_read(buffer.len());
        let guard = self.buffer.lock();
        let Some(end) = offset.checked_add(buffer.len() as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > guard.len() as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        let start = offset as usize;
        buffer.copy_from_slice(&guard[start..start + buffer.len()]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if buffer.is_empty() {
            return Ok(());
        }
        stats::note_write(buffer.len());
        let mut guard = self.buffer.lock();
        let Some(end) = offset.checked_add(buffer.len() as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > guard.len() as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        let start = offset as usize;
        guard[start..start + buffer.len()].copy_from_slice(buffer);
        Ok(())
    }

    /// One lock hold, one counted request, so a fixture measures a coalesced
    /// write the way [`stats`] sees it on hardware.
    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        let total = total_seg_len(segs)?;
        if total == 0 {
            return Ok(());
        }
        stats::note_write(total);
        let mut guard = self.buffer.lock();
        let Some(end) = offset.checked_add(total as u64) else {
            return Err(BlockDeviceError::OutOfBounds);
        };
        if end > guard.len() as u64 {
            return Err(BlockDeviceError::OutOfBounds);
        }
        let mut at = offset as usize;
        for seg in segs {
            guard[at..at + seg.len()].copy_from_slice(seg);
            at += seg.len();
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.buffer.lock().len() as u64
    }

    fn logical_block_size(&self) -> u32 {
        self.block_size
    }
}

/// Storage cost, counted at the device.
///
/// A request is one trip to the device; the block count beside it is what that
/// trip carried, and the gap between the two is what coalescing buys. Every
/// counter is one relaxed add, so nothing here reads a counter or allocates.
pub mod stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Unit the block counts are in, whatever a device's logical block size,
    /// so one number covers every device and filesystem block size.
    pub const SECTOR_BYTES: usize = 512;

    static READ_REQUESTS: AtomicU64 = AtomicU64::new(0);
    static BLOCKS_READ: AtomicU64 = AtomicU64::new(0);
    static WRITE_REQUESTS: AtomicU64 = AtomicU64::new(0);
    static BLOCKS_WRITTEN: AtomicU64 = AtomicU64::new(0);
    static FLUSHES: AtomicU64 = AtomicU64::new(0);
    static TRANSACTIONS: AtomicU64 = AtomicU64::new(0);
    static COMMITS: AtomicU64 = AtomicU64::new(0);
    static READ_CYCLES: AtomicU64 = AtomicU64::new(0);
    static WRITE_CYCLES: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
    pub struct Counters {
        pub read_requests: u64,
        pub blocks_read: u64,
        pub write_requests: u64,
        pub blocks_written: u64,
        pub flushes: u64,
        /// Outermost rollback scopes only; one per commit record.
        pub transactions: u64,
        pub commits: u64,
    }

    #[inline]
    pub fn note_read(bytes: usize) {
        READ_REQUESTS.fetch_add(1, Ordering::Relaxed);
        BLOCKS_READ.fetch_add(bytes.div_ceil(SECTOR_BYTES) as u64, Ordering::Relaxed);
    }

    #[inline]
    pub fn note_write(bytes: usize) {
        WRITE_REQUESTS.fetch_add(1, Ordering::Relaxed);
        BLOCKS_WRITTEN.fetch_add(bytes.div_ceil(SECTOR_BYTES) as u64, Ordering::Relaxed);
    }

    /// Time from a request's submission to its completion, in TSC cycles:
    /// what `prof=on` divides by the request count for a device's latency.
    #[inline]
    pub fn note_request_cycles(write: bool, cycles: u64) {
        let total = if write { &WRITE_CYCLES } else { &READ_CYCLES };
        total.fetch_add(cycles, Ordering::Relaxed);
    }

    /// `(read cycles, write cycles)` summed over every request.
    pub fn request_cycles() -> (u64, u64) {
        (
            READ_CYCLES.load(Ordering::Relaxed),
            WRITE_CYCLES.load(Ordering::Relaxed),
        )
    }

    #[inline]
    pub fn note_flush() {
        FLUSHES.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn note_transaction() {
        TRANSACTIONS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn note_commit() {
        COMMITS.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot() -> Counters {
        Counters {
            read_requests: READ_REQUESTS.load(Ordering::Relaxed),
            blocks_read: BLOCKS_READ.load(Ordering::Relaxed),
            write_requests: WRITE_REQUESTS.load(Ordering::Relaxed),
            blocks_written: BLOCKS_WRITTEN.load(Ordering::Relaxed),
            flushes: FLUSHES.load(Ordering::Relaxed),
            transactions: TRANSACTIONS.load(Ordering::Relaxed),
            commits: COMMITS.load(Ordering::Relaxed),
        }
    }

    pub fn reset() {
        READ_REQUESTS.store(0, Ordering::Relaxed);
        BLOCKS_READ.store(0, Ordering::Relaxed);
        WRITE_REQUESTS.store(0, Ordering::Relaxed);
        BLOCKS_WRITTEN.store(0, Ordering::Relaxed);
        FLUSHES.store(0, Ordering::Relaxed);
        TRANSACTIONS.store(0, Ordering::Relaxed);
        COMMITS.store(0, Ordering::Relaxed);
        READ_CYCLES.store(0, Ordering::Relaxed);
        WRITE_CYCLES.store(0, Ordering::Relaxed);
    }
}
