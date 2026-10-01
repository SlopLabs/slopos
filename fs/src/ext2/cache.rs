use slopos_mm::slab::MAX_ALLOC_SIZE;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::frame::{Frame, PageCacheMeta};
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota;
use slopos_ostd::{KBTreeMap, KBox, KVec, write_field};

use super::Ext2Error;
use super::blockcharge::BlockCharges;
use super::dirindex::{DirIndexSet, DirProbe};
use super::journal::Journal;
use super::ondisk::EXT2_MAX_BLOCK_SIZE;
use super::types::BlockNum;
use crate::blockdev::{BlockDevice, BlockDeviceError, WriteTicket, stats};

/// Frames the cache never drops below. Small enough for the appliance image,
/// large enough that a 16 GiB volume's whole allocation working set stays
/// resident (see [`cache_entries_for`]).
pub const CACHE_ENTRIES_MIN: usize = 512;

/// Frames the cache never grows past: 512 MiB at 4 KiB blocks. Also what
/// holds the per-slot `u32` records, each one allocation, under the heap's
/// single-allocation ceiling.
pub const CACHE_ENTRIES_MAX: usize = 128 * 1024;
const _: () = assert!(CACHE_ENTRIES_MAX * size_of::<u32>() <= MAX_ALLOC_SIZE / 2);

/// Entries per chunk of [`Slots`]; one chunk is one allocation.
const SLOT_CHUNK: usize = 4096;
const _: () = assert!(SLOT_CHUNK * size_of::<CacheEntry>() <= MAX_ALLOC_SIZE);

/// The share of usable memory one mount's cache may grow to: the page cache
/// Linux keeps in free memory, bounded so several mounts leave room for the
/// programs using them. Clean frames go back under pressure through the
/// reclaim tier.
const CACHE_MEMORY_SHARE: u64 = 8;

/// Frames a volume may keep resident: its allocation working set, and
/// beyond that as much of the volume as `usable_frames` affords.
///
/// Every allocation reads the group descriptor table and the block and inode
/// bitmaps of the group it lands in, and the next allocation reads them again.
/// A cache smaller than that working set evicts a bitmap it is about to need.
/// Past it, the cache is what file reads hit and what write-back batches in,
/// so it grows with memory rather than with the volume's group count.
pub fn cache_entries_for(volume_blocks: u64, blocks_per_group: u32, usable_frames: u64) -> usize {
    let per_group = blocks_per_group.max(1) as u64;
    let groups = volume_blocks.div_ceil(per_group);
    // 32-byte descriptors, so 32 to a 1 KiB block: the smallest block size an
    // image may carry is the one whose table takes the most blocks.
    let gdt = groups.div_ceil(32);
    let working_set = groups.saturating_mul(2).saturating_add(gdt);
    let memory = (usable_frames / CACHE_MEMORY_SHARE).min(volume_blocks);
    (working_set.max(memory).min(CACHE_ENTRIES_MAX as u64) as usize).max(CACHE_ENTRIES_MIN)
}

/// Not a slot: the end of the LRU chain, and the link value of a slot that is
/// not on it.
const NIL: u32 = u32::MAX;

/// Groups the allocation hints cover: one `u32` each, held at 256 KiB so a
/// pathological group count falls back to no hints rather than to a failed
/// mount.
const GROUP_HINTS_MAX: usize = 256 * 1024 / size_of::<u32>();

/// Blocks one writeback request may carry. The segment array is on the stack,
/// so this is a stack cost as much as an I/O size.
const FLUSH_RUN: usize = 32;

/// Writeback requests one flush may keep in flight, whatever more the device
/// would take.
const WRITE_DEPTH_MAX: usize = 4;

/// Dirty data copied out of the cache for a writer that does not hold the
/// mount lock: see [`BlockCache::stage_data_batch`].
#[derive(Default)]
pub struct DataBatch {
    /// The staged blocks' bytes, run after run.
    bytes: KVec<u8>,
    /// Block numbers, in `bytes` order.
    blocks: KVec<u32>,
    /// `(device offset, first index into blocks, length)`, one per request.
    runs: KVec<(u64, u32, u32)>,
    /// Which runs reached the device, filled by [`Self::write`].
    ok: KVec<bool>,
}

impl DataBatch {
    fn clear(&mut self) {
        self.bytes.clear();
        self.blocks.clear();
        self.runs.clear();
        self.ok.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Room for `blocks` blocks; the batch is empty when this is called.
    fn reserve(&mut self, blocks: usize, block_size: usize) -> Result<(), AllocError> {
        self.bytes.try_reserve_exact(blocks * block_size)?;
        self.blocks.try_reserve_exact(blocks)?;
        self.runs.try_reserve_exact(blocks)?;
        self.ok.try_reserve_exact(blocks)?;
        Ok(())
    }

    /// Whether any run failed to reach the device.
    pub fn failed(&self) -> bool {
        self.ok.iter().any(|ok| !ok)
    }

    /// Write every run, keeping the device's write depth in flight, and
    /// record which reached it. A run that failed is tried once more through
    /// the device's retrying road.
    pub fn write(&mut self, device: &dyn BlockDevice, block_size: u32) {
        let bs = block_size as usize;
        let depth = device.write_depth().clamp(1, WRITE_DEPTH_MAX);
        let runs = self.runs.len();
        self.ok.clear();
        if self.ok.try_reserve_exact(runs).is_err() {
            return;
        }
        let mut pending: [Option<(usize, WriteTicket)>; WRITE_DEPTH_MAX] =
            [const { None }; WRITE_DEPTH_MAX];
        let (mut head, mut count) = (0usize, 0usize);
        for _ in 0..runs {
            let _ = self.ok.push(false);
        }
        let mut next = 0usize;
        while next < runs || count > 0 {
            if next < runs && count < depth {
                let (offset, first, len) = self.runs.as_slice()[next];
                let span = &self.bytes.as_slice()[first as usize * bs..(first + len) as usize * bs];
                match device.submit_write(offset, &[span]) {
                    Ok(ticket) => {
                        pending[(head + count) % WRITE_DEPTH_MAX] = Some((next, ticket));
                        count += 1;
                        next += 1;
                        continue;
                    }
                    Err(BlockDeviceError::Busy) if count > 0 => {}
                    Err(_) => {
                        self.retry(device, next, bs);
                        next += 1;
                        continue;
                    }
                }
            }
            if let Some((k, ticket)) = pending[head].take() {
                if device.complete_write(ticket).is_ok() {
                    self.ok.as_mut_slice()[k] = true;
                } else {
                    self.retry(device, k, bs);
                }
            }
            head = (head + 1) % WRITE_DEPTH_MAX;
            count -= 1;
        }
    }

    fn retry(&mut self, device: &dyn BlockDevice, k: usize, bs: usize) {
        let (offset, first, len) = self.runs.as_slice()[k];
        let span = &self.bytes.as_slice()[first as usize * bs..(first + len) as usize * bs];
        self.ok.as_mut_slice()[k] = device.write_at(offset, span).is_ok();
    }
}

/// The runs one flush has submitted and not yet completed, oldest first. A
/// field rather than a local, and its run table on the heap: no frame can
/// carry it on top of the flush.
struct Inflight {
    tickets: [Option<WriteTicket>; WRITE_DEPTH_MAX],
    runs: KVec<[u32; FLUSH_RUN]>,
    lens: [u8; WRITE_DEPTH_MAX],
    head: usize,
    count: usize,
}

impl Inflight {
    fn new() -> Result<Self, AllocError> {
        let mut runs = KVec::with_capacity(WRITE_DEPTH_MAX).map_err(|_| AllocError)?;
        for _ in 0..WRITE_DEPTH_MAX {
            runs.push([0; FLUSH_RUN]).map_err(|_| AllocError)?;
        }
        Ok(Self {
            tickets: [const { None }; WRITE_DEPTH_MAX],
            runs,
            lens: [0; WRITE_DEPTH_MAX],
            head: 0,
            count: 0,
        })
    }

    /// Claim the next free entry; the caller has made room.
    fn push(&mut self) -> usize {
        let at = (self.head + self.count) % WRITE_DEPTH_MAX;
        self.count += 1;
        at
    }

    fn pop_oldest(&mut self) {
        self.head = (self.head + 1) % WRITE_DEPTH_MAX;
        self.count -= 1;
    }

    fn pop_newest(&mut self) {
        self.count -= 1;
    }
}

/// Snapshots one operation may hold. Each owns a block-sized copy, so at a
/// 4 KiB block size this is 2 MiB of rollback guard.
///
/// Only a block that was *already dirty* when the operation first touched it
/// costs a record. A clean acquire costs one bit, so neither directory size
/// nor write size is bounded by this number.
const MAX_UNDO: usize = 512;

/// Evictable entries a miss looks at, from the LRU end, before settling for
/// the best class it has found rather than walking on for a clean one.
const VICTIM_SCAN: usize = 64;

/// Directories one operation may name individually before a rollback stops
/// trying. Larger than [`super::dirindex::DIR_INDEX_DIRS`], so overflowing it
/// means dropping every index costs nothing that was going to survive.
const OP_DIRS_MAX: usize = 8;

/// Drives ordered writeback (ext2 `data=ordered`): data blocks must reach
/// stable storage *before* the metadata that references them, so a crash cannot
/// expose a fresh inode or dir-entry pointing at stale contents.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Data,
    Metadata,
}

/// What a block's contents refer to, which is what decides whether a
/// *per-inode* writeback may publish it (see [`super::Ext2Fs::sync_inode`]).
/// Whole-filesystem [`super::Ext2Fs::sync`] ignores it and writes everything.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BlockOwner {
    /// Allocation state: block and inode bitmaps, group descriptors. Names no
    /// file contents, so writing one ahead of the rest can only leak space an
    /// `e2fsck` reclaims.
    Alloc,
    /// An inode-table block, carrying the block pointers of every inode in
    /// `first..=last`. `last` may over-reach the group; a wider span only
    /// makes the co-residency pre-flush write more than it must.
    Inodes {
        first: u32,
        last: u32,
    },
    File(u32),
    /// Everything else — directory blocks above all. A directory block names
    /// inodes, so publishing one before every inode table is on disk can
    /// resurrect a freed inode under a fresh name.
    Other,
}

impl BlockOwner {
    /// The inode a block belongs to, when the owner names one. `None` for
    /// allocation state and inode tables, charged to no principal.
    pub fn charged_inode(self) -> Option<u32> {
        match self {
            BlockOwner::File(ino) => Some(ino),
            _ => None,
        }
    }
}

/// The `frame` is both the slot's storage and, through its typed metadata, the
/// dirty bit and the owner-key backref.
struct CacheEntry {
    block: BlockNum,
    frame: Frame<PageCacheMeta>,
    pinned: u16,
    /// Neighbours on the recency chain, `lru_prev` towards the MRU end. Every
    /// slot is on the chain exactly once.
    lru_prev: u32,
    lru_next: u32,
    valid: bool,
    kind: BlockKind,
    owner: BlockOwner,
    /// The open operation has touched this block, so it is both a rollback
    /// candidate and a victim of last resort (see [`BlockCache::find_or_evict`]).
    op_touched: bool,
    /// The open operation asked for this block to be dropped. Deferred to the
    /// commit — see [`BlockCache::invalidate`].
    op_invalidated: bool,
    /// The open operation found this block clean, so its rollback is to drop
    /// the entry and let the device's copy stand. A flag rather than an undo
    /// record: recording one would put an allocation on every directory scan.
    op_discard: bool,
    /// Transient, set while [`BlockCache::rollback_op`] runs: this block's
    /// snapshot has been put back, so the discard pass must leave it alone.
    op_restored: bool,
    /// This slot's index is in [`BlockCache::op_touched_slots`]. Not the same
    /// question as `op_touched`: an eviction clears that flag, and a slot
    /// listed twice could overrun the preallocated list.
    op_listed: bool,
    /// The block was dirty when the open operation first touched it.
    op_was_dirty: bool,
    /// The operation that last dirtied this block. A writeback pass writes
    /// nothing newer than the epoch it fixed, which is what lets it release
    /// the mount lock between chunks.
    dirty_epoch: u64,
    /// The operation that made this block dirty from clean. A data block a
    /// later operation rewrote is still data an earlier record names, so the
    /// pass's data phase selects on this rather than on `dirty_epoch`.
    dirtied_epoch: u64,
    /// Staged into a [`DataBatch`] the flusher is writing without the mount
    /// lock: marked clean already, so it must stay cached until the batch
    /// finishes and puts it back dirty if the write failed.
    inflight: bool,
}

