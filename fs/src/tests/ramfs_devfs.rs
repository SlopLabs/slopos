//! The bounds ramfs derives from the machine, the chunked file body those
//! bounds are honest about, and devfs's name lookup.

use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;
use slopos_ostd::{KArc, KVec, klog_info, lock_class};
use slopos_testing::{TestResult, fail};

use crate::blockdev::{BlockDevice, MemoryBlockDevice};
use crate::devfs::{devfs_block_device_by_name, devfs_register_block_device};
use crate::ramfs::{
    RAMFS_BYTES_PER_INODE, RAMFS_CHUNK_BATCH, RAMFS_FILE_CHUNK, RAMFS_MAX_INODES_CEILING,
    RAMFS_MEM_SHARE, RAMFS_MIN_FILE_SIZE, RAMFS_MIN_INODES, RamFs, derive_max_file_size,
    derive_max_inodes, probe::take_chunk_work, ramfs_max_file_size, ramfs_max_inodes,
};
use crate::vfs::{FileSystem, FileType, InodeId, VfsError};

/// Usable-memory figures spanning an unseeded allocator, a machine below the
/// floors, and machines the derivation has to answer for.
const USABLE_BYTES: [u64; 6] = [
    0,
    32 * 1024 * 1024,
    256 * 1024 * 1024,
    4 * 1024 * 1024 * 1024,
    64 * 1024 * 1024 * 1024,
    1024 * 1024 * 1024 * 1024,
];

/// A ramfs must never promise less than it used to, nor more than its share of
/// a machine that cannot swap any of it back out.
pub fn test_ramfs_limits_track_usable_memory() -> TestResult {
    let mut previous_size = 0usize;
    let mut previous_inodes = 0usize;

    for usable in USABLE_BYTES {
        let size = derive_max_file_size(usable);
        let inodes = derive_max_inodes(usable);

        if size < RAMFS_MIN_FILE_SIZE {
            return fail!("{} bytes usable gave a {}-byte file cap", usable, size);
        }
        if size > RAMFS_MIN_FILE_SIZE && size as u64 > usable / RAMFS_MEM_SHARE {
            return fail!(
                "{} bytes usable gave a {}-byte file cap, past a 1/{}th share",
                usable,
                size,
                RAMFS_MEM_SHARE
            );
        }
        if inodes < RAMFS_MIN_INODES || inodes > RAMFS_MAX_INODES_CEILING {
            return fail!(
                "{} bytes usable gave {} inodes, outside {}..={}",
                usable,
                inodes,
                RAMFS_MIN_INODES,
                RAMFS_MAX_INODES_CEILING
            );
        }
        if inodes > RAMFS_MIN_INODES && inodes as u64 > usable / RAMFS_BYTES_PER_INODE {
            return fail!(
                "{} bytes usable gave {} inodes, more than one per {} bytes",
                usable,
                inodes,
                RAMFS_BYTES_PER_INODE
            );
        }
        if size < previous_size || inodes < previous_inodes {
            return fail!(
                "{} bytes usable shrank the bounds to {}/{}",
                usable,
                size,
                inodes
            );
        }
        previous_size = size;
        previous_inodes = inodes;
    }

    // What the live filesystem enforces, derived from this boot's allocator.
    let size = ramfs_max_file_size();
    let inodes = ramfs_max_inodes();
    if size < RAMFS_MIN_FILE_SIZE {
        return fail!("this machine's file cap is {}", size);
    }
    if inodes < RAMFS_MIN_INODES || inodes > RAMFS_MAX_INODES_CEILING {
        return fail!("this machine's inode cap is {}", inodes);
    }
    klog_info!("RAMFS_TEST: file cap {} bytes, {} inodes", size, inodes);
    TestResult::Pass
}

/// Never mounted: these tests drive the filesystem's own operations, so what
/// they assert is the file body rather than the path walk above it.
static CHUNK_FS: RamFs = RamFs::new_const(lock_class!("RAMFS_CHUNK_TEST", LOCK_LEVEL_RESOURCE));

/// Past the 1 MiB a single allocation could hold, which is what a file's
/// bytes were before they were chunked.
const SPAN_BYTES: usize = 4 * 1024 * 1024;
/// Neither a multiple of the chunk size nor a divisor of it, so every write
/// but the first starts mid-chunk and every one ends mid-chunk.
const PIECE_BYTES: usize = 3 * RAMFS_FILE_CHUNK + 17;

