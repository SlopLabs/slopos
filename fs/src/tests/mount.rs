//! Mount identity, the mount table's child queries, and the paged listing's
//! mount pass.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use slopos_abi::fs::{FS_TYPE_DIRECTORY, UserFsEntry};
use slopos_ostd::lock_class;
use slopos_ostd::sync::LOCK_LEVEL_RESOURCE;
use slopos_ostd::{KArc, KBox, KVec};
use slopos_testing::TestResult;

use super::{Ext2ImageSpec, FIX_FILE_BLOCK, build_ext2_image};
use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::devfs::{devfs_block_device_by_name, devfs_register_block_device};
use crate::ext2::ReadOnlyReason;
use crate::ext2_vfs::{Ext2Mount, WRITEBACK_CHUNK};
use crate::ramfs::RamFs;
use crate::vfs::init::{
    vfs_claim_block_device, vfs_ext2_mount_named, vfs_ext2_pool_claim, vfs_ext2_pool_release,
    vfs_ext2_unmount_named,
};
use crate::vfs::traits::{FileStat, FileSystem, FileType, InodeId, VfsResult, same_filesystem};
use crate::vfs::{
    ListCursor, VfsError, mount, mount_at, unmount, vfs_init_builtin_filesystems, vfs_list_from,
    vfs_mkdir, vfs_open, vfs_rmdir, vfs_stat, vfs_statfs, with_mount_table,
};

/// Four fixture filesystems, each with its own lock class: a path walk
/// crossing a mount holds one mount's lock while taking the next one's.
static FIXTURE_FS: [RamFs; 4] = [
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_0", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_1", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_2", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_MOUNT_TEST_3", LOCK_LEVEL_RESOURCE)),
];

fn ready() -> bool {
    vfs_init_builtin_filesystems().is_ok()
}

/// Fixtures live under `/tmp`, always the RAM mount the boot step puts there,
/// so no test here depends on `/` being writable.
fn ensure_dir(path: &[u8]) -> bool {
    let _ = vfs_mkdir(path);
    mount_at(path).is_some()
        || crate::vfs::vfs_stat(path).map(|s| s.file_type) == Ok(FileType::Directory)
}

fn entry_name(entry: &UserFsEntry) -> &[u8] {
    let end = entry
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(entry.name.len());
    &entry.name[..end]
}

fn page_contains(page: &[UserFsEntry], name: &[u8]) -> bool {
    page.iter().any(|e| entry_name(e) == name)
}

/// A mount id is handed out once. Slot indices are reused the instant a mount
/// is released — which is exactly why the listing cursor may not be an ordinal.
pub fn test_mount_id_is_never_reused() -> TestResult {
    const MP: &[u8] = b"/tmp/mount_id_mp";

    if !ready() {
        return TestResult::Fail;
    }
    if !ensure_dir(MP) {
        return slopos_testing::fail!("could not create the mount point");
    }

    let outcome = (|| -> Result<(), &'static str> {
        mount(MP, &FIXTURE_FS[0], 0).map_err(|_| "first mount failed")?;
        let first = mount_at(MP)
            .ok_or("the first mount is not in the table")?
            .id;
        unmount(MP).map_err(|_| "first unmount failed")?;

        mount(MP, &FIXTURE_FS[1], 0).map_err(|_| "second mount failed")?;
        let second = mount_at(MP)
            .ok_or("the second mount is not in the table")?
            .id;
        let result = if second > first {
            Ok(())
        } else {
            Err("a released mount's identity came back")
        };
        unmount(MP).map_err(|_| "second unmount failed")?;
        result
    })();

    let _ = unmount(MP);
    let _ = vfs_rmdir(MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// `has_child_mount` answers for a *direct* child only, under either spelling
/// of the parent, and stops answering the moment the mount is released.
pub fn test_mount_table_child_queries() -> TestResult {
    const DIR: &[u8] = b"/tmp/child_q";
    const CHILD: &[u8] = b"/tmp/child_q/leaf";
    const DEEPER: &[u8] = b"/tmp/child_q/leaf/deeper";

    if !ready() || !ensure_dir(DIR) {
        return slopos_testing::fail!("could not create the fixture directory");
    }

    let outcome = (|| -> Result<(), &'static str> {
        mount(CHILD, &FIXTURE_FS[0], 0).map_err(|_| "mount failed")?;
        mount(DEEPER, &FIXTURE_FS[1], 0).map_err(|_| "deep mount failed")?;

        let checks = with_mount_table(|mt| {
            (
                mt.has_child_mount(DIR, b"leaf"),
                mt.has_child_mount(b"/tmp/child_q/", b"leaf"),
                mt.has_child_mount(DIR, b"deeper"),
                mt.has_child_mount(DIR, b"lea"),
            )
        });
        if !checks.0 {
            return Err("a direct child mount was not reported");
        }
        if !checks.1 {
            return Err("a trailing slash on the parent hid its child mount");
        }
        if checks.2 {
            return Err("a grandchild mount was reported as a direct child");
        }
        if checks.3 {
            return Err("a name prefix matched a child mount");
        }

        unmount(DEEPER).map_err(|_| "deep unmount failed")?;
        unmount(CHILD).map_err(|_| "unmount failed")?;
        if with_mount_table(|mt| mt.has_child_mount(DIR, b"leaf")) {
            return Err("a released mount is still reported as a child");
        }
        Ok(())
    })();

    let _ = unmount(DEEPER);
    let _ = unmount(CHILD);
    let _ = vfs_rmdir(DIR);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// A listing paged across a mount or unmount neither drops nor repeats an
/// entry. Both halves fail against a cursor keyed on an ordinal.
pub fn test_paged_listing_survives_a_mount_change() -> TestResult {
    if !ready() {
        return TestResult::Fail;
    }
    match (listing_drop_half(), listing_repeat_half()) {
        (Ok(()), Ok(())) => TestResult::Pass,
        (Err(msg), _) | (_, Err(msg)) => slopos_testing::fail!(msg),
    }
}

/// Page one of a three-entry buffer over a freshly created directory: `.`,
/// `..`, and the first child mount in id order.
fn first_page(dir: &[u8], entries: &mut [UserFsEntry]) -> Result<ListCursor, &'static str> {
    let mut cursor = ListCursor::start();
    let n = vfs_list_from(dir, entries, &mut cursor).map_err(|_| "the first page failed")?;
    if n != 3 || entry_name(&entries[2]) != b"m1" {
        return Err("the first page did not end on the lowest-id child mount");
    }
    // Through the ABI word, because that is what userland carries between
    // calls and the packing is part of what is under test.
    Ok(ListCursor::from_abi(cursor.to_abi()))
}

