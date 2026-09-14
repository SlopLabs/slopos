//! A bounded, droppable name index for the directories currently in use.
//!
//! The on-disk format stays plain linear ext2 — no htree — so `e2fsck` remains
//! the oracle and no hash has to agree with Linux's. The price is that every
//! directory operation scans every block; this buys the lookup back in memory.
//!
//! A hit is only ever a *candidate*: the record it names must still be read
//! and its name compared, which is what makes a stale entry harmless. The
//! dangerous direction is a name the index does not hold while claiming to
//! hold them all, so completeness — not freshness — is the invariant.
//! [`DirIndexSet::finish_build`] is the only thing that grants it, every
//! mutation must keep it, and anything that cannot drops the table and scans.

use slopos_mm::slab::MAX_ALLOC_SIZE;
use slopos_ostd::KVec;

/// Directories indexed at once, evicted by recency. Sized for the working set
/// of a path walk and a build, not for the tree.
pub const DIR_INDEX_DIRS: usize = 4;

/// Table slots one directory's index may hold, in one allocation. The bound is
/// set by the directory this exists for: a `target/debug/deps` of twenty
/// thousand names has to fit, or the lookup half of the problem is not solved.
const TABLE_MAX: usize = 32 * 1024;

/// Slots a fresh table starts at, doubling from here: a directory of a dozen
/// names must not pay for one of twenty thousand.
const TABLE_MIN: usize = 64;

/// Names one directory's index holds before it gives up and leaves the
/// directory to be scanned. The table is never loaded past three quarters.
pub const DIR_INDEX_MAX_NAMES: usize = TABLE_MAX * 3 / 4;

/// A position, keyed by its name's hash. `pos` is the record's own byte offset
/// from the start of the directory's data.
#[derive(Clone, Copy)]
struct Slot {
    hash: u32,
    pos: u32,
}

/// Never a real hash: an empty slot ends a probe.
const EMPTY: u32 = 0;
/// Never a real hash: a removed slot is reusable but does not end a probe.
const TOMB: u32 = u32::MAX;

const _: () = assert!(TABLE_MAX.is_power_of_two());
const _: () = assert!(TABLE_MIN.is_power_of_two());
const _: () = assert!(TABLE_MAX * size_of::<Slot>() <= 256 * 1024);
const _: () = assert!(256 * 1024 <= MAX_ALLOC_SIZE);

/// FNV-1a, folded away from the two reserved values.
pub fn hash_name(name: &[u8]) -> u32 {
    let mut h = 0x811c_9dc5u32;
    for &b in name {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    if h == EMPTY || h == TOMB { 1 } else { h }
}

/// What the index can say about one probe step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirProbe {
    /// A record whose name hashes to the one being sought. Read it and
    /// compare: the index never answers with an inode.
    Candidate(u32),
    /// The index holds every name in this directory and none of them was it.
    Absent,
    /// There is no index, or it is not complete. Scan.
    Unknown,
}

struct DirIndex {
    /// Directory this slot describes; `0` when free.
    ino: u32,
    table: KVec<Slot>,
    live: u32,
    /// Live plus tombstones — what the probe length depends on.
    occupied: u32,
    complete: bool,
    /// The directory outgrew [`TABLE_MAX`]; never spend another scan trying.
    oversized: bool,
    /// First file block known to have room for a new record, so an insert
    /// resumes there instead of restarting at block 0.
    free_hint: u32,
    /// A size `P` such that no block below [`Self::free_hint`] has `P` bytes
    /// of slack. Zero means nothing is known.
    ///
    /// Without it the hint only removes half the cost: a block that fills up
    /// sends the next insert back over the whole prefix. A record no smaller
    /// than `P` cannot fit where `P` bytes will not, so the prefix scan it
    /// skips is one that could not have found anything.
    hint_proof: u32,
    stamp: u64,
}

impl DirIndex {
    const fn new() -> Self {
        Self {
            ino: 0,
            table: KVec::new(),
            live: 0,
            occupied: 0,
            complete: false,
            oversized: false,
            free_hint: 0,
            hint_proof: 0,
            stamp: 0,
        }
    }

    fn reset(&mut self, ino: u32) {
        self.ino = ino;
        self.table = KVec::new();
        self.live = 0;
        self.occupied = 0;
        self.complete = false;
        self.oversized = false;
        self.free_hint = 0;
        self.hint_proof = 0;
        self.stamp = 0;
    }

