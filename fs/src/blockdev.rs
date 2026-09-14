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
    Timeout,
    /// The device completed the request but reported a failure.
    DeviceFault,
    Unsupported,
    OutOfMemory,
}

/// Stable, enumeration-order identity for a block device, assigned at probe
/// time: `disk0` is the first device claimed (by convention the root filesystem
/// image), `disk1` the second (a scratch device for destructive tests).
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockDeviceIndex(pub u16);

pub(crate) fn total_seg_len(segs: &[&[u8]]) -> Result<usize, BlockDeviceError> {
    let mut total = 0usize;
    for seg in segs {
        total = total
            .checked_add(seg.len())
            .ok_or(BlockDeviceError::OutOfBounds)?;
    }
    Ok(total)
}

pub trait BlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError>;
    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError>;
    fn capacity(&self) -> u64;

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
    /// (e.g. [`MemoryBlockDevice`], or a virtio-blk backend that did not
    /// negotiate `VIRTIO_BLK_F_FLUSH`).
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
}

impl MemoryBlockDevice {
    pub fn allocate(len: usize) -> Option<Self> {
        let mut buffer = KVec::with_capacity(len).ok()?;
        for _ in 0..len {
            buffer.push(0).ok()?;
        }
        Some(Self {
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
}

/// Storage cost, counted at the device.
///
/// A request is one trip to the device; the block count beside it is what that
/// trip carried, and the gap between the two is what coalescing buys. Every
/// counter is one relaxed add, so nothing here reads a counter or allocates.
pub mod stats {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// Unit the block counts are in: the logical sector every block device
    /// addresses in, so one number covers any filesystem block size.
    pub const SECTOR_BYTES: usize = 512;

    static READ_REQUESTS: AtomicU64 = AtomicU64::new(0);
    static BLOCKS_READ: AtomicU64 = AtomicU64::new(0);
    static WRITE_REQUESTS: AtomicU64 = AtomicU64::new(0);
    static BLOCKS_WRITTEN: AtomicU64 = AtomicU64::new(0);
    static FLUSHES: AtomicU64 = AtomicU64::new(0);
    static TRANSACTIONS: AtomicU64 = AtomicU64::new(0);
    static COMMITS: AtomicU64 = AtomicU64::new(0);

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
    }
}