fn listing_drop_half() -> Result<(), &'static str> {
    const DIR: &[u8] = b"/tmp/g9_drop";
    const M1: &[u8] = b"/tmp/g9_drop/m1";
    const M2: &[u8] = b"/tmp/g9_drop/m2";
    const M3: &[u8] = b"/tmp/g9_drop/m3";

    if !ensure_dir(DIR) {
        return Err("could not create the listing fixture");
    }
    let body = (|| -> Result<(), &'static str> {
        mount(M1, &FIXTURE_FS[0], 0).map_err(|_| "m1 mount failed")?;
        mount(M2, &FIXTURE_FS[1], 0).map_err(|_| "m2 mount failed")?;
        mount(M3, &FIXTURE_FS[2], 0).map_err(|_| "m3 mount failed")?;

        let mut entries = KVec::filled(UserFsEntry::new(), 3).map_err(|_| "entry buffer")?;
        let mut cursor = first_page(DIR, &mut entries)?;

        unmount(M1).map_err(|_| "m1 unmount failed")?;
        let n =
            vfs_list_from(DIR, &mut entries, &mut cursor).map_err(|_| "the second page failed")?;
        if !page_contains(&entries[..n], b"m2") {
            return Err("a mount was dropped when the set shrank between pages");
        }
        if page_contains(&entries[..n], b"m1") {
            return Err("an unmounted filesystem was still listed");
        }
        Ok(())
    })();

    let _ = unmount(M1);
    let _ = unmount(M2);
    let _ = unmount(M3);
    let _ = vfs_rmdir(DIR);
    body
}

fn listing_repeat_half() -> Result<(), &'static str> {
    const DIR: &[u8] = b"/tmp/g9_rep";
    const M0: &[u8] = b"/tmp/g9_rep/m0";
    const M1: &[u8] = b"/tmp/g9_rep/m1";
    const M2: &[u8] = b"/tmp/g9_rep/m2";
    const M3: &[u8] = b"/tmp/g9_rep/m3";

    if !ensure_dir(DIR) {
        return Err("could not create the listing fixture");
    }
    let body = (|| -> Result<(), &'static str> {
        mount(M0, &FIXTURE_FS[0], 0).map_err(|_| "m0 mount failed")?;
        mount(M1, &FIXTURE_FS[1], 0).map_err(|_| "m1 mount failed")?;
        mount(M2, &FIXTURE_FS[2], 0).map_err(|_| "m2 mount failed")?;
        // Frees the slot ahead of m1's, which the next mount takes.
        unmount(M0).map_err(|_| "m0 unmount failed")?;

        let mut entries = KVec::filled(UserFsEntry::new(), 3).map_err(|_| "entry buffer")?;
        let mut cursor = first_page(DIR, &mut entries)?;

        mount(M3, &FIXTURE_FS[3], 0).map_err(|_| "m3 mount failed")?;
        let n =
            vfs_list_from(DIR, &mut entries, &mut cursor).map_err(|_| "the second page failed")?;
        if page_contains(&entries[..n], b"m1") {
            return Err("a mount was listed twice when the set grew between pages");
        }
        if !page_contains(&entries[..n], b"m2") {
            return Err("a mount was dropped when the set grew between pages");
        }
        Ok(())
    })();

    let _ = unmount(M1);
    let _ = unmount(M2);
    let _ = unmount(M3);
    let _ = vfs_rmdir(DIR);
    body
}

