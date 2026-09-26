//! A physical redo log for ext2 metadata.
//!
//! The log lives in the blocks of an ordinary preallocated file, so `e2fsck`
//! sees a file and this kernel needs no on-disk feature bit and no jbd2
//! format compatibility. Nothing outside those blocks is written until a
//! transaction's commit record is on the medium.
//!
//! # Log format
//!
//! Slot 0 is the log's own superblock. Slots 1.. are records:
//!
//! | record   | slots   | meaning                                          |
//! |----------|---------|--------------------------------------------------|
//! | `DATA`   | 1 + *n* | the *n* payload slots hold the listed blocks     |
//! | `REVOKE` | 1       | the listed blocks must not be replayed from here |
//! | `COMMIT` | 1       | the records before it, this sequence, are final  |
//!
//! Recovery scans from slot 1 expecting the sequence the superblock names and
//! stops at the first record that is not the next committed transaction. The
//! committed region is then applied **in slot order**, so the last write of a
//! block wins and a `REVOKE` cancels every earlier write of the blocks it
//! lists — which is what makes a block freed and reused as file data safe.
//!
//! A payload is usually metadata, but a small write's file data is logged the
//! same way instead of being written home and barriered behind: replay then
//! restores both or neither, for a block write rather than a device flush.
//!
//! The commit record's CRC covers every byte of the transaction's other
//! records, so a torn or lost log write makes the transaction fail its own
//! check. That is what removes the barrier before the commit record — the
//! trade ext4 makes with `async_commit`.

use slopos_mm::slab::MAX_ALLOC_SIZE;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::{KBox, KVec, write_field};

use super::Ext2Error;
use crate::blockdev::{BlockDevice, stats};
use crate::verity::{CRC32_INIT, crc32_feed, crc32_finish};

/// "SLJS", the log superblock in slot 0.
const SB_MAGIC: u32 = 0x534A_4C53;
/// "SLJR", every record header.
const REC_MAGIC: u32 = 0x524A_4C53;
/// Version 2 adds the mount stamp; a version-1 superblock still replays.
const FORMAT_VERSION: u32 = 2;
const FORMAT_V1: u32 = 1;

const REC_DATA: u32 = 1;
const REC_REVOKE: u32 = 2;
const REC_COMMIT: u32 = 3;

/// Fixed part of a record header; the block numbers follow.
const REC_ENTRIES_OFF: usize = 24;
/// Where the log superblock records what volume and file it belongs to.
const SB_IDENTITY_OFF: usize = 20;
/// The `[s_mnt_count, s_mtime]` of the mount that last wrote the log
/// superblock.
const SB_STAMP_OFF: usize = 24;
/// Bytes of the log superblock the CRC covers; the CRC itself follows them.
const SB_CRC_SPAN: usize = 32;
const SB_CRC_SPAN_V1: usize = 24;
/// A stamp no volume carries: `s_mnt_count` is 16 bits.
const NO_STAMP: [u32; 2] = [u32::MAX, u32::MAX];

/// Smallest log worth attaching. An operation whose metadata does not fit
/// refuses, and refusing a routine `create` would be worse than having no
/// journal at all.
pub const MIN_LOG_SLOTS: u32 = 32;

/// Slots this kernel will index, the log superblock included. A `/.journal`
/// longer than this is used up to the cap and no further: a log bigger than
/// the kernel can index is the host's sizing decision, not a corrupt image.
///
/// The cap is what holds every per-slot array to `PER_SLOT_LIMIT`. 32 768
/// slots covers the largest log the image builder produces (64 MiB over 4 KiB
/// blocks is 16 384) with room to double.
pub const MAX_LOG_SLOTS: u32 = 32 * 1024;

/// What one per-slot array may take: a quarter of a single allocation's
/// ceiling, so the log's arrays together stay well inside the heap's
/// large-allocation tier.
const PER_SLOT_LIMIT: usize = MAX_ALLOC_SIZE / 4;
const _: () = assert!(MAX_LOG_SLOTS as usize * size_of::<(u32, u32)>() <= PER_SLOT_LIMIT);
const _: () = assert!(MAX_LOG_SLOTS as usize * size_of::<u32>() <= PER_SLOT_LIMIT);
const _: () =
    assert!((MAX_LOG_SLOTS as usize).next_power_of_two() * size_of::<u32>() <= PER_SLOT_LIMIT);

/// Chain terminator in the block index. Slot 0 holds the log superblock, so
/// it is never a record's slot and can stand in for "no link".
const NIL_SLOT: u32 = 0;

/// Slots one record write may gather. The segment array is on the stack, so
/// this is a stack cost as much as an I/O size: 32 segments is 512 bytes of
/// frame.
const RECORD_RUN: usize = 32;

/// Slots one check-point step may carry home in one request. Bounds the
/// staging buffer, which is preallocated at `RECORD_RUN`-independent size:
/// eight 4 KiB blocks is the 32 KiB a virtio-blk chain takes whole.
const CHECKPOINT_RUN: usize = 8;

/// The volume the log belongs to, so a target block read off the medium can be
/// refused before it becomes a write offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogExtent {
    pub first_data_block: u32,
    pub blocks_count: u32,
}

