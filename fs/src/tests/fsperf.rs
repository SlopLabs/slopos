//! The measured cost of a write, and of mounting a large volume.
//!
//! Two report lines, graded by `scripts/check_fs_throughput.sh`:
//!
//! ```text
//! FSPERF[tests]: bytes=N txns=N commits=N devwrites=N devblocks=N barriers=N ns=N rawbytes=N rawns=N
//! FSCAP[capacity]: blocks=N blocksize=N groups=N cacheentries=N mountreads=N mountns=N dirents=N lookupreads=N bytes=N ns=N files=N treebytes=N
//! ```
//!
//! The counts per MiB are properties of the code; the rate is not, which is
//! why the filesystem write is paired with a raw block-device write of the
//! same byte count in the same run. Their quotient describes the filesystem
//! rather than the host.
//!
//! The test phase runs before the boot's `rootfs`/`fs` steps, so `/` is still
//! a ramfs and no `/dev` node exists: both tests claim their device, mount it
//! themselves, and give the claim back to the boot step that follows.
//!
//! `slopos_fs::tests::fsperf` sorts after `slopos_fs::tests` and the registry
//! runs in `(module, name)` order, so `test_ext2_aaa_init` has brought the VFS
//! up before either of these.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::fs::{
    FS_TYPE_DIRECTORY, FS_TYPE_FILE, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY, UserFsEntry,
};
use slopos_abi::io::{IO_FILE_BATCH_SIZE, KernelIoBuf, KernelIoBufRef};
use slopos_kernel_services::clock::monotonic_ns;
use slopos_ostd::{KBox, KVec, klog_info};
use slopos_testing::{TestResult, fail};

use crate::blockdev::{BlockDevice, BlockDeviceError, stats, total_seg_len};
use crate::fileio::{
    FdTable, file_close_fd, file_open_at, file_read_fd, file_sync_fd, file_write_fd,
};
use crate::vfs::path::RESOLVE_FOLLOW;
use crate::vfs::{
    ListCursor, MOUNT_RDONLY, VfsError, VfsResult, mount, vfs_claim_block_device,
    vfs_ext2_mount_named, vfs_ext2_pool_claim, vfs_ext2_pool_release, vfs_ext2_unmount_named,
    vfs_list_from, vfs_mkdir, vfs_rmdir, vfs_stat, vfs_unlink,
};

/// Bytes each half of the report moves — the same count in both, or their
/// quotient means nothing. The reference write has to land on a real device,
/// and 2 MiB at [`RAW_OFFSET`] is the largest window of the scratch disk that
/// disturbs no other user of it.
const PERF_BYTES: usize = 2 * 1024 * 1024;

/// One filesystem call per chunk, which is what one transaction is: a larger
/// buffer would be split into these anyway and a smaller one would measure
/// the staging rather than the filesystem.
const PERF_CHUNK: usize = IO_FILE_BATCH_SIZE;

/// Byte 5 MiB of `disk1`: clear of the raw-sector tests (sectors 0..2,
/// 64..255, 2048, 3072, 4000, 4608, 5120, 5632 and 8192) and of the GPT
/// backup header in the last sector.
const RAW_OFFSET: u64 = 5 * 1024 * 1024;

/// `disk0`, the ext2 image this boot came from: a real filesystem on a real
/// device, which is what the report is about.
const PERF_DEVICE: &[u8] = b"vda";
const PERF_MOUNT: &[u8] = b"/fsperfroot";
const PERF_PATH: &[u8] = b"/fsperfroot/fsperf.dat";
const RAW_DEVICE: &[u8] = b"vdb";

/// Transactions per MiB this write is allowed. Staging 4 KiB per filesystem
/// call — the shape this replaced — is 256 of them; one 256 KiB batch per
/// transaction is 4.
const MAX_TXNS_PER_MIB: u64 = 32;