/// A real directory entry a mount covers appears exactly once across every
/// page, as a directory.
pub fn test_mount_shadowed_name_lists_once() -> TestResult {
    const DIR: &[u8] = b"/tmp/g9_shadow";
    const COVERED: &[u8] = b"/tmp/g9_shadow/covered";

    if !ready() || !ensure_dir(DIR) || !ensure_dir(COVERED) {
        return slopos_testing::fail!("could not create the shadowing fixture");
    }

    let outcome = (|| -> Result<(), &'static str> {
        mount(COVERED, &FIXTURE_FS[0], 0).map_err(|_| "mount failed")?;

        // One entry per page, so the shadowed name and the mount entry cannot
        // land on the same page and be de-duplicated there.
        let mut entries = KVec::filled(UserFsEntry::new(), 1).map_err(|_| "entry buffer")?;
        let mut cursor = ListCursor::start();
        let mut seen = 0usize;
        let mut pages = 0usize;

        while !cursor.is_end() {
            let n = vfs_list_from(DIR, &mut entries, &mut cursor).map_err(|_| "a page failed")?;
            for entry in entries.iter().take(n) {
                if entry_name(entry) != b"covered" {
                    continue;
                }
                seen += 1;
                if entry.type_ != FS_TYPE_DIRECTORY {
                    return Err("a mount point did not list as a directory");
                }
            }
            cursor = ListCursor::from_abi(cursor.to_abi());
            pages += 1;
            if pages > 16 {
                return Err("the paged listing did not terminate");
            }
        }

        if seen != 1 {
            return Err("a name shadowed by a mount was not listed exactly once");
        }
        Ok(())
    })();

    let _ = unmount(COVERED);
    let _ = vfs_rmdir(COVERED);
    let _ = vfs_rmdir(DIR);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// The pool `mount(2)` hands `fstype="ramfs"` out of exhausts at its size,
/// recovers on release, and resets a released instance.
pub fn test_ramfs_mount_pool_exhausts_and_recovers() -> TestResult {
    use crate::vfs::init::{RAMFS_POOL_LEN, vfs_ramfs_pool_claim, vfs_ramfs_pool_release};

    let mut claimed: [Option<&'static RamFs>; RAMFS_POOL_LEN] = [None; RAMFS_POOL_LEN];
    for slot in claimed.iter_mut() {
        *slot = vfs_ramfs_pool_claim();
    }

    let outcome = (|| -> Result<(), &'static str> {
        for slot in claimed.iter() {
            if slot.is_none() {
                return Err("the pool refused an instance while it had room");
            }
        }
        if vfs_ramfs_pool_claim().is_some() {
            return Err("the pool handed out more instances than it holds");
        }

        let first = claimed[0].ok_or("the pool is empty")?;
        first
            .create(first.root_inode(), b"stale", FileType::Regular)
            .map_err(|_| "could not write to a pooled instance")?;

        let instance: &'static dyn FileSystem = first;
        if !vfs_ramfs_pool_release(instance, false) {
            return Err("the pool did not recognise its own instance");
        }
        let again = vfs_ramfs_pool_claim().ok_or("a released instance did not come back")?;
        claimed[0] = Some(again);

        if again.lookup(again.root_inode(), b"stale").is_ok() {
            return Err("a re-claimed instance still held the previous mount's file");
        }
        Ok(())
    })();

    for slot in claimed.iter().flatten() {
        let instance: &'static dyn FileSystem = *slot;
        let _ = vfs_ramfs_pool_release(instance, false);
    }
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// Delegates to a ramfs, but fails the `n`th lookup of [`FLAKY_NAME`], or the
/// `n`th stat, once armed with `n` — the transient refusal a killed task's
/// device read answers with.
struct FlakyLookup {
    inner: &'static RamFs,
    fail_in: AtomicU32,
    fail_stat_in: AtomicU32,
}

fn countdown_hits(counter: &AtomicU32) -> bool {
    counter.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)) == Ok(1)
}

const FLAKY_NAME: &[u8] = b"displaced";

impl FileSystem for FlakyLookup {
    fn name(&self) -> &'static str {
        "flaky"
    }
    fn root_inode(&self) -> InodeId {
        self.inner.root_inode()
    }
    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        if name == FLAKY_NAME && countdown_hits(&self.fail_in) {
            return Err(VfsError::Interrupted);
        }
        self.inner.lookup(parent, name)
    }
    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        if countdown_hits(&self.fail_stat_in) {
            return Err(VfsError::Interrupted);
        }
        self.inner.stat(inode)
    }
    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.inner.read(inode, offset, buf)
    }
    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.inner.write(inode, offset, buf)
    }
    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.inner.create(parent, name, file_type)
    }
    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.inner.unlink(parent, name)
    }
    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.inner.readdir(inode, offset, callback)
    }
    fn rename(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<()> {
        self.inner
            .rename(old_parent, old_name, new_parent, new_name)
    }
}

