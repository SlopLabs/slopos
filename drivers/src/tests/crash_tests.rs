//! The crash store on the NVMe scratch namespaces: a GPT naming a crash
//! partition of four slots at 256 KiB, clear of every other test's region,
//! with a guard block either side that no write may reach. Records go in
//! through the panic queue and are found again by a store opened as the next
//! boot opens one: read, torn, erased, and the oldest overwritten once every
//! slot holds one.

use slopos_boot_core::Guid;
use slopos_boot_core::crash::{HEADER_BYTES, LOG_HEADING, SLOT_BYTES, Summary};
use slopos_boot_core::layout::CRASH_TYPE;
use slopos_fs::devfs::CrashRecord;
use slopos_fs::vfs::VfsError;
use slopos_ostd::{KVec, klog_info};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use super::gpt_fixture;
use crate::block;
use crate::crash::{Booted, OpenError, PanicRecord, REPORT_MAX, Store, Written};

const WINDOW: (u64, u64) = (256 << 10, 256 << 10);
const GUARD: usize = 4096;
const GUARD_BYTE: u8 = 0xA5;
const KERNEL: &str = "/boot/t/kernel.elf";
const BUILD: &str = "crash-test";
const BOOTED: Booted<'static> = Booted {
    kernel: KERNEL,
    cmdline: "crash_tests",
    build: BUILD,
};

fn show(name: &[u8]) -> &str {
    core::str::from_utf8(name).unwrap_or("?")
}

/// Run `body` with an empty crash partition laid out on `disk`, under GPT
/// disk GUID `guid`, and wipe the table after it.
#[inline(never)]
fn with_crash_partition(disk: &[u8], guid: Guid, body: impl FnOnce() -> TestResult) -> TestResult {
    let laid = block::claim(disk)
        .map_err(|_| "claim")
        .and_then(|whole| {
            let zeros = KVec::<u8>::zeroed(WINDOW.1 as usize).map_err(|_| "zeros alloc")?;
            whole.write_at(WINDOW.0, &zeros).map_err(|_| "zeroing")?;
            let mut guard = KVec::<u8>::zeroed(GUARD).map_err(|_| "guard alloc")?;
            guard.fill(GUARD_BYTE);
            for at in guards() {
                whole.write_at(at, &guard).map_err(|_| "guarding")?;
            }
            let crash = gpt_fixture::Entry {
                type_guid: CRASH_TYPE,
                unique: Guid([0xC7; 16]),
                start: WINDOW.0,
                len: WINDOW.1,
            };
            gpt_fixture::install(whole.as_ref(), guid, &[crash])
        })
        .and_then(|()| block::reread(disk).map_err(|_| "re-read"));
    let verdict = match laid {
        Ok(()) => body(),
        Err(why) => fail!("laying out {}: {}", show(disk), why),
    };
    let wiped = block::claim(disk)
        .map_err(|_| "claim")
        .and_then(|whole| {
            let mut guard = KVec::<u8>::zeroed(GUARD).map_err(|_| "guard alloc")?;
            for at in guards() {
                whole
                    .read_at(at, &mut guard)
                    .map_err(|_| "reading a guard")?;
                if guard.iter().any(|&b| b != GUARD_BYTE) {
                    return Err("a write reached past the crash partition");
                }
            }
            gpt_fixture::wipe(whole.as_ref())
        })
        .and_then(|()| block::reread(disk).map_err(|_| "re-read"));
    match (verdict, wiped) {
        (TestResult::Pass, Err(why)) => fail!("wiping {}: {}", show(disk), why),
        (verdict, _) => verdict,
    }
}

fn guards() -> [u64; 2] {
    [WINDOW.0 - GUARD as u64, WINDOW.0 + WINDOW.1]
}

fn open(guid: Guid) -> Result<Store, TestResult> {
    Store::open(guid, &BOOTED).map_err(|e| fail!("opening the store: {:?}", e))
}