    fn drop_table(&mut self) {
        self.table = KVec::new();
        self.live = 0;
        self.occupied = 0;
        self.complete = false;
    }

    fn mask(&self) -> usize {
        self.table.len() - 1
    }

    fn probe(&self, hash: u32, cursor: &mut u32) -> DirProbe {
        if self.table.is_empty() {
            return DirProbe::Unknown;
        }
        let mask = self.mask();
        while (*cursor as usize) <= mask {
            let i = (hash as usize).wrapping_add(*cursor as usize) & mask;
            *cursor += 1;
            let slot = self.table.as_slice()[i];
            if slot.hash == EMPTY {
                break;
            }
            if slot.hash == hash {
                return DirProbe::Candidate(slot.pos);
            }
        }
        if self.complete {
            DirProbe::Absent
        } else {
            DirProbe::Unknown
        }
    }

    fn insert(&mut self, hash: u32, pos: u32) -> bool {
        if self.oversized || !self.ensure_room() {
            return false;
        }
        let mask = self.mask();
        let mut i = hash as usize & mask;
        loop {
            let slot = self.table.as_slice()[i];
            if slot.hash == EMPTY || slot.hash == TOMB {
                if slot.hash == EMPTY {
                    self.occupied += 1;
                }
                self.table.as_mut_slice()[i] = Slot { hash, pos };
                self.live += 1;
                return true;
            }
            i = (i + 1) & mask;
        }
    }

    fn remove(&mut self, hash: u32, pos: u32) {
        if self.table.is_empty() {
            return;
        }
        let mask = self.mask();
        let mut i = hash as usize & mask;
        for _ in 0..=mask {
            let slot = self.table.as_slice()[i];
            if slot.hash == EMPTY {
                return;
            }
            if slot.hash == hash && slot.pos == pos {
                self.table.as_mut_slice()[i].hash = TOMB;
                self.live -= 1;
                return;
            }
            i = (i + 1) & mask;
        }
    }

    /// Room for one more, growing or purging tombstones as needed. `false`
    /// means the directory is past what this index will hold.
    fn ensure_room(&mut self) -> bool {
        let len = self.table.len();
        if len != 0 && (self.occupied as usize + 1) * 4 <= len * 3 {
            return true;
        }
        let want = if len == 0 { TABLE_MIN } else { len * 2 };
        if want <= TABLE_MAX {
            return self.rehash(want);
        }
        // Tombstones alone may be what filled it, in which case a same-size
        // rehash is the whole of the answer.
        if (self.live as usize + 1) * 4 <= TABLE_MAX * 3 {
            return self.rehash(TABLE_MAX);
        }
        self.oversized = true;
        false
    }

    fn rehash(&mut self, want: usize) -> bool {
        let Ok(mut fresh) = KVec::<Slot>::with_capacity(want) else {
            return false;
        };
        if fresh
            .resize(
                want,
                Slot {
                    hash: EMPTY,
                    pos: 0,
                },
            )
            .is_err()
        {
            return false;
        }
        let mask = want - 1;
        for &slot in self.table.as_slice() {
            if slot.hash == EMPTY || slot.hash == TOMB {
                continue;
            }
            let mut i = slot.hash as usize & mask;
            while fresh.as_slice()[i].hash != EMPTY {
                i = (i + 1) & mask;
            }
            fresh.as_mut_slice()[i] = slot;
        }
        self.occupied = self.live;
        self.table = fresh;
        true
    }
}

/// The indexed directories, least-recently-used first out.
pub struct DirIndexSet {
    dirs: [DirIndex; DIR_INDEX_DIRS],
    clock: u64,
}

impl DirIndexSet {
    /// Allocates nothing: a table is built the first time a scan fills one,
    /// which is also what makes moving the set in and out of the cache free.
    pub fn new() -> Self {
        Self {
            dirs: core::array::from_fn(|_| DirIndex::new()),
            clock: 0,
        }
    }

    fn find(&self, ino: u32) -> Option<usize> {
        if ino == 0 {
            return None;
        }
        self.dirs.iter().position(|d| d.ino == ino)
    }

    fn touch(&mut self, i: usize) {
        self.clock += 1;
        self.dirs[i].stamp = self.clock;
    }

