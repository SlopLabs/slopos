//! Name and attribute caches in front of an ext2 mount's lock.
//!
//! Every path component a walk resolves costs one `lookup` and one `stat`,
//! and without these both serialise on the mount's one sleeping mutex, where
//! a compile's thousands of path walks queue behind every write in flight.
//! The caches answer both without that lock.
//!
//! Validity is a generation check rather than an invalidation walk. Three
//! tables of counters, hashed by key, are bumped under the mount lock by the
//! ext2 code that changes what they cover:
//!
//! - *record*, by inode: its on-disk record was rewritten (`write_inode_num`),
//!   which is every change a `stat` can see;
//! - *entry*, by `(directory, name)`: an entry of that name was added to or
//!   removed from that directory, the only ways a name's binding moves;
//! - *life*, by inode: the inode was allocated or freed, so a directory number
//!   may now denote a different directory, or none.
//!
//! An entry records the counters it was filled under, read while the reader
//! still held the mount lock, and is answered only while they are unchanged.
//! A mutation after that read bumps one of them before the reader can insert,
//! so a late insert is born stale rather than served. A rolled-back operation
//! bumped on its way in, which costs a refill and nothing else. Detach and
//! attach bump every counter, which is how a pool slot handed a different
//! image forgets the last one without walking either table.
//!
//! Entries are seqlocked words, not locked slots: a reader writes nothing, so
//! the lookups every walk makes of `/`'s first few names do not bounce a
//! lock's cache line between CPUs, and no lock class joins the lock graph.
//!
//! `.` and `..` are never cached: the walk resolves `..` itself, and a
//! directory's `..` moves by rename without either of its names changing.

use core::sync::atomic::{AtomicU64, Ordering, fence};

use slopos_ostd::KVec;

use crate::ext2::Ext2Inode;
use crate::vfs::{FileStat, FileType, InodeId};

/// Buckets per generation table. A collision only costs a spurious miss.
const GEN_BUCKETS: usize = 4096;
const GEN_TABLES: usize = 3;
const RECORD_TABLE: usize = 0;
const ENTRY_TABLE: usize = 1;
const LIFE_TABLE: usize = 2;

/// Longest name a dentry holds. Longer names always take the mount lock; the
/// hashed names cargo writes into `deps/` fit.
pub(crate) const NAME_INLINE: usize = 56;
const NAME_WORDS: usize = NAME_INLINE / 8;

/// Two-way sets: a pair of hot names that collide do not evict each other.
const WAYS: usize = 2;
const DENTRY_SETS: usize = 4096;
const ATTR_SETS: usize = 4096;

/// Every slot's first word is its sequence: odd while a writer is in it.
const SEQ: usize = 0;

// Dentry words. `KEY` is `parent | len << 32`; zero, which names no
// directory, marks an empty slot.
const D_KEY: usize = 1;
const D_NAME: usize = 2;
const D_CHILD: usize = D_NAME + NAME_WORDS;
const D_ENTRY_GEN: usize = D_CHILD + 1;
const D_LIFE_GEN: usize = D_ENTRY_GEN + 1;
const D_WORDS: usize = D_LIFE_GEN + 1;

// Attribute words. `KEY` is the inode number, zero for an empty slot.
const A_KEY: usize = 1;
const A_SIZE: usize = 2;
const A_IDS: usize = 3;
const A_TIMES: usize = 4;
const A_CTIME: usize = 5;
const A_GEN: usize = 6;
const A_WORDS: usize = A_GEN + 1;

#[inline]
fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[inline]
fn bucket(hash: u64) -> usize {
    (hash >> 52) as usize & (GEN_BUCKETS - 1)
}

/// `name`, zero-padded into words, or `None` when the cache does not hold it.
#[inline]
fn pack_name(name: &[u8]) -> Option<[u64; NAME_WORDS]> {
    if name.is_empty() || name.len() > NAME_INLINE || name == b"." || name == b".." {
        return None;
    }
    let mut words = [0u64; NAME_WORDS];
    for (i, chunk) in name.chunks(8).enumerate() {
        let mut bytes = [0u8; 8];
        bytes[..chunk.len()].copy_from_slice(chunk);
        words[i] = u64::from_le_bytes(bytes);
    }
    Some(words)
}

#[inline]
fn name_hash(parent: u32, len: usize, words: &[u64; NAME_WORDS]) -> u64 {
    let mut h = mix(u64::from(parent) | (len as u64) << 32);
    for w in &words[..len.div_ceil(8)] {
        h = mix(h ^ w);
    }
    h
}