impl CacheEntry {
    fn new() -> Result<Self, Ext2Error> {
        let frame = Frame::<PageCacheMeta>::alloc().ok_or(Ext2Error::OutOfMemory)?;
        Ok(Self {
            block: BlockNum::ZERO,
            frame,
            pinned: 0,
            lru_prev: NIL,
            lru_next: NIL,
            valid: false,
            kind: BlockKind::Metadata,
            owner: BlockOwner::Other,
            op_touched: false,
            op_invalidated: false,
            op_discard: false,
            op_restored: false,
            op_listed: false,
            op_was_dirty: false,
            dirty_epoch: 0,
            dirtied_epoch: 0,
            inflight: false,
        })
    }
}

/// The cache's entries in fixed [`SLOT_CHUNK`]-sized allocations, so how many
/// the cache holds is bounded by memory rather than by one allocation. Chunks
/// are allocated at full capacity and never reallocate, so an entry never
/// moves except through [`Slots::swap_remove`].
struct Slots {
    chunks: KVec<KVec<CacheEntry>>,
    len: usize,
}

impl Slots {
    fn new() -> Self {
        Self {
            chunks: KVec::new(),
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, entry: CacheEntry) -> Result<(), AllocError> {
        if self.len == self.chunks.len() * SLOT_CHUNK {
            let chunk = KVec::with_capacity(SLOT_CHUNK)?;
            self.chunks.push(chunk)?;
        }
        let last = self.chunks.len() - 1;
        self.chunks[last].push(entry)?;
        self.len += 1;
        Ok(())
    }

    /// Move the last entry into `index` and drop the one that was there.
    fn swap_remove(&mut self, index: usize) {
        let last = self.len - 1;
        if index != last {
            let (a, b) = (index / SLOT_CHUNK, last / SLOT_CHUNK);
            if a == b {
                self.chunks[a].swap(index % SLOT_CHUNK, last % SLOT_CHUNK);
            } else {
                let (low, high) = self.chunks.split_at_mut(b);
                core::mem::swap(
                    &mut low[a][index % SLOT_CHUNK],
                    &mut high[0][last % SLOT_CHUNK],
                );
            }
        }
        let chunk = last / SLOT_CHUNK;
        drop(self.chunks[chunk].pop());
        if self.chunks[chunk].is_empty() {
            drop(self.chunks.pop());
        }
        self.len = last;
    }

    fn iter(&self) -> impl Iterator<Item = &CacheEntry> {
        self.chunks.iter().flat_map(|chunk| chunk.iter())
    }
}

impl core::ops::Index<usize> for Slots {
    type Output = CacheEntry;

    fn index(&self, index: usize) -> &CacheEntry {
        &self.chunks[index / SLOT_CHUNK][index % SLOT_CHUNK]
    }
}

impl core::ops::IndexMut<usize> for Slots {
    fn index_mut(&mut self, index: usize) -> &mut CacheEntry {
        &mut self.chunks[index / SLOT_CHUNK][index % SLOT_CHUNK]
    }
}

/// A block that was already dirty when the open operation first reached it.
///
/// The device holds *some earlier* state, so only this snapshot — taken before
/// the first mutation — is the committed one. A block found clean needs no
/// record; see [`CacheEntry::op_discard`].
struct UndoEntry {
    block: BlockNum,
    snapshot: KVec<u8>,
}

/// LRU-ordered, fixed-capacity, **write-back** block cache. One lives for the
/// lifetime of the mount, so dirty blocks accumulate across operations and are
/// written back on eviction, on [`Self::flush_all`], or by the background
/// flusher. Never issues device flushes itself: the barriers around ordered
/// phases are `Ext2Fs::sync`'s.
#[derive(slopos_ostd::SlotFields)]
pub struct BlockCache {
    entries: Slots,
    index: KBTreeMap<BlockNum, usize>,
    /// Frames this cache may hold, from the volume's size at mount. Also the
    /// ceiling on every per-slot record below, all of which are preallocated
    /// to it.
    capacity: usize,
    /// Ends of the recency chain: `lru_head` is the most recently used slot,
    /// `lru_tail` the first victim. Invalid slots are retired to the tail, so
    /// a miss finds an empty slot before it considers evicting anything.
    lru_head: u32,
    lru_tail: u32,
    block_size: u32,
    /// Blocks handed to the device since the last [`Self::note_barrier`].
    ///
    /// A clean cache is not a durable one — eviction writes back without a
    /// barrier by design — so this is what tells "nothing to do" from
    /// "nothing left to *write*".
    unbarriered: usize,
    /// Undo record of the open operation, empty when none is open.
    undo: KVec<UndoEntry>,
    /// Set when the open operation touched more blocks than [`MAX_UNDO`]
    /// permits. The scope can no longer be rolled back, so it must fail rather
    /// than commit half of itself.
    undo_overflow: bool,
    /// Nesting depth of [`Self::begin_op`]; only the outermost level records
    /// and only it rolls back.
    op_depth: u32,
    /// Counts operations, not writes, so a pass takes all of an operation's
    /// blocks or none of them.
    epoch: u64,
    /// The metadata redo log, when the image carries one. Owned here because
    /// every decision it changes — what an eviction may publish, what a
    /// rollback restores, where a miss reads from — is a cache decision.
    journal: Option<KBox<Journal>>,
    /// Block numbers staged for one log record. Preallocated so a commit
    /// allocates nothing.
    scratch: KVec<u32>,
    /// Who the open operation's block allocations are charged to.
    op_account: AccountId,
    /// Blocks the open operation has been charged for, refunded if it rolls
    /// back.
    op_charged: u32,
    /// Blocks charged for an allocation that then failed, refunded at the
    /// commit; a rollback gives back `op_charged` in full instead.
    op_cancelled: u32,
    /// Which principal each charged block belongs to, so a free credits the
    /// account that paid.
    charges: BlockCharges,
    /// An eviction wrote one of the open operation's *data* blocks home, so
    /// the commit owes a barrier whichever path it takes: `data=ordered`
    /// forbids publishing metadata that names a home write still in a cache.
    op_data_evicted: bool,
    /// Every slot carrying per-operation state — touched, invalidated, or
    /// discardable. Preallocated to [`Self::capacity`] and appended to at
    /// most once per slot per operation, so a commit walks what the operation
    /// touched instead of the whole cache and allocates nothing.
    op_touched_slots: KVec<u32>,
    /// Inodes the open operation acquired a block *for* — what a rollback owes
    /// the name index a retraction of.
    ///
    /// Recorded as the blocks are acquired rather than read back off the slots
    /// at rollback time: [`Self::find_or_evict`] reuses a listed slot
    /// mid-operation and overwrites its owner.
    op_dirs: [u32; OP_DIRS_MAX],
    op_dirs_len: u8,
    /// More directories than `op_dirs` holds, so a rollback drops every index
    /// rather than the ones it can still name.
    op_dirs_overflow: bool,
    /// One block-bitmap start hint per group, so an allocation resumes where
    /// the last one stopped. Empty when the group count does not fit the
    /// allocation ceiling — a missing hint only costs a scan.
    group_hints: KVec<u32>,
    /// Group an unhinted allocation starts from, so a full volume's sweep
    /// does not restart at group 0 every time.
    last_group: u32,
    /// Per-directory name index and free-space hints, bounded and droppable.
    /// Here because the block cache is the one long-lived, `&mut`-everywhere
    /// piece of mount state, and because a rollback's bookkeeping is what says
    /// which directories the index may no longer speak for.
    dir_index: DirIndexSet,
    /// Valid entries whose frame is dirty, kept as the frames change so the
    /// per-operation flusher hint costs nothing.
    dirty: usize,
    /// Blocks freed since the log was last durable. The allocator skips them:
    /// data goes home ahead of the log, and a crash that lost the free would
    /// leave another file's bytes in its old owner's block — jbd2's rule for
    /// blocks freed in an uncommitted transaction.
    freed: KBTreeMap<u32, ()>,
    /// The open operation's frees, kept apart from `freed` because a log sync
    /// inside the operation does not make them durable.
    op_freed: KBTreeMap<u32, ()>,
    inflight: Inflight,
}

impl BlockCache {
    /// The cache, on the heap — which every caller wants, because the 2 KiB
    /// stack gate refuses a frame carrying one alongside an `Ext2Fs`. Written
    /// field by field, so no whole-`BlockCache` rvalue lands on a frame.
    #[inline(never)]
    pub fn new_boxed(block_size: u32, target_entries: usize) -> Result<KBox<Self>, Ext2Error> {
        KBox::try_init(Self::init(block_size, target_entries)).map_err(|_| Ext2Error::OutOfMemory)
    }