fn write(store: &Store, text: &[u8]) -> Result<Written, TestResult> {
    write_in_chunks(store, text, SLOT_BYTES)
}

/// Write `text` as a record through the panic path's own write.
#[inline(never)]
fn write_in_chunks(store: &Store, text: &[u8], chunk: usize) -> Result<Written, TestResult> {
    let mut slot = KVec::<u8>::zeroed(SLOT_BYTES).map_err(|_| fail!("slot alloc"))?;
    slot[HEADER_BYTES..HEADER_BYTES + text.len()].copy_from_slice(text);
    let record: &mut [u8; SLOT_BYTES] =
        (&mut slot[..]).try_into().map_err(|_| fail!("slot size"))?;
    store
        .write_in_chunks(record, text.len(), chunk)
        .map_err(|e| fail!("the panic write failed: {:?}", e))
}

fn read_all(store: &Store, slot: usize, sequence: u64) -> Result<KVec<u8>, TestResult> {
    let len = store.record(slot).map_or(0, |r| r.text_len as usize);
    let mut text = KVec::<u8>::zeroed(len).map_err(|_| fail!("text alloc"))?;
    match store.read(slot, sequence, 0, &mut text) {
        Ok(n) if n == len => Ok(text),
        other => Err(fail!("reading slot {} answered {:?}", slot, other)),
    }
}

fn held(sequence: u64, text: &[u8], intact: bool) -> Option<CrashRecord> {
    Some(CrashRecord {
        sequence,
        text_len: text.len() as u64,
        intact,
    })
}

const FIRST: &[u8] = b"the first record\n";
const SECOND: &[u8] = b"the second record, a little longer\n";

/// Each step opens the store as a boot would, and the next opens it again.
fn records_persist(guid: Guid) -> TestResult {
    for step in [first_records, read_and_erase, fill_the_ring, tears, chunked] {
        let verdict = step(guid);
        if !matches!(verdict, TestResult::Pass) {
            return verdict;
        }
    }
    pass!()
}

#[inline(never)]
fn first_records(guid: Guid) -> TestResult {
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    assert_eq_test!(store.slots(), 4, "four slots in 256 KiB");
    assert_test!(
        (0..4).all(|slot| store.record(slot).is_none()),
        "a zeroed partition holds no record"
    );
    assert_test!(
        matches!(Store::open(guid, &BOOTED), Err(OpenError::Claim(_))),
        "a second store must not claim the partition"
    );
    match (write(&store, FIRST), write(&store, SECOND)) {
        (
            Ok(Written {
                slot: 0,
                sequence: 1,
            }),
            Ok(Written {
                slot: 1,
                sequence: 2,
            }),
        ) => pass!(),
        other => fail!(
            "the first two records landed at {:?}",
            (other.0.ok(), other.1.ok())
        ),
    }
}

#[inline(never)]
fn read_and_erase(guid: Guid) -> TestResult {
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    assert_eq_test!(
        store.record(0),
        held(1, FIRST, true),
        "slot 0 after a reopen"
    );
    assert_eq_test!(
        store.record(1),
        held(2, SECOND, true),
        "slot 1 after a reopen"
    );
    match read_all(&store, 1, 2) {
        Ok(text) => assert_test!(text[..] == *SECOND, "a record reads back as written"),
        Err(verdict) => return verdict,
    }
    assert_test!(
        matches!(store.read(1, 1, 0, &mut [0u8; 8]), Err(VfsError::NotFound)),
        "a read naming another record's sequence must reach nothing"
    );
    assert_test!(
        matches!(store.erase(0, 2), Err(VfsError::NotFound)),
        "an erase naming another record's sequence must erase nothing"
    );
    assert_test!(store.erase(0, 1).is_ok(), "erasing record 1");
    assert_test!(store.record(0).is_none(), "an erased slot holds nothing");
    pass!()
}