/// The byte belonging at `offset`. The period is coprime with the chunk size,
/// so a byte read out of the wrong chunk cannot match by coincidence.
fn pattern(offset: usize) -> u8 {
    (offset % 251) as u8
}

fn fresh_file(name: &[u8]) -> Result<InodeId, VfsError> {
    let root = CHUNK_FS.root_inode();
    if CHUNK_FS.lookup(root, name).is_ok() {
        CHUNK_FS.unlink(root, name)?;
    }
    CHUNK_FS.create(root, name, FileType::Regular)
}

fn drop_file(name: &[u8]) {
    let root = CHUNK_FS.root_inode();
    let _ = CHUNK_FS.unlink(root, name);
}

/// A file four times the old ceiling, written and read back in pieces that
/// straddle chunk boundaries, plus a read across every boundary in it.
pub fn test_ramfs_file_spans_chunks() -> TestResult {
    let outcome = span_chunks();
    drop_file(b"span");
    outcome
}

fn span_chunks() -> TestResult {
    let inode = match fresh_file(b"span") {
        Ok(inode) => inode,
        Err(e) => return fail!("creating the file answered {:?}", e),
    };
    let Ok(mut piece) = KVec::<u8>::zeroed(PIECE_BYTES) else {
        return TestResult::Skipped;
    };

    // Each phase is its own frame: every `fail!` site carries a formatting
    // buffer, and the `.stack_sizes` gate bounds one frame at 2 KiB.
    match fill_span(inode, piece.as_mut_slice()) {
        TestResult::Pass => {}
        other => return other,
    }
    match verify_span(inode, piece.as_mut_slice()) {
        TestResult::Pass => {}
        other => return other,
    }
    match verify_boundaries(inode) {
        TestResult::Pass => {}
        other => return other,
    }

    klog_info!(
        "RAMFS_TEST: {} bytes in {}-byte chunks read back exact",
        SPAN_BYTES,
        RAMFS_FILE_CHUNK
    );
    TestResult::Pass
}

#[inline(never)]
fn fill_span(inode: InodeId, piece: &mut [u8]) -> TestResult {
    let mut written = 0usize;
    while written < SPAN_BYTES {
        let n = PIECE_BYTES.min(SPAN_BYTES - written);
        let buf = &mut piece[..n];
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = pattern(written + i);
        }
        match CHUNK_FS.write(inode, written as u64, buf) {
            Ok(got) if got == n => {}
            other => return fail!("writing {} bytes at {} answered {:?}", n, written, other),
        }
        written += n;
    }

    match CHUNK_FS.stat(inode) {
        Ok(stat) if stat.size == SPAN_BYTES as u64 => TestResult::Pass,
        Ok(stat) => fail!(
            "{} bytes written reported a size of {}",
            SPAN_BYTES,
            stat.size
        ),
        Err(e) => fail!("stat answered {:?}", e),
    }
}

/// Reads one byte short of a chunk at a time, so successive reads start at
/// every offset within a chunk rather than repeating one alignment.
#[inline(never)]
fn verify_span(inode: InodeId, piece: &mut [u8]) -> TestResult {
    let step = RAMFS_FILE_CHUNK - 1;
    let mut read = 0usize;
    while read < SPAN_BYTES {
        let n = step.min(SPAN_BYTES - read);
        let buf = &mut piece[..n];
        buf.fill(0xFF);
        match CHUNK_FS.read(inode, read as u64, buf) {
            Ok(got) if got == n => {}
            other => return fail!("reading {} bytes at {} answered {:?}", n, read, other),
        }
        for (i, &byte) in buf.iter().enumerate() {
            if byte != pattern(read + i) {
                return fail!("offset {} read back {:#x}", read + i, byte);
            }
        }
        read += n;
    }
    TestResult::Pass
}

#[inline(never)]
fn verify_boundaries(inode: InodeId) -> TestResult {
    let mut boundary = RAMFS_FILE_CHUNK;
    while boundary < SPAN_BYTES {
        let mut pair = [0xFFu8; 2];
        match CHUNK_FS.read(inode, (boundary - 1) as u64, &mut pair) {
            Ok(2) => {}
            other => return fail!("a read across boundary {} answered {:?}", boundary, other),
        }
        if pair[0] != pattern(boundary - 1) || pair[1] != pattern(boundary) {
            return fail!(
                "boundary {} read back {:#x},{:#x}",
                boundary,
                pair[0],
                pair[1]
            );
        }
        boundary += RAMFS_FILE_CHUNK;
    }
    TestResult::Pass
}