/// What attaching a log did, for the mount log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalRecovery {
    /// Committed transactions the scan found and replayed.
    pub transactions: u32,
    /// Blocks written back to their home locations by the replay.
    pub blocks: u32,
    /// The log carries the stamp of the volume's last mount, so that mount
    /// logged every metadata write it made and nothing else mounted since.
    pub continuous: bool,
}

impl JournalRecovery {
    pub const NONE: Self = Self {
        transactions: 0,
        blocks: 0,
        continuous: false,
    };

    pub fn replayed(self) -> bool {
        self.transactions > 0
    }

    /// Whether the home locations are consistent after the attach, whatever
    /// `s_state` says: a replay made them so, or a continuous log with
    /// nothing to replay says there was nothing half done.
    pub fn recovered(self) -> bool {
        self.replayed() || self.continuous
    }
}

fn le32(data: &[u8], at: usize) -> u32 {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(&data[at..at + 4]);
    u32::from_le_bytes(raw)
}

fn put_le32(data: &mut [u8], at: usize, value: u32) {
    data[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// One record header, as read off the medium.
#[derive(Debug, Clone, Copy)]
struct RecHeader {
    kind: u32,
    count: u32,
    crc: u32,
}

#[derive(slopos_ostd::SlotFields)]
pub struct Journal {
    /// Home block of each log slot. `slots[0]` is the log superblock.
    slots: KVec<u32>,
    /// The filesystem block this slot holds a copy of, or zero once a revoke
    /// or a newer home write has cleared it.
    slot_block: KVec<u32>,
    /// Chained hash index over `slot_block`, so serving a cache miss from the
    /// log costs a bucket walk instead of a scan of every slot below the head.
    ///
    /// Each chain is newest-slot-first, so the first entry matching a block is
    /// its newest record. Inserts must therefore arrive in ascending slot
    /// order for any one block.
    buckets: KVec<u32>,
    chain_next: KVec<u32>,
    chain_prev: KVec<u32>,
    /// `32 - log2(buckets.len())`: the hash takes the *high* bits of a
    /// multiplicative mix, because a mask over the low ones puts every group
    /// bitmap of the volume in one bucket.
    bucket_shift: u32,
    /// Blocks the open operation freed or wrote home, awaiting a `REVOKE`.
    revokes: KVec<u32>,
    /// Mappings a `REVOKE` cleared, so an abort can put them back.
    revoke_undo: KVec<(u32, u32)>,
    /// Staging buffer for record headers. Preallocated: a block does not fit
    /// the 2 KiB stack budget and a commit must not allocate.
    header: KVec<u8>,
    /// Second staging buffer, for reading slots back during a check point.
    /// [`CHECKPOINT_RUN`] blocks long, so a run whose home locations are
    /// consecutive goes home in one request without allocating.
    transfer: KVec<u8>,
    block_size: u32,
    inode: u32,
    /// Every block the log writes to is checked against this range: a record's
    /// target becomes a write offset and the record came off the medium.
    blocks_count: u32,
    first_data_block: u32,
    /// Next free slot; 1 exactly when the log is empty.
    head: u32,
    /// Sequence the open transaction writes. Never reset, so a stale record
    /// left beyond the committed region can never be mistaken for the next
    /// transaction.
    seq: u32,
    /// Bumped by every [`Self::reset`]. A writeback pass records it, so a
    /// pass resumed after another emptied and refilled the log cannot mistake
    /// its own slot indices for the new generation's.
    generation: u32,
    /// Bumped by every abort that put mappings back. A pass whose cursor went
    /// by a slot before it was restored must not empty the log behind it.
    restores: u32,
    /// `head` when the open operation began, for the abort rewind.
    op_head: u32,
    /// Running CRC over the open transaction's records.
    crc: u32,
    /// Device writes issued since the caller last took the count. The cache
    /// owns the barrier accounting, so the log only reports.
    writes: usize,
    /// `[s_mnt_count, s_mtime]` of this mount, written into every log
    /// superblock.
    stamp: [u32; 2],
}

impl Journal {
    /// Take over `slots` as a log: validate the superblock, replay whatever a
    /// previous boot committed and did not check point, and leave the log
    /// empty. `slots[0]` is spent on the log superblock.
    ///
    /// `#[inline(never)]`, built field by field into the heap slot: a whole
    /// `Journal` rvalue plus the nine fallible allocations behind it does not
    /// fit the 2 KiB stack gate.
    ///
    /// `stamp` is the volume's `[s_mnt_count, s_mtime]` as it stands on the
    /// medium now. The log is reset under the stamp it already carried: only
    /// a mount that goes on to write may claim it, through [`Self::restamp`].
    #[inline(never)]
    pub fn attach(
        mut slots: KVec<u32>,
        block_size: u32,
        inode: u32,
        extent: LogExtent,
        stamp: [u32; 2],
        device: &dyn BlockDevice,
    ) -> Result<(KBox<Self>, JournalRecovery), Ext2Error> {
        if slots.len() < MIN_LOG_SLOTS as usize + 1 {
            return Err(Ext2Error::NoSpace);
        }
        // What holds every per-slot array to `PER_SLOT_LIMIT`. The truncated
        // count is what goes into the log superblock, so a later attach
        // validates against the same shape.
        slots.truncate(MAX_LOG_SLOTS as usize);
        // Refused rather than clamped: a slot outside the volume means the
        // mapping is not this file's, and past the filesystem extent is where
        // a verity trailer keeps the hashes that would have detected it.
        for block in slots.as_slice() {
            if *block < extent.first_data_block || *block >= extent.blocks_count {
                return Err(Ext2Error::InvalidBlock);
            }
        }
        let mut journal = KBox::try_init(Self::init(slots, block_size, inode, extent))
            .map_err(|_| Ext2Error::OutOfMemory)?;
        // A superblock that does not describe this file on this volume is one
        // this boot must not read; the reset below overwrites it.
        let recovery = match journal.read_superblock(device)? {
            Some((seq, written_by)) => {
                journal.seq = seq;
                journal.stamp = written_by.unwrap_or(NO_STAMP);
                let mut recovery = journal.replay(device)?;
                // A count pinned at its ceiling stops telling mounts apart.
                recovery.continuous = written_by == Some(stamp) && stamp[0] < u32::from(u16::MAX);
                recovery
            }
            None => JournalRecovery::NONE,
        };
        journal.reset(device)?;
        Ok((journal, recovery))
    }

    fn init(
        slots: KVec<u32>,
        block_size: u32,
        inode: u32,
        extent: LogExtent,
    ) -> impl Init<Self, AllocError> {
        let count = slots.len();
        let buckets = count.next_power_of_two();
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, slots, slots);
                write_field!(slot, slot_block, KVec::zeroed(count)?);
                write_field!(slot, buckets, KVec::zeroed(buckets)?);
                write_field!(slot, chain_next, KVec::zeroed(count)?);
                write_field!(slot, chain_prev, KVec::zeroed(count)?);
                write_field!(slot, bucket_shift, u32::BITS - buckets.trailing_zeros());
                write_field!(
                    slot,
                    revokes,
                    KVec::with_capacity(entries_per_header(block_size))?
                );
                write_field!(slot, revoke_undo, KVec::with_capacity(count)?);
                write_field!(slot, header, KVec::zeroed(block_size as usize)?);
                write_field!(
                    slot,
                    transfer,
                    KVec::zeroed(block_size as usize * CHECKPOINT_RUN)?
                );
                write_field!(slot, block_size, block_size);
                write_field!(slot, inode, inode);
                write_field!(slot, blocks_count, extent.blocks_count);
                write_field!(slot, first_data_block, extent.first_data_block);
                write_field!(slot, head, 1);
                write_field!(slot, seq, 1);
                write_field!(slot, generation, 0);
                write_field!(slot, restores, 0);
                write_field!(slot, op_head, 1);
                write_field!(slot, crc, CRC32_INIT);
                write_field!(slot, writes, 0);
                write_field!(slot, stamp, NO_STAMP);
                Ok(slot.finish())
            },
        )
    }

    /// Whether `block` is a block of this volume, and so a legal write target.
    fn in_volume(&self, block: u32) -> bool {
        block >= self.first_data_block && block < self.blocks_count
    }

    /// Which emptying of the log the current slot indices belong to.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn restores(&self) -> u32 {
        self.restores
    }

    pub fn inode(&self) -> u32 {
        self.inode
    }

    /// Log slots, the superblock's excluded.
    pub fn capacity(&self) -> u32 {
        (self.slots.len() as u32).saturating_sub(1)
    }

    pub fn free_slots(&self) -> u32 {
        (self.slots.len() as u32).saturating_sub(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.head <= 1
    }

    /// The log has enough room left that an ordinary operation will fit
    /// without a check point first.
    pub fn has_headroom(&self) -> bool {
        self.free_slots() >= self.low_water()
    }

    /// The log is filling and the flusher should drain it, well before an
    /// operation is forced to check point inline. Twice the low-water mark,
    /// not a hair above it: a drain is a whole check point, so a threshold the
    /// next operation crosses again turns one pass per burst into one per op.
    pub fn needs_drain(&self) -> bool {
        self.free_slots() < self.low_water().saturating_mul(2)
    }

    /// What one transaction is guaranteed: a quarter of the log, floored at
    /// [`MIN_LOG_SLOTS`].
    ///
    /// This is the bound on a single transaction: `Ext2Fs::transaction` check
    /// points whenever [`Self::has_headroom`] is false, and
    /// `BlockCache::commit_op` refuses with `NoSpace` if what it staged needs
    /// more. A quarter of a 16 384-slot log is 4 095 slots, against the 256 a
    /// flat cap allowed however large the log was.
    fn low_water(&self) -> u32 {
        (self.capacity() / 4).max(MIN_LOG_SLOTS)
    }

    /// Entries one record header can list.
    pub fn max_entries(&self) -> usize {
        entries_per_header(self.block_size)
    }

    /// Slots one [`Self::copy_run_to_home`] may carry, which is what the
    /// preallocated staging buffer holds. A caller that staged a longer run
    /// would have the tail of it silently dropped.
    pub fn home_run_max(&self) -> usize {
        CHECKPOINT_RUN
    }

    pub fn take_writes(&mut self) -> usize {
        core::mem::take(&mut self.writes)
    }

    /// Which bucket `block`'s chain hangs from.
    fn bucket_of(&self, block: u32) -> usize {
        (block.wrapping_mul(0x9E37_79B9) >> self.bucket_shift) as usize
    }

    /// Publish `slot` as holding `block`'s newest logged content.
    ///
    /// Callers must insert a given block's slots in ascending order: the
    /// chain is front-inserted, and that is what keeps it newest-first.
    fn index_insert(&mut self, slot: u32, block: u32) {
        let bucket = self.bucket_of(block);
        let head = self.buckets[bucket];
        self.chain_next[slot as usize] = head;
        self.chain_prev[slot as usize] = NIL_SLOT;
        if head != NIL_SLOT {
            self.chain_prev[head as usize] = slot;
        }
        self.buckets[bucket] = slot;
        self.slot_block[slot as usize] = block;
    }

    /// Drop `slot`'s mapping. A slot that maps nothing is already unlinked.
    fn index_remove(&mut self, slot: u32) {
        let block = self.slot_block[slot as usize];
        if block == 0 {
            return;
        }
        let next = self.chain_next[slot as usize];
        let prev = self.chain_prev[slot as usize];
        if prev == NIL_SLOT {
            let bucket = self.bucket_of(block);
            self.buckets[bucket] = next;
        } else {
            self.chain_next[prev as usize] = next;
        }
        if next != NIL_SLOT {
            self.chain_prev[next as usize] = prev;
        }
        self.chain_next[slot as usize] = NIL_SLOT;
        self.chain_prev[slot as usize] = NIL_SLOT;
        self.slot_block[slot as usize] = 0;
    }

    fn index_clear(&mut self) {
        self.slot_block.as_mut_slice().fill(0);
        self.buckets.as_mut_slice().fill(NIL_SLOT);
        self.chain_next.as_mut_slice().fill(NIL_SLOT);
        self.chain_prev.as_mut_slice().fill(NIL_SLOT);
    }

    /// Clear every mapping of `block`, newest first.
    ///
    /// With `undo`, a mapping from before the open operation is recorded so
    /// an abort can put it back — and the records come out of the list in
    /// descending slot order, which is what makes popping them restore the
    /// chain's newest-first order.
    fn clear_mappings(&mut self, block: u32, undo: bool) -> Result<(), Ext2Error> {
        if block == 0 {
            return Ok(());
        }
        let mut slot = self.buckets[self.bucket_of(block)];
        while slot != NIL_SLOT {
            let next = self.chain_next[slot as usize];
            if self.slot_block[slot as usize] == block {
                if undo && slot < self.op_head {
                    self.revoke_undo
                        .push((slot, block))
                        .map_err(|_| Ext2Error::OutOfMemory)?;
                }
                self.index_remove(slot);
            }
            slot = next;
        }
        Ok(())
    }

    /// Where a block's newest content lives, when that is the log rather than
    /// the block's own home.
    pub fn resident_slot(&self, block: u32) -> Option<u32> {
        if block == 0 {
            return None;
        }
        let mut slot = self.buckets[self.bucket_of(block)];
        while slot != NIL_SLOT {
            if self.slot_block[slot as usize] == block {
                return Some(slot);
            }
            slot = self.chain_next[slot as usize];
        }
        None
    }

    /// Copy a slot's payload into `out`, which must be one block long.
    pub fn read_slot(
        &self,
        slot: u32,
        device: &dyn BlockDevice,
        out: &mut [u8],
    ) -> Result<(), Ext2Error> {
        let offset = self.slot_offset(slot)?;
        device.read_at(offset, out).map_err(Ext2Error::from)
    }

    pub fn begin_op(&mut self) {
        self.op_head = self.head;
        self.revokes.clear();
        self.revoke_undo.clear();
        self.crc = CRC32_INIT;
    }

    /// Discard everything the open operation appended.
    ///
    /// Sound because no home block was written on its behalf, and the caller
    /// drops the operation's cache entries, so a later read comes back from
    /// the log or from the block's own home.
    pub fn abort_op(&mut self) {
        for slot in self.op_head..self.head {
            self.index_remove(slot);
        }
        if !self.revoke_undo.is_empty() {
            self.restores = self.restores.wrapping_add(1);
        }
        // Popped, not iterated: `clear_mappings` records a block's slots
        // newest-first, so the reverse order is the one that leaves each
        // chain newest-first again.
        while let Some((slot, block)) = self.revoke_undo.pop() {
            self.index_insert(slot, block);
        }
        self.head = self.op_head;
        self.revokes.clear();
        self.crc = CRC32_INIT;
    }

    /// The open operation appended nothing, so there is no transaction to
    /// commit and the sequence is not spent.
    pub fn op_is_empty(&self) -> bool {
        self.head == self.op_head
    }

    /// Note that `block` was freed, so no record before this point may be
    /// replayed into it. Flushes the queue when it fills, which is what keeps
    /// a long truncate's revoke list bounded.
    pub fn note_revoke(&mut self, block: u32, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if block == 0 {
            return Ok(());
        }
        self.revokes
            .push(block)
            .map_err(|_| Ext2Error::OutOfMemory)?;
        if self.revokes.len() >= self.max_entries() {
            self.flush_revokes(device)?;
        }
        Ok(())
    }

    /// `block`'s home is being written with contents newer than its records
    /// here. Neither a miss, a check point nor a replay may then take the
    /// older copy over it, so the mappings go now and a `REVOKE` joins the
    /// operation's commit.
    pub fn supersede(&mut self, block: u32, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.resident_slot(block).is_none() {
            return Ok(());
        }
        self.clear_mappings(block, true)?;
        self.note_revoke(block, device)
    }

    /// Emit the queued revokes. Called before any payload record, so a block
    /// freed and then reallocated as metadata within one operation is
    /// described by the later record rather than cancelled by the revoke.
    pub fn flush_revokes(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.revokes.is_empty() {
            return Ok(());
        }
        let slot = self.reserve(1)?;
        let count = self.revokes.len() as u32;
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, REC_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, self.seq);
        put_le32(self.header.as_mut_slice(), 8, REC_REVOKE);
        put_le32(self.header.as_mut_slice(), 12, count);
        for (i, block) in self.revokes.as_slice().iter().enumerate() {
            put_le32(self.header.as_mut_slice(), REC_ENTRIES_OFF + i * 4, *block);
        }
        self.write_slot_from_header(slot, device)?;

        // The in-memory half of the same rule. A mapping from *before* this
        // operation is recorded before it is cleared, because an abort makes
        // the block the inode's again; one this operation made needs no
        // record, since the rewind discards the record it names.
        for i in 0..self.revokes.len() {
            let block = self.revokes.as_slice()[i];
            self.clear_mappings(block, true)?;
        }
        self.revokes.clear();
        Ok(())
    }

    /// Append one `DATA` record for `targets` and return its first payload
    /// slot. Payload *i* is fetched from `payload(i)`, which is where the
    /// caller's cache frame comes from.
    ///
    /// The record header and its payloads occupy ascending slots, so a
    /// consecutive run of slot blocks goes out as one `write_vectored` with
    /// the header as its first segment — also the order a replay scan reads
    /// them in, so the CRC is unchanged. Runs are bounded by [`RECORD_RUN`],
    /// by the first slot whose block is not the next one, and by the first
    /// payload shorter than a block, since nothing may follow a segment that
    /// does not fill its slot. Issues no barrier: [`Self::write_commit`]'s
    /// record is still a separate write after every payload of the
    /// transaction has been handed to the device.
    pub fn write_record<'a>(
        &mut self,
        targets: &[u32],
        device: &dyn BlockDevice,
        payload: &mut dyn FnMut(usize) -> Option<&'a [u8]>,
    ) -> Result<u32, Ext2Error> {
        debug_assert!(targets.len() <= self.max_entries());
        self.flush_revokes(device)?;
        let header_slot = self.reserve(1 + targets.len() as u32)?;
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, REC_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, self.seq);
        put_le32(self.header.as_mut_slice(), 8, REC_DATA);
        put_le32(self.header.as_mut_slice(), 12, targets.len() as u32);
        for (i, block) in targets.iter().enumerate() {
            put_le32(self.header.as_mut_slice(), REC_ENTRIES_OFF + i * 4, *block);
        }
        let first = header_slot + 1;
        for (i, block) in targets.iter().enumerate() {
            self.index_insert(first + i as u32, *block);
        }

        let total = 1 + targets.len();
        let mut done = 0usize;
        while done < total {
            let want = (total - done).min(RECORD_RUN);
            let room = self.contiguous_slots(header_slot + done as u32, want);
            let took = self.write_record_chunk(
                header_slot + done as u32,
                done == 0,
                done.saturating_sub(1),
                room,
                device,
                payload,
            )?;
            if took == 0 {
                return Err(Ext2Error::InvalidBlock);
            }
            done += took;
        }
        Ok(first)
    }

    /// One gathered write of the slots `slot..slot + room`, whose blocks the
    /// caller has established are consecutive: the staged header when
    /// `with_header`, then payloads `from..`. Answers the slots it actually
    /// took, which is fewer than `room` when a short payload ends the run.
    ///
    /// `#[inline(never)]`: the segment array is 512 bytes of frame that
    /// [`Self::write_record`] cannot carry on top of its own.
    #[inline(never)]
    fn write_record_chunk<'a>(
        &mut self,
        slot: u32,
        with_header: bool,
        from: usize,
        room: usize,
        device: &dyn BlockDevice,
        payload: &mut dyn FnMut(usize) -> Option<&'a [u8]>,
    ) -> Result<usize, Ext2Error> {
        let bs = self.block_size as usize;
        let offset = self.slot_offset(slot)?;
        let (crc, took) = {
            let mut segs: [&[u8]; RECORD_RUN] = [&[]; RECORD_RUN];
            let mut n = 0usize;
            while n < room.min(RECORD_RUN) {
                let seg = if with_header && n == 0 {
                    &self.header.as_slice()[..bs]
                } else {
                    let index = from + n - usize::from(with_header);
                    payload(index).ok_or(Ext2Error::DeviceError)?
                };
                segs[n] = seg;
                n += 1;
                // A slot the payload did not fill leaves a gap, so the next
                // segment of a gathered write would land short of its slot.
                if seg.len() != bs {
                    break;
                }
            }
            device
                .write_vectored(offset, &segs[..n])
                .map_err(Ext2Error::from)?;
            let mut crc = self.crc;
            for seg in &segs[..n] {
                crc = crc32_feed(crc, seg);
            }
            (crc, n)
        };
        self.crc = crc;
        self.writes += took;
        Ok(took)
    }

    /// Slots from `slot` whose blocks are one consecutive run on the device,
    /// capped at `want`. Always at least one: a run of a single slot is what
    /// a log whose file is fragmented falls back to.
    fn contiguous_slots(&self, slot: u32, want: usize) -> usize {
        let slots = self.slots.as_slice();
        let Some(base) = slots.get(slot as usize).copied() else {
            return 0;
        };
        let mut n = 1usize;
        while n < want {
            let Some(next) = slots.get(slot as usize + n).copied() else {
                break;
            };
            if base.checked_add(n as u32) != Some(next) {
                break;
            }
            n += 1;
        }
        n
    }

    /// Put one block into the log on its own, so the cache can give its slot
    /// away without the block's home ever holding uncommitted content.
    pub fn spill(
        &mut self,
        block: u32,
        data: &[u8],
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        self.write_record(&[block], device, &mut |_| Some(data))
            .map(|_| ())
    }

    /// Close the transaction. Its records become replayable the moment this
    /// block reaches the medium, and unreadable garbage if it does not.
    pub fn write_commit(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.flush_revokes(device)?;
        let slot = self.reserve(1)?;
        let crc = crc32_finish(self.crc);
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, REC_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, self.seq);
        put_le32(self.header.as_mut_slice(), 8, REC_COMMIT);
        put_le32(self.header.as_mut_slice(), 16, crc);
        // Outside the CRC it carries, so this write is not fed back in.
        let offset = self.slot_offset(slot)?;
        device
            .write_at(offset, &self.header.as_slice()[..self.block_size as usize])
            .map_err(Ext2Error::from)?;
        self.writes += 1;
        stats::note_commit();
        self.seq = self.seq.wrapping_add(1);
        self.revoke_undo.clear();
        self.crc = CRC32_INIT;
        Ok(())
    }

    /// Copy the logged blocks in slots `slot..slot + len` to home locations
    /// `block..block + len`. The log, never the cache, is the source: the
    /// cache may hold an operation's uncommitted changes to the same block.
    ///
    /// The caller owns the claim that the *homes* are consecutive and that
    /// none of them already holds its newest contents, so the whole run goes
    /// home in one write; the read side is gathered the same way wherever the
    /// log's own slots are consecutive. Bounded by [`CHECKPOINT_RUN`], which
    /// is what the preallocated staging buffer holds, so a check point still
    /// allocates nothing. No barrier here: the caller barriers the home writes
    /// once before [`Self::reset`].
    pub fn copy_run_to_home(
        &mut self,
        slot: u32,
        block: u32,
        len: u32,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let n = (len as usize).clamp(1, CHECKPOINT_RUN);
        let last = block
            .checked_add(n as u32 - 1)
            .ok_or(Ext2Error::InvalidBlock)?;
        if !self.in_volume(block) || !self.in_volume(last) {
            return Err(Ext2Error::InvalidBlock);
        }
        let bs = self.block_size as usize;
        let mut got = 0usize;
        while got < n {
            let take = self.contiguous_slots(slot + got as u32, n - got);
            if take == 0 {
                return Err(Ext2Error::InvalidBlock);
            }
            let from = self.slot_offset(slot + got as u32)?;
            device
                .read_at(
                    from,
                    &mut self.transfer.as_mut_slice()[got * bs..(got + take) * bs],
                )
                .map_err(Ext2Error::from)?;
            got += take;
        }
        device
            .write_at(
                block as u64 * self.block_size as u64,
                &self.transfer.as_slice()[..n * bs],
            )
            .map_err(Ext2Error::from)?;
        self.writes += n;
        Ok(())
    }

    pub fn head(&self) -> u32 {
        self.head
    }

    pub fn slot_block_at(&self, slot: u32) -> u32 {
        self.slot_block
            .as_slice()
            .get(slot as usize)
            .copied()
            .unwrap_or(0)
    }

    /// The log's own blocks, in slot order.
    pub fn slots(&self) -> &[u32] {
        self.slots.as_slice()
    }

    /// Revokes queued for the open operation, not yet in a record.
    pub fn queued_revokes(&self) -> usize {
        self.revokes.len()
    }

    /// Declare every logged block checked pointed: the log is empty again.
    /// The caller must have barriered the home-location writes first.
    pub fn reset(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.write_superblock(device)?;
        self.head = 1;
        self.op_head = 1;
        self.generation = self.generation.wrapping_add(1);
        self.index_clear();
        self.revokes.clear();
        self.revoke_undo.clear();
        self.crc = CRC32_INIT;
        Ok(())
    }

    /// Record a new mount of the volume. Written through at once when the log
    /// is empty; otherwise the next [`Self::reset`] carries it, and a crash
    /// before then leaves a stamp that no longer matches, which only costs the
    /// next mount its write access.
    pub fn restamp(&mut self, stamp: [u32; 2], device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.stamp = stamp;
        if self.head != 1 {
            return Ok(());
        }
        self.write_superblock(device)
    }

    /// The log superblock, naming `seq` as the first record's sequence: only
    /// correct while the log is empty or being emptied.
    fn write_superblock(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.header.as_mut_slice().fill(0);
        put_le32(self.header.as_mut_slice(), 0, SB_MAGIC);
        put_le32(self.header.as_mut_slice(), 4, FORMAT_VERSION);
        put_le32(self.header.as_mut_slice(), 8, self.block_size);
        let capacity = self.capacity();
        put_le32(self.header.as_mut_slice(), 12, capacity);
        put_le32(self.header.as_mut_slice(), 16, self.seq);
        let identity = self.identity();
        put_le32(self.header.as_mut_slice(), SB_IDENTITY_OFF, identity);
        let [mnt_count, mtime] = self.stamp;
        put_le32(self.header.as_mut_slice(), SB_STAMP_OFF, mnt_count);
        put_le32(self.header.as_mut_slice(), SB_STAMP_OFF + 4, mtime);
        let crc = crate::verity::crc32(&self.header.as_slice()[..SB_CRC_SPAN]);
        put_le32(self.header.as_mut_slice(), SB_CRC_SPAN, crc);
        let offset = self.slot_offset(0)?;
        device
            .write_at(offset, &self.header.as_slice()[..self.block_size as usize])
            .map_err(Ext2Error::from)?;
        self.writes += 1;
        Ok(())
    }

    fn slot_offset(&self, slot: u32) -> Result<u64, Ext2Error> {
        let block = self
            .slots
            .as_slice()
            .get(slot as usize)
            .copied()
            .ok_or(Ext2Error::InvalidBlock)?;
        Ok(block as u64 * self.block_size as u64)
    }

    fn reserve(&mut self, want: u32) -> Result<u32, Ext2Error> {
        let slot = self.head;
        let end = slot.checked_add(want).ok_or(Ext2Error::NoSpace)?;
        if end as usize > self.slots.len() {
            return Err(Ext2Error::NoSpace);
        }
        self.head = end;
        Ok(slot)
    }

    fn write_slot_from_header(
        &mut self,
        slot: u32,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let bs = self.block_size as usize;
        let offset = self.slot_offset(slot)?;
        device
            .write_at(offset, &self.header.as_slice()[..bs])
            .map_err(Ext2Error::from)?;
        self.crc = crc32_feed(self.crc, &self.header.as_slice()[..bs]);
        self.writes += 1;
        Ok(())
    }

    /// The sequence a scan should expect at slot 1, or `None` for a log this
    /// boot must not read.
    ///
    /// Geometry alone is satisfied by a log built for a *different*
    /// filesystem of the same shape, and replaying that one writes its
    /// metadata into this volume — hence the identity field.
    /// The first record's sequence and, from a version-2 superblock, the
    /// stamp of the mount that wrote it.
    fn read_superblock(
        &mut self,
        device: &dyn BlockDevice,
    ) -> Result<Option<(u32, Option<[u32; 2]>)>, Ext2Error> {
        let bs = self.block_size as usize;
        let offset = self.slot_offset(0)?;
        device
            .read_at(offset, &mut self.header.as_mut_slice()[..bs])
            .map_err(Ext2Error::from)?;
        let identity = self.identity();
        let data = self.header.as_slice();
        let crc_span = match le32(data, 4) {
            FORMAT_VERSION => SB_CRC_SPAN,
            FORMAT_V1 => SB_CRC_SPAN_V1,
            _ => return Ok(None),
        };
        if le32(data, 0) != SB_MAGIC {
            return Ok(None);
        }
        if le32(data, 8) != self.block_size || le32(data, 12) != self.capacity() {
            return Ok(None);
        }
        if le32(data, SB_IDENTITY_OFF) != identity {
            return Ok(None);
        }
        if crate::verity::crc32(&data[..crc_span]) != le32(data, crc_span) {
            return Ok(None);
        }
        let stamp = (crc_span == SB_CRC_SPAN)
            .then(|| [le32(data, SB_STAMP_OFF), le32(data, SB_STAMP_OFF + 4)]);
        Ok(Some((le32(data, 16), stamp)))
    }

    /// What ties this log to this file on this volume. Not a hash: a mismatch
    /// means "not mine", which is all the mount needs to refuse a replay.
    fn identity(&self) -> u32 {
        let first = self.slots.as_slice().get(1).copied().unwrap_or(0);
        self.inode.wrapping_mul(0x0100_0193) ^ first.wrapping_mul(0x0100_0193) ^ self.blocks_count
    }

    /// The record at `slot`, or `None` when it is not the `expect` sequence's
    /// — which is how a scan finds the end of the committed region without a
    /// terminator.
    fn read_header(
        &mut self,
        slot: u32,
        expect: u32,
        device: &dyn BlockDevice,
    ) -> Result<Option<RecHeader>, Ext2Error> {
        if slot as usize >= self.slots.len() {
            return Ok(None);
        }
        let bs = self.block_size as usize;
        let offset = self.slot_offset(slot)?;
        device
            .read_at(offset, &mut self.header.as_mut_slice()[..bs])
            .map_err(Ext2Error::from)?;
        let data = self.header.as_slice();
        if le32(data, 0) != REC_MAGIC || le32(data, 4) != expect {
            return Ok(None);
        }
        Ok(Some(RecHeader {
            kind: le32(data, 8),
            count: le32(data, 12),
            crc: le32(data, 16),
        }))
    }

    /// Apply every committed transaction the log holds.
    ///
    /// Two passes over the same records: the first decides how far the
    /// committed region reaches, because a transaction is only replayable once
    /// its own commit record checks out; the second builds the disposition of
    /// each slot and writes the survivors home.
    fn replay(&mut self, device: &dyn BlockDevice) -> Result<JournalRecovery, Ext2Error> {
        let first_seq = self.seq;
        let mut expect = first_seq;
        let mut end = 1u32;
        let mut transactions = 0u32;
        while let Some(next) = self.scan_transaction(end, expect, device)? {
            end = next;
            expect = expect.wrapping_add(1);
            transactions += 1;
        }
        if transactions == 0 {
            return Ok(JournalRecovery::NONE);
        }
        self.build_disposition(end, first_seq, device)?;
        let blocks = self.write_home(end, device)?;
        device.flush().map_err(Ext2Error::from)?;
        self.seq = expect;
        self.head = end;
        Ok(JournalRecovery {
            transactions,
            blocks,
            continuous: false,
        })
    }

    /// Where the transaction beginning at `from` ends, if it committed and its
    /// CRC agrees with what is on the medium.
    fn scan_transaction(
        &mut self,
        from: u32,
        expect: u32,
        device: &dyn BlockDevice,
    ) -> Result<Option<u32>, Ext2Error> {
        let bs = self.block_size as usize;
        let mut slot = from;
        let mut crc = CRC32_INIT;
        loop {
            let Some(header) = self.read_header(slot, expect, device)? else {
                return Ok(None);
            };
            match header.kind {
                REC_COMMIT => {
                    return if crc32_finish(crc) == header.crc {
                        Ok(Some(slot + 1))
                    } else {
                        Ok(None)
                    };
                }
                REC_REVOKE => {
                    if header.count as usize > self.max_entries() {
                        return Ok(None);
                    }
                    crc = crc32_feed(crc, &self.header.as_slice()[..bs]);
                    slot += 1;
                }
                REC_DATA => {
                    let count = header.count;
                    if count as usize > self.max_entries()
                        || slot as usize + 1 + count as usize > self.slots.len()
                    {
                        return Ok(None);
                    }
                    // A target becomes a write offset and it came off the
                    // medium, so one outside the volume ends the committed
                    // region — the disposition every malformed field gets.
                    for i in 0..count as usize {
                        let block = le32(self.header.as_slice(), REC_ENTRIES_OFF + i * 4);
                        if !self.in_volume(block) {
                            return Ok(None);
                        }
                    }
                    crc = crc32_feed(crc, &self.header.as_slice()[..bs]);
                    for i in 0..count {
                        let offset = self.slot_offset(slot + 1 + i)?;
                        device
                            .read_at(offset, &mut self.transfer.as_mut_slice()[..bs])
                            .map_err(Ext2Error::from)?;
                        crc = crc32_feed(crc, &self.transfer.as_slice()[..bs]);
                    }
                    slot += 1 + count;
                }
                _ => return Ok(None),
            }
        }
    }

    /// Fill `slot_block` and its index for the committed region below `end`,
    /// applying each `REVOKE` to the records before it.
    fn build_disposition(
        &mut self,
        end: u32,
        first_seq: u32,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        self.index_clear();
        let mut expect = first_seq;
        let mut slot = 1u32;
        while slot < end {
            // Re-read rather than held from the scan: a second pass costs
            // reads the recovery path can afford, where a per-record
            // allocation is not.
            let Some(header) = self.read_header(slot, expect, device)? else {
                break;
            };
            match header.kind {
                REC_COMMIT => {
                    expect = expect.wrapping_add(1);
                    slot += 1;
                }
                REC_REVOKE => {
                    let count = (header.count as usize).min(self.max_entries());
                    for i in 0..count {
                        let block = le32(self.header.as_slice(), REC_ENTRIES_OFF + i * 4);
                        // Every indexed slot is below this record's, so the
                        // chain walk covers exactly the records before it.
                        self.clear_mappings(block, false)?;
                    }
                    slot += 1;
                }
                REC_DATA => {
                    // Bounded here as well as in the scan: a device that
                    // answers differently on this second read must not index
                    // out of the array. One local for both the indexing and
                    // the cursor — advancing by the *claimed* count would step
                    // past records the clamp just decided this one misses.
                    let count = (header.count as usize).min(self.max_entries());
                    for i in 0..count {
                        let target = slot as usize + 1 + i;
                        if target >= self.slot_block.len() {
                            break;
                        }
                        let block = le32(self.header.as_slice(), REC_ENTRIES_OFF + i * 4);
                        if self.in_volume(block) {
                            self.index_insert(target as u32, block);
                        }
                    }
                    slot += 1 + count as u32;
                }
                _ => break,
            }
        }
        Ok(())
    }

    fn write_home(&mut self, end: u32, device: &dyn BlockDevice) -> Result<u32, Ext2Error> {
        let limit = end.min(self.slot_block.len() as u32);
        let mut written = 0u32;
        let mut slot = 1u32;
        while slot < limit {
            let block = self.slot_block[slot as usize];
            if block == 0 || !self.in_volume(block) {
                slot += 1;
                continue;
            }
            // Ascending slot order is what makes "the last write of a block
            // wins" hold, so a run only ever grows forwards and only over the
            // next home block: merging changes the request count and never
            // the order.
            let mut len = 1u32;
            while (len as usize) < CHECKPOINT_RUN && slot + len < limit {
                let next = self.slot_block[(slot + len) as usize];
                if block.checked_add(len) != Some(next) || !self.in_volume(next) {
                    break;
                }
                len += 1;
            }
            self.copy_run_to_home(slot, block, len, device)?;
            written += len;
            slot += len;
        }
        Ok(written)
    }
}

fn entries_per_header(block_size: u32) -> usize {
    (block_size as usize).saturating_sub(REC_ENTRIES_OFF) / 4
}