static FLAKY_FS: FlakyLookup = FlakyLookup {
    inner: &FIXTURE_FS[3],
    fail_in: AtomicU32::new(0),
    fail_stat_in: AtomicU32::new(0),
};

const FLAKY_MP: &[u8] = b"/tmp/flaky_rename";
const FLAKY_SOURCE: &[u8] = b"/tmp/flaky_rename/source";
const FLAKY_TARGET: &[u8] = b"/tmp/flaky_rename/displaced";

/// A rename whose lookup of the name it would displace fails must fail. Taken
/// for "nothing there", it skipped the protection an open displaced file is
/// owed, and the filesystem's own lookup then displaced it anyway.
pub fn test_rename_fails_when_the_displaced_lookup_does() -> TestResult {
    if !ready() || !ensure_dir(FLAKY_MP) {
        return slopos_testing::fail!("the /tmp fixture directory is unavailable");
    }
    let outcome = flaky_rename_body();
    FLAKY_FS.fail_in.store(0, Ordering::Release);
    let _ = crate::vfs::vfs_unlink(FLAKY_SOURCE);
    let _ = crate::vfs::vfs_unlink(FLAKY_TARGET);
    let _ = unmount(FLAKY_MP);
    let _ = vfs_rmdir(FLAKY_MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

#[inline(never)]
fn flaky_rename_body() -> Result<(), &'static str> {
    mount(FLAKY_MP, &FLAKY_FS, 0).map_err(|_| "mount failed")?;
    vfs_open(FLAKY_SOURCE, true).map_err(|_| "could not create the source")?;
    vfs_open(FLAKY_TARGET, true).map_err(|_| "could not create the target")?;

    // The sealed-path check looks the name up first; the second lookup is the
    // one that decides what the rename displaces.
    FLAKY_FS.fail_in.store(2, Ordering::Release);
    let renamed = crate::vfs::vfs_rename(FLAKY_SOURCE, FLAKY_TARGET);
    if FLAKY_FS.fail_in.swap(0, Ordering::AcqRel) != 0 {
        return Err("the rename never looked up the name it would displace");
    }
    if renamed.is_ok() {
        return Err("the rename went ahead without knowing what it displaced");
    }
    if vfs_stat(FLAKY_SOURCE).is_err() || vfs_stat(FLAKY_TARGET).is_err() {
        return Err("a refused rename moved a name");
    }
    Ok(())
}

/// `unlink(2)` and `rmdir(2)` refuse, and remove nothing, whichever of their
/// lookups or stats fails, and never answer `ENOENT` for it: an error is not
/// an absent name, and not a last one.
pub fn test_removal_fails_when_a_lookup_does() -> TestResult {
    if !ready() || !ensure_dir(FLAKY_MP) {
        return slopos_testing::fail!("the /tmp fixture directory is unavailable");
    }
    let outcome = flaky_removal_body();
    FLAKY_FS.fail_in.store(0, Ordering::Release);
    FLAKY_FS.fail_stat_in.store(0, Ordering::Release);
    let _ = crate::vfs::vfs_unlink(FLAKY_TARGET);
    let _ = vfs_rmdir(FLAKY_TARGET);
    let _ = unmount(FLAKY_MP);
    let _ = vfs_rmdir(FLAKY_MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

#[inline(never)]
fn flaky_removal_body() -> Result<(), &'static str> {
    let unlink = || crate::fileio::file_unlink_at(FLAKY_TARGET, b"/");
    let rmdir = || crate::fileio::file_rmdir_at(FLAKY_TARGET, b"/");
    let file = || vfs_open(FLAKY_TARGET, true).map(|_| ());
    mount(FLAKY_MP, &FLAKY_FS, 0).map_err(|_| "mount failed")?;
    for failing in [&FLAKY_FS.fail_in, &FLAKY_FS.fail_stat_in] {
        file().map_err(|_| "could not create the file")?;
        refuses_at_every_failure(failing, unlink)?;
        vfs_mkdir(FLAKY_TARGET).map_err(|_| "could not create the directory")?;
        refuses_at_every_failure(failing, rmdir)?;
    }
    Ok(())
}

fn refuses_at_every_failure(
    failing: &AtomicU32,
    remove: impl Fn() -> i32,
) -> Result<(), &'static str> {
    for n in 1..=16 {
        failing.store(n, Ordering::Release);
        let rc = remove();
        if failing.swap(0, Ordering::AcqRel) != 0 {
            return match (n, rc) {
                (1, _) => Err("the removal never failed where it was told to"),
                (_, 0) => Ok(()),
                _ => Err("a removal with every call answered failed"),
            };
        }
        if rc == 0 {
            return Err("a removal went ahead past a failed call");
        }
        if rc == slopos_abi::Errno::ENOENT.raw() {
            return Err("a failed call was reported as an absent name");
        }
        if vfs_stat(FLAKY_TARGET).is_err() {
            return Err("a refused removal took the name");
        }
    }
    Err("the removal failed at every attempt")
}

/// Blocks in the fixture images these tests attach: 512 KiB at the builder's
/// 1 KiB block size, a handful of device writes to copy onto a scratch device.
const IMAGE_BLOCKS: u32 = 512;

/// The disposable scratch device the harness attaches as disk1.
const SCRATCH_DEVICE: &[u8] = b"vdb";

fn fixture_image(blocks: u32) -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks,
        inodes: 32,
        file_name: None,
        file_data: None,
        file_block: FIX_FILE_BLOCK,
    })
}