/// Names the capacity report puts in one directory: past the point where the
/// first-fit insert it replaced was quadratic.
const CAP_DIRENTS: u32 = 4000;
const CAP_BYTES: usize = 4 * 1024 * 1024;
const CAP_DEVICE: &[u8] = b"vdd";
const CAP_MOUNT: &[u8] = b"/fsperfcap";

/// The subtrees `_fs-image-capacity` populates the volume with: a checked-out
/// copy of this repository and the pinned toolchain sysroot. The sysroot half
/// is absent on a host without rustup, which the floors below allow for.
const CAP_TREES: [&[u8]; 2] = [b"repo", b"sysroot"];

/// Files read back so the report describes a tree that is genuinely readable
/// and not merely a set of names. Pinned by name, never by content.
const CAP_VERIFY: [&[u8]; 2] = [b"repo/Cargo.toml", b"repo/rust-toolchain.toml"];

/// Bytes read back from each of them.
const CAP_VERIFY_BYTES: usize = 128;

/// The walk's budget over a staged tree of 1207 directories, a longest path of
/// 114 bytes and 78 directories pending at its deepest: reaching one of these
/// fails rather than reporting a partial tree. The work list is reserved up
/// front because a `KVec` grown an entry at a time frees as it goes.
const WALK_MAX_DIRS: usize = 4096;
const WALK_PATH_MAX: usize = 128;
const WALK_PENDING_RESERVE: usize = 256;

/// Entries one listing call answers. `UserFsEntry` is 272 bytes, so the page
/// is heap-allocated; the count trades round trips against that allocation
/// and bounds no directory, since the cursor pages the rest.
const WALK_PAGE: usize = 32;

/// Floors on what the walk must find: a staging step that silently produced
/// nothing looks exactly like a tree that got cheap to walk. Under the repo's
/// own ~1500 files, since the sysroot half needs rustup on the host.
const WALK_MIN_FILES: u64 = 1000;
const WALK_MIN_BYTES: u64 = 8 * 1024 * 1024;

/// A `KVec` rather than a stack array: `PERF_CHUNK` is 256 KiB.
#[inline(never)]
fn payload() -> Option<KVec<u8>> {
    let mut buf = KVec::<u8>::zeroed(PERF_CHUNK).ok()?;
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8) ^ 0x5A;
    }
    Some(buf)
}

fn ensure_dir(target: &[u8]) -> bool {
    vfs_mkdir(target).is_ok() || vfs_stat(target).is_ok()
}

/// Write `bytes` to `path` through the descriptor path a `write(2)` takes,
/// then `fsync` it. Answers the bytes written, or a negative errno.
#[inline(never)]
fn write_through_vfs(table: FdTable, path: &[u8], bytes: usize, chunk: &[u8]) -> isize {
    let fd = file_open_at(
        table,
        path,
        b"/",
        O_WRONLY | O_CREAT | O_TRUNC,
        RESOLVE_FOLLOW,
        Some(0o644),
    );
    if fd < 0 {
        return fd as isize;
    }
    let mut done = 0usize;
    while done < bytes {
        let want = (bytes - done).min(chunk.len());
        let source = KernelIoBufRef::new(&chunk[..want]);
        let wrote = file_write_fd(table, fd, &source);
        if wrote <= 0 {
            let _ = file_close_fd(table, fd);
            return wrote;
        }
        done += wrote as usize;
    }
    let rc = file_sync_fd(table, fd, false);
    let _ = file_close_fd(table, fd);
    if rc < 0 {
        return rc as isize;
    }
    done as isize
}

/// The accelerator-invariant reference: the same byte count straight at a
/// block device, with no filesystem anywhere in it. Answers the nanoseconds.
#[inline(never)]
fn raw_reference(chunk: &[u8]) -> Result<u64, &'static str> {
    let device = match vfs_claim_block_device(RAW_DEVICE) {
        Ok(device) => device,
        Err(e) => {
            klog_info!(
                "FSPERF: claiming the scratch device for the reference write failed: {:?}",
                e
            );
            return Err("could not claim the scratch device (disk1)");
        }
    };
    if device.capacity() < RAW_OFFSET + PERF_BYTES as u64 {
        return Err("the scratch device is too small for the reference window");
    }
    let start = monotonic_ns();
    let mut done = 0usize;
    while done < PERF_BYTES {
        let want = (PERF_BYTES - done).min(chunk.len());
        device
            .write_at(RAW_OFFSET + done as u64, &chunk[..want])
            .map_err(|_| "the reference write failed")?;
        done += want;
    }
    device.flush().map_err(|_| "the reference flush failed")?;
    Ok(monotonic_ns().saturating_sub(start))
}

