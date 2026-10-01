//! The metadata log: the volume's jbd2 journal, in the journal inode.
//!
//! Metadata reaches the journal in a committed transaction before its home is
//! written; file data never goes through it, reaching its home ahead of the
//! commit block that names it (`data=ordered`). The format is jbd2's —
//! version-3 tags, revoke and commit blocks, checksummed — so `e2fsck` and
//! Linux replay what this kernel logged, and [`Journal::attach`] replays what
//! they logged.
//!
//! The log is used linearly from `s_first` and emptied ([`Journal::reset`])
//! once every logged block is home. Sequence numbers never go back, so a stale
//! block from an earlier use never reads as the next transaction.
//!
//! A commit block goes out only behind a flush covering its transaction's
//! blocks and the data they name, so replay trusts a commit block alone.
//! Nothing goes home from a transaction until a second flush has made its
//! commit durable.
//!
//! Records wait in an in-memory ring, and the operations waiting there form
//! one compound transaction with one commit block ([`Journal::seal`]): jbd2's
//! group commit. A block whose newest record is an unwritten image in the
//! compound is rewritten in place ([`Journal::rewrite`]) rather than logged
//! again, and checksums, escapes and revoke blocks are settled at the seal.
//!
//! A replayed revoke cancels the block's copies in every transaction up to and
//! including its own, so a block revoked and then logged again in one compound
//! must not be revoked there: revokes wait for the seal, and only those whose
//! block has no newer copy are written.

use slopos_mm::slab::MAX_ALLOC_SIZE;
use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::frame::{Frame, PageCacheMeta};
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::{KBTreeMap, KBox, KVec, write_field};

use slopos_ext4_core::jbd2::{self, Format, Tag, TagCursor, blocktype, feature, tag_flag};
use slopos_ext4_core::recovery::{self, JournalIo, RecoverError, RevokeTable};

use super::Ext2Error;
use crate::blockdev::{BlockDevice, stats};

/// Smallest log this kernel attaches: an operation whose metadata does not
/// fit refuses, so a smaller one would refuse a routine `create`.
pub const MIN_LOG_SLOTS: u32 = 32;

/// Slots this kernel uses, the journal superblock included. A journal longer
/// than this is used up to the cap and no further; its tail stays readable
/// for replaying what another implementation logged there.
///
/// The cap is what holds every per-slot array to `PER_SLOT_LIMIT`. 32 768
/// slots covers a 128 MiB journal over 4 KiB blocks.
pub const MAX_LOG_SLOTS: u32 = 32 * 1024;

/// What one per-slot array may take: a quarter of a single allocation's
/// ceiling, so the log's arrays together stay well inside the heap's
/// large-allocation tier.
const PER_SLOT_LIMIT: usize = MAX_ALLOC_SIZE / 4;
const _: () = assert!(MAX_LOG_SLOTS as usize * size_of::<(u32, u32)>() <= PER_SLOT_LIMIT);
const _: () = assert!(MAX_LOG_SLOTS as usize * size_of::<u32>() <= PER_SLOT_LIMIT);
const _: () =
    assert!((MAX_LOG_SLOTS as usize).next_power_of_two() * size_of::<u32>() <= PER_SLOT_LIMIT);

/// Chain terminator in the block index. Slot 0 holds the journal superblock,
/// so it is never a record's slot and can stand in for "no link".
const NIL_SLOT: u32 = 0;

/// Slots one record write may gather. The segment array is on the stack, so
/// this is a stack cost as much as an I/O size: 32 segments is 512 bytes of
/// frame.
const RECORD_RUN: usize = 32;

/// Slots one check-point step may carry home in one request. Bounds the
/// staging buffer, which is preallocated: eight 4 KiB blocks, well inside one
/// block request.
const CHECKPOINT_RUN: usize = 8;

/// Slots whose images the log may hold in memory: 8 MiB at 4 KiB blocks.
/// Past it an operation's records force the ring out first.
const PENDING_SLOTS_MAX: usize = 2048;

/// Slots sealing may write beyond the revoke blocks: the commit of a compound
/// sealed ahead of a mid-operation record, the open operation's own commit,
/// and the revoke block splitting the revokes between the two can add.
const SEAL_RESERVE: u32 = 3;

/// Ring slots [`Journal::spill`] needs beyond [`Journal::pending_room`]'s
/// reserve, which its record reserves afresh: the commit its seal writes, the
/// revoke block splitting committed from open revokes can add, a descriptor
/// and the payload.
pub const SPILL_SLOTS: u32 = 4;

/// The volume the log belongs to, so a target block read off the medium can be
/// refused before it becomes a write offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogExtent {
    pub first_data_block: u32,
    pub blocks_count: u32,
}

impl LogExtent {
    fn contains(self, block: u64) -> bool {
        block >= u64::from(self.first_data_block) && block < u64::from(self.blocks_count)
    }
}

/// What the journal needs to know about the filesystem it serves.
#[derive(Debug, Clone, Copy)]
pub struct LogVolume {
    pub block_size: u32,
    pub extent: LogExtent,
    /// The volume checksums its metadata, so the log checksums its blocks.
    pub csum: bool,
    /// The volume's block numbers are 64-bit, and so are the log's.
    pub bit64: bool,
    pub inode: u32,
    /// The volume says its journal holds transactions to replay.
    pub needs_recovery: bool,
}

/// Where the journal inode's blocks are: `(first journal block, first
/// filesystem block, length)` runs covering the whole journal in order.
pub type JournalRuns = KVec<(u32, u32, u32)>;

/// What attaching a log did, for the mount log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalRecovery {
    /// Committed transactions the scan found and replayed.
    pub transactions: u32,
    /// Blocks written back to their home locations by the replay.
    pub blocks: u32,
}

impl JournalRecovery {
    pub const NONE: Self = Self {
        transactions: 0,
        blocks: 0,
    };

    pub fn replayed(self) -> bool {
        self.transactions > 0
    }
}

/// The journal could not be used, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    Fs(Ext2Error),
    /// The journal superblock is not one, or does not describe this volume.
    BadSuperblock,
    /// The journal declares a feature this kernel neither replays nor writes.
    Unsupported,
    /// A committed transaction is damaged or names a block replay may not
    /// write; `e2fsck` decides what to keep.
    Corrupt,
    /// The journal holds transactions the volume does not say need recovery,
    /// which may be older than the homes they would overwrite; `e2fsck`
    /// decides.
    Unflagged,
}

