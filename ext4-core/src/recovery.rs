//! Replaying a jbd2 journal, whatever implementation wrote it.
//!
//! Three passes over the live log from `s_start`: find the last transaction
//! whose commit checks out; collect the committed revokes, each block keeping
//! the newest transaction that revoked it, and hold every committed copy to
//! its tag's checksum; then write each copy home in log order unless its own
//! or a later transaction revoked it, so a block logged twice ends newest.
//!
//! Nothing is written home until every committed block has checked out: a
//! damaged journal fails the whole replay, left to a repair tool, rather than
//! half applying. A tag may not name a block outside the volume or inside the
//! journal, whose copy written home could rewrite the log between passes.

use crate::jbd2::{self, BlockHeader, Format, Superblock, TagCursor, blocktype, tag_flag};

/// Reads the journal and writes the filesystem.
pub trait JournalIo {
    type Error;

    /// Read journal block `block`, counted from the journal's start.
    fn read(&mut self, block: u32, buf: &mut [u8]) -> Result<(), Self::Error>;

    /// Write filesystem block `block`.
    fn write_home(&mut self, block: u64, data: &[u8]) -> Result<(), Self::Error>;
}

/// The revokes of a replay. A transaction is named by its place in the log,
/// counted from the first one replayed, so the newest is the largest.
pub trait RevokeTable {
    /// Transaction `age` revoked `block`; `false` when the table could not
    /// grow.
    fn note(&mut self, block: u64, age: u32) -> bool;
    /// The newest transaction that revoked `block`, once every revoke is
    /// noted.
    fn newest(&mut self, block: u64) -> Option<u32>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverError<E> {
    Io(E),
    /// A committed transaction is damaged: a block of it fails its checksum,
    /// a revoke block miscounts, or a tag names a block replay may not write.
    Corrupt,
    /// The journal uses a feature this implementation does not replay.
    Unsupported,
    OutOfMemory,
}

/// What a replay did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Recovery {
    /// Committed transactions found and applied.
    pub transactions: u32,
    /// Copies written home.
    pub blocks: u32,
    /// The sequence the next transaction must carry: one past the first that
    /// did not commit, whose stale blocks may still sit in the log.
    pub next_sequence: u32,
}

/// One transaction as the scan found it; `next` is where the following one
/// starts.
enum Scanned {
    Committed { next: u32, commit_secs: u64 },
    BadCommit { next: u32 },
    End,
}

type RevokeVisitor<'v, J> =
    dyn FnMut(&Format, &[u8], u32) -> Result<(), RecoverError<<J as JournalIo>::Error>> + 'v;

type CopyVisitor<'v, J> = dyn FnMut(
        &mut J,
        &Format,
        &jbd2::Tag,
        u32,
        u32,
        &mut [u8],
    ) -> Result<(), RecoverError<<J as JournalIo>::Error>>
    + 'v;

struct Log<'a, J: JournalIo> {
    io: &'a mut J,
    sb: Superblock,
    fmt: Format,
    meta: &'a mut [u8],
    data: &'a mut [u8],
    writable: &'a dyn Fn(u64) -> bool,
}