fn boxed_device(image: MemoryBlockDevice) -> Option<KBox<dyn BlockDevice + Send + Sync>> {
    let boxed = KBox::try_new(image).ok()?;
    Some(boxed)
}

/// A clean ext2 fixture on a heap image, boxed as a device. Public because the
/// `mount(2)` tests live in `slopos-core` and the image builder lives here.
pub fn fixture_image_device(blocks: u32) -> Option<KBox<dyn BlockDevice + Send + Sync>> {
    boxed_device(fixture_image(blocks)?)
}

/// Lay a clean ext2 fixture down on the named block device, through the same
/// exclusive claim a writable mount takes. `false` when the device is absent
/// or already claimed — a skip, not a failure.
pub fn write_scratch_ext2(device: &[u8]) -> bool {
    const CHUNK: usize = 32 * 1024;
    let Some(image) = fixture_image(IMAGE_BLOCKS) else {
        return false;
    };
    let Ok(target) = vfs_claim_block_device(device) else {
        return false;
    };
    let Ok(mut buf) = KVec::<u8>::zeroed(CHUNK) else {
        return false;
    };
    let total = u64::from(IMAGE_BLOCKS) * 1024;
    let mut at = 0u64;
    while at < total {
        let n = CHUNK.min((total - at) as usize);
        if image.read_at(at, &mut buf.as_mut_slice()[..n]).is_err()
            || target.write_at(at, &buf.as_slice()[..n]).is_err()
        {
            return false;
        }
        at += n as u64;
    }
    // Zeroed past the image: a verity trailer an earlier test left at this
    // offset would otherwise be read as this image's, and refuse the mount.
    buf.as_mut_slice().fill(0);
    if target.write_at(total, buf.as_slice()).is_err() {
        return false;
    }
    target.flush().is_ok()
}

const PAIR_MP_A: &[u8] = b"/tmp/ext2_pair_a";
const PAIR_MP_B: &[u8] = b"/tmp/ext2_pair_b";
const PAIR_FILE_A: &[u8] = b"/tmp/ext2_pair_a/alpha";
const PAIR_FILE_B: &[u8] = b"/tmp/ext2_pair_b/beta";
const PAIR_CROSS_A: &[u8] = b"/tmp/ext2_pair_a/beta";
const PAIR_CROSS_B: &[u8] = b"/tmp/ext2_pair_b/alpha";

/// Two ext2 filesystems mounted at once, each over its own image: the table
/// holds them as two filesystems rather than one placed twice, and the state
/// that used to be a handful of `static`s is genuinely per instance.
pub fn test_two_ext2_mounts_are_independent() -> TestResult {
    if !ready() || !ensure_dir(PAIR_MP_A) || !ensure_dir(PAIR_MP_B) {
        return slopos_testing::fail!("the /tmp fixture directories are unavailable");
    }
    let (Some(dev_a), Some(dev_b)) = (
        fixture_image(IMAGE_BLOCKS).and_then(boxed_device),
        fixture_image(IMAGE_BLOCKS / 2).and_then(boxed_device),
    ) else {
        return TestResult::Skipped;
    };
    let Some(fs_a) = vfs_ext2_pool_claim() else {
        return slopos_testing::fail!("the ext2 pool handed out no instance");
    };
    let Some(fs_b) = vfs_ext2_pool_claim() else {
        vfs_ext2_pool_release(fs_a, false);
        return slopos_testing::fail!("the ext2 pool handed out only one instance");
    };

    let outcome = pair_body(fs_a, fs_b, dev_a, dev_b);

    let _ = unmount(PAIR_MP_A);
    let _ = unmount(PAIR_MP_B);
    vfs_ext2_pool_release(fs_a, false);
    vfs_ext2_pool_release(fs_b, false);
    let _ = vfs_rmdir(PAIR_MP_A);
    let _ = vfs_rmdir(PAIR_MP_B);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// Its own frame per phase: one `Ext2MountInfo` plus one `FsStats` plus a
/// mount call already fills a 2 KiB frame.
#[inline(never)]
fn pair_body(
    fs_a: &'static Ext2Mount,
    fs_b: &'static Ext2Mount,
    dev_a: KBox<dyn BlockDevice + Send + Sync>,
    dev_b: KBox<dyn BlockDevice + Send + Sync>,
) -> Result<(), &'static str> {
    if same_filesystem(fs_a, fs_b) {
        return Err("two pooled ext2 instances share one identity");
    }
    pair_attach(fs_a, dev_a, PAIR_MP_A)?;
    pair_attach(fs_b, dev_b, PAIR_MP_B)?;
    pair_files()?;
    pair_geometry()
}