#[inline]
fn ino_hash(ino: u32) -> u64 {
    mix(u64::from(ino) ^ 0x9e37_79b9_7f4a_7c15)
}

/// A name as the dentry table keys it.
pub(crate) struct NameKey {
    parent: u32,
    len: usize,
    words: [u64; NAME_WORDS],
    hash: u64,
}

impl NameKey {
    /// `None` for a name the cache never holds, which then always takes the
    /// mount lock.
    #[inline]
    pub(crate) fn new(parent: InodeId, name: &[u8]) -> Option<Self> {
        let parent = u32::try_from(parent).ok().filter(|&p| p != 0)?;
        let words = pack_name(name)?;
        let len = name.len();
        Some(Self {
            parent,
            len,
            words,
            hash: name_hash(parent, len, &words),
        })
    }

    #[inline]
    fn key_word(&self) -> u64 {
        u64::from(self.parent) | (self.len as u64) << 32
    }
}

/// The counters a slow-path reader read under the mount lock.
#[derive(Clone, Copy)]
pub(crate) struct NameStamp {
    entry: u64,
    life: u64,
}

/// Generation counters, bumped by the ext2 code under the mount lock.
pub struct Ext2Gens {
    /// `GEN_TABLES` tables of `GEN_BUCKETS`; empty when the allocation failed,
    /// which disables the caches rather than the mount.
    words: KVec<AtomicU64>,
}

impl Ext2Gens {
    fn new() -> Self {
        Self {
            words: atomic_words(GEN_TABLES * GEN_BUCKETS),
        }
    }

    #[inline]
    fn slot(&self, table: usize, hash: u64) -> Option<&AtomicU64> {
        self.words.get(table * GEN_BUCKETS + bucket(hash))
    }