/// Recorded, not logged: the line itself is emitted at the phase boundary by
/// [`crate::fsreport::fs_cost_report`], because a passing test's klog is not
/// on the wire at the default verbosity.
#[allow(clippy::too_many_arguments)]
fn emit_perf(
    bytes: u64,
    txns: u64,
    commits: u64,
    devwrites: u64,
    devblocks: u64,
    barriers: u64,
    ns: u64,
    rawbytes: u64,
    rawns: u64,
) {
    crate::fsreport::record_write_cost(&crate::fsreport::WriteCost {
        bytes,
        txns,
        commits,
        devwrites,
        devblocks,
        barriers,
        ns,
        rawbytes,
        rawns,
    });
}

/// Counts what the measured mount carries, scoped to this one device because
/// [`stats`] is process-global and the measured write wakes the ext2 flusher,
/// which syncs every bound instance inside the window. Transactions and
/// commits stay global: no device handle sees them, and this is the only mount.
struct CountedDevice {
    inner: KBox<dyn BlockDevice + Send + Sync>,
}

/// One `CountedDevice` is alive at a time — [`perf_mount`] builds it and the
/// unmount drops it — so the window needs no shared allocation to read.
static DEV_WRITES: AtomicU64 = AtomicU64::new(0);
static DEV_SECTORS: AtomicU64 = AtomicU64::new(0);
static DEV_BARRIERS: AtomicU64 = AtomicU64::new(0);

fn dev_reset() {
    DEV_WRITES.store(0, Ordering::Relaxed);
    DEV_SECTORS.store(0, Ordering::Relaxed);
    DEV_BARRIERS.store(0, Ordering::Relaxed);
}

/// Requests, sectors, barriers.
fn dev_counts() -> (u64, u64, u64) {
    (
        DEV_WRITES.load(Ordering::Relaxed),
        DEV_SECTORS.load(Ordering::Relaxed),
        DEV_BARRIERS.load(Ordering::Relaxed),
    )
}

fn note_dev_write(bytes: usize) {
    DEV_WRITES.fetch_add(1, Ordering::Relaxed);
    DEV_SECTORS.fetch_add(
        bytes.div_ceil(stats::SECTOR_BYTES) as u64,
        Ordering::Relaxed,
    );
}

impl BlockDevice for CountedDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        note_dev_write(buffer.len());
        self.inner.write_at(offset, buffer)
    }

    /// Forwarded rather than left to the trait default, which would split one
    /// gathered write into a request per segment, in the count and at the device.
    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        note_dev_write(total_seg_len(segs).unwrap_or(0));
        self.inner.write_vectored(offset, segs)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn write_protected(&self) -> bool {
        self.inner.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        DEV_BARRIERS.fetch_add(1, Ordering::Relaxed);
        self.inner.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.inner.checkpoint()
    }
}