#[inline(never)]
fn pair_attach(
    fs: &'static Ext2Mount,
    device: KBox<dyn BlockDevice + Send + Sync>,
    at: &[u8],
) -> Result<(), &'static str> {
    fs.attach(device, false)
        .map_err(|_| "an image would not attach")?;
    mount(at, fs, 0).map_err(|_| "a mount failed")
}

#[inline(never)]
fn pair_files() -> Result<(), &'static str> {
    let handle_a = vfs_open(PAIR_FILE_A, true).map_err(|_| "create on the first mount failed")?;
    handle_a
        .write(0, b"first")
        .map_err(|_| "write on the first mount failed")?;
    let handle_b = vfs_open(PAIR_FILE_B, true).map_err(|_| "create on the second mount failed")?;
    handle_b
        .write(0, b"second")
        .map_err(|_| "write on the second mount failed")?;

    let mut buf = [0u8; 16];
    let n = handle_a
        .read(0, &mut buf)
        .map_err(|_| "read back on the first mount failed")?;
    if &buf[..n] != b"first" {
        return Err("the first mount served the wrong bytes");
    }
    let n = handle_b
        .read(0, &mut buf)
        .map_err(|_| "read back on the second mount failed")?;
    if &buf[..n] != b"second" {
        return Err("the second mount served the wrong bytes");
    }

    if vfs_stat(PAIR_CROSS_A).is_ok() {
        return Err("the first mount resolved the second's file");
    }
    if vfs_stat(PAIR_CROSS_B).is_ok() {
        return Err("the second mount resolved the first's file");
    }
    Ok(())
}

#[inline(never)]
fn pair_geometry() -> Result<(), &'static str> {
    let blocks_a = vfs_statfs(PAIR_MP_A)
        .map_err(|_| "statfs of the first mount failed")?
        .0
        .blocks;
    let blocks_b = vfs_statfs(PAIR_MP_B)
        .map_err(|_| "statfs of the second mount failed")?
        .0
        .blocks;
    if blocks_a == blocks_b {
        return Err("both mounts reported one image's geometry");
    }
    Ok(())
}

const REMOUNT_MP: &[u8] = b"/tmp/ext2_remount";
const REMOUNT_FILE: &[u8] = b"/tmp/ext2_remount/persisted";

/// The re-mount is what proves the first mount gave the device's exclusive
/// write claim back: a leaked token makes `open_writer` answer `AlreadyClaimed`
/// forever. The file surviving the round trip shows the unmount reached the
/// medium rather than merely dropping the instance.
pub fn test_ext2_remount_of_the_same_device_succeeds() -> TestResult {
    if !ready() || !ensure_dir(REMOUNT_MP) {
        return slopos_testing::fail!("the /tmp fixture directory is unavailable");
    }
    if !write_scratch_ext2(SCRATCH_DEVICE) {
        return TestResult::Skipped;
    }

    let outcome = remount_body();
    let _ = vfs_ext2_unmount_named(REMOUNT_MP);
    let _ = vfs_rmdir(REMOUNT_MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

#[inline(never)]
fn remount_body() -> Result<(), &'static str> {
    let info = vfs_ext2_mount_named(SCRATCH_DEVICE, REMOUNT_MP, false)
        .map_err(|_| "the first mount of the scratch device failed")?;
    if info.read_only {
        return Err("a writable claim came up read-only");
    }
    let handle =
        vfs_open(REMOUNT_FILE, true).map_err(|_| "create through the first mount failed")?;
    handle
        .write(0, b"persisted")
        .map_err(|_| "write through the first mount failed")?;
    vfs_ext2_unmount_named(REMOUNT_MP).map_err(|_| "the first unmount failed")?;

    vfs_ext2_mount_named(SCRATCH_DEVICE, REMOUNT_MP, false)
        .map_err(|_| "the re-mount failed — the first mount leaked the write claim")?;
    let again = vfs_open(REMOUNT_FILE, false).map_err(|_| "the re-mount lost the file")?;
    let mut buf = [0u8; 16];
    let n = again
        .read(0, &mut buf)
        .map_err(|_| "read after the re-mount failed")?;
    if &buf[..n] != b"persisted" {
        return Err("the re-mount served different bytes");
    }
    vfs_ext2_unmount_named(REMOUNT_MP).map_err(|_| "the second unmount failed")
}

/// Device writes since [`CountingDevice::new`]; a static, so no test frame has
/// to carry the handle.
static COUNTED_WRITES: AtomicUsize = AtomicUsize::new(0);

struct CountingDevice {
    inner: MemoryBlockDevice,
}

impl CountingDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        COUNTED_WRITES.store(0, Ordering::Relaxed);
        Self { inner }
    }
}

impl BlockDevice for CountingDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        COUNTED_WRITES.fetch_add(1, Ordering::Relaxed);
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
}

const HEADROOM_MP: &[u8] = b"/tmp/ext2_headroom";
/// Operations one burst may take to drive the log past its low-water mark: two
/// or three suffice against the fixture's 47-slot log, the rest is margin for
/// the writeback kthread draining one from under us.
const HEADROOM_ROUNDS: usize = 64;