/// A write past the end reads back as zeros before it, including across a
/// chunk a shrink left in place after it held other bytes.
pub fn test_ramfs_write_past_end_reads_zeros() -> TestResult {
    let outcome = write_past_end();
    drop_file(b"sparse");
    outcome
}

const GAP: usize = 2 * RAMFS_FILE_CHUNK + 100;
/// The shrink leaves a chunk in place holding bytes past this point, so the
/// gap the later write opens runs over storage that is not freshly allocated.
const KEPT: usize = 100;

fn write_past_end() -> TestResult {
    let inode = match fresh_file(b"sparse") {
        Ok(inode) => inode,
        Err(e) => return fail!("creating the file answered {:?}", e),
    };
    let Ok(mut buf) = KVec::<u8>::zeroed(GAP + 8) else {
        return TestResult::Skipped;
    };

    buf.as_mut_slice().fill(0xCD);
    if let Err(e) = CHUNK_FS.write(inode, 0, buf.as_slice()) {
        return fail!("the first write answered {:?}", e);
    }
    if let Err(e) = CHUNK_FS.truncate(inode, KEPT as u64) {
        return fail!("truncating down answered {:?}", e);
    }

    let tail = [0xABu8; 8];
    match CHUNK_FS.write(inode, GAP as u64, &tail) {
        Ok(8) => {}
        other => return fail!("the write at {} answered {:?}", GAP, other),
    }
    match CHUNK_FS.stat(inode) {
        Ok(stat) if stat.size == (GAP + 8) as u64 => {}
        Ok(stat) => return fail!("a write ending at {} reported size {}", GAP + 8, stat.size),
        Err(e) => return fail!("stat answered {:?}", e),
    }

    buf.as_mut_slice().fill(0xFF);
    match CHUNK_FS.read(inode, 0, buf.as_mut_slice()) {
        Ok(got) if got == GAP + 8 => {}
        other => return fail!("reading the whole file answered {:?}", other),
    }
    if buf.as_slice()[..KEPT].iter().any(|&b| b != 0xCD) {
        return fail!("the bytes the truncate kept did not read back");
    }
    if let Some(offset) = buf.as_slice()[KEPT..GAP].iter().position(|&b| b != 0) {
        return fail!(
            "offset {} in the gap read back {:#x}",
            KEPT + offset,
            buf.as_slice()[KEPT + offset]
        );
    }
    if buf.as_slice()[GAP..] != tail {
        return fail!("the bytes actually written did not read back");
    }
    TestResult::Pass
}

/// Two megabytes, which is two megabytes of chunks until a truncate down
/// gives them back.
const SHRINK_BYTES: usize = 2 * 1024 * 1024;
const SHRINK_KEEP: usize = 1000;
const SHRINK_REGROW: usize = 3 * RAMFS_FILE_CHUNK + 7;

/// Shrinking releases chunks, and growing back into the released range reads
/// as zeros rather than as what those chunks held.
pub fn test_ramfs_shrink_releases_chunks() -> TestResult {
    let outcome = shrink_releases();
    drop_file(b"shrink");
    outcome
}