#[inline(never)]
fn fill_the_ring(guid: Guid) -> TestResult {
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    assert_test!(
        store.record(0).is_none() && store.record(1).is_some(),
        "an erase must outlast a reopen"
    );
    let mut landed = [None; 4];
    for (at, text) in landed.iter_mut().zip([FIRST, SECOND, FIRST, SECOND]) {
        *at = write(&store, text).ok().map(|w| (w.slot, w.sequence));
    }
    assert_eq_test!(
        landed,
        [Some((2, 3)), Some((3, 4)), Some((0, 5)), Some((1, 6))],
        "records follow the newest, then overwrite the oldest"
    );
    pass!()
}

/// A byte of record 3's text changed under the store reads back as torn.
#[inline(never)]
fn tears(guid: Guid) -> TestResult {
    let partition = match block::locate_partition(guid, CRASH_TYPE) {
        Ok(located) => located.partition,
        Err(e) => return fail!("the crash partition is not found: {:?}", e),
    };
    let flipped = block::claim(partition.as_bytes()).map(|device| {
        let at = 2 * SLOT_BYTES as u64 + HEADER_BYTES as u64;
        let mut byte = [0u8; 1];
        device.read_at(at, &mut byte).is_ok()
            && device.write_at(at, &[byte[0] ^ 0x20]).is_ok()
            && device.flush().is_ok()
    });
    if flipped != Ok(true) {
        return fail!("could not change slot 2's text");
    }
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    assert_eq_test!(
        store.record(2),
        held(3, FIRST, false),
        "a changed record is torn"
    );
    assert_eq_test!(
        store.record(3),
        held(4, SECOND, true),
        "its neighbour is whole"
    );
    pass!()
}

/// A record written a block at a time, as a drive with a small largest
/// transfer takes it, over the torn one, the oldest.
#[inline(never)]
fn chunked(guid: Guid) -> TestResult {
    let mut text = match KVec::<u8>::zeroed(10_000) {
        Ok(text) => text,
        Err(_) => return fail!("text alloc"),
    };
    for (i, byte) in text.iter_mut().enumerate() {
        *byte = b'a' + (i % 26) as u8;
    }
    let landed = match open(guid) {
        Ok(store) => write_in_chunks(&store, &text, 4096),
        Err(verdict) => return verdict,
    };
    match landed {
        Ok(Written {
            slot: 2,
            sequence: 7,
        }) => reads_back(guid, 2, 7, &text),
        other => fail!("the chunked record landed at {:?}", other.ok()),
    }
}

#[inline(never)]
fn reads_back(guid: Guid, slot: usize, sequence: u64, text: &[u8]) -> TestResult {
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    assert_eq_test!(
        store.record(slot),
        held(sequence, text, true),
        "the record is whole"
    );
    match read_all(&store, slot, sequence) {
        Ok(back) => assert_test!(back[..] == *text, "the record reads back"),
        Err(verdict) => return verdict,
    }
    pass!()
}

pub fn test_crash_store_on_512_byte_blocks() -> TestResult {
    let guid = Guid([0xE5; 16]);
    with_crash_partition(b"nvme0n2", guid, || records_persist(guid))
}

pub fn test_crash_store_on_4096_byte_blocks() -> TestResult {
    let guid = Guid([0xE4; 16]);
    with_crash_partition(b"nvme1n2", guid, || records_persist(guid))
}

/// A disk with no polled queue for the panic path keeps no store.
pub fn test_crash_store_needs_a_panic_queue() -> TestResult {
    let guid = Guid([0xE3; 16]);
    with_crash_partition(b"vdb", guid, || {
        assert_test!(
            matches!(Store::open(guid, &BOOTED), Err(OpenError::NotNvme)),
            "a virtio disk's crash partition must not be armed"
        );
        pass!()
    })
}

const PANIC: &str = "crash_tests: a panic for the record";
const MARKER: &str = "CRASH_TEST: the log line before the panic";

