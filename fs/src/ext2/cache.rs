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
use crate::blockdev::{BlockDevice, stats};

/// Frames the cache never drops below. Small enough for the appliance image,
/// large enough that a 16 GiB volume's whole allocation working set stays
/// resident (see [`cache_entries_for`]).
pub const CACHE_ENTRIES_MIN: usize = 512;

/// Frames the cache never grows past: 8192 blocks of at most 4 KiB is 32 MiB
/// of page-cache frames.
pub const CACHE_ENTRIES_MAX: usize = 8192;

/// The descriptors are one contiguous allocation, so a bump to the entry count
/// or the entry size that no longer fits fails the build, not the mount.
const _: () = assert!(CACHE_ENTRIES_MAX * size_of::<CacheEntry>() <= MAX_ALLOC_SIZE);

/// Frames a volume wants resident, from its own size.
///
/// Every allocation reads the group descriptor table and the block and inode
/// bitmaps of the group it lands in, and the next allocation reads them again.
/// A cache smaller than that working set evicts a bitmap it is about to need.
pub fn cache_entries_for(volume_blocks: u64, blocks_per_group: u32) -> usize {
    let per_group = blocks_per_group.max(1) as u64;
    let groups = volume_blocks.div_ceil(per_group);
    // 32-byte descriptors, so 32 to a 1 KiB block: the smallest block size an
    // image may carry is the one whose table takes the most blocks.
    let gdt = groups.div_ceil(32);
    let want = groups.saturating_mul(2).saturating_add(gdt);
    (want.min(CACHE_ENTRIES_MAX as u64) as usize).max(CACHE_ENTRIES_MIN)
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

/// Data blocks an operation may put *in* the log rather than write home and
/// barrier behind. A log block costs a write, the barrier it replaces costs a
/// device flush; the bound keeps a large write out of the log.
const DATA_LOG_LIMIT: usize = 16;

/// Snapshots one operation may hold. Each owns a block-sized copy, so at a
/// 4 KiB block size this is 2 MiB of rollback guard.
///
/// Only a block that was *already dirty* when the operation first touched it
/// costs a record. A clean acquire costs one bit, so neither directory size
/// nor write size is bounded by this number.
const MAX_UNDO: usize = 512;

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
    /// The operation that last dirtied this block. A writeback pass writes
    /// nothing newer than the epoch it fixed, which is what lets it release
    /// the mount lock between chunks.
    dirty_epoch: u64,
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
            dirty_epoch: 0,
        })
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
    entries: KVec<CacheEntry>,
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
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, entries, Self::build_entries(capacity)?);
                write_field!(slot, index, KBTreeMap::new());
                write_field!(slot, capacity, capacity);
                write_field!(slot, lru_head, capacity as u32 - 1);
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
                Ok(slot.finish())
            },
        )
    }

    /// The frames, linked into the recency chain tail first so the initial
    /// fill hands out slot 0 upwards.
    fn build_entries(capacity: usize) -> Result<KVec<CacheEntry>, AllocError> {
        let mut entries = KVec::with_capacity(capacity)?;
        for i in 0..capacity {
            let mut entry = CacheEntry::new().map_err(|_| AllocError)?;
            entry.lru_prev = if i + 1 < capacity { i as u32 + 1 } else { NIL };
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
        self.op_depth = 0;
        self.settle_charges(true);
        self.undo.clear();
        self.undo_overflow = false;
        for k in 0..self.op_touched_slots.len() {
            let i = self.op_touched_slots.as_slice()[k] as usize;
            if self.entries[i].op_invalidated {
                self.drop_entry(i);
            }
        }
        self.forget_op_slots();
        Ok(())
    }

    /// Put every block the scope touched back the way it was.
    ///
    /// With a log no home block carries the operation's changes, so dropping
    /// every entry it touched is the whole of it. Without one the scope is
    /// cache-deep: an eviction may already have put a touched block on the
    /// device, which is why [`Self::find_or_evict`] makes it the last resort.
    pub fn rollback_op(&mut self) {
        self.op_depth = self.op_depth.saturating_sub(1);
        if self.op_depth > 0 {
            return;
        }
        self.forget_op_dir_indexes();
        if let Some(journal) = self.journal.as_mut() {
            journal.abort_op();
            for k in 0..self.op_touched_slots.len() {
                let i = self.op_touched_slots.as_slice()[k] as usize;
                if self.entries[i].op_touched {
                    self.drop_entry(i);
                }
            }
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
            let entry = &mut self.entries[slot];
            let n = bs.min(record.snapshot.len());
            entry.frame.as_bytes_mut()[..n].copy_from_slice(&record.snapshot.as_slice()[..n]);
            entry.frame.set_dirty(true);
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
            entry.op_restored = false;
            entry.op_listed = false;
        }
        self.op_touched_slots.clear();
        self.op_dirs_len = 0;
        self.op_dirs_overflow = false;
    }

    /// Publish the open operation through the log.
    ///
    /// A small write's data goes *into* the log with its metadata, so replay
    /// restores both or neither and no ordering is needed. A large one writes
    /// home and buys the ordering with a barrier, issued only if a home write
    /// happened — so a `create` or an `unlink` pays nothing for it.
    fn log_transaction(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.journal.is_none() {
            return Ok(());
        }
        // Staged and reserved *before* anything is published: a commit that
        // ran out of log room afterwards would retract the metadata with the
        // data already on the medium.
        let log_data = self.count_op_data() <= DATA_LOG_LIMIT;
        if !log_data {
            self.supersede_op_data(device)?;
        }
        self.stage_blocks(log_data)?;
        let needed = self.log_slots_needed();
        if self
            .journal
            .as_ref()
            .is_some_and(|journal| journal.free_slots() < needed)
        {
            return Err(Ext2Error::NoSpace);
        }

        // Set by an eviction, so it is owed even on the path that writes no
        // data of its own.
        let mut wrote_data = self.op_data_evicted;
        if !log_data {
            for k in 0..self.op_touched_slots.len() {
                let i = self.op_touched_slots.as_slice()[k] as usize;
                if !Self::goes_home(&self.entries[i]) {
                    continue;
                }
                // Only this operation's own dirty *data*, so a run can never
                // reach a metadata block and never crosses the `data=ordered`
                // boundary the single `device.flush()` below draws.
                let n = self.write_run(device, i, usize::MAX, &mut |e| Self::goes_home(e))?;
                self.unbarriered += n;
                wrote_data = true;
            }
        }
        if wrote_data {
            device.flush().map_err(Ext2Error::from)?;
            self.unbarriered = 0;
        }

        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        let result = self.log_records(&mut journal, device);
        self.unbarriered += journal.take_writes();
        self.journal = Some(journal);
        result
    }

    /// A dirty *data* block of the open operation: what the large-write path
    /// writes home and barriers behind, rather than putting in the log.
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

    /// Write the dirty run that starts at `first` as one gathered request,
    /// and answer how many blocks it carried. Every block in it is left
    /// clean; a failure leaves all of them dirty for the next attempt.
    ///
    /// The run is the *device's*, not the cache's: consecutive block numbers,
    /// whichever slots hold them, extended while `keep` accepts the next
    /// block and bounded by [`FLUSH_RUN`] and `max`. Issues no barrier and
    /// removes none — `keep` is what keeps a run inside one phase.
    ///
    /// `#[inline(never)]`: the segment array and the slot run are 640 bytes
    /// of frame no caller can afford on top of its own.
    #[inline(never)]
    fn write_run(
        &mut self,
        device: &dyn BlockDevice,
        first: usize,
        max: usize,
        keep: &mut dyn FnMut(&CacheEntry) -> bool,
    ) -> Result<usize, Ext2Error> {
        let bs = self.block_size as usize;
        let base = self.entries[first].block;
        let mut run = [first as u32; FLUSH_RUN];
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
            run[len] = peer as u32;
            len += 1;
        }
        let offset = base.to_disk_offset(self.block_size).raw();
        {
            let mut segs: [&[u8]; FLUSH_RUN] = [&[]; FLUSH_RUN];
            for k in 0..len {
                segs[k] = &self.entries[run[k] as usize].frame.as_bytes()[..bs];
            }
            device
                .write_vectored(offset, &segs[..len])
                .map_err(Ext2Error::from)?;
        }
        for k in 0..len {
            self.entries[run[k] as usize].frame.set_dirty(false);
        }
        Ok(len)
    }

    /// Collect the blocks the commit will log, metadata always and data when
    /// the caller decided to log it too.
    fn stage_blocks(&mut self, with_data: bool) -> Result<(), Ext2Error> {
        self.scratch.clear();
        for k in 0..self.op_touched_slots.len() {
            let entry = &self.entries[self.op_touched_slots.as_slice()[k] as usize];
            let logged = match entry.kind {
                BlockKind::Metadata => true,
                BlockKind::Data => with_data,
            };
            if logged
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
        if journal.op_is_empty() && self.scratch.is_empty() {
            return Ok(());
        }
        let bs = self.block_size as usize;
        let per_record = journal.max_entries();
        let mut done = 0usize;
        while done < self.scratch.len() {
            let take = (self.scratch.len() - done).min(per_record);
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
        journal.write_commit(device)
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
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.pinned = 0;
        entry.frame.set_dirty(false);
        entry.frame.set_owner_key(0);
        self.index.remove(&block);
        self.lru_retire(slot);
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
            // re-reads rather than restores: one bit, whatever the state.
            self.entries[slot].op_touched = true;
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
        // A block sourced from the log is *not* clean: its home holds the
        // state before the log's oldest uncheckpointed transaction, and a
        // clean entry would have the check point skip it and the reset drop
        // the only copy.
        entry.frame.set_dirty(logged.is_some());
        entry.dirty_epoch = epoch;
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
            self.entries[slot].frame.set_dirty(true);
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
        entry.frame.set_dirty(true);
        entry.dirty_epoch = epoch;
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
        let entry = &mut self.entries[slot];
        if !entry.frame.dirty() {
            return Ok(false);
        }
        let offset = entry.block.to_disk_offset(self.block_size);
        let bs = self.block_size as usize;
        device
            .write_at(offset.raw(), &entry.frame.as_bytes()[..bs])
            .map_err(Ext2Error::from)?;
        entry.frame.set_dirty(false);
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
        mut select: impl FnMut(BlockKind, BlockOwner) -> bool,
    ) -> Result<FlushProgress, Ext2Error> {
        let mut first_err: Option<Ext2Error> = None;
        let mut written = 0usize;
        let mut more = false;
        let mut slot = 0usize;
        while slot < self.entries.len() {
            let entry = &self.entries[slot];
            if !(entry.valid
                && entry.frame.dirty()
                && entry.dirty_epoch <= epoch
                && select(entry.kind, entry.owner))
            {
                slot += 1;
                continue;
            }
            if written >= budget {
                more = true;
                break;
            }
            // Bounded by the remaining budget so a caller that asked for one
            // write still gets one.
            let room = budget - written;
            let outcome = self.write_run(device, slot, room, &mut |e| {
                e.valid && e.frame.dirty() && e.dirty_epoch <= epoch && select(e.kind, e.owner)
            });
            match outcome {
                Ok(len) => written += len,
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
            slot += 1;
        }
        self.unbarriered += written;
        match first_err {
            Some(e) => Err(e),
            None => Ok(FlushProgress { written, more }),
        }
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

    /// Copy home every logged block the cache no longer holds, from `cursor`
    /// onwards, and answer where to resume.
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
        let Some(mut journal) = self.journal.take() else {
            return Ok(CheckpointProgress {
                cursor: 0,
                more: false,
            });
        };
        let end = limit.min(journal.head());
        let mut slot = cursor.max(1);
        let mut written = 0usize;
        let mut result = Ok(());
        while slot < end {
            if written >= budget {
                break;
            }
            let block = journal.slot_block_at(slot);
            // Only a block's newest record goes home. An older one may be
            // older than the home already is: another pass, interleaved with
            // this one, can have put the newer copy there and been free to
            // empty the log once its own cursor passed it. A clean entry says
            // the home matches; a dirty one may be newer, but nothing says
            // the metadata phase has run for its epoch, so the committed copy
            // goes home regardless.
            if block == 0 || journal.resident_slot(block) != Some(slot) || self.home_matches(block)
            {
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

    /// Drop every clean entry.
    ///
    /// A replay writes home locations directly, so anything the cache read
    /// before it ran may now be stale. Clean only, because a dirty block is
    /// newer than the medium by construction and a replay runs before any
    /// operation can have dirtied one.
    pub fn invalidate_all_clean(&mut self) {
        for i in 0..self.entries.len() {
            if self.entries[i].valid && !self.entries[i].frame.dirty() {
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
        self.entries
            .iter()
            .filter(|e| e.valid && e.frame.dirty())
            .count()
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
            if self.entries[i].pinned != 0 || self.entries[i].frame.dirty() {
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
    /// found before any live block is considered. Among live blocks:
    /// allocation state is what the *next* allocation re-reads, so it is kept
    /// while anything else is evictable, and a block the open operation
    /// dirtied is the last resort, its home not being allowed uncommitted
    /// content.
    fn pick_victim(&self) -> (Option<usize>, Option<usize>) {
        let mut classes = [usize::MAX; 4];
        let mut cursor = self.lru_tail;
        while cursor != NIL {
            let entry = &self.entries[cursor as usize];
            if !entry.valid {
                return (Some(cursor as usize), None);
            }
            if entry.pinned == 0 {
                let class = usize::from(entry.op_touched) * 2
                    + usize::from(matches!(entry.owner, BlockOwner::Alloc));
                if classes[class] == usize::MAX {
                    classes[class] = cursor as usize;
                    if class == 0 {
                        break;
                    }
                }
            }
            cursor = entry.lru_prev;
        }
        (None, classes.iter().copied().find(|&s| s != usize::MAX))
    }

    fn find_or_evict(&mut self, device: &dyn BlockDevice) -> Result<usize, Ext2Error> {
        let (free, victim) = self.pick_victim();
        if let Some(slot) = free {
            return Ok(slot);
        }

        // Re-grow after a reclaim took entries away: otherwise the cache stays
        // permanently shrunk and every later miss evicts a live block.
        if self.entries.len() < self.entry_ceiling()
            && let Ok(entry) = CacheEntry::new()
            && self.entries.push(entry).is_ok()
        {
            let slot = self.entries.len() - 1;
            self.lru_link_lru(slot);
            return Ok(slot);
        }

        let slot = victim.ok_or(Ext2Error::DeviceError)?;

        // Eviction is a cache-replacement event, not a durability point, so no
        // barrier is issued here; the commit and FS-level `sync` provide the
        // ordering, which is why a data block written home from here is
        // recorded as owing one.
        if self.entries[slot].frame.dirty() {
            let bs = self.block_size as usize;
            let spill = self.journal.is_some()
                && self.entries[slot].op_touched
                && self.entries[slot].kind == BlockKind::Metadata;
            if spill {
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

        self.index.remove(&self.entries[slot].block);
        let entry = &mut self.entries[slot];
        entry.valid = false;
        entry.frame.set_dirty(false);
        entry.frame.set_owner_key(0);
        entry.op_touched = false;
        entry.op_invalidated = false;
        entry.op_discard = false;
        entry.op_restored = false;
        self.lru_retire(slot);

        Ok(slot)
    }
}

/// What one [`BlockCache::flush_bounded`] step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushProgress {
    pub written: usize,
    /// Blocks in this pass's epoch are still dirty: the step ran out of
    /// budget, not of work.
    pub more: bool,
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
        let entry = &mut self.cache.entries[self.slot];
        entry.frame.set_dirty(true);
        entry.dirty_epoch = epoch;
        &mut entry.frame.as_bytes_mut()[..bs]
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