    fn init(block_size: u32, target_entries: usize) -> impl Init<Self, AllocError> {
        // Callers validate this; a larger size would silently truncate
        // sub-block reads.
        debug_assert!(block_size as usize <= EXT2_MAX_BLOCK_SIZE as usize);
        let capacity = target_entries.clamp(CACHE_ENTRIES_MIN, CACHE_ENTRIES_MAX);
        // The rest are allocated by the misses that need them.
        let initial = capacity.min(CACHE_ENTRIES_MIN);
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, entries, Self::build_entries(initial)?);
                write_field!(slot, index, KBTreeMap::new());
                write_field!(slot, capacity, capacity);
                write_field!(slot, lru_head, initial as u32 - 1);
                write_field!(slot, lru_tail, 0);
                write_field!(slot, block_size, block_size);
                write_field!(slot, unbarriered, 0);
                write_field!(slot, undo, KVec::new());
                write_field!(slot, undo_overflow, false);
                write_field!(slot, op_depth, 0);
                write_field!(slot, epoch, 1);
                write_field!(slot, journal, None);
                write_field!(slot, scratch, KVec::new());
                write_field!(slot, op_account, AccountId::NONE);
                write_field!(slot, op_charged, 0);
                write_field!(slot, op_cancelled, 0);
                write_field!(slot, charges, BlockCharges::new().map_err(|_| AllocError)?);
                write_field!(slot, op_data_evicted, false);
                write_field!(slot, op_touched_slots, KVec::with_capacity(capacity)?);
                write_field!(slot, op_dirs, [0u32; OP_DIRS_MAX]);
                write_field!(slot, op_dirs_len, 0);
                write_field!(slot, op_dirs_overflow, false);
                write_field!(slot, group_hints, KVec::new());
                write_field!(slot, last_group, 0);
                write_field!(slot, dir_index, DirIndexSet::new());
                write_field!(slot, dirty, 0);
                write_field!(slot, freed, KBTreeMap::new());
                write_field!(slot, op_freed, KBTreeMap::new());
                write_field!(slot, inflight, Inflight::new()?);
                Ok(slot.finish())
            },
        )
    }

    /// The frames, linked into the recency chain tail first so the initial
    /// fill hands out slot 0 upwards.
    fn build_entries(count: usize) -> Result<Slots, AllocError> {
        let mut entries = Slots::new();
        for i in 0..count {
            let mut entry = CacheEntry::new().map_err(|_| AllocError)?;
            entry.lru_prev = if i + 1 < count { i as u32 + 1 } else { NIL };
            entry.lru_next = if i == 0 { NIL } else { i as u32 - 1 };
            entries.push(entry)?;
        }
        Ok(entries)
    }

    /// Hand the mount's redo log to the cache. Once installed, an operation's
    /// atomicity comes from the log rather than from undo snapshots.
    pub fn install_journal(&mut self, journal: KBox<Journal>) -> Result<(), Ext2Error> {
        // Sized to the cache, not to one record: an operation can dirty every
        // slot, and a commit must not allocate.
        self.scratch = KVec::with_capacity(self.capacity).map_err(|_| Ext2Error::OutOfMemory)?;
        self.journal = Some(journal);
        Ok(())
    }

    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_deref()
    }

    pub fn journal_mut(&mut self) -> Option<&mut Journal> {
        self.journal.as_deref_mut()
    }

    /// Whether the log has room for another operation without a check point
    /// first. `true` with no log at all: there is nothing to run out of.
    pub fn journal_has_headroom(&self) -> bool {
        self.journal.as_ref().is_none_or(|j| j.has_headroom())
    }

    /// Charge `blocks` to `account` for `ino`, for the open operation.
    ///
    /// Op-scoped because the transaction scope lives here: a rollback restores
    /// the bitmaps, so it owes the charge back too. `try_charge` takes no lock
    /// and allocates nothing, so this is legal under the mount lock. `ino` is
    /// what makes the refund answerable — see [`super::blockcharge`].
    pub fn charge_blocks(
        &mut self,
        account: AccountId,
        ino: Option<u32>,
        blocks: u32,
    ) -> Result<(), Ext2Error> {
        if blocks == 0 {
            return Ok(());
        }
        quota::charge_blocks(account, blocks).map_err(|_| Ext2Error::NoSpace)?;
        self.op_account = account;
        self.op_charged = self.op_charged.saturating_add(blocks);
        self.charges.charge(ino, account, blocks);
        Ok(())
    }

    /// Give `blocks` of `ino` back to whoever is charged for them, at the
    /// commit rather than here: a rollback restores the bitmap and the
    /// operation would still owe them. The freeing principal is not a
    /// parameter because it is not the one being credited.
    pub fn note_blocks_freed(&mut self, ino: Option<u32>, blocks: u32) {
        if blocks == 0 {
            return;
        }
        self.charges.free(ino, blocks);
        if self.op_depth == 0 {
            self.charges.commit();
        }
    }

    /// Give back a charge whose allocation never happened.
    ///
    /// Not a free: no block changed hands, so only this operation's own record
    /// is undone. Deferred to the commit because a rollback refunds the whole
    /// of `op_charged`, this charge among it.
    pub fn cancel_block_charge(&mut self, account: AccountId, ino: Option<u32>, blocks: u32) {
        if blocks == 0 {
            return;
        }
        self.charges.cancel(ino, account, blocks);
        if self.op_depth == 0 {
            quota::refund_blocks(account, blocks);
            return;
        }
        self.op_cancelled = self.op_cancelled.saturating_add(blocks);
    }

    fn settle_charges(&mut self, committed: bool) {
        self.op_data_evicted = false;
        let (account, charged, cancelled) = (self.op_account, self.op_charged, self.op_cancelled);
        self.op_account = AccountId::NONE;
        self.op_charged = 0;
        self.op_cancelled = 0;
        if committed {
            self.charges.commit();
            quota::refund_blocks(account, cancelled);
        } else {
            self.charges.rollback();
            quota::refund_blocks(account, charged);
        }
    }

    /// Whether the log has filled far enough that the flusher should drain it.
    /// `false` with no log: there is nothing to drain.
    pub fn journal_needs_drain(&self) -> bool {
        self.journal.as_ref().is_some_and(|j| j.needs_drain())
    }

    /// Note a block as freed, so no log record written before this point is
    /// ever replayed into it.
    pub fn note_revoke(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        // A full revoke list is written as a record of its own.
        self.ensure_log_room(device, 1)?;
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = journal.note_revoke(block.raw(), device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// The open operation is writing `block` home ahead of the log.
    fn supersede_logged(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        self.ensure_log_room(device, 1)?;
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = journal.supersede(block.raw(), device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// Supersede the log's copies of every data block this commit writes home,
    /// before the log's room for the commit is measured.
    fn supersede_op_data(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        for k in 0..self.op_touched_slots.len() {
            let slot = self.op_touched_slots.as_slice()[k] as usize;
            if Self::goes_home(&self.entries[slot]) {
                self.supersede_logged(self.entries[slot].block, device)?;
            }
        }
        Ok(())
    }

    /// The epoch a writeback pass should fix. Blocks dirtied after it are a
    /// later operation's and are left for the next pass.
    pub fn writeback_epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether the open scope has outgrown its undo record. A `true` here is
    /// what turns an operation too large to undo into a refusal rather than a
    /// partial commit.
    pub fn op_undo_overflowed(&self) -> bool {
        self.undo_overflow
    }

    /// Open a rollback scope. Nested calls only count: an inner failure
    /// propagates outwards and the outermost scope is what rolls back, so a
    /// composite operation is undone as one.
    pub fn begin_op(&mut self) {
        self.op_depth += 1;
        if self.op_depth > 1 {
            return;
        }
        stats::note_transaction();
        self.epoch = self.epoch.wrapping_add(1);
        self.undo.clear();
        self.undo_overflow = false;
        self.forget_op_slots();
        if let Some(journal) = self.journal.as_mut() {
            journal.begin_op();
        }
    }

    /// Accept every mutation the scope made.
    ///
    /// With a log this is where the operation becomes real, through
    /// [`Self::log_transaction`]. Without one the blocks merely stay dirty for
    /// the flusher, `sync`, or eviction to publish.
    pub fn commit_op(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.op_depth > 1 {
            self.op_depth -= 1;
            return Ok(());
        }
        // Before the flags are cleared: the log records are selected by them.
        self.log_transaction(device)?;
        self.forget_op_frees();
        self.op_depth = 0;
        self.settle_charges(true);
        self.undo.clear();
        self.undo_overflow = false;
        for k in 0..self.op_touched_slots.len() {
            let i = self.op_touched_slots.as_slice()[k] as usize;
            if self.entries[i].op_invalidated && self.home_unlogged_data(device, i) {
                self.drop_entry(i);
            }
        }
        self.forget_op_slots();
        Ok(())
    }

    /// Whether slot `i`, a block this operation freed, may be dropped: with a
    /// log, a dirty data block with no record is the only copy of what an
    /// earlier operation — whose commit may reach the medium before this
    /// one's — published, so it goes home first, as that commit's ordered
    /// data. A failed write keeps the entry dirty for the next data flush.
    fn home_unlogged_data(&mut self, device: &dyn BlockDevice, i: usize) -> bool {
        let entry = &self.entries[i];
        let Some(journal) = self.journal.as_ref() else {
            return true;
        };
        if entry.kind != BlockKind::Data
            || !entry.valid
            || !entry.frame.dirty()
            || journal.resident_slot(entry.block.raw()).is_some()
        {
            return true;
        }
        let offset = entry.block.to_disk_offset(self.block_size).raw();
        let bs = self.block_size as usize;
        if device
            .write_at(offset, &entry.frame.as_bytes()[..bs])
            .is_err()
        {
            return false;
        }
        self.set_dirty(i, false);
        self.unbarriered += 1;
        true
    }

    /// Put every block the scope touched back the way it was.
    ///
    /// With a log no home block carries the operation's changes, so dropping
    /// every entry it touched is the whole of it — except a data block that
    /// was already dirty and has no record: a large write's data waits in the
    /// cache for the next log sync, so this entry is its only copy, and data
    /// may keep a failed write's bytes as a short write keeps them. Without a
    /// log the scope is cache-deep: an eviction may already have put a
    /// touched block on the device, which is why [`Self::find_or_evict`] makes
    /// it the last resort.
    pub fn rollback_op(&mut self) {
        self.op_depth = self.op_depth.saturating_sub(1);
        if self.op_depth > 0 {
            return;
        }
        self.forget_op_dir_indexes();
        self.forget_op_frees();
        if let Some(mut journal) = self.journal.take() {
            journal.abort_op();
            for k in 0..self.op_touched_slots.len() {
                let i = self.op_touched_slots.as_slice()[k] as usize;
                let entry = &self.entries[i];
                if !entry.op_touched {
                    continue;
                }
                // In flight counts as dirty: until its batch finishes, the
                // device may not hold what the cache marked clean.
                let sole_copy = entry.kind == BlockKind::Data
                    && (entry.op_was_dirty || entry.inflight)
                    && journal.resident_slot(entry.block.raw()).is_none();
                if !sole_copy {
                    self.drop_entry(i);
                }
            }
            self.journal = Some(journal);
            self.clear_op_flags();
            return;
        }
        let bs = self.block_size as usize;
        let epoch = self.epoch;
        // Snapshots first. A block that was dirty on first touch has its
        // committed contents only here, so restoring must win over the
        // discard pass below, which a later eviction-and-re-acquire of the
        // same block would otherwise have flagged.
        while let Some(record) = self.undo.pop() {
            let Some(&slot) = self.index.get(&record.block) else {
                continue;
            };
            let n = bs.min(record.snapshot.len());
            self.entries[slot].frame.as_bytes_mut()[..n]
                .copy_from_slice(&record.snapshot.as_slice()[..n]);
            self.set_dirty(slot, true);
            let entry = &mut self.entries[slot];
            entry.dirty_epoch = epoch;
            entry.op_restored = true;
        }
        for k in 0..self.op_touched_slots.len() {
            let i = self.op_touched_slots.as_slice()[k] as usize;
            if self.entries[i].op_discard && !self.entries[i].op_restored {
                self.drop_entry(i);
            }
        }
        self.clear_op_flags();
    }

    /// Retract the name index of every inode the failed scope touched: the
    /// blocks are about to go back to their committed contents, and an index
    /// is a claim about what those blocks hold — including, once complete,
    /// the claim that a name is *not* there.
    fn forget_op_dir_indexes(&mut self) {
        if self.op_dirs_overflow {
            self.dir_index.clear();
            return;
        }
        for k in 0..self.op_dirs_len as usize {
            self.dir_index.forget(self.op_dirs[k]);
        }
    }

    /// Record that the open operation is working on `owner`'s blocks.
    ///
    /// On acquisition, the only moment the block and the inode it belongs to
    /// are known together: a later eviction gives the slot to another owner.
    fn note_op_dir(&mut self, owner: BlockOwner) {
        if self.op_depth == 0 {
            return;
        }
        let Some(ino) = owner.charged_inode() else {
            return;
        };
        let len = self.op_dirs_len as usize;
        if self.op_dirs[..len].contains(&ino) {
            return;
        }
        if len == OP_DIRS_MAX {
            self.op_dirs_overflow = true;
            return;
        }
        self.op_dirs[len] = ino;
        self.op_dirs_len += 1;
    }

    /// A candidate position for `name`'s hash, or the index's verdict on the
    /// whole directory. `cursor` starts at zero and carries the probe across
    /// the block read each candidate costs.
    pub fn dir_probe(&mut self, ino: u32, hash: u32, cursor: &mut u32) -> DirProbe {
        self.dir_index.probe(ino, hash, cursor)
    }

    /// Move the index set out for the length of a scan.
    ///
    /// A scan needs the cache mutably for every block it reads and the index
    /// mutably for every record it files, and the index lives in the cache.
    /// [`DirIndexSet::new`] allocates nothing, so the swap is a move of a
    /// couple of hundred bytes.
    pub fn take_dir_index(&mut self) -> DirIndexSet {
        core::mem::take(&mut self.dir_index)
    }

    pub fn put_dir_index(&mut self, index: DirIndexSet) {
        self.dir_index = index;
    }

    /// Where an insert into `ino` should resume, and what the passes that set
    /// it proved about the blocks below.
    pub fn dir_free_hint(&self, ino: u32) -> (u32, u32) {
        self.dir_index.free_hint(ino)
    }

    pub fn set_dir_hint(&mut self, ino: u32, block: u32, proof: u32) {
        self.dir_index.set_hint(ino, block, proof);
    }

    pub fn lower_dir_free_hint(&mut self, ino: u32, block: u32) {
        self.dir_index.lower_free_hint(ino, block);
    }

    pub fn note_dir_insert(&mut self, ino: u32, hash: u32, pos: u32) {
        self.dir_index.note_insert(ino, hash, pos);
    }

    pub fn note_dir_remove(&mut self, ino: u32, hash: u32, pos: u32) {
        self.dir_index.note_remove(ino, hash, pos);
    }

    /// Drop everything the index believes about `ino` — its names and its
    /// free-space hint. Owed whenever the inode itself stops being the
    /// directory the index was built from.
    pub fn forget_dir_index(&mut self, ino: u32) {
        self.dir_index.forget(ino);
    }

    /// Whether `ino`'s index can answer a miss without a scan.
    pub fn dir_index_complete(&self, ino: u32) -> bool {
        self.dir_index.is_complete(ino)
    }

    /// Also settles the operation's disk charges, which is why the rollback
    /// and commit paths both end here.
    fn clear_op_flags(&mut self) {
        self.settle_charges(false);
        self.undo_overflow = false;
        self.forget_op_slots();
    }

    /// Record that `slot` carries state belonging to the open operation.
    ///
    /// The list is preallocated to the cache's capacity and a slot joins it at
    /// most once per operation, so this never allocates — which is what lets a
    /// commit be allocation-free.
    fn note_op_slot(&mut self, slot: usize) {
        if self.entries[slot].op_listed {
            return;
        }
        debug_assert!(self.op_touched_slots.len() < self.op_touched_slots.capacity());
        if self.op_touched_slots.push(slot as u32).is_ok() {
            self.entries[slot].op_listed = true;
        }
    }

    /// Drop every per-operation record, walking the slots the operation
    /// reached rather than the whole cache.
    fn forget_op_slots(&mut self) {
        for k in 0..self.op_touched_slots.len() {
            let i = self.op_touched_slots.as_slice()[k] as usize;
            let entry = &mut self.entries[i];
            entry.op_touched = false;
            // The invalidations the operation asked for are undone with it:
            // the blocks it was freeing are still the inode's.
            entry.op_invalidated = false;
            entry.op_discard = false;
            entry.op_was_dirty = false;
            entry.op_restored = false;
            entry.op_listed = false;
        }
        self.op_touched_slots.clear();
        self.op_dirs_len = 0;
        self.op_dirs_overflow = false;
    }

    /// Publish the open operation's metadata through the log.
    ///
    /// Data is never logged: it stays dirty here, and [`Self::sync_log`] writes
    /// it home and barriers before any record that names it reaches the
    /// medium — `data=ordered` — so a commit issues no I/O at all unless the
    /// ring is full or the operation outgrew it. Logging it as well would
    /// write every block twice for a barrier the group commit pays anyway.
    fn log_transaction(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let Some(per_record) = self.journal.as_ref().map(|j| j.max_entries().max(1)) else {
            return Ok(());
        };
        // Staged and reserved *before* anything is published: a commit that
        // ran out of log room afterwards would retract the metadata with the
        // data already on the medium.
        let op_data = self.count_op_data();
        if op_data > 0 {
            // A full revoke list takes a slot of its own.
            self.ensure_log_room(device, (op_data / per_record + 1) as u32)?;
            self.supersede_op_data(device)?;
        }
        self.stage_metadata()?;
        let needed = self.log_slots_needed();
        // Making room seals the open compound, whose commit record the log
        // must also hold. The ring needs no such slot: `pending_room` holds
        // it back already, so charging it there too would write a compound
        // out one operation early.
        if self.journal.as_ref().is_some_and(|journal| {
            journal.free_slots() < needed + u32::from(journal.has_unsealed())
        }) {
            return Err(Ext2Error::NoSpace);
        }
        self.ensure_log_room(device, needed)?;
        if self.journal.as_ref().is_some_and(|j| j.writes_through()) {
            self.home_data_for_records(device)?;
        }

        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = self.log_records(&mut journal, device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// Records about to go straight to the medium: every dirty data block
    /// home and a barrier behind them first, `data=ordered`. The barrier is
    /// owed even with nothing written here when an eviction already put one
    /// of this operation's data blocks home.
    fn home_data_for_records(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let wrote = self.flush_where(device, |kind, _| kind == BlockKind::Data)?;
        if wrote > 0 || self.op_data_evicted {
            self.barrier(device)?;
        }
        Ok(())
    }

    fn barrier(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        device.flush().map_err(Ext2Error::from)?;
        self.note_barrier();
        Ok(())
    }

    /// Room in the ring for `want` more slots. A full ring is written out
    /// first; an operation larger than the whole ring writes its own records
    /// through, behind a log made durable.
    fn ensure_log_room(&mut self, device: &dyn BlockDevice, want: u32) -> Result<(), Ext2Error> {
        if !self
            .journal
            .as_ref()
            .is_some_and(|j| j.defers() && j.pending_room() < want)
        {
            return Ok(());
        }
        self.sync_log(device)?;
        if let Some(journal) = self.journal.as_mut()
            && journal.pending_room() < want
        {
            journal.set_write_through();
        }
        Ok(())
    }

    /// Make every appended record durable: dirty data home, a barrier, the
    /// log's ring, a barrier. What a home write of logged metadata, an
    /// `fsync`, a check point and a full ring all wait for.
    ///
    /// Legal inside an operation: the data it writes home may include the
    /// open operation's, which a rollback then leaves as a failed write
    /// leaves its bytes, and the open operation's records carry no commit.
    pub fn sync_log(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let Some(journal) = self.journal.as_ref() else {
            return Ok(());
        };
        if journal.is_durable() && self.unbarriered == 0 {
            return Ok(());
        }
        if journal.owes_write() {
            self.flush_where(device, |kind, _| kind == BlockKind::Data)?;
            if self.unbarriered > 0 {
                self.barrier(device)?;
            }
            let Some(mut journal) = self.journal.take() else {
                return Ok(());
            };
            let result = journal.write_pending(device);
            self.unbarriered += journal.take_writes();
            self.journal = Some(journal);
            result?;
        }
        self.barrier(device)
    }

    /// Close the log's open compound with its commit record, staged in the
    /// ring. What a writeback pass does as it opens: the records below its
    /// limit then end at a commit of their own, so no operation that runs
    /// while the pass is open — whose data the pass does not write — can
    /// join a transaction the pass commits.
    pub fn seal_log(&mut self) {
        if let Some(journal) = self.journal.as_mut() {
            journal.seal_ring();
        }
    }

    /// One bounded step of writing the ring's records below `limit` out.
    /// Answers whether any remain. No barrier: the pass issues it.
    pub fn write_log_until(
        &mut self,
        device: &dyn BlockDevice,
        limit: u32,
        budget: usize,
    ) -> Result<bool, Ext2Error> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(false);
        };
        let result = journal.write_pending_until(limit, budget, device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// Whether every record below `limit` is on the medium behind a barrier.
    pub fn log_durable_below(&self, limit: u32) -> bool {
        self.journal
            .as_ref()
            .is_none_or(|j| j.slot_durable(limit.min(j.head()).saturating_sub(1)))
    }

    /// Records the ring holds that the device has not taken yet.
    pub fn journal_pending(&self) -> u32 {
        self.journal.as_ref().map_or(0, |j| j.pending_slots())
    }

    /// Whether the flusher should write the ring out before an operation
    /// finds it full.
    pub fn journal_ring_filling(&self) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|j| j.defers() && j.pending_slots() * 2 >= j.pending_capacity())
    }

    /// Whether `block`'s home may be written now: its newest record, if it
    /// has one, is durable.
    fn home_write_allowed(&self, block: BlockNum) -> bool {
        self.journal.as_ref().is_none_or(|j| {
            j.resident_slot(block.raw())
                .is_none_or(|slot| j.slot_durable(slot))
        })
    }

    /// A block the open operation freed. See [`Self::freed`].
    pub fn note_block_freed(&mut self, block: BlockNum) {
        if self.journal.is_none() {
            return;
        }
        let set = if self.op_depth > 0 {
            &mut self.op_freed
        } else {
            &mut self.freed
        };
        set.insert(block.raw(), ());
    }

    /// Whether an allocation must pass `block` over: see [`Self::freed`].
    pub fn reuse_blocked(&self, block: BlockNum) -> bool {
        self.freed.contains_key(&block.raw()) || self.op_freed.contains_key(&block.raw())
    }

    /// Whether any free still waits on a log sync, so a search that found
    /// nothing may find something after one.
    pub fn has_blocked_frees(&self) -> bool {
        !self.freed.is_empty() || !self.op_freed.is_empty()
    }

    /// Everything appended is durable now, so only the open operation's own
    /// frees still name a record the medium lacks.
    fn prune_freed(&mut self) {
        self.freed.clear();
    }

    /// The open operation's frees stay blocked until the next durable point
    /// whichever way it ends: committed, they are in a record the medium may
    /// lack; rolled back, the bitmap owns them again anyway.
    fn forget_op_frees(&mut self) {
        for &block in self.op_freed.keys() {
            self.freed.insert(block, ());
        }
        self.op_freed.clear();
    }

    /// A dirty *data* block of the open operation: written home and barriered
    /// behind, never put in the log.
    fn goes_home(entry: &CacheEntry) -> bool {
        entry.valid && entry.op_touched && entry.frame.dirty() && entry.kind == BlockKind::Data
    }

    /// Dirty data blocks the open operation touched.
    fn count_op_data(&self) -> usize {
        let mut count = 0usize;
        for k in 0..self.op_touched_slots.len() {
            if Self::goes_home(&self.entries[self.op_touched_slots.as_slice()[k] as usize]) {
                count += 1;
            }
        }
        count
    }

    /// The dirty run that starts at `first`, as cache slots: the *device's*
    /// run, not the cache's — consecutive block numbers, whichever slots hold
    /// them, extended while `keep` accepts the next block and bounded by
    /// [`FLUSH_RUN`] and `max`. `keep` is what keeps a run inside one phase.
    fn plan_run(
        &self,
        first: usize,
        max: usize,
        keep: &mut dyn FnMut(&CacheEntry) -> bool,
        run: &mut [u32; FLUSH_RUN],
    ) -> usize {
        let base = self.entries[first].block;
        run[0] = first as u32;
        let mut len = 1usize;
        while len < FLUSH_RUN.min(max) {
            let Some(raw) = base.raw().checked_add(len as u32) else {
                break;
            };
            let Some(&peer) = self.index.get(&BlockNum(raw)) else {
                break;
            };
            if !keep(&self.entries[peer]) {
                break;
            }
            if self.entries[peer].kind == BlockKind::Metadata
                && !self.home_write_allowed(BlockNum(raw))
            {
                break;
            }
            run[len] = peer as u32;
            len += 1;
        }
        len
    }

    /// Hand `run` to the device as one gathered request, and mark it clean:
    /// the bytes are the device's once this returns, and a scan that reaches
    /// another block of the run before it completes must not send it twice.
    /// [`Self::finish_run`] puts back what a failure owes. With no slot free
    /// and no other run in flight it writes synchronously, since waiting then
    /// holds nothing up.
    ///
    /// `#[inline(never)]`: the segment array is 512 bytes of frame no caller
    /// can afford on top of its own.
    #[inline(never)]
    fn submit_run(
        &mut self,
        device: &dyn BlockDevice,
        inflight: usize,
    ) -> Result<WriteTicket, BlockDeviceError> {
        let bs = self.block_size as usize;
        let len = usize::from(self.inflight.lens[inflight]);
        let run = self.inflight.runs[inflight];
        let offset = self.entries[run[0] as usize]
            .block
            .to_disk_offset(self.block_size)
            .raw();
        let ticket = {
            let mut segs: [&[u8]; FLUSH_RUN] = [&[]; FLUSH_RUN];
            for k in 0..len {
                segs[k] = &self.entries[run[k] as usize].frame.as_bytes()[..bs];
            }
            match device.submit_write(offset, &segs[..len]) {
                Err(BlockDeviceError::Busy) if self.inflight.count == 1 => {
                    device.write_vectored(offset, &segs[..len])?;
                    WriteTicket::new(WriteTicket::DONE, 0)
                }
                submitted => submitted?,
            }
        };
        for k in 0..len {
            self.set_dirty(run[k] as usize, false);
        }
        Ok(ticket)
    }

    /// Write a submitted run again, synchronously: its blocks are marked clean
    /// but their frames are untouched until [`Self::finish_run`].
    #[inline(never)]
    fn rewrite_run(&self, device: &dyn BlockDevice, inflight: usize) -> Result<(), Ext2Error> {
        let bs = self.block_size as usize;
        let len = usize::from(self.inflight.lens[inflight]);
        let run = &self.inflight.runs[inflight];
        let offset = self.entries[run[0] as usize]
            .block
            .to_disk_offset(self.block_size)
            .raw();
        let mut segs: [&[u8]; FLUSH_RUN] = [&[]; FLUSH_RUN];
        for k in 0..len {
            segs[k] = &self.entries[run[k] as usize].frame.as_bytes()[..bs];
        }
        device
            .write_vectored(offset, &segs[..len])
            .map_err(Ext2Error::from)
    }

    /// A submitted run's outcome: on failure every block of it is dirty again,
    /// in the epoch it was dirtied in, for the next attempt.
    fn finish_run(&mut self, inflight: usize, ok: bool) -> usize {
        let len = usize::from(self.inflight.lens[inflight]);
        if ok {
            return len;
        }
        for k in 0..len {
            let slot = self.inflight.runs[inflight][k] as usize;
            let entry = &mut self.entries[slot];
            if !entry.frame.dirty() {
                entry.frame.set_dirty(true);
                self.dirty += 1;
            }
        }
        0
    }

    /// Collect the metadata blocks the commit will log.
    fn stage_metadata(&mut self) -> Result<(), Ext2Error> {
        self.scratch.clear();
        for k in 0..self.op_touched_slots.len() {
            let entry = &self.entries[self.op_touched_slots.as_slice()[k] as usize];
            if entry.kind == BlockKind::Metadata
                && entry.valid
                && entry.op_touched
                && entry.frame.dirty()
                && !entry.op_invalidated
            {
                self.scratch
                    .push(entry.block.raw())
                    .map_err(|_| Ext2Error::OutOfMemory)?;
            }
        }
        Ok(())
    }

    /// Slots the staged metadata plus its headers, the queued revokes and the
    /// commit record will take.
    fn log_slots_needed(&self) -> u32 {
        let Some(journal) = self.journal.as_ref() else {
            return 0;
        };
        let per_record = journal.max_entries().max(1);
        let payloads = self.scratch.len();
        let records = payloads.div_ceil(per_record);
        // One revoke record per full entry list, plus the commit record.
        let revokes = journal.queued_revokes().div_ceil(per_record);
        (payloads + records + revokes + 1) as u32
    }

    fn log_records(
        &mut self,
        journal: &mut Journal,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        journal.flush_revokes(device)?;
        let bs = self.block_size as usize;
        let appended = self.partition_rewrites(journal)?;
        let per_record = journal.max_entries();
        let mut done = 0usize;
        while done < appended {
            let take = (appended - done).min(per_record);
            let targets = &self.scratch.as_slice()[done..done + take];
            let (index, entries) = (&self.index, &self.entries);
            // The log gathers these; the commit record is still a separate
            // write after every one of them, which is the only ordering the
            // log itself needs.
            journal.write_record(targets, device, &mut |k| {
                let slot = *index.get(&BlockNum(targets[k]))?;
                Some(&entries[slot].frame.as_bytes()[..bs])
            })?;
            done += take;
        }
        // Last, once nothing can fail: a rewrite keeps no copy of the image it
        // replaces, so an abort after one could not put the earlier
        // operation's contents back. `commit_op` can only fail writing
        // through, and a log writing through has nothing rewritable.
        for k in appended..self.scratch.len() {
            let block = self.scratch.as_slice()[k];
            let slot = journal.rewritable_slot(block);
            let entry = self.index.get(&BlockNum(block)).copied();
            // Both checked by the partition, and the appends only index
            // other blocks.
            debug_assert!(slot.is_some() && entry.is_some());
            if let (Some(slot), Some(entry)) = (slot, entry) {
                journal.rewrite(slot, &self.entries[entry].frame.as_bytes()[..bs]);
            }
        }
        journal.commit_op(device)
    }

    /// Order the staged blocks so those whose newest record is still an
    /// unwritten image of the open transaction come last, and answer how many
    /// come before them. Those are rewritten in place rather than appended:
    /// the compound's one commit covers whatever its images finally hold.
    ///
    /// Decided before anything is appended, since a block appended here would
    /// itself read as rewritable. Every block's cache entry is checked now,
    /// which is what lets the rewrites after the appends not fail.
    fn partition_rewrites(&mut self, journal: &Journal) -> Result<usize, Ext2Error> {
        let mut appended = 0usize;
        for k in 0..self.scratch.len() {
            let block = self.scratch.as_slice()[k];
            if !self.index.contains_key(&BlockNum(block)) {
                return Err(Ext2Error::DeviceError);
            }
            if journal.rewritable_slot(block).is_none() {
                self.scratch.as_mut_slice().swap(k, appended);
                appended += 1;
            }
        }
        Ok(appended)
    }

    /// Forget a slot's contents without writing them back.
    ///
    /// An already-invalid slot is left alone: it keeps its old block number,
    /// so unindexing on that number would unindex whichever live slot holds
    /// it now.
    fn drop_entry(&mut self, slot: usize) {
        if !self.entries[slot].valid {
            return;
        }
        let block = self.entries[slot].block;
        self.set_dirty(slot, false);
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.pinned = 0;
        entry.inflight = false;
        entry.frame.set_owner_key(0);
        self.index.remove(&block);
        self.lru_retire(slot);
    }

    /// Set a valid entry's dirty bit, keeping [`Self::dirty`] in step.
    fn set_dirty(&mut self, slot: usize, dirty: bool) {
        let entry = &mut self.entries[slot];
        if entry.frame.dirty() == dirty {
            return;
        }
        entry.frame.set_dirty(dirty);
        if dirty {
            entry.dirtied_epoch = self.epoch;
            self.dirty += 1;
        } else {
            self.dirty -= 1;
        }
    }

    /// Record how `slot` is put back, before the caller can mutate it.
    ///
    /// Taken on *acquire* rather than on the first `data_mut`, because that
    /// accessor cannot fail and a snapshot allocates; reads therefore record
    /// too, which costs one copy per distinct dirty metadata block reached.
    ///
    /// Every fallible step happens before the entry is mutated. An entry
    /// mutated with no undo record behind it is a block the rollback cannot
    /// see, which a later flush would publish over live data.
    fn note_op_touch(&mut self, slot: usize) -> Result<(), Ext2Error> {
        if self.op_depth == 0 || self.entries[slot].op_touched {
            return Ok(());
        }
        if self.journal.is_some() {
            // No home block carries this operation's changes, so the rollback
            // re-reads rather than restores — unless this is the only copy.
            let entry = &mut self.entries[slot];
            entry.op_touched = true;
            entry.op_was_dirty = entry.frame.dirty();
            self.note_op_slot(slot);
            return Ok(());
        }
        if !self.entries[slot].frame.dirty() {
            // Clean: the device holds the committed contents, so the rollback
            // is a drop and needs no snapshot.
            self.entries[slot].op_touched = true;
            self.entries[slot].op_discard = true;
            self.note_op_slot(slot);
            return Ok(());
        }
        if self.undo.len() >= MAX_UNDO {
            // An eviction clears `op_touched`, so a long operation can record
            // the same dirty block afresh. Refusing bounds the snapshot memory
            // and turns the excess into a rollback rather than an allocation
            // storm that fails somewhere less recoverable.
            self.undo_overflow = true;
            return Err(Ext2Error::NoSpace);
        }
        let block = self.entries[slot].block;
        let bs = self.block_size as usize;
        let mut snapshot = KVec::<u8>::zeroed(bs).map_err(|_| Ext2Error::OutOfMemory)?;
        snapshot
            .as_mut_slice()
            .copy_from_slice(&self.entries[slot].frame.as_bytes()[..bs]);
        self.undo
            .push(UndoEntry { block, snapshot })
            .map_err(|_| Ext2Error::OutOfMemory)?;
        self.entries[slot].op_touched = true;
        self.note_op_slot(slot);
        Ok(())
    }

    /// Mark a freshly acquired block for discard on rollback.
    ///
    /// Infallible, which is what lets the acquire paths set it *after* the
    /// entry is installed: a block just read or zeroed has no committed cache
    /// state to preserve, so one bit is the whole record.
    fn note_op_fresh(&mut self, slot: usize) {
        if self.op_depth == 0 {
            return;
        }
        self.entries[slot].op_touched = true;
        self.entries[slot].op_discard = true;
        self.note_op_slot(slot);
    }

    pub fn unbarriered_writes(&self) -> usize {
        self.unbarriered
    }

    /// Call after issuing a [`BlockDevice::flush`].
    pub fn note_barrier(&mut self) {
        self.unbarriered = 0;
        let durable = self.journal.as_mut().is_some_and(|journal| {
            journal.note_barrier();
            journal.is_durable()
        });
        if durable {
            self.prune_freed();
        }
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Size the per-group allocation hints from the volume's group count, once
    /// per mount. A group count whose table would outgrow [`GROUP_HINTS_MAX`]
    /// gets none: a missing hint costs a bitmap scan, not correctness.
    pub fn size_group_hints(&mut self, groups: u32) {
        if self.group_hints.len() == groups as usize {
            return;
        }
        self.group_hints = KVec::new();
        self.last_group = 0;
        if groups == 0 || groups as usize > GROUP_HINTS_MAX {
            return;
        }
        let Ok(mut hints) = KVec::<u32>::with_capacity(groups as usize) else {
            return;
        };
        for _ in 0..groups {
            if hints.push(0).is_err() {
                return;
            }
        }
        self.group_hints = hints;
    }

    /// The bit a scan of `group`'s block bitmap should start from: everything
    /// below it was allocated by an earlier search and rescanning it is what
    /// makes a nearly-full volume quadratic.
    pub fn group_hint(&self, group: u32) -> usize {
        self.group_hints
            .as_slice()
            .get(group as usize)
            .copied()
            .unwrap_or(0) as usize
    }

    /// Record where `group`'s next scan should start. `0` resets it, which is
    /// what a search that found nothing above the hint owes.
    pub fn set_group_hint(&mut self, group: u32, bit: u32) {
        if let Some(hint) = self.group_hints.as_mut_slice().get_mut(group as usize) {
            *hint = bit;
        }
    }

    /// The group an allocation with no goal of its own sweeps from.
    pub fn alloc_group(&self) -> u32 {
        self.last_group
    }

    pub fn set_alloc_group(&mut self, group: u32) {
        self.last_group = group;
    }

    /// Get a metadata block, reading from the device on a miss.
    pub fn get<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Metadata, BlockOwner::Other)
    }

    pub fn get_owned<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Metadata, owner)
    }

    /// Get a file-data block from the cache (see [`BlockKind`]).
    pub fn get_data<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.get_kind(block, device, BlockKind::Data, owner)
    }

    fn get_kind<'a>(
        &'a mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        kind: BlockKind,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'a>, Ext2Error> {
        self.note_op_dir(owner);
        if let Some(&slot) = self.index.get(&block) {
            self.note_op_touch(slot)?;
            self.lru_touch(slot);
            self.entries[slot].pinned += 1;
            self.entries[slot].owner = owner;
            // Re-reached after a deferred invalidation: this operation is
            // using the block again, so the commit must not drop it.
            self.entries[slot].op_invalidated = false;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let bs = self.block_size as usize;
        let home = block.to_disk_offset(self.block_size).raw();
        let staged = self.journal.take();
        let logged = staged.as_ref().and_then(|j| j.resident_slot(block.raw()));
        let read = {
            let entry = &mut self.entries[slot];
            let buffer = &mut entry.frame.as_bytes_mut()[..bs];
            match (logged, staged.as_ref()) {
                (Some(log_slot), Some(journal)) => journal.read_slot(log_slot, device, buffer),
                _ => device.read_at(home, buffer).map_err(Ext2Error::from),
            }
        };
        self.journal = staged;
        read?;

        self.lru_touch(slot);
        let epoch = self.epoch;
        let entry = &mut self.entries[slot];
        entry.block = block;
        entry.kind = kind;
        entry.owner = owner;
        entry.frame.set_owner_key(block.raw() as u64);
        entry.dirty_epoch = epoch;
        // A block sourced from the log is *not* clean: its home holds the
        // state before the log's oldest uncheckpointed transaction, and a
        // clean entry would have the check point skip it and the reset drop
        // the only copy.
        self.set_dirty(slot, logged.is_some());
        let entry = &mut self.entries[slot];
        entry.pinned = 1;
        entry.valid = true;
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;
        self.index.insert(block, slot);
        self.note_op_fresh(slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Get a metadata block and zero-fill it (for newly allocated blocks — no
    /// disk read).
    pub fn get_zero(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Metadata, BlockOwner::Other)
    }

    pub fn get_zero_owned(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Metadata, owner)
    }

    /// Get a file-data block and zero-fill it (newly allocated data block).
    pub fn get_zero_data(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.get_zero_kind(block, device, BlockKind::Data, owner)
    }

    fn get_zero_kind(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
        kind: BlockKind,
        owner: BlockOwner,
    ) -> Result<CachedBlock<'_>, Ext2Error> {
        self.note_op_dir(owner);
        if let Some(&slot) = self.index.get(&block) {
            self.note_op_touch(slot)?;
            let bs = self.block_size as usize;
            self.entries[slot].frame.as_bytes_mut()[..bs].fill(0);
            self.set_dirty(slot, true);
            self.entries[slot].dirty_epoch = self.epoch;
            self.entries[slot].kind = kind;
            self.entries[slot].owner = owner;
            // Reused by this same operation, so the deferred invalidation no
            // longer applies.
            self.entries[slot].op_invalidated = false;
            self.lru_touch(slot);
            self.entries[slot].pinned += 1;
            return Ok(CachedBlock { cache: self, slot });
        }

        let slot = self.find_or_evict(device)?;
        let bs = self.block_size as usize;
        let epoch = self.epoch;

        self.lru_touch(slot);
        let entry = &mut self.entries[slot];
        entry.frame.as_bytes_mut()[..bs].fill(0);
        entry.block = block;
        entry.kind = kind;
        entry.owner = owner;
        entry.frame.set_owner_key(block.raw() as u64);
        entry.dirty_epoch = epoch;
        self.set_dirty(slot, true);
        let entry = &mut self.entries[slot];
        entry.pinned = 1;
        entry.valid = true;
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;
        self.index.insert(block, slot);
        // A freshly zeroed block has no committed contents to snapshot,
        // whatever the slot's predecessor was.
        self.note_op_fresh(slot);

        Ok(CachedBlock { cache: self, slot })
    }

    /// Answers whether the block needed writing, so a caller can skip the
    /// device barrier that would otherwise order nothing.
    pub fn flush_block(
        &mut self,
        block: BlockNum,
        device: &dyn BlockDevice,
    ) -> Result<bool, Ext2Error> {
        let Some(&slot) = self.index.get(&block) else {
            return Ok(false);
        };
        if !self.entries[slot].frame.dirty() {
            return Ok(false);
        }
        if self.entries[slot].kind == BlockKind::Metadata && !self.home_write_allowed(block) {
            self.sync_log(device)?;
        }
        let entry = &self.entries[slot];
        let offset = entry.block.to_disk_offset(self.block_size);
        let bs = self.block_size as usize;
        device
            .write_at(offset.raw(), &entry.frame.as_bytes()[..bs])
            .map_err(Ext2Error::from)?;
        self.set_dirty(slot, false);
        self.unbarriered += 1;
        Ok(true)
    }

    /// Attempts every slot even after a write fails, returning the first error
    /// once the pass completes; failed blocks stay dirty for the next flush.
    pub fn flush_where(
        &mut self,
        device: &dyn BlockDevice,
        select: impl FnMut(BlockKind, BlockOwner) -> bool,
    ) -> Result<usize, Ext2Error> {
        self.flush_bounded(device, u64::MAX, usize::MAX, select)
            .map(|progress| progress.written)
    }

    /// One bounded step of a writeback pass.
    ///
    /// `epoch` excludes blocks a later operation dirtied and `budget` caps the
    /// device writes, which together are what let the mount lock be released
    /// between steps: an operation that runs in the gap carries a newer epoch,
    /// so a resumed pass can neither miss its data nor publish its metadata
    /// early.
    pub fn flush_bounded(
        &mut self,
        device: &dyn BlockDevice,
        epoch: u64,
        budget: usize,
        select: impl FnMut(BlockKind, BlockOwner) -> bool,
    ) -> Result<FlushProgress, Ext2Error> {
        self.flush_bounded_at(device, epoch, budget, 0, select)
    }

    /// [`Self::flush_bounded`] scanning from slot `start`, so a caller that
    /// drives a flush a step at a time resumes where the last step stopped
    /// instead of rescanning every clean slot before it.
    pub fn flush_bounded_at(
        &mut self,
        device: &dyn BlockDevice,
        epoch: u64,
        budget: usize,
        start: u32,
        mut select: impl FnMut(BlockKind, BlockOwner) -> bool,
    ) -> Result<FlushProgress, Ext2Error> {
        self.flush_matching(device, budget, start, &mut |e| {
            e.dirty_epoch <= epoch && select(e.kind, e.owner)
        })
    }

    /// [`Self::flush_bounded_at`] for data blocks that were already dirty
    /// when `epoch` was current, however recently rewritten: what the records
    /// below a pass's limit may name.
    pub fn flush_data_dirty_since(
        &mut self,
        device: &dyn BlockDevice,
        epoch: u64,
        budget: usize,
        start: u32,
    ) -> Result<FlushProgress, Ext2Error> {
        self.flush_matching(device, budget, start, &mut |e| {
            e.dirtied_epoch <= epoch && e.kind == BlockKind::Data
        })
    }

    /// Copy dirty data blocks dirtied by `epoch` into `batch`, from slot
    /// `start`, until it holds `max` blocks or a whole circle found nothing
    /// more: what [`Self::flush_data_dirty_since`] would write, for a caller
    /// that writes it after giving the mount lock back. The blocks are marked
    /// clean and in flight; [`Self::finish_data_batch`] settles them.
    pub fn stage_data_batch(
        &mut self,
        epoch: u64,
        start: u32,
        max: usize,
        batch: &mut DataBatch,
    ) -> Result<FlushProgress, Ext2Error> {
        self.stage_batch(start, max, batch, &|e: &CacheEntry| {
            e.kind == BlockKind::Data && e.dirtied_epoch <= epoch
        })
    }

    fn stage_batch(
        &mut self,
        start: u32,
        max: usize,
        batch: &mut DataBatch,
        select: &dyn Fn(&CacheEntry) -> bool,
    ) -> Result<FlushProgress, Ext2Error> {
        batch.clear();
        let bs = self.block_size as usize;
        // Reserved up front, so staging never stops half way through with
        // blocks marked clean that no batch carries.
        batch.reserve(max, bs).map_err(|_| Ext2Error::OutOfMemory)?;
        let slots = self.entries.len();
        let mut slot = if (start as usize) < slots {
            start as usize
        } else {
            0
        };
        let mut seen = 0usize;
        let mut more = false;
        let wanted = |e: &CacheEntry| e.valid && e.frame.dirty() && !e.inflight && select(e);
        // Exact, because the data phase must not stop with a block its commit
        // names still unwritten: every entry dirty at the start is counted
        // once, when the scan reaches it or when a run stages it ahead of the
        // scan (marked clean then, so not counted again).
        let dirty_at_start = self.dirty;
        let mut dirty_seen = 0usize;
        while seen < slots && dirty_seen < dirty_at_start {
            let entry = &self.entries[slot];
            dirty_seen += usize::from(entry.valid && entry.frame.dirty());
            // A metadata block whose newest record is not durable yet waits
            // for a pass that finds it so; `plan_run` stops a run at one.
            if wanted(entry)
                && (entry.kind == BlockKind::Data || self.home_write_allowed(entry.block))
            {
                if batch.blocks.len() >= max {
                    more = true;
                    break;
                }
                let mut run = [0u32; FLUSH_RUN];
                let len =
                    self.plan_run(slot, max - batch.blocks.len(), &mut |e| wanted(e), &mut run);
                let first = batch.blocks.len() as u32;
                for &peer in &run[..len] {
                    let peer = peer as usize;
                    let _ = batch
                        .bytes
                        .extend_from_slice(&self.entries[peer].frame.as_bytes()[..bs]);
                    let _ = batch.blocks.push(self.entries[peer].block.raw());
                    self.set_dirty(peer, false);
                    self.entries[peer].inflight = true;
                }
                let offset = self.entries[run[0] as usize]
                    .block
                    .to_disk_offset(self.block_size)
                    .raw();
                let _ = batch.runs.push((offset, first, len as u32));
                dirty_seen += len - 1;
                // Owed now, not when the batch finishes: a commit that takes
                // the lock meanwhile must barrier behind these writes.
                self.unbarriered += len;
            }
            slot = if slot + 1 == slots { 0 } else { slot + 1 };
            seen += 1;
        }
        Ok(FlushProgress {
            written: batch.blocks.len(),
            more,
            next: slot as u32,
        })
    }

    /// Settle a batch [`Self::stage_data_batch`] staged: every block leaves
    /// flight, and those in a run that failed are dirty again unless an
    /// operation already made them so. A block dropped meanwhile — freed, or
    /// its operation rolled back — is no longer the cache's to settle.
    pub fn finish_data_batch(&mut self, batch: &DataBatch) {
        for (k, &(_, first, len)) in batch.runs.as_slice().iter().enumerate() {
            let ok = batch.ok.get(k).copied().unwrap_or(false);
            for &block in &batch.blocks.as_slice()[first as usize..(first + len) as usize] {
                let Some(&slot) = self.index.get(&BlockNum(block)) else {
                    continue;
                };
                let entry = &mut self.entries[slot];
                if !entry.valid || !entry.inflight {
                    continue;
                }
                entry.inflight = false;
                if !ok && !entry.frame.dirty() {
                    entry.frame.set_dirty(true);
                    self.dirty += 1;
                }
            }
        }
    }

    /// One circle over the slots from `start`, writing what `wanted` accepts
    /// until `budget` runs out. `more` is only false once a whole circle found
    /// nothing left, so a resumed scan cannot miss a slot behind its start.
    fn flush_matching(
        &mut self,
        device: &dyn BlockDevice,
        budget: usize,
        start: u32,
        wanted: &mut dyn FnMut(&CacheEntry) -> bool,
    ) -> Result<FlushProgress, Ext2Error> {
        let depth = device.write_depth().clamp(1, WRITE_DEPTH_MAX);
        let mut first_err: Option<Ext2Error> = None;
        let mut written = 0usize;
        let mut submitted = 0usize;
        let mut more = false;
        let slots = self.entries.len();
        let mut slot = if (start as usize) < slots {
            start as usize
        } else {
            0
        };
        let mut seen = 0usize;
        while seen < slots && self.dirty > 0 {
            let entry = &self.entries[slot];
            if entry.valid && entry.frame.dirty() && wanted(entry) {
                if submitted >= budget {
                    more = true;
                    break;
                }
                // Logged metadata goes home only from a durable record.
                if self.entries[slot].kind == BlockKind::Metadata
                    && !self.home_write_allowed(self.entries[slot].block)
                {
                    written += self.complete_inflight(device, 0, &mut first_err);
                    if let Err(e) = self.sync_log(device) {
                        first_err.get_or_insert(e);
                        break;
                    }
                }
                written += self.complete_inflight(device, depth - 1, &mut first_err);
                let at = self.inflight.push();
                let mut run = [0u32; FLUSH_RUN];
                // Bounded by the remaining budget so a caller that asked for
                // one write still gets one.
                let len = self.plan_run(
                    slot,
                    budget - submitted,
                    &mut |e| e.valid && e.frame.dirty() && wanted(e),
                    &mut run,
                );
                self.inflight.runs[at] = run;
                self.inflight.lens[at] = len as u8;
                let ticket = loop {
                    match self.submit_run(device, at) {
                        Err(BlockDeviceError::Busy) if self.inflight.count > 1 => {
                            let keep = self.inflight.count - 1;
                            written += self.complete_inflight(device, keep, &mut first_err);
                        }
                        ticket => break ticket,
                    }
                };
                match ticket {
                    Ok(ticket) => self.inflight.tickets[at] = Some(ticket),
                    Err(e) => {
                        self.inflight.pop_newest();
                        first_err.get_or_insert(e.into());
                    }
                }
                submitted += len;
            }
            slot = if slot + 1 == slots { 0 } else { slot + 1 };
            seen += 1;
        }
        written += self.complete_inflight(device, 0, &mut first_err);
        self.unbarriered += written;
        match first_err {
            Some(e) => Err(e),
            None => Ok(FlushProgress {
                written,
                more,
                next: slot as u32,
            }),
        }
    }

    /// Complete the oldest submitted runs until at most `keep` stay in
    /// flight, and answer how many blocks reached the device.
    fn complete_inflight(
        &mut self,
        device: &dyn BlockDevice,
        keep: usize,
        first_err: &mut Option<Ext2Error>,
    ) -> usize {
        let mut written = 0usize;
        while self.inflight.count > keep {
            let at = self.inflight.head;
            let Some(ticket) = self.inflight.tickets[at].take() else {
                self.inflight.pop_oldest();
                continue;
            };
            // A chain that failed goes again through the device's retrying
            // road before it counts as an error.
            let outcome = device
                .complete_write(ticket)
                .map_err(Ext2Error::from)
                .or_else(|_| self.rewrite_run(device, at));
            written += self.finish_run(at, outcome.is_ok());
            if let Err(e) = outcome {
                first_err.get_or_insert(e);
            }
            self.inflight.pop_oldest();
        }
        written
    }

    pub fn flush_kind(
        &mut self,
        kind: BlockKind,
        device: &dyn BlockDevice,
    ) -> Result<usize, Ext2Error> {
        self.flush_where(device, |entry_kind, _| entry_kind == kind)
    }

    /// Data first, then metadata, without device barriers: callers needing
    /// durability ordering interleave [`BlockDevice::flush`] between the phases.
    pub fn flush_all(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        let data = self.flush_kind(BlockKind::Data, device)?;
        let meta = self.flush_kind(BlockKind::Metadata, device)?;
        Ok(data + meta)
    }

    /// Copy home the newest record of every logged block the cache does not
    /// hold clean, from `cursor` onwards, and answer where to resume.
    ///
    /// The cache-resident half of the check point is an ordinary metadata
    /// flush; this is the remainder — blocks a rollback dropped or an eviction
    /// spilled, whose only copy is the log.
    pub fn checkpoint_logged(
        &mut self,
        device: &dyn BlockDevice,
        cursor: u32,
        budget: usize,
        limit: u32,
    ) -> Result<CheckpointProgress, Ext2Error> {
        let needs_sync = self
            .journal
            .as_ref()
            .is_some_and(|j| !j.slot_durable(limit.min(j.head()).saturating_sub(1)));
        if needs_sync {
            self.sync_log(device)?;
        }
        let Some(mut journal) = self.journal.take() else {
            return Ok(CheckpointProgress {
                cursor: 0,
                more: false,
            });
        };
        debug_assert!(self.op_depth == 0, "a check point inside an operation");
        let end = limit.min(journal.head());
        let mut slot = cursor.max(1);
        let mut written = 0usize;
        let mut result = Ok(());
        while slot < end {
            if written >= budget {
                break;
            }
            let block = journal.slot_block_at(slot);
            // Only a block's newest record: an interleaved pass may have put a
            // newer copy home already. A dirty entry still sends it home, since
            // nothing says the metadata phase has run for its epoch.
            if journal.resident_slot(block) != Some(slot) || self.home_matches(block) {
                slot += 1;
                continue;
            }
            // A run is ascending slots whose *homes* are the next block, so one
            // request replaces several. A slot the loop above would skip ends
            // the run rather than being skipped inside it. No barrier moves —
            // the caller still barriers once behind the whole check point.
            let room = (budget - written).min(journal.home_run_max());
            let mut len = 1u32;
            while (len as usize) < room && slot + len < end {
                let next = journal.slot_block_at(slot + len);
                if block.checked_add(len) != Some(next)
                    || journal.resident_slot(next) != Some(slot + len)
                    || self.home_matches(next)
                {
                    break;
                }
                len += 1;
            }
            if let Err(e) = journal.copy_run_to_home(slot, block, len, device) {
                result = Err(e);
                break;
            }
            written += len as usize;
            slot += len;
        }
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result?;
        Ok(CheckpointProgress {
            cursor: slot,
            more: slot < end,
        })
    }

    /// Whether the cache says `block`'s home location already holds its
    /// newest contents.
    fn home_matches(&self, block: u32) -> bool {
        self.index
            .get(&BlockNum(block))
            .is_some_and(|&slot| self.entries[slot].valid && !self.entries[slot].frame.dirty())
    }

    /// Whether `block`'s home location holds its newest contents and the cache
    /// holds no copy of it: what a read may take straight off the device.
    pub fn home_is_current(&self, block: BlockNum) -> bool {
        !self.index.contains_key(&block)
            && self
                .journal
                .as_ref()
                .is_none_or(|j| j.resident_slot(block.raw()).is_none())
    }

    /// The log slot holding `block`'s newest committed content, if any.
    #[cfg(feature = "tests")]
    pub fn journal_newest_slot(&self, block: u32) -> Option<u32> {
        self.journal.as_ref()?.resident_slot(block)
    }

    /// Where the log's append point stands, or 1 when there is no log.
    pub fn journal_head(&self) -> u32 {
        self.journal.as_ref().map_or(1, |j| j.head())
    }

    /// Which emptying of the log the current slot indices belong to. A pass
    /// records it so one resumed after another reset the log cannot mistake
    /// its own indices for the new generation's.
    pub fn journal_generation(&self) -> u32 {
        self.journal.as_ref().map_or(0, |j| j.generation())
    }

    /// How many aborts have put log mappings back. See [`Journal::abort_op`].
    pub fn journal_restores(&self) -> u32 {
        self.journal.as_ref().map_or(0, |j| j.restores())
    }

    /// Drop every clean entry.
    ///
    /// A replay writes home locations directly, so anything the cache read
    /// before it ran may now be stale. Clean only, because a dirty block is
    /// newer than the medium by construction and a replay runs before any
    /// operation can have dirtied one.
    pub fn invalidate_all_clean(&mut self) {
        for i in 0..self.entries.len() {
            if self.entries[i].valid && !self.entries[i].frame.dirty() && !self.entries[i].inflight
            {
                self.drop_entry(i);
            }
        }
    }

    /// Declare the log check pointed. The caller must have barriered the home
    /// writes first.
    ///
    /// Its own write deliberately does not count as owing a barrier: losing it
    /// costs a redundant replay of transactions already applied, and counting
    /// it would leave a sync with nothing else to do reporting itself
    /// perpetually unfinished.
    pub fn journal_reset(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = journal.reset(device);
        let _ = journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// Whether the log holds transactions a mount would have to replay.
    pub fn journal_is_empty(&self) -> bool {
        self.journal.as_ref().is_none_or(|j| j.is_empty())
    }

    #[cfg(feature = "tests")]
    pub fn kind_of(&self, block: BlockNum) -> Option<BlockKind> {
        self.index.get(&block).map(|&slot| self.entries[slot].kind)
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty
    }

    /// Invalidate a cached block (evict without writing).
    ///
    /// Inside an open operation this is *deferred* to the commit: dropping the
    /// slot at once would throw away the undo snapshot the block's own record
    /// names, and a later rollback would revert it to whatever the device last
    /// held. Deferring costs nothing — callers invalidate blocks they are
    /// *freeing*, and a reallocation reaches them through `get_zero_*`.
    pub fn invalidate(&mut self, block: BlockNum) {
        let Some(&slot) = self.index.get(&block) else {
            return;
        };
        if self.op_depth > 0 {
            self.entries[slot].op_invalidated = true;
            self.note_op_slot(slot);
            return;
        }
        self.drop_entry(slot);
    }

    /// Frames holding a clean, unpinned block — what [`Self::shrink_clean`]
    /// could give back right now.
    pub fn reclaimable(&self) -> u32 {
        self.entries
            .iter()
            .filter(|e| e.pinned == 0 && !e.frame.dirty())
            .count() as u32
    }

    /// Drop up to `want` clean, unpinned entries, returning their frames to the
    /// buddy. Clean only: dropping one costs a re-read, whereas a dirty block
    /// would need a device write on a path that runs *because* memory is short.
    pub fn shrink_clean(&mut self, want: u32) -> u32 {
        if want == 0 {
            return 0;
        }
        // Pure accelerator, and the heap it sits on is the heap that is short.
        // Its frames are not what this call counts.
        self.dir_index.clear();
        let mut released = 0u32;
        // From the end, so a `swap_remove` only ever moves an entry that has
        // already been considered.
        let mut i = self.entries.len();
        while i > 0 && released < want {
            i -= 1;
            if self.entries[i].pinned != 0
                || self.entries[i].frame.dirty()
                || self.entries[i].inflight
            {
                continue;
            }
            // Repaired in place: rebuilding the index needs
            // `KBTreeMap::insert`, which allocates, on the path that runs
            // *because* allocation failed. Only a valid slot owns its index
            // entry — `drop_entry` leaves `block` set on the slot it
            // invalidates.
            if self.entries[i].valid {
                self.index.remove(&self.entries[i].block);
            }
            self.lru_detach(i);
            let moved_from = self.entries.len() - 1;
            let moved = if moved_from != i {
                Some(self.entries[moved_from].block)
            } else {
                None
            };
            let removed_valid = self.entries[moved_from].valid;
            self.entries.swap_remove(i);
            if moved.is_some() {
                // The entry that moved into `i` is still linked under its old
                // index.
                self.lru_reindex(i);
            }
            if let Some(block) = moved
                && removed_valid
                && let Some(slot) = self.index.get_mut(&block)
            {
                *slot = i;
            }
            released += 1;
        }
        if released > 0 {
            self.relist_op_slots();
        }
        released
    }

    /// Slots the per-operation records can name. A commit stages one `u32` per
    /// touched slot into preallocated vectors, so the cache may never hold
    /// more entries than those have room for — that is what makes a commit
    /// allocation-free.
    fn entry_ceiling(&self) -> usize {
        let mut ceiling = self.capacity.min(self.op_touched_slots.capacity());
        if self.journal.is_some() {
            ceiling = ceiling.min(self.scratch.capacity());
        }
        ceiling
    }

    /// Entries the cache may grow to.
    pub fn capacity(&self) -> usize {
        self.entry_ceiling()
    }

    fn lru_detach(&mut self, slot: usize) {
        let prev = self.entries[slot].lru_prev;
        let next = self.entries[slot].lru_next;
        if prev == NIL {
            self.lru_head = next;
        } else {
            self.entries[prev as usize].lru_next = next;
        }
        if next == NIL {
            self.lru_tail = prev;
        } else {
            self.entries[next as usize].lru_prev = prev;
        }
        self.entries[slot].lru_prev = NIL;
        self.entries[slot].lru_next = NIL;
    }

    fn lru_link_mru(&mut self, slot: usize) {
        let head = self.lru_head;
        self.entries[slot].lru_prev = NIL;
        self.entries[slot].lru_next = head;
        if head == NIL {
            self.lru_tail = slot as u32;
        } else {
            self.entries[head as usize].lru_prev = slot as u32;
        }
        self.lru_head = slot as u32;
    }

    fn lru_link_lru(&mut self, slot: usize) {
        let tail = self.lru_tail;
        self.entries[slot].lru_next = NIL;
        self.entries[slot].lru_prev = tail;
        if tail == NIL {
            self.lru_head = slot as u32;
        } else {
            self.entries[tail as usize].lru_next = slot as u32;
        }
        self.lru_tail = slot as u32;
    }

    fn lru_touch(&mut self, slot: usize) {
        self.lru_detach(slot);
        self.lru_link_mru(slot);
    }

    /// Put an emptied slot at the LRU end, so the next miss takes it before it
    /// considers evicting a live block.
    fn lru_retire(&mut self, slot: usize) {
        self.lru_detach(slot);
        self.lru_link_lru(slot);
    }

    /// Repair the chain around the entry that a `swap_remove` moved into
    /// `slot`: its neighbours still name the index it came from.
    fn lru_reindex(&mut self, slot: usize) {
        let prev = self.entries[slot].lru_prev;
        let next = self.entries[slot].lru_next;
        if prev == NIL {
            self.lru_head = slot as u32;
        } else {
            self.entries[prev as usize].lru_next = slot as u32;
        }
        if next == NIL {
            self.lru_tail = slot as u32;
        } else {
            self.entries[next as usize].lru_prev = slot as u32;
        }
    }

    /// Rebuild the per-operation slot list after a reclaim moved entries
    /// between slots. Reuses the preallocated vector: a reclaim runs
    /// *because* allocation is failing.
    fn relist_op_slots(&mut self) {
        self.op_touched_slots.clear();
        for i in 0..self.entries.len() {
            let carries = self.entries[i].op_touched
                || self.entries[i].op_invalidated
                || self.entries[i].op_discard;
            self.entries[i].op_listed = false;
            if carries {
                self.note_op_slot(i);
            }
        }
    }

    /// A slot a miss can take: `(free, victim)`.
    ///
    /// Walks from the LRU end, so an invalid slot — always retired there — is
    /// found before any live block is considered. Among live blocks: a clean
    /// one costs nothing to give up, where a dirty one is a device write on
    /// the path of the operation that missed; allocation state is what the
    /// *next* allocation re-reads, so it is kept while anything else is
    /// evictable; and a block the open operation dirtied is the last resort,
    /// its home not being allowed uncommitted content.
    fn pick_victim(&self) -> Option<usize> {
        let mut classes = [usize::MAX; 8];
        let mut cursor = self.lru_tail;
        let mut seen = 0usize;
        while cursor != NIL {
            let entry = &self.entries[cursor as usize];
            if !entry.valid {
                return Some(cursor as usize);
            }
            if entry.pinned == 0 && !entry.inflight {
                let class = usize::from(entry.op_touched) * 4
                    + usize::from(entry.frame.dirty()) * 2
                    + usize::from(matches!(entry.owner, BlockOwner::Alloc));
                if classes[class] == usize::MAX {
                    classes[class] = cursor as usize;
                    if class == 0 {
                        break;
                    }
                }
                seen += 1;
                // A cache that is mostly dirty would otherwise walk every
                // entry on every miss looking for a clean one.
                if seen >= VICTIM_SCAN && classes.iter().any(|&s| s != usize::MAX) {
                    break;
                }
            }
            cursor = entry.lru_prev;
        }
        classes.iter().copied().find(|&s| s != usize::MAX)
    }

    fn find_or_evict(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        // Invalid slots are retired to the LRU end, so one look finds any.
        let tail = self.lru_tail;
        if tail != NIL && !self.entries[tail as usize].valid {
            return Ok(tail as usize);
        }

        // Grow before evicting: the cache is sized to what memory allows and
        // filled on demand, and a reclaim gives frames back under pressure.
        if self.entries.len() < self.entry_ceiling()
            && let Ok(entry) = CacheEntry::new()
            && self.entries.push(entry).is_ok()
        {
            let slot = self.entries.len() - 1;
            self.lru_link_lru(slot);
            return Ok(slot);
        }

        let slot = self.pick_victim().ok_or(Ext2Error::DeviceError)?;
        if !self.entries[slot].valid {
            return Ok(slot);
        }

        // Eviction is a cache-replacement event, not a durability point, so no
        // barrier is issued here; the commit and FS-level `sync` provide the
        // ordering, which is why a data block written home from here is
        // recorded as owing one.
        if self.entries[slot].frame.dirty() {
            let bs = self.block_size as usize;
            let metadata = self.entries[slot].kind == BlockKind::Metadata;
            let spill = self.journal.is_some() && self.entries[slot].op_touched && metadata;
            if spill {
                // A header and a payload, and a revoke record the spill may
                // flush ahead of them.
                self.ensure_log_room(device, 3)?;
                let staged = self.journal.take();
                let block = self.entries[slot].block.raw();
                let outcome = match staged {
                    Some(mut journal) => {
                        let r = journal.spill(
                            block,
                            &self.entries[slot].frame.as_bytes()[..bs],
                            device,
                        );
                        self.unbarriered += journal.take_writes();
                        self.journal = Some(journal);
                        r
                    }
                    None => Ok(()),
                };
                outcome?;
            } else {
                let block = self.entries[slot].block;
                if metadata && !self.home_write_allowed(block) {
                    self.sync_log(device)?;
                }
                let op_data =
                    self.entries[slot].op_touched && self.entries[slot].kind == BlockKind::Data;
                if op_data {
                    self.supersede_logged(block, device)?;
                }
                let offset = block.to_disk_offset(self.block_size);
                device
                    .write_at(offset.raw(), &self.entries[slot].frame.as_bytes()[..bs])
                    .map_err(Ext2Error::from)?;
                self.unbarriered += 1;
                if op_data {
                    self.op_data_evicted = true;
                }
            }
        }

        self.retire_slot(slot);
        Ok(slot)
    }

    /// Unmap `slot`, whose contents are on the medium or discarded.
    fn retire_slot(&mut self, slot: usize) {
        self.index.remove(&self.entries[slot].block);
        self.set_dirty(slot, false);
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.inflight = false;
        entry.frame.set_owner_key(0);
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;
        self.lru_retire(slot);
    }

    /// A slot for a block read off the device, without a write: a free one, a
    /// new one while the cache may still grow, or a clean victim.
    fn clean_slot(&mut self) -> Option<usize> {
        let tail = self.lru_tail;
        if tail != NIL && !self.entries[tail as usize].valid {
            return Some(tail as usize);
        }
        if self.entries.len() < self.entry_ceiling()
            && let Ok(entry) = CacheEntry::new()
            && self.entries.push(entry).is_ok()
        {
            let slot = self.entries.len() - 1;
            self.lru_link_lru(slot);
            return Some(slot);
        }
        let slot = self.pick_victim()?;
        let entry = &self.entries[slot];
        if entry.valid {
            if entry.frame.dirty() || entry.op_touched {
                return None;
            }
            self.retire_slot(slot);
        }
        Some(slot)
    }

    /// Keep the blocks a read took straight off the device, from `first`, as
    /// clean entries, so reading them again costs no request. Best effort: a
    /// block the cache already holds keeps its entry, and the fill stops at
    /// the first block that would need a write to make room.
    pub fn install_clean_run(&mut self, first: BlockNum, bytes: &[u8], owner: BlockOwner) {
        let bs = self.block_size as usize;
        for (i, chunk) in bytes.chunks_exact(bs).enumerate() {
            let Some(raw) = first.raw().checked_add(i as u32) else {
                return;
            };
            let block = BlockNum(raw);
            if self.index.contains_key(&block) {
                continue;
            }
            let Some(slot) = self.clean_slot() else {
                return;
            };
            self.entries[slot].frame.as_bytes_mut()[..bs].copy_from_slice(chunk);
            self.lru_touch(slot);
            let epoch = self.epoch;
            let entry = &mut self.entries[slot];
            entry.block = block;
            entry.kind = BlockKind::Data;
            entry.owner = owner;
            entry.frame.set_owner_key(u64::from(raw));
            entry.dirty_epoch = epoch;
            entry.pinned = 0;
            entry.valid = true;
            entry.op_touched = false;
            entry.op_invalidated = false;
            entry.op_discard = false;
            entry.op_restored = false;
            self.index.insert(block, slot);
        }
    }
}

/// What one [`BlockCache::flush_bounded`] step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushProgress {
    pub written: usize,
    /// Blocks in this pass's epoch are still dirty: the step ran out of
    /// budget, not of work.
    pub more: bool,
    /// The slot the next step should resume its scan from.
    pub next: u32,
}