    #[inline]
    fn bump(&self, table: usize, hash: u64) {
        if let Some(gen_word) = self.slot(table, hash) {
            gen_word.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[inline]
    fn read(&self, table: usize, hash: u64) -> u64 {
        self.slot(table, hash)
            .map_or(0, |gen_word| gen_word.load(Ordering::Acquire))
    }

    /// `ino`'s record was rewritten.
    #[inline]
    pub(crate) fn note_record(&self, ino: u32) {
        self.bump(RECORD_TABLE, ino_hash(ino));
    }

    /// An entry `name` was added to or removed from directory `dir`.
    #[inline]
    pub(crate) fn note_entry(&self, dir: u32, name: &[u8]) {
        if let Some(words) = pack_name(name) {
            self.bump(ENTRY_TABLE, name_hash(dir, name.len(), &words));
        }
    }

    /// `ino` was allocated or freed.
    #[inline]
    pub(crate) fn note_life(&self, ino: u32) {
        self.bump(LIFE_TABLE, ino_hash(ino));
    }

    /// Invalidate everything: counters only ever grow, so a stored value
    /// below every bucket's current one matches none of them.
    fn bump_all(&self) {
        for gen_word in self.words.iter() {
            gen_word.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// The fields of an ext2 record a [`FileStat`] carries.
#[derive(Clone, Copy)]
pub(crate) struct InodeAttr {
    mode: u16,
    uid: u16,
    gid: u16,
    links: u16,
    size: u64,
    atime: u32,
    mtime: u32,
    ctime: u32,
    sealed: bool,
}

impl InodeAttr {
    pub(crate) fn of(inode: &Ext2Inode) -> Self {
        Self {
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            links: inode.links_count,
            size: inode.size,
            atime: inode.atime,
            mtime: inode.mtime,
            ctime: inode.ctime,
            // `EXT2_IMMUTABLE_FL` is the carrier, so the seal survives a
            // reboot and reads as one to `lsattr` and `e2fsck`.
            sealed: inode.is_immutable(),
        }
    }

    pub(crate) fn to_stat(self, inode: InodeId) -> FileStat {
        FileStat {
            inode,
            file_type: mode_file_type(self.mode),
            size: self.size,
            mode: self.mode,
            nlink: u32::from(self.links),
            uid: u32::from(self.uid),
            gid: u32::from(self.gid),
            atime: u64::from(self.atime),
            mtime: u64::from(self.mtime),
            ctime: u64::from(self.ctime),
            dev_major: 0,
            dev_minor: 0,
            sealed: self.sealed,
        }
    }

    fn encode(self) -> [u64; 4] {
        [
            self.size,
            u64::from(self.mode)
                | u64::from(self.uid) << 16
                | u64::from(self.gid) << 32
                | u64::from(self.links) << 48,
            u64::from(self.atime) | u64::from(self.mtime) << 32,
            u64::from(self.ctime) | u64::from(self.sealed) << 32,
        ]
    }

    fn decode(w: [u64; 4]) -> Self {
        Self {
            size: w[0],
            mode: w[1] as u16,
            uid: (w[1] >> 16) as u16,
            gid: (w[1] >> 32) as u16,
            links: (w[1] >> 48) as u16,
            atime: w[2] as u32,
            mtime: (w[2] >> 32) as u32,
            ctime: w[3] as u32,
            sealed: (w[3] >> 32) & 1 != 0,
        }
    }
}

pub(crate) fn mode_file_type(mode: u16) -> FileType {
    match mode & 0xF000 {
        0x4000 => FileType::Directory,
        0x8000 => FileType::Regular,
        0xA000 => FileType::Symlink,
        0x2000 => FileType::CharDevice,
        0x6000 => FileType::BlockDevice,
        0x1000 => FileType::Pipe,
        0xC000 => FileType::Socket,
        _ => FileType::Regular,
    }
}

fn atomic_words(len: usize) -> KVec<AtomicU64> {
    let Ok(mut words) = KVec::with_capacity(len) else {
        return KVec::new();
    };
    for _ in 0..len {
        if words.push(AtomicU64::new(0)).is_err() {
            return KVec::new();
        }
    }
    words
}

/// Read `N` words of a seqlocked slot into `out`, `false` when a writer was
/// in it.
#[inline]
fn read_slot<const N: usize>(slot: &[AtomicU64], out: &mut [u64; N]) -> bool {
    let seq = slot[SEQ].load(Ordering::Acquire);
    if seq & 1 != 0 {
        return false;
    }
    for (o, w) in out.iter_mut().zip(slot.iter()).skip(1) {
        *o = w.load(Ordering::Relaxed);
    }
    fence(Ordering::Acquire);
    out[SEQ] = seq;
    slot[SEQ].load(Ordering::Relaxed) == seq
}

/// Overwrite a seqlocked slot, or do nothing when another writer holds it:
/// an insert is only ever an optimisation.
#[inline]
fn write_slot(slot: &[AtomicU64], words: &[u64]) {
    let seq = slot[SEQ].load(Ordering::Relaxed);
    if seq & 1 != 0
        || slot[SEQ]
            .compare_exchange(seq, seq + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    fence(Ordering::Release);
    for (w, &v) in slot.iter().zip(words.iter()).skip(1) {
        w.store(v, Ordering::Relaxed);
    }
    slot[SEQ].store(seq + 2, Ordering::Release);
}

/// Which way of a set an insert of `key` takes: its own, an empty one, or
/// else the one the set's total write count picks, which moves on with every
/// insert into the set.
#[inline]
fn victim_way(set: &[AtomicU64], words_per_slot: usize, key_at: usize, key: u64) -> usize {
    let mut empty = None;
    let mut writes = 0u64;
    for way in 0..WAYS {
        let slot = &set[way * words_per_slot..];
        let k = slot[key_at].load(Ordering::Relaxed);
        if k == key {
            return way;
        }
        if k == 0 && empty.is_none() {
            empty = Some(way);
        }
        writes = writes.wrapping_add(slot[SEQ].load(Ordering::Relaxed) >> 1);
    }
    empty.unwrap_or(writes as usize % WAYS)
}

/// One mount's caches. Allocated at a pool slot's first attach and kept for
/// the slot's lifetime, so a reader never holds a reference into freed words.
pub struct Ext2Dcache {
    gens: Ext2Gens,
    dentries: KVec<AtomicU64>,
    attrs: KVec<AtomicU64>,
}

impl Ext2Dcache {
    pub fn new() -> Self {
        let gens = Ext2Gens::new();
        let (dentries, attrs) = if gens.words.is_empty() {
            (KVec::new(), KVec::new())
        } else {
            (
                atomic_words(DENTRY_SETS * WAYS * D_WORDS),
                atomic_words(ATTR_SETS * WAYS * A_WORDS),
            )
        };
        Self {
            gens,
            dentries,
            attrs,
        }
    }

    pub(crate) fn gens(&self) -> &Ext2Gens {
        &self.gens
    }

    /// Forget everything cached: the slot's image is changing.
    pub(crate) fn invalidate_all(&self) {
        self.gens.bump_all();
    }

    #[inline]
    fn dentry_set(&self, hash: u64) -> Option<&[AtomicU64]> {
        let set = hash as usize & (DENTRY_SETS - 1);
        let words = WAYS * D_WORDS;
        self.dentries.get(set * words..(set + 1) * words)
    }

    #[inline]
    fn attr_set(&self, hash: u64) -> Option<&[AtomicU64]> {
        let set = hash as usize & (ATTR_SETS - 1);
        let words = WAYS * A_WORDS;
        self.attrs.get(set * words..(set + 1) * words)
    }

    /// Read under the mount lock, before the lookup it will stamp.
    #[inline]
    pub(crate) fn name_stamp(&self, key: &NameKey) -> NameStamp {
        NameStamp {
            entry: self.gens.read(ENTRY_TABLE, key.hash),
            life: self.gens.read(LIFE_TABLE, ino_hash(key.parent)),
        }
    }

    /// `Some(Some(ino))` for a cached binding, `Some(None)` for a cached
    /// absence, `None` when the cache cannot say.
    #[inline]
    pub(crate) fn lookup(&self, key: &NameKey) -> Option<Option<u32>> {
        let set = self.dentry_set(key.hash)?;
        let key_word = key.key_word();
        for slot in set.chunks_exact(D_WORDS) {
            if slot[D_KEY].load(Ordering::Relaxed) != key_word {
                continue;
            }
            let mut w = [0u64; D_WORDS];
            if !read_slot(slot, &mut w)
                || w[D_KEY] != key_word
                || w[D_NAME..D_NAME + NAME_WORDS] != key.words
            {
                continue;
            }
            let current = self.name_stamp(key);
            if w[D_ENTRY_GEN] != current.entry || w[D_LIFE_GEN] != current.life {
                return None;
            }
            let child = w[D_CHILD] as u32;
            return Some((child != 0).then_some(child));
        }
        None
    }

    /// Record what the lookup stamped `stamp` found: a child, or `None` for a
    /// name proven absent. Never for any other outcome.
    pub(crate) fn insert_name(&self, key: &NameKey, stamp: NameStamp, child: Option<u32>) {
        let Some(set) = self.dentry_set(key.hash) else {
            return;
        };
        let key_word = key.key_word();
        let way = victim_way(set, D_WORDS, D_KEY, key_word);
        let mut w = [0u64; D_WORDS];
        w[D_KEY] = key_word;
        w[D_NAME..D_NAME + NAME_WORDS].copy_from_slice(&key.words);
        w[D_CHILD] = u64::from(child.unwrap_or(0));
        w[D_ENTRY_GEN] = stamp.entry;
        w[D_LIFE_GEN] = stamp.life;
        write_slot(&set[way * D_WORDS..(way + 1) * D_WORDS], &w);
    }

    /// Read under the mount lock, before the record read it will stamp.
    #[inline]
    pub(crate) fn attr_stamp(&self, ino: u32) -> u64 {
        self.gens.read(RECORD_TABLE, ino_hash(ino))
    }

    #[inline]
    pub(crate) fn attr(&self, ino: u32) -> Option<InodeAttr> {
        if ino == 0 {
            return None;
        }
        let hash = ino_hash(ino);
        let set = self.attr_set(hash)?;
        for slot in set.chunks_exact(A_WORDS) {
            if slot[A_KEY].load(Ordering::Relaxed) != u64::from(ino) {
                continue;
            }
            let mut w = [0u64; A_WORDS];
            if !read_slot(slot, &mut w) || w[A_KEY] != u64::from(ino) {
                continue;
            }
            if w[A_GEN] != self.gens.read(RECORD_TABLE, hash) {
                return None;
            }
            return Some(InodeAttr::decode([
                w[A_SIZE], w[A_IDS], w[A_TIMES], w[A_CTIME],
            ]));
        }
        None
    }

    pub(crate) fn insert_attr(&self, ino: u32, stamp: u64, attr: InodeAttr) {
        if ino == 0 {
            return;
        }
        let Some(set) = self.attr_set(ino_hash(ino)) else {
            return;
        };
        let way = victim_way(set, A_WORDS, A_KEY, u64::from(ino));
        let [size, ids, times, ctime] = attr.encode();
        let mut w = [0u64; A_WORDS];
        w[A_KEY] = u64::from(ino);
        w[A_SIZE] = size;
        w[A_IDS] = ids;
        w[A_TIMES] = times;
        w[A_CTIME] = ctime;
        w[A_GEN] = stamp;
        write_slot(&set[way * A_WORDS..(way + 1) * A_WORDS], &w);
    }
}

impl Default for Ext2Dcache {
    fn default() -> Self {
        Self::new()
    }
}