/// What [`vfs_ext2_mount_named`] does for `PERF_DEVICE`, with the claimed
/// handle wrapped in a [`CountedDevice`] before the instance takes it.
#[inline(never)]
fn perf_mount() -> VfsResult<()> {
    // The pool slot before the device: taking a slot is what sweeps a retired
    // instance, and a retired instance still holds the claim on its device.
    let fs = vfs_ext2_pool_claim().ok_or(VfsError::NoSpace)?;
    let device = match vfs_claim_block_device(PERF_DEVICE) {
        Ok(device) => device,
        Err(e) => {
            vfs_ext2_pool_release(fs, false);
            return Err(e);
        }
    };
    let counted = match KBox::try_new(CountedDevice { inner: device }) {
        Ok(counted) => counted,
        Err(_) => {
            vfs_ext2_pool_release(fs, false);
            return Err(VfsError::IoError);
        }
    };
    let info = match fs.attach(counted, false) {
        Ok(info) => info,
        Err(e) => {
            vfs_ext2_pool_release(fs, false);
            return Err(e);
        }
    };
    let flags = if info.read_only { MOUNT_RDONLY } else { 0 };
    if let Err(e) = mount(PERF_MOUNT, fs, flags) {
        vfs_ext2_pool_release(fs, false);
        return Err(e);
    }
    Ok(())
}

/// What `PERF_BYTES` through the filesystem costs, against what the same
/// bytes cost at the device underneath it.
pub fn test_fsperf_capacity_write_cost() -> TestResult {
    if !ensure_dir(PERF_MOUNT) {
        return fail!("could not create the mount point for the ext2 image");
    }
    if let Err(e) = perf_mount() {
        return fail!("could not mount the ext2 image on disk0: {:?}", e);
    }
    let outcome = perf_body();
    if let Err(e) = vfs_ext2_unmount_named(PERF_MOUNT) {
        // The boot's own `fs init` step claims disk0 after the tests; a claim
        // this test kept would take the machine's root filesystem with it.
        return fail!("could not unmount the ext2 image: {:?}", e);
    }
    let _ = vfs_rmdir(PERF_MOUNT);
    outcome
}

/// Its own frame, between the mount and the unmount.
#[inline(never)]
fn perf_body() -> TestResult {
    let Some(chunk) = payload() else {
        return fail!("could not allocate the {}-byte payload", PERF_CHUNK);
    };
    let Some(process) = super::ScratchProcess::new() else {
        return fail!("could not register a process to own the descriptor");
    };
    let table = process.table();

    // Quiesce first, then open the window. Anything still dirty when the
    // counters are reset — the mount's own metadata, whatever the boot left —
    // is written back by the flusher *during* the measured write and counted
    // against it, which is how a row that is a property of the code became a
    // property of how much the image happened to be carrying: measured 22
    // device requests per MiB on one tree and 22-33 on the same code with a
    // larger image behind it.
    if let Err(e) = crate::vfs::vfs_sync_all() {
        return fail!("could not quiesce the filesystem before measuring: {:?}", e);
    }
    stats::reset();
    dev_reset();
    let start = monotonic_ns();
    let wrote = write_through_vfs(table, PERF_PATH, PERF_BYTES, chunk.as_slice());
    // And close it at the far end too, so the number is the *whole* cost of
    // getting 2 MiB onto the medium rather than however much of it happened to
    // have landed when the counters were read. The write already fdatasyncs;
    // what this absorbs is the flusher's own pass over the metadata the write
    // dirtied, which otherwise falls inside or outside the window depending on
    // where the flusher's idle timer happens to be — the same block written
    // once either way, counted or not depending on the clock.
    if let Err(e) = crate::vfs::vfs_sync_all() {
        return fail!("could not flush the filesystem after measuring: {:?}", e);
    }
    let ns = monotonic_ns().saturating_sub(start);
    let counters = stats::snapshot();
    let (devwrites, devblocks, barriers) = dev_counts();

    // Before any assertion: a second boot of this image must start where the
    // first one did.
    let removed = vfs_unlink(PERF_PATH);

    if wrote != PERF_BYTES as isize {
        return fail!("writing {} bytes to ext2 returned {}", PERF_BYTES, wrote);
    }
    if let Err(e) = removed {
        return fail!("could not remove the measurement file: {:?}", e);
    }

    let rawns = match raw_reference(chunk.as_slice()) {
        Ok(ns) => ns,
        Err(why) => return fail!("{}", why),
    };

    emit_perf(
        PERF_BYTES as u64,
        counters.transactions,
        counters.commits,
        devwrites,
        devblocks,
        barriers,
        ns,
        PERF_BYTES as u64,
        rawns,
    );

    let per_mib = counters
        .transactions
        .saturating_mul(1024 * 1024)
        .div_ceil(PERF_BYTES as u64);
    if per_mib > MAX_TXNS_PER_MIB {
        return fail!(
            "{} transactions for {} bytes is {} per MiB, above the {} a batched write must stay \
             under (one per 4 KiB, which this replaced, would be 256)",
            counters.transactions,
            PERF_BYTES,
            per_mib,
            MAX_TXNS_PER_MIB
        );
    }
    if devwrites == 0 {
        return fail!("the write reached the device not once — nothing was measured");
    }
    TestResult::Pass
}