/// Where a check point reached, so the next step resumes there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointProgress {
    pub cursor: u32,
    pub more: bool,
}

/// Pins its cache slot until dropped.
pub struct CachedBlock<'a> {
    cache: &'a mut BlockCache,
    slot: usize,
}

impl<'a> CachedBlock<'a> {
    pub fn data(&self) -> &[u8] {
        let bs = self.cache.block_size as usize;
        &self.cache.entries[self.slot].frame.as_bytes()[..bs]
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let bs = self.cache.block_size as usize;
        let epoch = self.cache.epoch;
        self.cache.set_dirty(self.slot, true);
        let entry = &mut self.cache.entries[self.slot];
        entry.dirty_epoch = epoch;
        &mut entry.frame.as_bytes_mut()[..bs]
    }

    /// [`BlockCache::reuse_blocked`], asked while this block pins the cache.
    pub fn reuse_blocked(&self, block: BlockNum) -> bool {
        self.cache.reuse_blocked(block)
    }

    /// A fixed-size window into the block, or `None` if it does not fit.
    ///
    /// Parsers take the array rather than a slice, so a short or misplaced
    /// window is a `None` at the caller instead of a length the parser trusts.
    pub fn window<const N: usize>(&self, at: usize) -> Option<&[u8; N]> {
        let data = self.data();
        let end = at.checked_add(N)?;
        if end > data.len() {
            return None;
        }
        data[at..end].try_into().ok()
    }

    pub fn window_mut<const N: usize>(&mut self, at: usize) -> Option<&mut [u8; N]> {
        let data = self.data_mut();
        let end = at.checked_add(N)?;
        if end > data.len() {
            return None;
        }
        (&mut data[at..end]).try_into().ok()
    }

    pub fn block_num(&self) -> BlockNum {
        self.cache.entries[self.slot].block
    }
}

impl Drop for CachedBlock<'_> {
    fn drop(&mut self) {
        self.cache.entries[self.slot].pinned =
            self.cache.entries[self.slot].pinned.saturating_sub(1);
    }
}