/// An operation that finds the log short of headroom completes, and the pass
/// that makes room gives the mount lock back between bounded steps. That a
/// **read** restores the headroom is the distinguishing part: it runs no
/// transaction, so only `Ext2Mount::with_fs`'s pre-flight can have drained it.
pub fn test_ext2_journal_headroom_is_restored_off_the_mount_lock() -> TestResult {
    if !ready() || !ensure_dir(HEADROOM_MP) {
        return slopos_testing::fail!("the /tmp fixture directory is unavailable");
    }
    let Some(image) = super::journal::journal_image() else {
        return TestResult::Skipped;
    };
    let Some(device) = boxed_device_counting(image) else {
        return TestResult::Skipped;
    };
    let Some(fs) = vfs_ext2_pool_claim() else {
        return slopos_testing::fail!("the ext2 pool handed out no instance");
    };

    let outcome = headroom_body(fs, device);

    let _ = unmount(HEADROOM_MP);
    vfs_ext2_pool_release(fs, false);
    let _ = vfs_rmdir(HEADROOM_MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

fn boxed_device_counting(image: MemoryBlockDevice) -> Option<KBox<dyn BlockDevice + Send + Sync>> {
    let boxed = KBox::try_new(CountingDevice::new(image)).ok()?;
    Some(boxed)
}

#[inline(never)]
fn headroom_body(
    fs: &'static Ext2Mount,
    device: KBox<dyn BlockDevice + Send + Sync>,
) -> Result<(), &'static str> {
    fs.attach(device, false)
        .map_err(|_| "the log-carrying fixture would not attach")?;
    if fs.is_read_only() {
        return Err("the fixture mounted read-only, so nothing can fill its log");
    }
    mount(HEADROOM_MP, fs, 0).map_err(|_| "the mount failed")?;

    let mut short = false;
    for round in 0..HEADROOM_ROUNDS {
        let mut name = *b"/tmp/ext2_headroom/f000";
        name[20] = b'0' + (round / 100) as u8;
        name[21] = b'0' + ((round / 10) % 10) as u8;
        name[22] = b'0' + (round % 10) as u8;
        let handle = vfs_open(&name, true).map_err(|_| "a create in the burst failed")?;
        handle
            .write(0, b"a record the log has to carry")
            .map_err(|_| "a write in the burst failed")?;
        if !fs.journal_has_headroom() {
            short = true;
            break;
        }
    }
    if !short {
        return Err("the burst never drove the log past its low-water mark");
    }

    // A stat, not a write: no transaction runs, so the wrapper's pre-flight is
    // the only thing that can empty the log.
    let before = COUNTED_WRITES.load(Ordering::Relaxed);
    let _ = crate::vfs::vfs_stat(b"/tmp/ext2_headroom/f000")
        .map_err(|_| "the read after the burst failed")?;
    if COUNTED_WRITES.load(Ordering::Relaxed) == before {
        return Err("a read over a full log issued no writeback at all");
    }
    if !fs.journal_has_headroom() {
        return Err("the log still had no headroom after an operation over it");
    }

    // The pass itself: bounded, and within its per-step budget.
    let handle = vfs_open(b"/tmp/ext2_headroom/after", true)
        .map_err(|_| "the mount stopped accepting writes")?;
    handle
        .write(0, b"still writable")
        .map_err(|_| "a write after the check point failed")?;
    let before = COUNTED_WRITES.load(Ordering::Relaxed);
    let (result, steps) = fs.sync_pass();
    result.map_err(|_| "the writeback pass failed")?;
    let written = COUNTED_WRITES.load(Ordering::Relaxed) - before;
    if steps < 2 {
        return Err("the pass ran in one step, so it never gave the mount lock back");
    }
    if written > steps * WRITEBACK_CHUNK {
        return Err("a writeback step wrote more than its budget");
    }
    if !fs.journal_is_empty() {
        return Err("the pass left records in the log");
    }
    Ok(())
}

const RDONLY_MP: &[u8] = b"/tmp/ext2_rdonly";
const RDONLY_FILE: &[u8] = b"/tmp/ext2_rdonly/denied";
/// A device of this test's own, published through a handle that takes writes —
/// as the root disk's own `/dev` node does — so the refusal under test is the
/// mount's and not the medium's.
const RDONLY_PROBE: &[u8] = b"romountprobe0";

/// Publish [`RDONLY_PROBE`]. A rerun within one boot finds the name taken, and
/// the node behind it is this same unwritten fixture.
fn register_rdonly_probe() -> bool {
    let Some(image) = fixture_image(IMAGE_BLOCKS) else {
        return false;
    };
    let Ok(counted) = KArc::try_new(CountingDevice::new(image)) else {
        return false;
    };
    let device: KArc<dyn BlockDevice + Send + Sync> = counted;
    matches!(
        devfs_register_block_device(RDONLY_PROBE, device),
        Ok(_) | Err(VfsError::AlreadyExists)
    )
}

/// `MS_RDONLY` is the caller's word, not a guess at what the device would take:
/// naming a device whose `/dev` node accepts writes must still refuse every
/// mutation and leave the medium untouched. Inferring read-only from the
/// device is what mounted the live root writable.
pub fn test_ext2_readonly_mount_refuses_a_writable_device() -> TestResult {
    if !ready() || !ensure_dir(RDONLY_MP) {
        return slopos_testing::fail!("the /tmp fixture directory is unavailable");
    }
    if !register_rdonly_probe() {
        return TestResult::Skipped;
    }

    let outcome = rdonly_mount_body();

    let _ = vfs_ext2_unmount_named(RDONLY_MP);
    let _ = vfs_rmdir(RDONLY_MP);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

#[inline(never)]
fn rdonly_mount_body() -> Result<(), &'static str> {
    let Some(node) = devfs_block_device_by_name("romountprobe0") else {
        return Err("the probe device is not in devfs");
    };
    if node.write_protected() {
        return Err("the probe device refuses writes on its own, so it proves nothing");
    }
    drop(node);

    let before = COUNTED_WRITES.load(Ordering::Relaxed);
    let info = vfs_ext2_mount_named(RDONLY_PROBE, RDONLY_MP, true)
        .map_err(|_| "the read-only mount of the probe device failed")?;
    if info.read_only_reason != Some(ReadOnlyReason::Requested) {
        return Err("an MS_RDONLY mount did not refuse writes on the caller's say-so");
    }
    match vfs_open(RDONLY_FILE, true) {
        Err(VfsError::ReadOnly) => {}
        Err(_) => return Err("a create through the read-only mount failed for the wrong reason"),
        Ok(_) => return Err("a create through an MS_RDONLY mount was accepted"),
    }
    // Through the unmount, because the teardown syncs and stamps: a read-only
    // mount owes the medium nothing at either end.
    vfs_ext2_unmount_named(RDONLY_MP).map_err(|_| "the unmount of the read-only mount failed")?;
    if COUNTED_WRITES.load(Ordering::Relaxed) != before {
        return Err("a read-only mount reached the device with a write");
    }
    Ok(())
}

const LABEL_MP: &[u8] = b"/tmp/ext2_label";
const LABEL_PROBE: &[u8] = b"labelprobe0";
const LABEL: &[u8] = b"stest-label";

/// A fixture carrying [`LABEL`] in `s_volume_name`, published in devfs.
fn register_label_probe() -> bool {
    let Some(image) = fixture_image(IMAGE_BLOCKS) else {
        return false;
    };
    image.with_buffer_mut(|buf| buf[1024 + 120..1024 + 120 + LABEL.len()].copy_from_slice(LABEL));
    let Ok(device) = KArc::try_new(image) else {
        return false;
    };
    let device: KArc<dyn BlockDevice + Send + Sync> = device;
    matches!(
        devfs_register_block_device(LABEL_PROBE, device),
        Ok(_) | Err(VfsError::AlreadyExists)
    )
}

/// `LABEL=` names the device whose superblock carries exactly that label: a
/// prefix of it names none.
pub fn test_ext2_mount_by_label() -> TestResult {
    if !ready() || !ensure_dir(LABEL_MP) {
        return slopos_testing::fail!("the /tmp fixture directory is unavailable");
    }
    if !register_label_probe() {
        return TestResult::Skipped;
    }

    let prefix = vfs_ext2_mount_named(b"LABEL=stest-labe", LABEL_MP, true).err();
    let mounted = vfs_ext2_mount_named(b"LABEL=stest-label", LABEL_MP, true).is_ok()
        && mount_at(LABEL_MP).is_some();

    let _ = vfs_ext2_unmount_named(LABEL_MP);
    let _ = vfs_rmdir(LABEL_MP);
    if prefix != Some(VfsError::NotFound) {
        return slopos_testing::fail!("a label prefix resolved: {:?}", prefix);
    }
    if !mounted {
        return slopos_testing::fail!("the labelled fixture did not mount by LABEL=");
    }
    TestResult::Pass
}

slopos_testing::stest!(name = test_mount_id_is_never_reused, suite = fs);
slopos_testing::stest!(name = test_mount_table_child_queries, suite = fs);
slopos_testing::stest!(
    name = test_paged_listing_survives_a_mount_change,
    suite = fs
);
slopos_testing::stest!(name = test_two_ext2_mounts_are_independent, suite = fs);
slopos_testing::stest!(
    name = test_ext2_remount_of_the_same_device_succeeds,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_headroom_is_restored_off_the_mount_lock,
    suite = fs
);
slopos_testing::stest!(name = test_mount_shadowed_name_lists_once, suite = fs);
slopos_testing::stest!(
    name = test_ramfs_mount_pool_exhausts_and_recovers,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_readonly_mount_refuses_a_writable_device,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_mount_by_label, suite = fs);
slopos_testing::stest!(
    name = test_rename_fails_when_the_displaced_lookup_does,
    suite = fs
);
slopos_testing::stest!(name = test_removal_fails_when_a_lookup_does, suite = fs);