impl From<Ext2Error> for AttachError {
    fn from(e: Ext2Error) -> Self {
        Self::Fs(e)
    }
}

#[derive(slopos_ostd::SlotFields)]
pub struct Journal {
    /// Home block of each journal block the log uses. `slots[0]` is the
    /// journal superblock.
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
    /// Blocks revoked in the open transaction, to the operation that revoked
    /// each: below `op_seq` a committed one.
    revokes: KBTreeMap<u32, u32>,
    /// The open operation's own entries in `revokes`, for an abort.
    op_revokes: KVec<u32>,
    /// Numbers operations; the open one's entries carry it.
    op_seq: u32,
    /// Mappings a revoke cleared, so an abort can put them back.
    revoke_undo: KVec<(u32, u32)>,
    /// Staging buffer for a descriptor, revoke or commit block. Preallocated:
    /// a block does not fit the 2 KiB stack budget and a commit must not
    /// allocate.
    header: KVec<u8>,
    /// Second staging buffer, for reading slots back during a check point and
    /// for an escaped copy. [`CHECKPOINT_RUN`] blocks long, so a run whose
    /// home locations are consecutive goes home in one request.
    transfer: KVec<u8>,
    /// The journal superblock as last written, a block long, so every field
    /// this kernel does not own is written back as it was found.
    jsb: KVec<u8>,
    fmt: Format,
    block_size: u32,
    inode: u32,
    /// Every block the log writes to is checked against this range: a record's
    /// target becomes a write offset and the record came off the medium.
    blocks_count: u32,
    first_data_block: u32,
    /// `s_first`: where the log's records begin.
    first: u32,
    /// Next free slot; `first` exactly when the log is empty.
    head: u32,
    /// Sequence the open transaction carries. Never reset.
    seq: u32,
    /// Sequence of the transaction at `first`: what the journal superblock
    /// names while the log is live.
    base_seq: u32,
    /// The journal superblock says the log is live (`s_start` nonzero).
    live: bool,
    /// Bumped by every [`Self::reset`]. A writeback pass records it, so a
    /// pass resumed after another emptied and refilled the log cannot mistake
    /// its own slot indices for the new generation's.
    generation: u32,
    /// Bumped by every abort that put mappings back. A pass whose cursor went
    /// by a slot before it was restored must not empty the log behind it.
    restores: u32,
    /// `head` when the open operation began, for the abort rewind; `head`
    /// again once it ends, so between operations `sealed..op_head` is every
    /// committed record still waiting for a commit block.
    op_head: u32,
    /// First slot of the open transaction: every record below it is covered
    /// by a commit block.
    sealed: u32,
    /// Device writes issued since the last flush or the last time the caller
    /// took the count.
    writes: usize,
    /// Flushes the log issued itself since the caller last took the count.
    flushes: usize,
    /// Images of the slots `written..head`, slot `s` in
    /// `pending[s % pending.len()]`. Empty is a write-through log.
    pending: KVec<Frame<PageCacheMeta>>,
    /// Slots below this have been handed to the device.
    written: u32,
    /// Records below this are final: their copies escaped and checksummed,
    /// their descriptors sealed. Never below `written`, so nothing reaches
    /// the device unsettled, and nothing below it is rewritten in place.
    settled: u32,
    /// End of the newest commit block handed to the device before the last
    /// flush: the durable prefix. Never inside a transaction.
    barriered: u32,
    /// End of the newest commit block handed to the device so far.
    committed_written: u32,
    /// One bit per slot, set on a commit block's.
    commit_marks: KVec<u64>,
    /// One bit per slot, set on a copy whose first word was zeroed because it
    /// read as the journal magic; a read of it puts the word back.
    escaped: KVec<u64>,
    /// The open operation outgrew the ring, so its records go straight to
    /// the device.
    write_through: bool,
}

/// The revokes a replay found: block to the newest transaction that revoked
/// it, open-addressed over chunks no larger than one allocation may be, and
/// grown fallibly, so a journal of more revokes than memory holds is an
/// error rather than a panic.
struct Revoked {
    chunks: KVec<KVec<(u32, u32)>>,
    capacity: usize,
    len: usize,
}

/// Entries per chunk: 64 KiB of them.
const REVOKE_CHUNK: usize = 8192;

/// No block a replay writes: a volume's blocks are below `u32::MAX`.
const NO_BLOCK: u32 = u32::MAX;

impl Revoked {
    const fn new() -> Self {
        Self {
            chunks: KVec::new(),
            capacity: 0,
            len: 0,
        }
    }

    fn slot(&self, i: usize) -> (u32, u32) {
        self.chunks.as_slice()[i / REVOKE_CHUNK].as_slice()[i % REVOKE_CHUNK]
    }

    fn slot_mut(&mut self, i: usize) -> &mut (u32, u32) {
        &mut self.chunks.as_mut_slice()[i / REVOKE_CHUNK].as_mut_slice()[i % REVOKE_CHUNK]
    }

    /// The slot holding `block`, or the empty one it would go in.
    fn find(&self, block: u32) -> usize {
        let mask = self.capacity - 1;
        let mut at = (u64::from(block).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize & mask;
        while !matches!(self.slot(at).0, b if b == block || b == NO_BLOCK) {
            at = (at + 1) & mask;
        }
        at
    }

    fn grow(&mut self) -> bool {
        let capacity = (self.capacity * 2).max(REVOKE_CHUNK);
        let Ok(mut chunks) = KVec::with_capacity(capacity / REVOKE_CHUNK) else {
            return false;
        };
        for _ in 0..capacity / REVOKE_CHUNK {
            let Ok(chunk) = KVec::filled((NO_BLOCK, 0), REVOKE_CHUNK) else {
                return false;
            };
            if chunks.push(chunk).is_err() {
                return false;
            }
        }
        let old = core::mem::replace(&mut self.chunks, chunks);
        self.capacity = capacity;
        for &(block, age) in old.as_slice().iter().flat_map(|c| c.as_slice()) {
            if block != NO_BLOCK {
                let at = self.find(block);
                *self.slot_mut(at) = (block, age);
            }
        }
        true
    }
}

impl RevokeTable for Revoked {
    fn note(&mut self, block: u64, age: u32) -> bool {
        // A block no tag may name has no copy to cancel.
        let Ok(block) = u32::try_from(block) else {
            return true;
        };
        if block == NO_BLOCK {
            return true;
        }
        if (self.len + 1) * 2 > self.capacity && !self.grow() {
            return false;
        }
        let at = self.find(block);
        let slot = self.slot_mut(at);
        if slot.0 == NO_BLOCK {
            *slot = (block, age);
            self.len += 1;
        } else {
            slot.1 = slot.1.max(age);
        }
        true
    }