fn shrink_releases() -> TestResult {
    let baseline = CHUNK_FS.resident_bytes();
    let inode = match fresh_file(b"shrink") {
        Ok(inode) => inode,
        Err(e) => return fail!("creating the file answered {:?}", e),
    };
    let Ok(mut piece) = KVec::<u8>::zeroed(PIECE_BYTES) else {
        return TestResult::Skipped;
    };
    piece.as_mut_slice().fill(0xCD);

    let mut written = 0usize;
    while written < SHRINK_BYTES {
        let n = PIECE_BYTES.min(SHRINK_BYTES - written);
        match CHUNK_FS.write(inode, written as u64, &piece.as_slice()[..n]) {
            Ok(got) if got == n => {}
            other => return fail!("writing {} bytes at {} answered {:?}", n, written, other),
        }
        written += n;
    }

    let grown = CHUNK_FS.resident_bytes();
    if grown != baseline + SHRINK_BYTES {
        return fail!(
            "{} bytes of file hold {} bytes of chunks, not {}",
            SHRINK_BYTES,
            grown - baseline,
            SHRINK_BYTES
        );
    }

    if let Err(e) = CHUNK_FS.truncate(inode, SHRINK_KEEP as u64) {
        return fail!("truncating down answered {:?}", e);
    }
    let shrunk = CHUNK_FS.resident_bytes();
    if shrunk != baseline + RAMFS_FILE_CHUNK {
        return fail!(
            "a {}-byte file holds {} bytes of chunks, not {}",
            SHRINK_KEEP,
            shrunk - baseline,
            RAMFS_FILE_CHUNK
        );
    }

    if let Err(e) = CHUNK_FS.truncate(inode, SHRINK_REGROW as u64) {
        return fail!("truncating back up answered {:?}", e);
    }
    let Ok(mut buf) = KVec::<u8>::zeroed(SHRINK_REGROW) else {
        return TestResult::Skipped;
    };
    buf.as_mut_slice().fill(0xFF);
    match CHUNK_FS.read(inode, 0, buf.as_mut_slice()) {
        Ok(got) if got == SHRINK_REGROW => {}
        other => return fail!("reading the regrown file answered {:?}", other),
    }
    if buf.as_slice()[..SHRINK_KEEP].iter().any(|&b| b != 0xCD) {
        return fail!("the bytes the truncate kept did not survive it");
    }
    if let Some(offset) = buf.as_slice()[SHRINK_KEEP..].iter().position(|&b| b != 0) {
        return fail!(
            "offset {} past the old end read back {:#x}",
            SHRINK_KEEP + offset,
            buf.as_slice()[SHRINK_KEEP + offset]
        );
    }

    drop_file(b"shrink");
    let released = CHUNK_FS.resident_bytes();
    if released != baseline {
        return fail!(
            "unlinking left {} bytes of chunks behind",
            released - baseline
        );
    }
    TestResult::Pass
}

/// The derived cap is still what a write is refused at, and the refusal is
/// still `NoSpace`.
pub fn test_ramfs_write_past_cap_refused() -> TestResult {
    let outcome = past_cap_refused();
    drop_file(b"cap");
    outcome
}

fn past_cap_refused() -> TestResult {
    let cap = ramfs_max_file_size();
    let inode = match fresh_file(b"cap") {
        Ok(inode) => inode,
        Err(e) => return fail!("creating the file answered {:?}", e),
    };

    for offset in [cap as u64, cap as u64 - 1, u64::MAX] {
        match CHUNK_FS.write(inode, offset, &[0xEF; 2]) {
            Err(VfsError::NoSpace) => {}
            other => {
                return fail!(
                    "a write ending past the {}-byte cap at offset {} answered {:?}",
                    cap,
                    offset,
                    other
                );
            }
        }
    }

    // The refusal costs the file nothing: it still holds what it held.
    match CHUNK_FS.stat(inode) {
        Ok(stat) if stat.size == 0 => TestResult::Pass,
        Ok(stat) => fail!("a refused write left the size at {}", stat.size),
        Err(e) => fail!("stat answered {:?}", e),
    }
}

/// Sixteen batches' worth, which the single-hold grow would have allocated
/// and zeroed with interrupts off.
const HOLD_CHUNKS: usize = 16 * RAMFS_CHUNK_BATCH;

/// A grow allocates its chunks with no filesystem lock held, and no hold
/// installs or gives back more than one batch. `SpinLock::lock` disables
/// interrupts, so a chunk allocated under it is allocated interrupts-off —
/// and a file at the derived ceiling is 131072 chunks.
pub fn test_ramfs_chunk_work_stays_off_the_lock() -> TestResult {
    let outcome = chunk_work_off_lock();
    drop_file(b"holds");
    outcome
}