#[allow(clippy::too_many_arguments)]
fn emit_cap(
    blocks: u64,
    blocksize: u32,
    groups: u32,
    cacheentries: usize,
    mountreads: u64,
    mountns: u64,
    dirents: u32,
    lookupreads: u64,
    bytes: u64,
    ns: u64,
    files: u64,
    treebytes: u64,
) {
    crate::fsreport::record_capacity_cost(&crate::fsreport::CapacityCost {
        blocks,
        blocksize,
        groups,
        cacheentries: cacheentries as u64,
        mountreads,
        mountns,
        dirents: u64::from(dirents),
        lookupreads,
        bytes,
        ns,
        files,
        treebytes,
    });
}

/// Volume geometry, read off the device's own superblock. The claim is taken
/// and released here so the mount below can take its own, and in its own
/// frame: `mount_params` stages a whole block.
#[inline(never)]
fn cap_geometry() -> Option<(u64, u32, u32, usize)> {
    let device = match vfs_claim_block_device(CAP_DEVICE) {
        Ok(device) => device,
        Err(e) => {
            klog_info!("FSPERF: the capacity device is not claimable: {:?}", e);
            return None;
        }
    };
    let geometry = super::mount_geometry(&*device).ok()?;
    Some((
        geometry.blocks_count() as u64,
        geometry.block_size(),
        geometry.groups_count(),
        crate::ext2::cache::cache_entries_for(
            geometry.blocks_count() as u64,
            geometry.blocks_per_group(),
        ),
    ))
}

/// Entry `i`'s name: `n` plus seven digits, so every record is the same size
/// and the last name is no cheaper to find than the first.
fn cap_name(path: &mut [u8], base: usize, i: u32) -> usize {
    path[base] = b'n';
    let mut n = i;
    for k in 0..7 {
        path[base + 7 - k] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    base + 8
}

/// Create `CAP_DIRENTS` names in one directory, answering the path length of
/// the last of them.
#[inline(never)]
fn cap_fill(table: FdTable, path: &mut [u8], base: usize) -> Result<usize, &'static str> {
    let mut last = base;
    for i in 0..CAP_DIRENTS {
        last = cap_name(path, base, i);
        let fd = file_open_at(
            table,
            &path[..last],
            b"/",
            O_WRONLY | O_CREAT,
            RESOLVE_FOLLOW,
            Some(0o644),
        );
        if fd < 0 {
            return Err("a create in the large directory failed");
        }
        let _ = file_close_fd(table, fd);
    }
    Ok(last)
}

/// One pending directory of the walk. A fixed slot, so the work list is one
/// heap object and a path is never borrowed from the entry that named it.
#[derive(Copy, Clone)]
struct WalkDir {
    len: u16,
    bytes: [u8; WALK_PATH_MAX],
}

impl WalkDir {
    const EMPTY: Self = Self {
        len: 0,
        bytes: [0; WALK_PATH_MAX],
    };
}

fn walk_entry_name(entry: &UserFsEntry) -> &[u8] {
    let end = entry
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(entry.name.len());
    &entry.name[..end]
}