impl<J: JournalIo> Log<'_, J> {
    fn read_meta(&mut self, block: u32) -> Result<Option<BlockHeader>, RecoverError<J::Error>> {
        self.io.read(block, self.meta).map_err(RecoverError::Io)?;
        Ok(jbd2::block_header(self.meta))
    }

    /// Scan transaction `sequence` from `block`, spending `budget`, the log
    /// left to read, so a log that runs back onto itself ends.
    ///
    /// Damage no commit follows is a transaction a crash cut short and ends
    /// the log; damage before a commit no older than `last_time` is the medium's.
    fn scan_transaction(
        &mut self,
        mut block: u32,
        sequence: u32,
        budget: &mut u32,
        last_time: u64,
    ) -> Result<Scanned, RecoverError<J::Error>> {
        let mut damaged = false;
        loop {
            let Some(left) = budget.checked_sub(1) else {
                return Ok(Scanned::End);
            };
            *budget = left;
            let Some(h) = self.read_meta(block)? else {
                return Ok(Scanned::End);
            };
            if h.sequence != sequence {
                return Ok(Scanned::End);
            }
            block = self.sb.next(block);
            match h.blocktype {
                blocktype::DESCRIPTOR => {
                    damaged |= !jbd2::verify_descriptor(&self.fmt, self.meta);
                    let writable = self.writable;
                    let mut outside = false;
                    let count = jbd2::tags(&self.fmt, self.meta, &mut |tag| {
                        outside |= !writable(tag.block);
                    }) as u32;
                    damaged |= outside;
                    let Some(left) = budget.checked_sub(count) else {
                        return Ok(Scanned::End);
                    };
                    *budget = left;
                    for _ in 0..count {
                        block = self.sb.next(block);
                    }
                }
                blocktype::REVOKE => {
                    damaged |= !jbd2::verify_revoke(&self.fmt, self.meta)
                        || jbd2::revoked(&self.fmt, self.meta, &mut |_| {}).is_none();
                }
                blocktype::COMMIT => {
                    if !jbd2::verify_commit(&self.fmt, self.meta) {
                        return Ok(Scanned::BadCommit { next: block });
                    }
                    let commit_secs = jbd2::commit_seconds(self.meta);
                    return match (damaged, commit_secs >= last_time) {
                        (false, _) => Ok(Scanned::Committed {
                            next: block,
                            commit_secs,
                        }),
                        (true, true) => Err(RecoverError::Corrupt),
                        (true, false) => Ok(Scanned::End),
                    };
                }
                _ => return Ok(Scanned::End),
            }
        }
    }

    /// Visit the blocks of the transactions before sequence `last` in log
    /// order, failing on any block the scan would not accept or beyond the
    /// first `blocks`: `on_revoke` per revoke block, `on_copy` per data copy.
    fn walk(
        &mut self,
        last: u32,
        mut blocks: u32,
        on_revoke: &mut RevokeVisitor<'_, J>,
        on_copy: &mut CopyVisitor<'_, J>,
    ) -> Result<(), RecoverError<J::Error>> {
        let mut block = self.sb.start;
        let mut sequence = self.sb.sequence;
        while sequence != last {
            blocks = blocks.checked_sub(1).ok_or(RecoverError::Corrupt)?;
            let h = self.read_meta(block)?.ok_or(RecoverError::Corrupt)?;
            if h.sequence != sequence {
                return Err(RecoverError::Corrupt);
            }
            block = self.sb.next(block);
            match h.blocktype {
                blocktype::DESCRIPTOR => {
                    if !jbd2::verify_descriptor(&self.fmt, self.meta) {
                        return Err(RecoverError::Corrupt);
                    }
                    let mut cursor = TagCursor::new();
                    while let Some(tag) = cursor.next(&self.fmt, self.meta) {
                        blocks = blocks.checked_sub(1).ok_or(RecoverError::Corrupt)?;
                        if !(self.writable)(tag.block) {
                            return Err(RecoverError::Corrupt);
                        }
                        on_copy(self.io, &self.fmt, &tag, sequence, block, self.data)?;
                        block = self.sb.next(block);
                    }
                }
                blocktype::REVOKE => {
                    if !jbd2::verify_revoke(&self.fmt, self.meta) {
                        return Err(RecoverError::Corrupt);
                    }
                    on_revoke(&self.fmt, self.meta, sequence)?;
                }
                blocktype::COMMIT => sequence = sequence.wrapping_add(1),
                _ => return Err(RecoverError::Corrupt),
            }
        }
        Ok(())
    }
}

/// Read the copy at `at` into `data` and hold it to its tag.
fn checked_copy<J: JournalIo>(
    io: &mut J,
    fmt: &Format,
    tag: &jbd2::Tag,
    sequence: u32,
    at: u32,
    data: &mut [u8],
) -> Result<(), RecoverError<J::Error>> {
    io.read(at, data).map_err(RecoverError::Io)?;
    if jbd2::verify_data(fmt, tag, sequence, data) {
        Ok(())
    } else {
        Err(RecoverError::Corrupt)
    }
}