    fn newest(&mut self, block: u64) -> Option<u32> {
        let block = u32::try_from(block).ok().filter(|&b| b != NO_BLOCK)?;
        if self.len == 0 {
            return None;
        }
        let slot = self.slot(self.find(block));
        (slot.0 == block).then_some(slot.1)
    }
}

/// The journal's blocks for a replay, wherever the runs put them.
struct ReplayIo<'a> {
    runs: &'a [(u32, u32, u32)],
    block_size: u32,
    device: &'a dyn BlockDevice,
}

impl ReplayIo<'_> {
    fn home_of(&self, block: u32) -> Option<u64> {
        let i = self.runs.partition_point(|&(start, _, _)| start <= block);
        let (start, home, len) = *self.runs.get(i.checked_sub(1)?)?;
        (block - start < len).then(|| u64::from(home) + u64::from(block - start))
    }
}

/// Whether filesystem block `block` is one of the journal's own.
fn in_runs(runs: &[(u32, u32, u32)], block: u64) -> bool {
    runs.iter()
        .any(|&(_, home, len)| block >= u64::from(home) && block < u64::from(home) + u64::from(len))
}

impl JournalIo for ReplayIo<'_> {
    type Error = Ext2Error;

    fn read(&mut self, block: u32, buf: &mut [u8]) -> Result<(), Ext2Error> {
        let home = self.home_of(block).ok_or(Ext2Error::InvalidBlock)?;
        self.device
            .read_at(home * u64::from(self.block_size), buf)
            .map_err(Ext2Error::from)
    }

    fn write_home(&mut self, block: u64, data: &[u8]) -> Result<(), Ext2Error> {
        let at = block
            .checked_mul(u64::from(self.block_size))
            .ok_or(Ext2Error::InvalidBlock)?;
        self.device.write_at(at, data).map_err(Ext2Error::from)
    }
}

fn bit(marks: &[u64], slot: u32) -> bool {
    marks[slot as usize / 64] & (1 << (slot % 64)) != 0
}

fn set_bit(marks: &mut [u64], slot: u32, on: bool) {
    let word = &mut marks[slot as usize / 64];
    if on {
        *word |= 1 << (slot % 64);
    } else {
        *word &= !(1 << (slot % 64));
    }
}

impl Journal {
    /// Take over the journal: validate its superblock, replay whatever a
    /// previous mount — this kernel's or another's — committed and did not
    /// write home, and leave the log empty, its superblock saying so.
    ///
    /// `#[inline(never)]`, built field by field into the heap slot: a whole
    /// `Journal` rvalue plus the allocations behind it does not fit the
    /// 2 KiB stack gate.
    #[inline(never)]
    pub fn attach(
        runs: &JournalRuns,
        volume: LogVolume,
        device: &dyn BlockDevice,
    ) -> Result<(KBox<Self>, JournalRecovery), AttachError> {
        let bs = volume.block_size as usize;
        let mut jsb = KVec::<u8>::zeroed(bs).map_err(|_| Ext2Error::OutOfMemory)?;
        let mut io = ReplayIo {
            runs: runs.as_slice(),
            block_size: volume.block_size,
            device,
        };
        io.read(0, jsb.as_mut_slice())?;
        let sb = jbd2::Superblock::parse(jsb.as_slice()).map_err(|_| AttachError::BadSuperblock)?;
        let mapped = runs
            .as_slice()
            .last()
            .map_or(0, |&(s, _, l)| s.saturating_add(l));
        if sb.block_size != volume.block_size || sb.maxlen > mapped {
            return Err(AttachError::BadSuperblock);
        }
        if !sb.supported() || sb.incompat & feature::INCOMPAT_FAST_COMMIT != 0 {
            return Err(AttachError::Unsupported);
        }
        if sb.start != 0 && !volume.needs_recovery {
            return Err(AttachError::Unflagged);
        }
        let recovery = Self::replay(&mut io, &sb, volume)?;
        if recovery.transactions > 0 {
            device.flush().map_err(Ext2Error::from)?;
        }
        let used = sb.maxlen.min(MAX_LOG_SLOTS);
        if used < sb.first.saturating_add(MIN_LOG_SLOTS) {
            return Err(AttachError::BadSuperblock);
        }
        let slots = Self::slot_homes(&io, used, volume)?;

        let mut incompat = feature::INCOMPAT_REVOKE;
        if volume.bit64 {
            incompat |= feature::INCOMPAT_64BIT;
        }
        if volume.csum {
            incompat |= feature::INCOMPAT_CSUM_V3;
        }
        jbd2::set_features(jsb.as_mut_slice(), incompat);
        let fmt = jbd2::Superblock::parse(jsb.as_slice())
            .map_err(|_| AttachError::BadSuperblock)?
            .format();
        let mut journal = KBox::try_init(Self::init(slots, jsb, fmt, volume, sb.first))
            .map_err(|_| Ext2Error::OutOfMemory)?;
        journal.seq = recovery.next_sequence;
        journal.base_seq = recovery.next_sequence;
        journal.reset(device)?;
        journal.alloc_pending();
        Ok((
            journal,
            JournalRecovery {
                transactions: recovery.transactions,
                blocks: recovery.blocks,
            },
        ))
    }

    #[inline(never)]
    fn replay(
        io: &mut ReplayIo<'_>,
        sb: &jbd2::Superblock,
        volume: LogVolume,
    ) -> Result<recovery::Recovery, AttachError> {
        let bs = volume.block_size as usize;
        let mut meta = KVec::<u8>::zeroed(bs).map_err(|_| Ext2Error::OutOfMemory)?;
        let mut data = KVec::<u8>::zeroed(bs).map_err(|_| Ext2Error::OutOfMemory)?;
        let mut revokes = Revoked::new();
        let runs = io.runs;
        recovery::recover(
            io,
            &mut revokes,
            sb,
            meta.as_mut_slice(),
            data.as_mut_slice(),
            &|b| volume.extent.contains(b) && !in_runs(runs, b),
        )
        .map_err(|e| match e {
            RecoverError::Io(e) => AttachError::Fs(e),
            RecoverError::Corrupt => AttachError::Corrupt,
            RecoverError::Unsupported => AttachError::Unsupported,
            RecoverError::OutOfMemory => AttachError::Fs(Ext2Error::OutOfMemory),
        })
    }