/// Put `<dir>/<name>` on the work list. Its own frame: the slot is 130 bytes,
/// which the walk's frame must not carry on top of its path buffer.
#[inline(never)]
fn walk_push(work: &mut KVec<WalkDir>, dir: &[u8], name: &[u8]) -> Result<(), &'static str> {
    let len = dir.len() + 1 + name.len();
    if len > WALK_PATH_MAX {
        return Err("a path in the staged tree is longer than the walk's path buffer");
    }
    if work.len() >= WALK_MAX_DIRS {
        return Err("the staged tree has more directories pending than the walk's budget");
    }
    let mut slot = WalkDir::EMPTY;
    slot.bytes[..dir.len()].copy_from_slice(dir);
    slot.bytes[dir.len()] = b'/';
    slot.bytes[dir.len() + 1..len].copy_from_slice(name);
    slot.len = len as u16;
    work.push(slot)
        .map_err(|_| "the walk's work list did not fit in the heap")
}

/// What the volume holds: regular files and their summed sizes under
/// [`CAP_TREES`], with the directory count the walk paid for. An explicit work
/// list and one shared path buffer, because a frame per level over a 15-level
/// tree is how a 2 KiB stack budget gets spent.
#[inline(never)]
fn cap_walk() -> Result<(u64, u64, usize), &'static str> {
    let mut work = KVec::<WalkDir>::with_capacity(WALK_PENDING_RESERVE)
        .map_err(|_| "the walk's work list did not fit in the heap")?;
    let mut entries = KVec::filled(UserFsEntry::new(), WALK_PAGE)
        .map_err(|_| "the listing page did not fit in the heap")?;
    let mut path = [0u8; WALK_PATH_MAX];
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut dirs = 0usize;

    // An absent subtree is skipped rather than fatal: the sysroot half needs
    // rustup on the host. An empty volume fails the read-back instead.
    for tree in CAP_TREES {
        let len = CAP_MOUNT.len() + 1 + tree.len();
        path[..CAP_MOUNT.len()].copy_from_slice(CAP_MOUNT);
        path[CAP_MOUNT.len()] = b'/';
        path[CAP_MOUNT.len() + 1..len].copy_from_slice(tree);
        if vfs_stat(&path[..len]).is_err() {
            continue;
        }
        walk_push(&mut work, CAP_MOUNT, tree)?;
    }

    while !work.is_empty() {
        let last = work.len() - 1;
        let dlen = work.as_slice()[last].len as usize;
        path[..dlen].copy_from_slice(&work.as_slice()[last].bytes[..dlen]);
        work.truncate(last);
        dirs += 1;
        if dirs > WALK_MAX_DIRS {
            return Err("the staged tree has more directories than the walk's budget");
        }

        let mut cursor = ListCursor::start();
        while !cursor.is_end() {
            let n = vfs_list_from(&path[..dlen], entries.as_mut_slice(), &mut cursor)
                .map_err(|_| "listing a directory of the staged tree failed")?;
            if n == 0 {
                break;
            }
            for entry in entries.as_slice().iter().take(n) {
                let name = walk_entry_name(entry);
                if name.is_empty() || name == b"." || name == b".." {
                    continue;
                }
                if entry.type_ == FS_TYPE_DIRECTORY {
                    walk_push(&mut work, &path[..dlen], name)?;
                } else if entry.type_ == FS_TYPE_FILE {
                    files += 1;
                    bytes = bytes.saturating_add(entry.size);
                }
            }
        }
    }
    Ok((files, bytes, dirs))
}

/// Bytes of `path` read back through the descriptor path a `read(2)` takes.
#[inline(never)]
fn read_head(table: FdTable, path: &[u8], out: &mut [u8]) -> Result<usize, &'static str> {
    let fd = file_open_at(table, path, b"/", O_RDONLY, RESOLVE_FOLLOW, None);
    if fd < 0 {
        return Err("opening a staged file failed");
    }
    let mut done = 0usize;
    while done < out.len() {
        let n = file_read_fd(table, fd, &mut KernelIoBuf::new(&mut out[done..]));
        if n <= 0 {
            break;
        }
        done += n as usize;
    }
    let _ = file_close_fd(table, fd);
    Ok(done)
}