/// Apply every committed transaction in the journal whose superblock is
/// `sb`. `meta` and `data` are block-sized buffers. `writable` says which
/// blocks a tag may name: the volume's, less the journal's own.
pub fn recover<J: JournalIo, R: RevokeTable>(
    io: &mut J,
    revokes: &mut R,
    sb: &Superblock,
    meta: &mut [u8],
    data: &mut [u8],
    writable: &dyn Fn(u64) -> bool,
) -> Result<Recovery, RecoverError<J::Error>> {
    let mut done = Recovery {
        next_sequence: sb.sequence,
        ..Recovery::default()
    };
    if sb.start == 0 {
        return Ok(done);
    }
    if !sb.supported() || sb.incompat & jbd2::feature::INCOMPAT_FAST_COMMIT != 0 {
        return Err(RecoverError::Unsupported);
    }
    let mut log = Log {
        io,
        sb: *sb,
        fmt: sb.format(),
        meta,
        data,
        writable,
    };

    let total = sb.maxlen - sb.first;
    let mut budget = total;
    let mut used = 0;
    let mut at = sb.start;
    let mut last = sb.sequence;
    let mut last_time = 0;
    loop {
        match log.scan_transaction(at, last, &mut budget, last_time)? {
            Scanned::Committed { next, commit_secs } => {
                at = next;
                last = last.wrapping_add(1);
                last_time = commit_secs;
                used = total - budget;
                done.transactions += 1;
            }
            // A later commit is written only behind a flush covering this
            // one, so damage to this one is the medium's.
            Scanned::BadCommit { next } => {
                let mut rest = budget;
                let after =
                    log.scan_transaction(next, last.wrapping_add(1), &mut rest, last_time)?;
                if matches!(after, Scanned::Committed { .. }) {
                    return Err(RecoverError::Corrupt);
                }
                break;
            }
            Scanned::End => break,
        }
    }
    done.next_sequence = last.wrapping_add(1);
    if done.transactions == 0 {
        return Ok(done);
    }

    let age = |sequence: u32| sequence.wrapping_sub(sb.sequence);
    let mut grew = true;
    log.walk(
        last,
        used,
        &mut |fmt, block, sequence| {
            let counted = jbd2::revoked(fmt, block, &mut |b| {
                grew &= revokes.note(b, age(sequence));
            });
            counted.map(|_| ()).ok_or(RecoverError::Corrupt)
        },
        &mut checked_copy::<J>,
    )?;
    if !grew {
        return Err(RecoverError::OutOfMemory);
    }

    let mut blocks = 0u32;
    log.walk(
        last,
        used,
        &mut |_, _, _| Ok(()),
        &mut |io, fmt, tag, sequence, at, data| {
            if revokes
                .newest(tag.block)
                .is_some_and(|r| age(sequence) <= r)
            {
                return Ok(());
            }
            checked_copy(io, fmt, tag, sequence, at, data)?;
            if tag.flags & tag_flag::ESCAPE != 0 {
                jbd2::unescape(data);
            }
            io.write_home(tag.block, data).map_err(RecoverError::Io)?;
            blocks += 1;
            Ok(())
        },
    )?;
    done.blocks = blocks;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jbd2::{Csum, Tag};
    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    const BS: usize = 1024;
    const LEN: u32 = 64;
    const JOURNAL_VOLUME_BLOCK: u64 = 1000;

    struct Mem {
        log: Vec<Vec<u8>>,
        home: BTreeMap<u64, Vec<u8>>,
    }

    impl JournalIo for Mem {
        type Error = ();

        fn read(&mut self, block: u32, buf: &mut [u8]) -> Result<(), ()> {
            buf.copy_from_slice(&self.log[block as usize]);
            Ok(())
        }

        fn write_home(&mut self, block: u64, data: &[u8]) -> Result<(), ()> {
            self.home.insert(block, data.to_vec());
            if (JOURNAL_VOLUME_BLOCK..JOURNAL_VOLUME_BLOCK + u64::from(LEN)).contains(&block) {
                self.log[(block - JOURNAL_VOLUME_BLOCK) as usize].copy_from_slice(data);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Revokes(BTreeMap<u64, u32>);

    impl RevokeTable for Revokes {
        fn note(&mut self, block: u64, age: u32) -> bool {
            let newest = self.0.entry(block).or_insert(age);
            *newest = (*newest).max(age);
            true
        }

        fn newest(&mut self, block: u64) -> Option<u32> {
            self.0.get(&block).copied()
        }
    }

    fn sb(start: u32, sequence: u32) -> Superblock {
        Superblock {
            block_size: BS as u32,
            maxlen: LEN,
            first: 1,
            sequence,
            start,
            errno: 0,
            compat: 0,
            incompat: jbd2::feature::INCOMPAT_CSUM_V3 | jbd2::feature::INCOMPAT_64BIT,
            ro_compat: 0,
            uuid: *b"recovery-tests!!",
        }
    }

    struct Writer {
        mem: Mem,
        fmt: Format,
        at: u32,
    }

    impl Writer {
        fn new(start: u32) -> Self {
            let fmt = sb(start, 0).format();
            assert_eq!(fmt.csum, Csum::V3);
            Self {
                mem: Mem {
                    log: vec![vec![0u8; BS]; LEN as usize],
                    home: BTreeMap::new(),
                },
                fmt,
                at: start,
            }
        }

        fn put(&mut self, block: &[u8]) -> u32 {
            let at = self.at;
            self.mem.log[at as usize].copy_from_slice(block);
            self.at = sb(1, 0).next(at);
            at
        }

        /// Log a descriptor and its copies; returns the descriptor's block.
        fn descriptor(&mut self, sequence: u32, copies: &[(u64, u8)]) -> u32 {
            let mut d = vec![0u8; BS];
            jbd2::begin_descriptor(&mut d, sequence);
            let mut data = Vec::new();
            for (i, &(block, fill)) in copies.iter().enumerate() {
                let copy = vec![fill; BS];
                let tag = Tag {
                    block,
                    flags: 0,
                    checksum: self.fmt.data_checksum(sequence, &copy),
                };
                jbd2::put_tag(&self.fmt, &mut d, i, copies.len(), tag);
                data.push(copy);
            }
            jbd2::seal_descriptor(&self.fmt, &mut d);
            let at = self.put(&d);
            for copy in &data {
                self.put(copy);
            }
            at
        }

        fn commit(&mut self, sequence: u32, sec: u64) {
            let mut c = vec![0u8; BS];
            jbd2::encode_commit(&self.fmt, &mut c, sequence, sec, 0);
            self.put(&c);
        }

        fn replay(&mut self, start: u32, sequence: u32) -> Result<Recovery, RecoverError<()>> {
            let (mut meta, mut data) = (vec![0u8; BS], vec![0u8; BS]);
            recover(
                &mut self.mem,
                &mut Revokes::default(),
                &sb(start, sequence),
                &mut meta,
                &mut data,
                &|b| b < 4096,
            )
        }
    }

    #[test]
    fn the_next_sequence_skips_the_one_that_did_not_commit() {
        let mut w = Writer::new(1);
        w.descriptor(5, &[(10, 1)]);
        w.commit(5, 100);
        w.descriptor(6, &[(11, 2)]);
        let done = w.replay(1, 5).unwrap();
        assert_eq!((done.transactions, done.next_sequence), (1, 7));
        assert_eq!(w.mem.home.get(&10), Some(&vec![1u8; BS]));
        assert!(!w.mem.home.contains_key(&11));
    }

    #[test]
    fn a_transaction_may_wrap_past_the_end_of_the_log() {
        let mut w = Writer::new(LEN - 2);
        w.descriptor(9, &[(20, 3), (21, 4), (22, 5)]);
        w.commit(9, 100);
        let done = w.replay(LEN - 2, 9).unwrap();
        assert_eq!((done.transactions, done.blocks), (1, 3));
        assert_eq!(w.mem.home.get(&22), Some(&vec![5u8; BS]));
    }

    /// A copy whose home is a journal block would turn the next
    /// transaction's descriptor into another between the passes.
    #[test]
    fn a_tag_naming_the_journal_is_refused_before_anything_is_written() {
        let mut w = Writer::new(1);
        w.descriptor(1, &[(10, 1), (JOURNAL_VOLUME_BLOCK + 8, 2)]);
        w.commit(1, 100);
        let (mut meta, mut data) = (vec![0u8; BS], vec![0u8; BS]);
        let journal = JOURNAL_VOLUME_BLOCK..JOURNAL_VOLUME_BLOCK + u64::from(LEN);
        let result = recover(
            &mut w.mem,
            &mut Revokes::default(),
            &sb(1, 1),
            &mut meta,
            &mut data,
            &|b| b < 4096 && !journal.contains(&b),
        );
        assert_eq!(result, Err(RecoverError::Corrupt));
        assert!(w.mem.home.is_empty());
    }

    /// Damage in a transaction that committed is the medium's; damage that
    /// no commit follows is a write a crash cut short.
    #[test]
    fn a_damaged_descriptor_fails_only_a_committed_transaction() {
        let mut w = Writer::new(1);
        w.descriptor(1, &[(10, 1)]);
        w.commit(1, 100);
        let bad = w.descriptor(2, &[(11, 2)]);
        w.commit(2, 101);
        w.mem.log[bad as usize][40] ^= 1;
        assert_eq!(w.replay(1, 1), Err(RecoverError::Corrupt));
        assert!(w.mem.home.is_empty());

        let mut w = Writer::new(1);
        w.descriptor(1, &[(10, 1)]);
        w.commit(1, 100);
        let bad = w.descriptor(2, &[(11, 2)]);
        w.mem.log[bad as usize][40] ^= 1;
        let done = w.replay(1, 1).unwrap();
        assert_eq!(done.transactions, 1);
    }

    /// A commit older than the last one belongs to an earlier pass over the
    /// log, so the damage before it is that pass's leftovers.
    #[test]
    fn a_stale_commit_after_damage_ends_the_log() {
        let mut w = Writer::new(1);
        w.descriptor(1, &[(10, 1)]);
        w.commit(1, 100);
        let bad = w.descriptor(2, &[(11, 2)]);
        w.commit(2, 50);
        w.mem.log[bad as usize][40] ^= 1;
        assert_eq!(w.replay(1, 1).map(|d| d.transactions), Ok(1));
    }

    /// A commit block a crash tore ends the log; one a later commit follows
    /// reached the medium whole, so the medium damaged it.
    #[test]
    fn a_bad_commit_fails_the_replay_only_when_a_later_one_commits() {
        let mut w = Writer::new(1);
        w.descriptor(1, &[(10, 1)]);
        w.commit(1, 100);
        w.descriptor(2, &[(11, 2)]);
        let bad = w.at;
        w.commit(2, 101);
        w.mem.log[bad as usize][100] ^= 1;
        let done = w.replay(1, 1).unwrap();
        assert_eq!((done.transactions, done.next_sequence), (1, 3));

        w.descriptor(3, &[(12, 3)]);
        w.commit(3, 102);
        w.mem.home.clear();
        assert_eq!(w.replay(1, 1), Err(RecoverError::Corrupt));
        assert!(w.mem.home.is_empty());
    }

    #[test]
    fn a_revoke_block_that_miscounts_fails_its_transaction() {
        let mut w = Writer::new(1);
        w.descriptor(1, &[(10, 1)]);
        let mut r = vec![0u8; BS];
        jbd2::begin_revoke(&mut r, 1);
        jbd2::finish_revoke(&w.fmt, &mut r, BS);
        assert!(jbd2::verify_revoke(&w.fmt, &r));
        w.put(&r);
        w.commit(1, 100);
        assert_eq!(w.replay(1, 1), Err(RecoverError::Corrupt));
    }
}