fn chunk_work_off_lock() -> TestResult {
    let baseline = CHUNK_FS.resident_bytes();
    let inode = match fresh_file(b"holds") {
        Ok(inode) => inode,
        Err(e) => return fail!("creating the file answered {:?}", e),
    };
    // Creating the file is not what is being measured.
    let _ = take_chunk_work();

    let grown = HOLD_CHUNKS * RAMFS_FILE_CHUNK;
    if let Err(e) = CHUNK_FS.truncate(inode, grown as u64) {
        return fail!("growing to {} bytes answered {:?}", grown, e);
    }
    let grow = take_chunk_work();
    if grow.allocs < HOLD_CHUNKS {
        return fail!(
            "a {}-chunk grow allocated {} chunks",
            HOLD_CHUNKS,
            grow.allocs
        );
    }
    if grow.allocs_irq_off != 0 {
        return fail!(
            "{} of {} chunk allocations ran with interrupts disabled",
            grow.allocs_irq_off,
            grow.allocs
        );
    }
    if grow.most_per_hold > RAMFS_CHUNK_BATCH {
        return fail!(
            "one hold installed {} chunks, past the bound of {}",
            grow.most_per_hold,
            RAMFS_CHUNK_BATCH
        );
    }
    match CHUNK_FS.stat(inode) {
        Ok(stat) if stat.size == grown as u64 => {}
        Ok(stat) => return fail!("a grow to {} reported size {}", grown, stat.size),
        Err(e) => return fail!("stat answered {:?}", e),
    }
    if CHUNK_FS.resident_bytes() != baseline + grown {
        return fail!(
            "a {}-byte file holds {} bytes of chunks",
            grown,
            CHUNK_FS.resident_bytes() - baseline
        );
    }

    // The release is bounded the same way, and gives every chunk back.
    if let Err(e) = CHUNK_FS.truncate(inode, 0) {
        return fail!("truncating to zero answered {:?}", e);
    }
    let shrink = take_chunk_work();
    if shrink.most_per_hold > RAMFS_CHUNK_BATCH {
        return fail!(
            "one hold gave back {} chunks, past the bound of {}",
            shrink.most_per_hold,
            RAMFS_CHUNK_BATCH
        );
    }
    if CHUNK_FS.resident_bytes() != baseline {
        return fail!(
            "truncating to zero left {} bytes of chunks",
            CHUNK_FS.resident_bytes() - baseline
        );
    }

    klog_info!(
        "RAMFS_TEST: {} chunks allocated with interrupts on, at most {} per hold",
        grow.allocs,
        grow.most_per_hold.max(shrink.most_per_hold)
    );
    TestResult::Pass
}

const LOOKUP_DEVICE_BYTES: usize = 4096;

/// `mount(2)` names a disk with whatever the source argument carried, and gets
/// back a handle it can reach the driver through.
pub fn test_devfs_block_device_lookup_by_name() -> TestResult {
    let Some(memory) = MemoryBlockDevice::allocate(LOOKUP_DEVICE_BYTES) else {
        return TestResult::Skipped;
    };
    let Ok(memory) = KArc::try_new(memory) else {
        return TestResult::Skipped;
    };
    let device: KArc<dyn BlockDevice + Send + Sync> = memory;

    match devfs_register_block_device(b"lookuptest0", device) {
        Ok(_) => {}
        // A rerun of the same boot's suite finds the name already taken; the
        // node it left behind is this same device.
        Err(VfsError::AlreadyExists) => {}
        Err(e) => return fail!("registration failed: {:?}", e),
    }

    let Some(bare) = devfs_block_device_by_name("lookuptest0") else {
        return fail!("the bare name resolved to nothing");
    };
    let Some(path) = devfs_block_device_by_name("/dev/lookuptest0") else {
        return fail!("the /dev/ spelling resolved to nothing");
    };
    if devfs_block_device_by_name("lookuptest_absent").is_some() {
        return fail!("an unregistered name resolved");
    }
    if devfs_block_device_by_name("/dev/").is_some() || devfs_block_device_by_name("").is_some() {
        return fail!("an empty name resolved");
    }

    // Reaching the device takes its own lock: an answer here is proof the
    // registry lock was released and the handle is the caller's own.
    if bare.capacity() != LOOKUP_DEVICE_BYTES as u64 {
        return fail!("capacity {} through the bare name", bare.capacity());
    }
    if path.capacity() != bare.capacity() {
        return fail!("the two spellings named different devices");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_ramfs_limits_track_usable_memory, suite = fs);
slopos_testing::stest!(name = test_ramfs_file_spans_chunks, suite = fs);
slopos_testing::stest!(name = test_ramfs_write_past_end_reads_zeros, suite = fs);
slopos_testing::stest!(name = test_ramfs_shrink_releases_chunks, suite = fs);
slopos_testing::stest!(name = test_ramfs_write_past_cap_refused, suite = fs);
slopos_testing::stest!(name = test_ramfs_chunk_work_stays_off_the_lock, suite = fs);
slopos_testing::stest!(name = test_devfs_block_device_lookup_by_name, suite = fs);