/// What a directory entry cannot prove: that the bytes behind a name come
/// back. Each file must read back `stat`'s count, as text rather than the
/// zeros a hole reads as, and differently from the other — a volume handing
/// one block back for two inodes would otherwise pass.
#[inline(never)]
fn cap_verify(table: FdTable) -> Result<u64, &'static str> {
    let mut path = [0u8; WALK_PATH_MAX];
    let mut heads = [[0u8; CAP_VERIFY_BYTES]; CAP_VERIFY.len()];
    let mut total = 0u64;
    for (i, name) in CAP_VERIFY.iter().enumerate() {
        let len = CAP_MOUNT.len() + 1 + name.len();
        if len > WALK_PATH_MAX {
            return Err("a verification path is longer than the path buffer");
        }
        path[..CAP_MOUNT.len()].copy_from_slice(CAP_MOUNT);
        path[CAP_MOUNT.len()] = b'/';
        path[CAP_MOUNT.len() + 1..len].copy_from_slice(name);

        let Ok(found) = vfs_stat(&path[..len]) else {
            return Err(
                "a file staged at the root of the repo subtree is missing — rebuild the volume: \
                 rm -f fs/assets/ext2-capacity.img fs/assets/ext2-capacity.img.stamp",
            );
        };
        let want = (found.size as usize).min(CAP_VERIFY_BYTES);
        if want == 0 {
            return Err("a staged file stats as empty");
        }
        let got = read_head(table, &path[..len], &mut heads[i][..want])?;
        if got != want {
            return Err("a staged file read back fewer bytes than stat promised");
        }
        if !heads[i][..want]
            .iter()
            .all(|&b| b == b'\t' || b == b'\n' || b == b'\r' || (0x20..0x7f).contains(&b))
        {
            return Err("a staged text file read back as something other than text");
        }
        total += want as u64;
    }
    if heads[0] == heads[1] {
        return Err("two different staged files read back identical bytes");
    }
    Ok(total)
}

/// Walk the staged tree, read part of it back, and answer the files and bytes
/// the report carries. Its own frame: the counters, the timing and the log
/// line's arguments are more than [`cap_body`] has left under 2 KiB.
#[inline(never)]
fn cap_residency(table: FdTable) -> Result<(u64, u64), &'static str> {
    stats::reset();
    let start = monotonic_ns();
    let (files, bytes, walked) = cap_walk()?;
    let walkns = monotonic_ns().saturating_sub(start);
    let walkreads = stats::snapshot().read_requests;
    let verified = cap_verify(table)?;
    if files < WALK_MIN_FILES || bytes < WALK_MIN_BYTES {
        klog_info!(
            "FSPERF: the staged tree is {} files / {} bytes in {} directories, under the {} \
             files / {} bytes a checked-out copy of this repository alone is",
            files,
            bytes,
            walked,
            WALK_MIN_FILES,
            WALK_MIN_BYTES
        );
        return Err(
            "the capacity volume holds no tree worth reporting — it was built without \
             FS_POPULATE_DIR",
        );
    }
    klog_info!(
        "FSPERF: the capacity volume holds {} files / {} bytes in {} directories, walked in {} \
         device reads / {}ns; {} bytes read back and compared",
        files,
        bytes,
        walked,
        walkreads,
        walkns,
        verified
    );
    Ok((files, bytes))
}