    /// The slot for `ino`, taking the least recently used one when the set is
    /// full.
    fn slot(&mut self, ino: u32) -> usize {
        if let Some(i) = self.find(ino) {
            self.touch(i);
            return i;
        }
        let mut victim = 0usize;
        for i in 0..DIR_INDEX_DIRS {
            if self.dirs[i].ino == 0 {
                victim = i;
                break;
            }
            if self.dirs[i].stamp < self.dirs[victim].stamp {
                victim = i;
            }
        }
        self.dirs[victim].reset(ino);
        self.touch(victim);
        victim
    }

    pub fn probe(&mut self, ino: u32, hash: u32, cursor: &mut u32) -> DirProbe {
        let Some(i) = self.find(ino) else {
            return DirProbe::Unknown;
        };
        if *cursor == 0 {
            self.touch(i);
        }
        self.dirs[i].probe(hash, cursor)
    }

    /// Start over on `ino`: the caller is about to walk every record, so a
    /// table built from that walk is exact. `false` when there is no point.
    pub fn begin_build(&mut self, ino: u32) -> bool {
        if ino == 0 {
            return false;
        }
        let i = self.slot(ino);
        if self.dirs[i].oversized {
            return false;
        }
        self.dirs[i].drop_table();
        true
    }

    /// Feed one record to a build. `false` retires the build; the entries
    /// already in the table stay valid, they merely are not complete.
    pub fn add(&mut self, ino: u32, hash: u32, pos: u32) -> bool {
        match self.find(ino) {
            Some(i) => self.dirs[i].insert(hash, pos),
            None => false,
        }
    }

    /// The walk reached the end of the directory with every record recorded,
    /// so a probe that finds no candidate is now a definitive answer.
    pub fn finish_build(&mut self, ino: u32) {
        if let Some(i) = self.find(ino) {
            self.dirs[i].complete = true;
        }
    }

    /// A name a mutation added. Failing to record it is what would turn a
    /// complete index into a liar, so the table goes instead.
    pub fn note_insert(&mut self, ino: u32, hash: u32, pos: u32) {
        let Some(i) = self.find(ino) else {
            return;
        };
        if self.dirs[i].table.is_empty() {
            return;
        }
        if !self.dirs[i].insert(hash, pos) {
            self.dirs[i].drop_table();
        }
    }

    pub fn note_remove(&mut self, ino: u32, hash: u32, pos: u32) {
        if let Some(i) = self.find(ino) {
            self.dirs[i].remove(hash, pos);
        }
    }

    /// Where an insert should resume, and what is known about the blocks
    /// below it.
    pub fn free_hint(&self, ino: u32) -> (u32, u32) {
        match self.find(ino) {
            Some(i) => (self.dirs[i].free_hint, self.dirs[i].hint_proof),
            None => (0, 0),
        }
    }

    /// Where the record went, and what the passes that put it there proved
    /// about everything before it. The caller owes that arithmetic: only it
    /// knows which ranges were scanned and for what size.
    pub fn set_hint(&mut self, ino: u32, block: u32, proof: u32) {
        if ino == 0 {
            return;
        }
        let i = self.slot(ino);
        self.dirs[i].free_hint = block;
        self.dirs[i].hint_proof = proof;
    }

    /// A removal freed space in `block`; an insert that starts past it would
    /// grow the directory instead of reusing what it just gave back, and no
    /// earlier proof survives space coming back below the hint.
    pub fn lower_free_hint(&mut self, ino: u32, block: u32) {
        if let Some(i) = self.find(ino)
            && self.dirs[i].free_hint > block
        {
            self.dirs[i].free_hint = block;
            self.dirs[i].hint_proof = 0;
        }
    }

    pub fn forget(&mut self, ino: u32) {
        if let Some(i) = self.find(ino) {
            self.dirs[i].reset(0);
        }
    }

    pub fn clear(&mut self) {
        for d in &mut self.dirs {
            d.reset(0);
        }
        self.clock = 0;
    }

    /// Whether `ino`'s index can answer a miss on its own. Diagnostic; the
    /// lookup path asks [`Self::probe`], which answers this and the position.
    pub fn is_complete(&self, ino: u32) -> bool {
        self.find(ino).is_some_and(|i| self.dirs[i].complete)
    }
}

impl Default for DirIndexSet {
    fn default() -> Self {
        Self::new()
    }
}
