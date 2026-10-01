//! `/dev/crash` over a store of three slots held in statics: one whole
//! record, one empty slot and one torn record.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::KVec;
use slopos_testing::{TestResult, assert_eq_test, assert_test, fail, pass};

use crate::devfs::crash::{self, CRASH_DIR};
use crate::devfs::{CrashRecord, CrashStoreOps};
use crate::vfs::{FileType, InodeId, VfsError, VfsResult};

const TEXTS: [&[u8]; 3] = [b"record seven\n", b"", b"record nine, torn\n"];
static HELD: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn slots() -> usize {
    TEXTS.len()
}

fn record(slot: usize) -> Option<CrashRecord> {
    let sequence = HELD.get(slot)?.load(Ordering::Acquire);
    (sequence != 0).then(|| CrashRecord {
        sequence,
        text_len: TEXTS[slot].len() as u64,
        intact: slot != 2,
    })
}

fn read(slot: usize, sequence: u64, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
    if record(slot).map(|r| r.sequence) != Some(sequence) {
        return Err(VfsError::NotFound);
    }
    let text = TEXTS[slot].get(offset as usize..).unwrap_or_default();
    let n = text.len().min(buf.len());
    buf[..n].copy_from_slice(&text[..n]);
    Ok(n)
}

fn erase(slot: usize, sequence: u64) -> VfsResult<()> {
    HELD[slot]
        .compare_exchange(sequence, 0, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| VfsError::NotFound)
}

static STORE: CrashStoreOps = CrashStoreOps {
    slots,
    record,
    read,
    erase,
};

fn listing(cookie: u64) -> KVec<(u64, KVec<u8>, InodeId)> {
    let mut seen = KVec::new();
    crash::walk(&STORE, cookie, &mut |next, name, inode, _| {
        let mut owned = KVec::new();
        owned.extend_from_slice(name).is_ok() && seen.push((next, owned, inode)).is_ok()
    });
    seen
}

fn names(listed: &[(u64, KVec<u8>, InodeId)]) -> KVec<&[u8]> {
    let mut names = KVec::new();
    for (_, name, _) in listed {
        let _ = names.push(&name[..]);
    }
    names
}

pub fn test_crash_devfs_lists_and_names_records() -> TestResult {
    HELD[0].store(7, Ordering::Release);
    HELD[2].store(9, Ordering::Release);
    let listed = listing(0);
    assert_eq_test!(
        &names(&listed)[..],
        &[&b"."[..], b"..", b"7", b"9-torn"][..],
        "the directory lists its records, a torn one so named"
    );
    let resumed = listing(listed[2].0);
    assert_eq_test!(
        &names(&resumed)[..],
        &[&b"9-torn"[..]][..],
        "a listing resumed after a record starts at the next"
    );
    let Ok(seven) = crash::lookup(&STORE, b"7") else {
        return fail!("record 7 is not found");
    };
    assert_eq_test!(seven, listed[2].2, "a lookup finds the listed inode");
    for absent in [&b"9"[..], b"07", b"8", b"7-torn", b""] {
        assert_test!(
            crash::lookup(&STORE, absent) == Err(VfsError::NotFound),
            "a name no record carries must not be found"
        );
    }
    assert_eq_test!(
        crash::lookup(&STORE, b"."),
        Ok(CRASH_DIR),
        "the directory's own name"
    );
    let Some(stat) = crash::stat(&STORE, seven) else {
        return fail!("record 7 has no stat");
    };
    assert_test!(
        stat.file_type == FileType::Regular && stat.size == TEXTS[0].len() as u64,
        "a record is a regular file of its text's length"
    );
    assert_test!(
        crash::stat(&STORE, CRASH_DIR).is_some_and(|s| s.file_type == FileType::Directory),
        "the directory stats as one"
    );
    pass!()
}

pub fn test_crash_devfs_reads_and_erases_with_the_right() -> TestResult {
    HELD[0].store(7, Ordering::Release);
    HELD[2].store(9, Ordering::Release);
    let Ok(seven) = crash::lookup(&STORE, b"7") else {
        return fail!("record 7 is not found");
    };
    let mut buf = [0u8; 32];
    assert_eq_test!(
        crash::read(&STORE, seven, 0, &mut buf, false),
        Err(VfsError::PermissionDenied),
        "a read without the raw-device right"
    );
    assert_eq_test!(
        crash::read(&STORE, seven, 7, &mut buf, true),
        Ok(TEXTS[0].len() - 7),
        "a read from an offset"
    );
    assert_test!(buf[..6] == *b"seven\n", "the read's bytes");
    assert_eq_test!(
        crash::unlink(&STORE, b"7", false),
        Err(VfsError::PermissionDenied),
        "an erase without the raw-device right"
    );
    assert_test!(record(0).is_some(), "a refused erase leaves the record");
    assert_eq_test!(crash::unlink(&STORE, b"7", true), Ok(()), "an erase");
    assert_eq_test!(
        crash::lookup(&STORE, b"7"),
        Err(VfsError::NotFound),
        "an erased record is gone"
    );
    assert_eq_test!(
        crash::read(&STORE, seven, 0, &mut buf, true),
        Err(VfsError::NotFound),
        "its inode reads nothing"
    );
    assert_eq_test!(
        crash::unlink(&STORE, b"9-torn", true),
        Ok(()),
        "a torn record erases too"
    );
    let emptied = listing(0);
    assert_eq_test!(
        &names(&emptied)[..],
        &[&b"."[..], b".."][..],
        "an empty store"
    );
    pass!()
}

slopos_testing::stest!(name = test_crash_devfs_lists_and_names_records, suite = fs);
slopos_testing::stest!(
    name = test_crash_devfs_reads_and_erases_with_the_right,
    suite = fs
);