    /// The filesystem block behind each of the first `used` journal blocks.
    #[inline(never)]
    fn slot_homes(
        io: &ReplayIo<'_>,
        used: u32,
        volume: LogVolume,
    ) -> Result<KVec<u32>, AttachError> {
        let mut slots = KVec::with_capacity(used as usize).map_err(|_| Ext2Error::OutOfMemory)?;
        for block in 0..used {
            let home = io.home_of(block).ok_or(AttachError::BadSuperblock)?;
            if !volume.extent.contains(home) {
                return Err(AttachError::BadSuperblock);
            }
            slots
                .push(home as u32)
                .map_err(|_| Ext2Error::OutOfMemory)?;
        }
        Ok(slots)
    }

    fn init(
        slots: KVec<u32>,
        jsb: KVec<u8>,
        fmt: Format,
        volume: LogVolume,
        first: u32,
    ) -> impl Init<Self, AllocError> {
        let count = slots.len();
        let buckets = count.next_power_of_two();
        let block_size = volume.block_size;
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                write_field!(slot, slots, slots);
                write_field!(slot, slot_block, KVec::zeroed(count)?);
                write_field!(slot, buckets, KVec::zeroed(buckets)?);
                write_field!(slot, chain_next, KVec::zeroed(count)?);
                write_field!(slot, chain_prev, KVec::zeroed(count)?);
                write_field!(slot, bucket_shift, u32::BITS - buckets.trailing_zeros());
                write_field!(slot, revokes, KBTreeMap::new());
                write_field!(slot, op_revokes, KVec::new());
                write_field!(slot, op_seq, 0);
                write_field!(slot, revoke_undo, KVec::with_capacity(count)?);
                write_field!(slot, header, KVec::zeroed(block_size as usize)?);
                write_field!(
                    slot,
                    transfer,
                    KVec::zeroed(block_size as usize * CHECKPOINT_RUN)?
                );
                write_field!(slot, jsb, jsb);
                write_field!(slot, fmt, fmt);
                write_field!(slot, block_size, block_size);
                write_field!(slot, inode, volume.inode);
                write_field!(slot, blocks_count, volume.extent.blocks_count);
                write_field!(slot, first_data_block, volume.extent.first_data_block);
                write_field!(slot, first, first);
                write_field!(slot, head, first);
                write_field!(slot, seq, 1);
                write_field!(slot, base_seq, 1);
                write_field!(slot, live, false);
                write_field!(slot, generation, 0);
                write_field!(slot, restores, 0);
                write_field!(slot, op_head, first);
                write_field!(slot, sealed, first);
                write_field!(slot, writes, 0);
                write_field!(slot, flushes, 0);
                write_field!(slot, pending, KVec::new());
                write_field!(slot, written, first);
                write_field!(slot, settled, first);
                write_field!(slot, barriered, first);
                write_field!(slot, committed_written, first);
                write_field!(slot, commit_marks, KVec::zeroed(count.div_ceil(64))?);
                write_field!(slot, escaped, KVec::zeroed(count.div_ceil(64))?);
                write_field!(slot, write_through, false);
                Ok(slot.finish())
            },
        )
    }

    /// The in-memory ring, as large as the log and [`PENDING_SLOTS_MAX`]
    /// allow. Best effort: whatever could not be allocated only shortens
    /// the ring, and none at all leaves the log writing through.
    fn alloc_pending(&mut self) {
        let want = (self.capacity() as usize).min(PENDING_SLOTS_MAX);
        if self.pending.try_reserve(want).is_err() {
            return;
        }
        while self.pending.len() < want {
            let Some(frame) = Frame::<PageCacheMeta>::alloc() else {
                break;
            };
            if self.pending.push(frame).is_err() {
                break;
            }
        }
    }

    /// Shorten the ring to `slots` images, so a test reaches the full-ring
    /// path on a log small enough to build in memory.
    #[cfg(feature = "tests")]
    pub fn shrink_ring_for_test(&mut self, slots: usize) {
        debug_assert!(
            self.written == self.head,
            "shrinking a ring that holds records"
        );
        self.pending.truncate(slots);
    }

    /// Records go to the ring, not the device.
    pub fn defers(&self) -> bool {
        !self.pending.is_empty() && !self.write_through
    }

    /// Slots appended and not yet handed to the device.
    pub fn pending_slots(&self) -> u32 {
        self.head.saturating_sub(self.written)
    }

    /// Slots a seal may still need: its revoke blocks and the commits.
    pub fn seal_room(&self) -> u32 {
        SEAL_RESERVE + self.revoke_blocks(self.revokes.len())
    }

    fn revoke_blocks(&self, entries: usize) -> u32 {
        entries.div_ceil(self.fmt.revokes_per_block().max(1)) as u32
    }

    /// Slots the ring can still take for the open operation's records before
    /// it must be written out, what sealing needs held back.
    pub fn pending_room(&self) -> u32 {
        (self.pending.len() as u32).saturating_sub(self.pending_slots() + self.seal_room())
    }

    /// Slots the ring holds at most.
    pub fn pending_capacity(&self) -> u32 {
        self.pending.len() as u32
    }

    /// Send the rest of the open operation's records straight to the
    /// device: it needs more slots than the ring has. The caller has made
    /// every earlier record durable, so none is overtaken on the medium.
    pub fn set_write_through(&mut self) {
        self.write_through = true;
    }

    pub fn writes_through(&self) -> bool {
        self.pending.is_empty() || self.write_through
    }

    /// Whether `slot` belongs to a transaction whose commit block was made
    /// durable.
    pub fn slot_durable(&self, slot: u32) -> bool {
        slot < self.barriered
    }

    /// Every committed operation is on the medium behind a flush, commit
    /// block included, and the ring holds nothing the device lacks. The open
    /// operation's own records carry no commit and are not waited for.
    pub fn is_durable(&self) -> bool {
        !self.has_unsealed() && self.written >= self.head && self.barriered >= self.sealed
    }

    /// A write-out has something to do: slots the device lacks, or committed
    /// operations still waiting for their commit block.
    pub fn owes_write(&self) -> bool {
        self.pending_slots() > 0 || self.has_unsealed()
    }

    /// Committed operations wait in the open transaction for a commit block:
    /// records of theirs, or only revokes.
    pub fn has_unsealed(&self) -> bool {
        self.sealed < self.op_head || self.committed_revokes() > 0
    }

    fn committed_revokes(&self) -> usize {
        self.revokes.len() - self.op_revokes.len()
    }

    /// The device has flushed everything handed to it so far.
    pub fn note_barrier(&mut self) {
        self.barriered = self.committed_written;
    }

    /// Flush the device on the log's own account: what a commit block waits
    /// for. Counted for the cache, which owns the barrier bookkeeping.
    fn flush(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        device.flush().map_err(Ext2Error::from)?;
        self.note_barrier();
        self.writes = 0;
        self.flushes += 1;
        Ok(())
    }

    /// Seal the compound and hand the ring's slots to the device. Every
    /// commit block goes behind a flush; the caller flushes behind the last.
    pub fn write_pending(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        self.seal(device)?;
        self.write_pending_until(self.head, usize::MAX, device)
            .map(|_| ())
    }

    /// The ring's slots below `limit`, at most `budget` of them, unsealed:
    /// the caller chose `limit` at a commit block. Answers whether any below
    /// `limit` are still in the ring.
    pub fn write_pending_until(
        &mut self,
        limit: u32,
        budget: usize,
        device: &dyn BlockDevice,
    ) -> Result<bool, Ext2Error> {
        let end = limit.min(self.head);
        self.settle_descriptors(end);
        let mut left = budget;
        while self.written < end && left > 0 {
            if bit(&self.commit_marks, self.written) {
                self.flush(device)?;
                self.write_ring_run(self.written, 1, device)?;
                self.committed_written = self.written + 1;
                self.written += 1;
                left -= 1;
                continue;
            }
            let want = ((end - self.written) as usize).min(RECORD_RUN).min(left);
            let mut run = self.contiguous_slots(self.written, want);
            if run == 0 {
                return Err(Ext2Error::InvalidBlock);
            }
            if let Some(k) = (0..run as u32).find(|&k| bit(&self.commit_marks, self.written + k)) {
                run = k as usize;
            }
            self.write_ring_run(self.written, run, device)?;
            self.written += run as u32;
            left -= run;
        }
        Ok(self.written < end)
    }

    /// `#[inline(never)]`: the segment array is 512 bytes of frame.
    #[inline(never)]
    fn write_ring_run(
        &mut self,
        slot: u32,
        run: usize,
        device: &dyn BlockDevice,
    ) -> Result<(), Ext2Error> {
        let bs = self.block_size as usize;
        {
            let mut segs: [&[u8]; RECORD_RUN] = [&[]; RECORD_RUN];
            for (k, seg) in segs.iter_mut().enumerate().take(run) {
                *seg = &self.ring_frame(slot + k as u32).as_bytes()[..bs];
            }
            device
                .write_vectored(self.slot_offset(slot)?, &segs[..run])
                .map_err(Ext2Error::from)?;
        }
        self.writes += run;
        Ok(())
    }

    /// The slot whose image may be rewritten with `block`'s newer contents:
    /// its newest record, when that is still an unwritten image of the open
    /// transaction. A slot the device has taken, or one below the open
    /// transaction, belongs to a sealed transaction and is final.
    pub fn rewritable_slot(&self, block: u32) -> Option<u32> {
        if !self.defers() {
            return None;
        }
        let slot = self.resident_slot(block)?;
        (slot >= self.sealed && slot >= self.settled).then_some(slot)
    }

    /// Replace the image in `slot`, one [`Self::rewritable_slot`] answered.
    /// Final: nothing keeps the image it overwrites, so the caller does this
    /// only once its operation can no longer fail.
    pub fn rewrite(&mut self, slot: u32, bytes: &[u8]) {
        debug_assert!(slot >= self.sealed && slot >= self.settled && slot < self.head);
        self.stage(slot, bytes);
    }

    fn ring_frame(&self, slot: u32) -> &Frame<PageCacheMeta> {
        &self.pending[slot as usize % self.pending.len()]
    }

    fn ring_bytes_mut(&mut self, slot: u32) -> &mut [u8] {
        let bs = self.block_size as usize;
        let n = self.pending.len();
        &mut self.pending[slot as usize % n].as_bytes_mut()[..bs]
    }

    /// Put `bytes` into `slot`'s ring image, zero-padded to a block.
    fn stage(&mut self, slot: u32, bytes: &[u8]) {
        let image = self.ring_bytes_mut(slot);
        let len = bytes.len().min(image.len());
        image[..len].copy_from_slice(&bytes[..len]);
        image[len..].fill(0);
    }

    fn stage_header(&mut self, slot: u32) {
        let bs = self.block_size as usize;
        let n = self.pending.len();
        self.pending[slot as usize % n].as_bytes_mut()[..bs]
            .copy_from_slice(&self.header.as_slice()[..bs]);
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

    /// Slots records may use.
    pub fn capacity(&self) -> u32 {
        (self.slots.len() as u32).saturating_sub(self.first)
    }

    pub fn free_slots(&self) -> u32 {
        (self.slots.len() as u32).saturating_sub(self.head)
    }

    pub fn is_empty(&self) -> bool {
        self.head <= self.first
    }

    /// The first slot records occupy; the log's head is here when empty.
    pub fn first_slot(&self) -> u32 {
        self.first
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
    /// more.
    fn low_water(&self) -> u32 {
        (self.capacity() / 4).max(MIN_LOG_SLOTS)
    }

    /// Tags one descriptor block holds.
    pub fn max_entries(&self) -> usize {
        self.fmt.tags_per_descriptor()
    }

    /// Slots the revoke blocks for the revokes held would take at a seal.
    pub fn revoke_slots(&self) -> u32 {
        self.revoke_blocks(self.revokes.len())
    }

    /// Slots one [`Self::copy_run_to_home`] may carry.
    pub fn home_run_max(&self) -> usize {
        CHECKPOINT_RUN
    }

    pub fn take_writes(&mut self) -> usize {
        core::mem::take(&mut self.writes)
    }

    /// Flushes the log issued since the last call. Every write handed to the
    /// device before the last of them is durable, the cache's included.
    pub fn take_flushes(&mut self) -> usize {
        core::mem::take(&mut self.flushes)
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

    /// Clear every mapping of `block`, newest first, recording those from
    /// before the open operation so an abort can put them back — popped in
    /// reverse, which restores each chain's newest-first order.
    fn clear_mappings(&mut self, block: u32) -> Result<(), Ext2Error> {
        if block == 0 {
            return Ok(());
        }
        let mut slot = self.buckets[self.bucket_of(block)];
        while slot != NIL_SLOT {
            let next = self.chain_next[slot as usize];
            if self.slot_block[slot as usize] == block {
                if slot < self.op_head {
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

    /// Copy a slot's payload into `out`, which must be one block long. A slot
    /// still in the ring is served from it; an escaped copy gets its first
    /// word back.
    pub fn read_slot(
        &self,
        slot: u32,
        device: &dyn BlockDevice,
        out: &mut [u8],
    ) -> Result<(), Ext2Error> {
        if slot >= self.written && !self.pending.is_empty() {
            let bs = self.block_size as usize;
            let n = out.len().min(bs);
            out[..n].copy_from_slice(&self.ring_frame(slot).as_bytes()[..n]);
        } else {
            let offset = self.slot_offset(slot)?;
            device.read_at(offset, out).map_err(Ext2Error::from)?;
        }
        if bit(&self.escaped, slot) {
            jbd2::unescape(out);
        }
        Ok(())
    }

    /// Open an operation. Its records join the open transaction.
    pub fn begin_op(&mut self) {
        self.op_head = self.head;
        self.op_revokes.clear();
        self.revoke_undo.clear();
        self.write_through = false;
    }

    /// Discard everything the open operation appended and revoked.
    ///
    /// Sound because no home block was written on its behalf, and the caller
    /// drops the operation's cache entries, so a later read comes back from
    /// the log or from the block's own home. No image below `op_head` needs
    /// putting back: the cache rewrites the compound's images only as the last
    /// step of a commit that can no longer fail.
    pub fn abort_op(&mut self) {
        for slot in self.op_head..self.head {
            self.index_remove(slot);
            set_bit(&mut self.escaped, slot, false);
        }
        if !self.revoke_undo.is_empty() {
            self.restores = self.restores.wrapping_add(1);
        }
        while let Some((slot, block)) = self.revoke_undo.pop() {
            self.index_insert(slot, block);
        }
        for &block in self.op_revokes.as_slice() {
            self.revokes.remove(&block);
        }
        self.op_revokes.clear();
        self.op_seq = self.op_seq.wrapping_add(1);
        self.head = self.op_head;
        // The rewound slots are rewritten by whatever appends next; a copy
        // already on the medium is a record no commit covers.
        self.written = self.written.min(self.head);
        self.settled = self.settled.min(self.head);
        self.write_through = false;
    }

    /// `block` was freed. When the log still holds a copy of it, no replay
    /// may write that copy over what the block holds next: it is revoked.
    pub fn note_revoke(&mut self, block: u32) -> Result<(), Ext2Error> {
        if block == 0 || self.resident_slot(block).is_none() {
            return Ok(());
        }
        if !self.revokes.contains_key(&block) {
            let needed = self.revoke_blocks(self.revokes.len() + 1) + SEAL_RESERVE;
            if self.head + needed > self.slots.len() as u32 {
                return Err(Ext2Error::NoSpace);
            }
            self.op_revokes
                .push(block)
                .map_err(|_| Ext2Error::OutOfMemory)?;
            self.revokes.insert(block, self.op_seq);
        }
        self.clear_mappings(block)
    }

    /// `block`'s home is being written with contents newer than its copies
    /// here. Neither a miss, a check point nor a replay may then take an
    /// older copy over it.
    pub fn supersede(&mut self, block: u32) -> Result<(), Ext2Error> {
        self.note_revoke(block)
    }

    /// Append one descriptor for `targets` and their copies, and return the
    /// first copy's slot. Copy *i* is fetched from `payload(i)`, which is
    /// where the caller's cache frame comes from.
    ///
    /// In the ring the descriptor's checksums wait for the seal, since the
    /// cache may still rewrite the copies. Written through, they are settled
    /// at once and the descriptor goes out in front of its copies.
    pub fn write_record<'a>(
        &mut self,
        targets: &[u32],
        device: &dyn BlockDevice,
        payload: &mut dyn FnMut(usize) -> Option<&'a [u8]>,
    ) -> Result<u32, Ext2Error> {
        debug_assert!(targets.len() <= self.max_entries());
        let header_slot = self.reserve(1 + targets.len() as u32)?;
        let first = header_slot + 1;
        let count = targets.len();
        jbd2::begin_descriptor(self.header.as_mut_slice(), self.seq);
        for (i, &block) in targets.iter().enumerate() {
            let tag = Tag {
                block: u64::from(block),
                flags: 0,
                checksum: 0,
            };
            jbd2::put_tag(&self.fmt, self.header.as_mut_slice(), i, count, tag);
        }
        for (i, &block) in targets.iter().enumerate() {
            self.index_insert(first + i as u32, block);
        }

        if self.defers() {
            self.stage_header(header_slot);
            for i in 0..count {
                let seg = payload(i).ok_or(Ext2Error::DeviceError)?;
                self.stage(first + i as u32, seg);
            }
            return Ok(first);
        }

        for (i, &block) in targets.iter().enumerate() {
            let seg = payload(i).ok_or(Ext2Error::DeviceError)?;
            let escape = jbd2::needs_escape(seg);
            let checksum = if escape {
                let bs = self.block_size as usize;
                let copy = &mut self.transfer.as_mut_slice()[..bs];
                copy.copy_from_slice(&seg[..bs]);
                jbd2::escape(copy);
                self.fmt.data_checksum(self.seq, copy)
            } else {
                self.fmt.data_checksum(self.seq, seg)
            };
            let flags = if escape { tag_flag::ESCAPE } else { 0 };
            set_bit(&mut self.escaped, first + i as u32, escape);
            let tag = Tag {
                block: u64::from(block),
                flags,
                checksum,
            };
            jbd2::put_tag(&self.fmt, self.header.as_mut_slice(), i, count, tag);
        }
        jbd2::seal_descriptor(&self.fmt, self.header.as_mut_slice());
        self.write_header_at(header_slot, device)?;
        for i in 0..count {
            let slot = first + i as u32;
            let seg = payload(i).ok_or(Ext2Error::DeviceError)?;
            if bit(&self.escaped, slot) {
                let bs = self.block_size as usize;
                let copy = &mut self.transfer.as_mut_slice()[..bs];
                copy.copy_from_slice(&seg[..bs]);
                jbd2::escape(copy);
                let offset = self.slot_offset(slot)?;
                device
                    .write_at(offset, &self.transfer.as_slice()[..bs])
                    .map_err(Ext2Error::from)?;
            } else {
                let offset = self.slot_offset(slot)?;
                device.write_at(offset, seg).map_err(Ext2Error::from)?;
            }
            self.writes += 1;
        }
        self.written = self.written.max(first + count as u32);
        self.settled = self.settled.max(self.written);
        Ok(first)
    }

    fn write_header_at(&mut self, slot: u32, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let bs = self.block_size as usize;
        let offset = self.slot_offset(slot)?;
        device
            .write_at(offset, &self.header.as_slice()[..bs])
            .map_err(Ext2Error::from)?;
        self.writes += 1;
        self.written = self.written.max(slot + 1);
        Ok(())
    }

    /// Slots from `slot` whose blocks are one consecutive run on the device,
    /// capped at `want`. Always at least one: a run of a single slot is what
    /// a fragmented journal falls back to.
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
        // The open operation's content, which a commit of the compound must
        // not cover.
        self.seal(device)?;
        self.write_record(&[block], device, &mut |_| Some(data))
            .map(|_| ())
    }

    /// End the open operation. In the ring its records stay in the open
    /// compound for the commit block a write-out puts behind it; written
    /// through, they are committed now.
    pub fn commit_op(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if !self.defers() && (self.head > self.sealed || !self.revokes.is_empty()) {
            self.close_transaction_direct(device, true)?;
        }
        self.op_head = self.head;
        self.op_revokes.clear();
        self.op_seq = self.op_seq.wrapping_add(1);
        self.revoke_undo.clear();
        Ok(())
    }

    /// Put the commit block behind the committed operations waiting in the
    /// open transaction. Nothing when there are none, and refused while the
    /// open operation has records of its own in it: those are not committed.
    pub fn seal(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if !self.has_unsealed() {
            return Ok(());
        }
        if self.head != self.op_head {
            debug_assert!(false, "sealing over an open operation's records");
            return Err(Ext2Error::DeviceError);
        }
        if self.defers() {
            self.close_transaction_staged()?;
        } else {
            self.close_transaction_direct(device, false)?;
        }
        self.op_head = self.head;
        Ok(())
    }

    /// [`Self::seal`] for a log that stages its records, where it is only ring
    /// images and cannot fail: every record reservation left the slots. A
    /// writeback pass seals as it opens, so its records end at a commit block
    /// and no later operation joins them.
    pub fn seal_ring(&mut self) {
        if self.defers() && self.has_unsealed() && self.head == self.op_head {
            let staged = self.close_transaction_staged();
            debug_assert!(staged.is_ok(), "no slot left to seal the ring");
            if staged.is_ok() {
                self.op_head = self.head;
            }
        }
    }

    /// Settle the open transaction in the ring: descriptor tags and
    /// checksums over the copies as they finally stand, the revoke blocks,
    /// and the commit block.
    fn close_transaction_staged(&mut self) -> Result<(), Ext2Error> {
        self.settle_descriptors(self.head);
        while self.committed_revokes() > 0 {
            if self.encode_revokes(false) > 0 {
                let slot = self.reserve_slots(1, 1)?;
                self.stage_header(slot);
            }
        }
        let slot = self.reserve_slots(1, 0)?;
        self.encode_commit_block();
        self.stage_header(slot);
        self.transaction_closed(slot);
        Ok(())
    }

    /// Written through: the revokes, a flush behind everything the
    /// transaction is made of, then the commit block. `with_open` takes the
    /// open operation's revokes too, which it may only when closing itself.
    fn close_transaction_direct(
        &mut self,
        device: &dyn BlockDevice,
        with_open: bool,
    ) -> Result<(), Ext2Error> {
        let pending = |j: &Self| {
            if with_open {
                j.revokes.len()
            } else {
                j.committed_revokes()
            }
        };
        while pending(self) > 0 {
            if self.encode_revokes(with_open) > 0 {
                let slot = self.reserve_slots(1, 1)?;
                self.write_header_at(slot, device)?;
            }
        }
        let slot = self.reserve_slots(1, 0)?;
        self.flush(device)?;
        self.encode_commit_block();
        self.write_header_at(slot, device)?;
        self.committed_written = slot + 1;
        self.transaction_closed(slot);
        Ok(())
    }

    /// Settle every record that starts below `end`: escape each copy that
    /// reads as the journal magic, checksum it, and seal its descriptor.
    fn settle_descriptors(&mut self, end: u32) {
        let bs = self.block_size as usize;
        let mut slot = self.settled;
        while slot < end {
            let ring = self.pending.len();
            let image = &self.pending[slot as usize % ring].as_bytes()[..bs];
            let descriptor =
                jbd2::block_header(image).is_some_and(|h| h.blocktype == blocktype::DESCRIPTOR);
            if !descriptor {
                slot += 1;
                continue;
            }
            self.header.as_mut_slice()[..bs].copy_from_slice(image);
            let count = jbd2::tags(&self.fmt, self.header.as_slice(), &mut |_| {});
            // Rewriting tags under the cursor is safe: every tag after the
            // first shares the UUID, so the count alone fixes each position.
            let mut cursor = TagCursor::new();
            for i in 0..count {
                let Some(old) = cursor.next(&self.fmt, self.header.as_slice()) else {
                    break;
                };
                let copy = slot + 1 + i as u32;
                let escape = jbd2::needs_escape(&self.ring_frame(copy).as_bytes()[..bs]);
                if escape {
                    jbd2::escape(self.ring_bytes_mut(copy));
                }
                set_bit(&mut self.escaped, copy, escape);
                let checksum = self
                    .fmt
                    .data_checksum(self.seq, &self.ring_frame(copy).as_bytes()[..bs]);
                let tag = Tag {
                    block: old.block,
                    flags: if escape { tag_flag::ESCAPE } else { 0 },
                    checksum,
                };
                jbd2::put_tag(&self.fmt, self.header.as_mut_slice(), i, count, tag);
            }
            jbd2::seal_descriptor(&self.fmt, self.header.as_mut_slice());
            self.stage_header(slot);
            slot += 1 + count as u32;
        }
        self.settled = self.settled.max(slot);
    }

    /// Encode one revoke block, up to a block's worth of the revokes (the
    /// committed ones only unless `with_open`), into the header buffer. A
    /// revoke whose block has a newer copy in the log is dropped, since it
    /// would cancel that copy too. Every revoke settled, written or dropped,
    /// leaves the set; answers how many it wrote.
    fn encode_revokes(&mut self, with_open: bool) -> usize {
        let per = self.fmt.revokes_per_block().min(self.transfer.len() / 4);
        jbd2::begin_revoke(self.header.as_mut_slice(), self.seq);
        let mut settled = 0usize;
        let mut written = 0usize;
        for (&block, &op) in self.revokes.iter() {
            if settled == per {
                break;
            }
            if !with_open && op == self.op_seq {
                continue;
            }
            let at = settled * 4;
            self.transfer.as_mut_slice()[at..at + 4].copy_from_slice(&block.to_le_bytes());
            settled += 1;
            if self.resident_slot(block).is_none() {
                jbd2::put_revoke(
                    &self.fmt,
                    self.header.as_mut_slice(),
                    written,
                    u64::from(block),
                );
                written += 1;
            }
        }
        jbd2::finish_revoke(&self.fmt, self.header.as_mut_slice(), written);
        for k in 0..settled {
            let raw = &self.transfer.as_slice()[k * 4..k * 4 + 4];
            let block = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            self.revokes.remove(&block);
        }
        written
    }

    fn encode_commit_block(&mut self) {
        let (sec, nsec) = slopos_kernel_services::clock::realtime_timespec()
            .map_or((0, 0), |(s, ns)| (s as u64, ns));
        jbd2::encode_commit(&self.fmt, self.header.as_mut_slice(), self.seq, sec, nsec);
    }

    fn transaction_closed(&mut self, slot: u32) {
        set_bit(&mut self.commit_marks, slot, true);
        stats::note_commit();
        self.seq = self.seq.wrapping_add(1);
        self.sealed = slot + 1;
        self.settled = self.settled.max(self.sealed);
    }

    /// Copy the logged blocks in slots `slot..slot + len` to home locations
    /// `block..block + len`. The log, never the cache, is the source: the
    /// cache may hold an operation's uncommitted changes to the same block.
    ///
    /// The caller owns the claim that the *homes* are consecutive and that
    /// none of them already holds its newest contents, so the whole run goes
    /// home in one write; the read side is gathered the same way wherever the
    /// log's own slots are consecutive. No flush here: the caller flushes the
    /// home writes once before [`Self::reset`].
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
        // A home write from a record the medium may not hold yet would
        // publish a transaction a crash could still lose.
        if !self.slot_durable(slot + n as u32 - 1) {
            return Err(Ext2Error::DeviceError);
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
        for k in 0..n {
            if bit(&self.escaped, slot + k as u32) {
                jbd2::unescape(&mut self.transfer.as_mut_slice()[k * bs..(k + 1) * bs]);
            }
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

    /// Declare every logged block home: the log is empty again and the next
    /// transaction starts at `s_first`. The caller must have flushed the
    /// home-location writes first.
    pub fn reset(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.written < self.head || self.has_unsealed() {
            return Err(Ext2Error::DeviceError);
        }
        self.base_seq = self.seq;
        self.write_superblock(device)?;
        // Durable before a slot is reused: a crash that lost this superblock
        // write would replay older copies over newer homes.
        self.flush(device)?;
        let first = self.first;
        self.head = first;
        self.op_head = first;
        self.sealed = first;
        self.written = first;
        self.settled = first;
        self.barriered = first;
        self.committed_written = first;
        self.commit_marks.as_mut_slice().fill(0);
        self.escaped.as_mut_slice().fill(0);
        self.write_through = false;
        self.generation = self.generation.wrapping_add(1);
        self.index_clear();
        self.revokes.clear();
        self.op_revokes.clear();
        self.revoke_undo.clear();
        Ok(())
    }

    /// Say in the journal superblock whether the log is live — what replay
    /// reads from `s_start` — or empty. The volume's `needs_recovery` flag is
    /// set before the log goes live and cleared after it goes empty.
    pub fn set_live(&mut self, live: bool, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        if self.live == live {
            return Ok(());
        }
        if !live && !self.is_empty() {
            return Err(Ext2Error::DeviceError);
        }
        self.live = live;
        self.write_superblock(device)
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    /// The journal superblock, naming where the log starts — `s_first` while
    /// live, zero while empty — and the sequence expected there.
    fn write_superblock(&mut self, device: &dyn BlockDevice) -> Result<(), Ext2Error> {
        let start = if self.live { self.first } else { 0 };
        let sequence = if self.live { self.base_seq } else { self.seq };
        jbd2::set_log_state(self.jsb.as_mut_slice(), sequence, start);
        let offset = self.slot_offset(0)?;
        let bs = self.block_size as usize;
        device
            .write_at(offset, &self.jsb.as_slice()[..bs])
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

    /// Slots for a record, which always leaves what sealing the transaction
    /// it joins will need.
    fn reserve(&mut self, want: u32) -> Result<u32, Ext2Error> {
        let spare = self.seal_room();
        self.reserve_slots(want, spare)
    }

    fn reserve_slots(&mut self, want: u32, spare: u32) -> Result<u32, Ext2Error> {
        let slot = self.head;
        let end = slot.checked_add(want).ok_or(Ext2Error::NoSpace)?;
        if end as usize + spare as usize > self.slots.len() {
            return Err(Ext2Error::NoSpace);
        }
        // The cache makes room before every append; running out here would
        // overwrite an image the device has not taken yet.
        if self.defers() && end - self.written + spare > self.pending.len() as u32 {
            debug_assert!(false, "log ring overrun");
            return Err(Ext2Error::NoSpace);
        }
        self.head = end;
        for s in slot..end {
            set_bit(&mut self.commit_marks, s, false);
            set_bit(&mut self.escaped, s, false);
        }
        Ok(slot)
    }
}