/// One record as the panic path writes it, read back: the summary, what the
/// report wrote, and the kernel log's newest lines.
#[inline(never)]
fn panic_record_reads_back(guid: Guid) -> TestResult {
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    // A log line another CPU is writing as the record is closed holds the
    // ring, and the panic path, whose peers are stopped, never meets that.
    for _ in 0..3 {
        klog_info!("{}", MARKER);
        let Some(mut record) = PanicRecord::begin(&store, PANIC) else {
            return fail!("the panic record's buffer is taken");
        };
        let _ = core::fmt::Write::write_str(&mut record, "=== KERNEL PANIC ===\n");
        let written = match record.commit() {
            Ok(written) => written,
            Err(e) => return fail!("committing the record failed: {:?}", e),
        };
        let text = match read_all(&store, written.slot, written.sequence) {
            Ok(text) => text,
            Err(verdict) => return verdict,
        };
        let Ok(text) = core::str::from_utf8(&text) else {
            return fail!("the record is not UTF-8");
        };
        let Some(summary) = Summary::parse(text) else {
            return fail!("the record opens with no summary: {}", text);
        };
        assert_eq_test!(summary.kernel, KERNEL, "the summary's kernel");
        assert_eq_test!(summary.build, BUILD, "the summary's build");
        assert_eq_test!(
            summary.cmdline,
            BOOTED.cmdline,
            "the summary's command line"
        );
        assert_eq_test!(summary.slot(), Some("t"), "the summary's slot");
        assert_eq_test!(summary.panic, PANIC, "the summary's panic");
        assert_test!(
            text.contains("\n\n=== KERNEL PANIC ===\n"),
            "the report follows the summary"
        );
        let Some((_, log)) = text.split_once(LOG_HEADING) else {
            return fail!("the record has no log heading");
        };
        if log.contains(MARKER) {
            assert_test!(log.ends_with('\n'), "the log's tail is whole lines");
            return pass!();
        }
    }
    fail!("no record carried the log line before its panic")
}

pub fn test_crash_panic_record() -> TestResult {
    let guid = Guid([0xE6; 16]);
    with_crash_partition(b"nvme0n2", guid, || panic_record_reads_back(guid))
}

/// Two-byte characters, so a cut at a byte count would split one.
const WIDE_LINE: &str = "ééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééééé\n";

/// A report longer than it may be is cut short whole characters in, and the
/// log still has its heading.
#[inline(never)]
fn report_is_bounded(guid: Guid) -> TestResult {
    let store = match open(guid) {
        Ok(store) => store,
        Err(verdict) => return verdict,
    };
    let Some(mut record) = PanicRecord::begin(&store, PANIC) else {
        return fail!("the panic record's buffer is taken");
    };
    for _ in 0..REPORT_MAX / WIDE_LINE.len() + 2 {
        let _ = core::fmt::Write::write_str(&mut record, WIDE_LINE);
    }
    let written = match record.commit() {
        Ok(written) => written,
        Err(e) => return fail!("committing the record failed: {:?}", e),
    };
    let text = match read_all(&store, written.slot, written.sequence) {
        Ok(text) => text,
        Err(verdict) => return verdict,
    };
    let Ok(text) = core::str::from_utf8(&text) else {
        return fail!("a cut report must stay UTF-8");
    };
    let Some(heading) = text.find(LOG_HEADING) else {
        return fail!("a full report must leave the log its heading");
    };
    assert_test!(heading <= REPORT_MAX + 1, "the report stops at its share");
    pass!()
}

pub fn test_crash_panic_record_bounds_the_report() -> TestResult {
    let guid = Guid([0xE7; 16]);
    with_crash_partition(b"nvme1n2", guid, || report_is_bounded(guid))
}

slopos_testing::stest!(name = test_crash_store_on_512_byte_blocks, suite = crash);
slopos_testing::stest!(name = test_crash_store_on_4096_byte_blocks, suite = crash);
slopos_testing::stest!(name = test_crash_store_needs_a_panic_queue, suite = crash);
slopos_testing::stest!(name = test_crash_panic_record, suite = crash);
slopos_testing::stest!(
    name = test_crash_panic_record_bounds_the_report,
    suite = crash
);