/// The capacity volume, when the harness attached one: `just test-capacity`
/// builds the 16 GiB image, and an ordinary run has no such device and passes.
pub fn test_fsperf_capacity_volume() -> TestResult {
    let Some((blocks, blocksize, groups, entries)) = cap_geometry() else {
        klog_info!(
            "FSPERF: no ext2 capacity volume attached — skipping the capacity report \
             (attach one with `just test-capacity`)"
        );
        return TestResult::Pass;
    };
    if !ensure_dir(CAP_MOUNT) {
        return fail!("could not create the capacity mount point");
    }

    // Reads, not seconds: bounded mount cost is a claim about I/O per unit of
    // volume, and reads are deterministic where wall time is not.
    stats::reset();
    let start = monotonic_ns();
    let mounted = vfs_ext2_mount_named(CAP_DEVICE, CAP_MOUNT, false);
    let mountns = monotonic_ns().saturating_sub(start);
    let mountreads = stats::snapshot().read_requests;
    if let Err(e) = mounted {
        return fail!("mounting the capacity volume failed: {:?}", e);
    }

    let outcome = cap_body(blocks, blocksize, groups, entries, mountreads, mountns);
    if let Err(e) = vfs_ext2_unmount_named(CAP_MOUNT) {
        return fail!("unmounting the capacity volume failed: {:?}", e);
    }
    let _ = vfs_rmdir(CAP_MOUNT);
    outcome
}

/// Everything between the mount and the unmount, in its own frame so the path
/// buffer does not share one with the mount.
#[inline(never)]
fn cap_body(
    blocks: u64,
    blocksize: u32,
    groups: u32,
    entries: usize,
    mountreads: u64,
    mountns: u64,
) -> TestResult {
    let Some(process) = super::ScratchProcess::new() else {
        return fail!("could not register a process to own the descriptors");
    };
    let table = process.table();
    let Some(chunk) = payload() else {
        return fail!("could not allocate the payload");
    };

    // `<mount>/d/n0000000`..: the directory, then the names inside it.
    let mut path = [0u8; 64];
    path[..CAP_MOUNT.len()].copy_from_slice(CAP_MOUNT);
    path[CAP_MOUNT.len()] = b'/';
    path[CAP_MOUNT.len() + 1] = b'd';
    let dir = CAP_MOUNT.len() + 2;
    // The capacity volume is preserved between runs, so its fixture is rebuilt
    // over itself: no `O_EXCL`, and the directory may already be there.
    match vfs_mkdir(&path[..dir]) {
        Ok(()) | Err(crate::vfs::VfsError::AlreadyExists) => {}
        Err(e) => return fail!("could not create the large directory: {:?}", e),
    }
    path[dir] = b'/';

    let last = match cap_fill(table, &mut path, dir + 1) {
        Ok(len) => len,
        Err(why) => return fail!("{}", why),
    };

    // The second lookup, not the first: a rolled-back allocation scope retracts
    // the directory's name index, which a fill into nearly-full groups does
    // hit, so the first lookup pays for the rebuild's scan and the second is
    // what is graded.
    if vfs_stat(&path[..last]).is_err() {
        return fail!("the last of {} names did not look up", CAP_DIRENTS);
    }
    stats::reset();
    let found = vfs_stat(&path[..last]);
    let lookupreads = stats::snapshot().read_requests;
    if found.is_err() {
        return fail!(
            "the last of {} names looked up once and then not again",
            CAP_DIRENTS
        );
    }

    // `<mount>/b`, not a name in the crowded directory: the write is
    // measuring the volume, not the directory.
    path[CAP_MOUNT.len() + 1] = b'b';
    let start = monotonic_ns();
    let wrote = write_through_vfs(table, &path[..dir], CAP_BYTES, chunk.as_slice());
    let ns = monotonic_ns().saturating_sub(start);
    if wrote != CAP_BYTES as isize {
        return fail!(
            "writing {} bytes to the capacity volume returned {}",
            CAP_BYTES,
            wrote
        );
    }

    // Residency last: a walk of 1207 directories evicts exactly what the fill
    // and the lookup warmed, and every cost above is measured warm.
    let (files, treebytes) = match cap_residency(table) {
        Ok(found) => found,
        Err(why) => return fail!("{}", why),
    };

    emit_cap(
        blocks,
        blocksize,
        groups,
        entries,
        mountreads,
        mountns,
        CAP_DIRENTS,
        lookupreads,
        CAP_BYTES as u64,
        ns,
        files,
        treebytes,
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_fsperf_capacity_write_cost, suite = fs);
slopos_testing::stest!(name = test_fsperf_capacity_volume, suite = fs);
