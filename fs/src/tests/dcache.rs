//! The ext2 name and attribute caches never answer from before a change.
//!
//! Every check reads a name or an inode twice before the change it is about,
//! so the answer after it would come from the cache if nothing invalidated it.

use slopos_ostd::KBox;
use slopos_testing::TestResult;

use super::{Ext2ImageSpec, FIX_FILE_BLOCK, build_ext2_image};
use crate::blockdev::BlockDevice;
use crate::ext2_vfs::Ext2Mount;
use crate::vfs::VfsError;
use crate::vfs::init::{vfs_ext2_pool_claim, vfs_ext2_pool_release};
use crate::vfs::traits::{FileStat, FileSystem, FileType, InodeId, VfsResult};

const IMAGE_BLOCKS: u32 = 512;

type Check = Result<(), &'static str>;

fn image(file: Option<(&[u8], &[u8])>) -> Option<KBox<dyn BlockDevice + Send + Sync>> {
    let image = build_ext2_image(Ext2ImageSpec {
        blocks: IMAGE_BLOCKS,
        inodes: 32,
        file_name: file.map(|(name, _)| name),
        file_data: file.map(|(_, data)| data),
        file_block: FIX_FILE_BLOCK,
    })?;
    let boxed = KBox::try_new(image).ok()?;
    Some(boxed)
}

/// Run `body` against a pooled instance with a fresh fixture attached.
fn on_fresh_mount(body: fn(&'static Ext2Mount) -> Check) -> TestResult {
    let Some(device) = image(None) else {
        return TestResult::Skipped;
    };
    let Some(fs) = vfs_ext2_pool_claim() else {
        return slopos_testing::fail!("the ext2 pool handed out no instance");
    };
    let outcome = match fs.attach(device, false) {
        Ok(_) => body(fs),
        Err(_) => Err("the fixture would not attach"),
    };
    vfs_ext2_pool_release(fs, false);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

/// `lookup` twice, both answering `want`: the second is the cached one.
fn resolves(fs: &Ext2Mount, parent: InodeId, name: &[u8], want: InodeId) -> Check {
    for _ in 0..2 {
        if fs.lookup(parent, name) != Ok(want) {
            return Err("a name resolved to the wrong inode");
        }
    }
    Ok(())
}

fn absent(fs: &Ext2Mount, parent: InodeId, name: &[u8]) -> Check {
    for _ in 0..2 {
        if fs.lookup(parent, name) != Err(VfsError::NotFound) {
            return Err("a name that should be absent resolved");
        }
    }
    Ok(())
}

/// `stat` twice, answering the second.
fn stat2(fs: &Ext2Mount, inode: InodeId) -> VfsResult<FileStat> {
    fs.stat(inode)?;
    fs.stat(inode)
}

fn made(result: VfsResult<InodeId>) -> Result<InodeId, &'static str> {
    result.map_err(|_| "a create failed")
}

/// A rename moves the binding out of the old directory and into the new one,
/// a directory's `..` follows it, and a rename over a name frees the inode it
/// displaced.
pub fn test_ext2_dcache_rename_across_directories() -> TestResult {
    on_fresh_mount(rename_body)
}

#[inline(never)]
fn rename_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let d1 = made(fs.create(root, b"d1", FileType::Directory))?;
    let d2 = made(fs.create(root, b"d2", FileType::Directory))?;
    let f = made(fs.create(d1, b"f", FileType::Regular))?;
    let sub = made(fs.create(d1, b"sub", FileType::Directory))?;
    resolves(fs, d1, b"f", f)?;
    absent(fs, d2, b"f")?;
    resolves(fs, sub, b"..", d1)?;
    if stat2(fs, d1).map(|s| s.nlink) != Ok(3) {
        return Err("a directory with one subdirectory did not count three links");
    }

    fs.rename(d1, b"f", d2, b"f")
        .map_err(|_| "the file rename failed")?;
    absent(fs, d1, b"f")?;
    resolves(fs, d2, b"f", f)?;

    fs.rename(d1, b"sub", d2, b"sub")
        .map_err(|_| "the directory rename failed")?;
    absent(fs, d1, b"sub")?;
    resolves(fs, d2, b"sub", sub)?;
    resolves(fs, sub, b"..", d2)?;
    if fs.stat(d1).map(|s| s.nlink) != Ok(2) || fs.stat(d2).map(|s| s.nlink) != Ok(3) {
        return Err("the parents' link counts did not follow the moved directory");
    }

    let g = made(fs.create(d1, b"g", FileType::Regular))?;
    if stat2(fs, f).map(|s| s.nlink) != Ok(1) {
        return Err("a fresh file did not count one link");
    }
    fs.rename(d1, b"g", d2, b"f")
        .map_err(|_| "the rename over a file failed")?;
    resolves(fs, d2, b"f", g)?;
    absent(fs, d1, b"g")?;
    if fs.stat(f).map(|s| s.nlink) != Ok(0) {
        return Err("the displaced file still counted its link");
    }
    Ok(())
}

/// An unlinked name is absent, a name cached as absent appears when created,
/// and an orphan's record follows it through its detach and its release.
pub fn test_ext2_dcache_unlink_and_create() -> TestResult {
    on_fresh_mount(unlink_create_body)
}

#[inline(never)]
fn unlink_create_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let f = made(fs.create(root, b"u", FileType::Regular))?;
    resolves(fs, root, b"u", f)?;
    fs.unlink(root, b"u").map_err(|_| "the unlink failed")?;
    absent(fs, root, b"u")?;
    let again = made(fs.create(root, b"u", FileType::Regular))?;
    resolves(fs, root, b"u", again)?;

    absent(fs, root, b"fresh")?;
    let fresh = made(fs.create(root, b"fresh", FileType::Directory))?;
    resolves(fs, root, b"fresh", fresh)?;
    absent(fs, fresh, b"inner")?;
    let inner = made(fs.create(fresh, b"inner", FileType::Regular))?;
    resolves(fs, fresh, b"inner", inner)?;

    let orphan = made(fs.create(root, b"o", FileType::Regular))?;
    if stat2(fs, orphan).map(|s| s.nlink) != Ok(1) {
        return Err("a fresh file did not count one link");
    }
    if FileSystem::detach(fs, root, b"o") != Ok(Some(orphan)) {
        return Err("the last name of a file did not detach it");
    }
    absent(fs, root, b"o")?;
    if stat2(fs, orphan).map(|s| (s.nlink, s.mode & 0xF000)) != Ok((0, 0x8000)) {
        return Err("a detached file did not read as an unlinked regular file");
    }
    fs.release_detached(orphan)
        .map_err(|_| "the orphan release failed")?;
    if fs.stat(orphan).map(|s| s.mode) != Ok(0) {
        return Err("a released orphan still read as a file");
    }
    Ok(())
}

/// Every change a record takes is what the next `stat` reports: mode, size
/// both ways, the link count through `link` and `unlink`, times and the seal.
pub fn test_ext2_dcache_stat_follows_every_record_change() -> TestResult {
    on_fresh_mount(attr_body)
}

#[inline(never)]
fn attr_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let f = made(fs.create(root, b"a", FileType::Regular))?;
    if stat2(fs, f).map(|s| (s.mode & 0o777, s.size, s.nlink)) != Ok((0o644, 0, 1)) {
        return Err("a fresh file read with the wrong mode, size or link count");
    }
    fs.set_mode(f, 0o600).map_err(|_| "chmod failed")?;
    if stat2(fs, f).map(|s| s.mode & 0o777) != Ok(0o600) {
        return Err("stat missed a chmod");
    }
    if fs.write(f, 0, &[0x5a; 100]) != Ok(100) {
        return Err("the write failed");
    }
    if stat2(fs, f).map(|s| s.size) != Ok(100) {
        return Err("stat missed a write that grew the file");
    }
    fs.truncate(f, 10).map_err(|_| "the truncate failed")?;
    if stat2(fs, f).map(|s| s.size) != Ok(10) {
        return Err("stat missed a truncate");
    }

    absent(fs, root, b"b")?;
    fs.link(root, b"b", f).map_err(|_| "the link failed")?;
    resolves(fs, root, b"b", f)?;
    if stat2(fs, f).map(|s| s.nlink) != Ok(2) {
        return Err("stat missed a link");
    }
    fs.unlink(root, b"a").map_err(|_| "the unlink failed")?;
    absent(fs, root, b"a")?;
    if stat2(fs, f).map(|s| s.nlink) != Ok(1) {
        return Err("stat missed an unlink of one of two names");
    }

    fs.set_times(f, Some(1_000), Some(2_000))
        .map_err(|_| "utimes failed")?;
    if stat2(fs, f).map(|s| (s.atime, s.mtime)) != Ok((1_000, 2_000)) {
        return Err("stat missed a utimes");
    }
    fs.set_sealed(f).map_err(|_| "the seal failed")?;
    if !stat2(fs, f).is_ok_and(|s| s.sealed) {
        return Err("stat missed the seal");
    }
    Ok(())
}

/// A create that fails and rolls back after allocating its inode leaves the
/// name absent, and the create that later succeeds is seen.
pub fn test_ext2_dcache_failed_create_stays_absent() -> TestResult {
    on_fresh_mount(failed_create_body)
}

#[inline(never)]
fn failed_create_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let dir = made(fs.create(root, b"full", FileType::Directory))?;
    let big = made(fs.create(root, b"big", FileType::Regular))?;
    fill_volume(fs, big)?;
    fill_directory(fs, dir)?;
    let dir_size = stat2(fs, dir)
        .map_err(|_| "stat of the full directory failed")?
        .size;

    absent(fs, dir, b"probe")?;
    if fs.create(dir, b"probe", FileType::Regular) != Err(VfsError::NoSpace) {
        return Err("a create into a full directory on a full volume did not fail");
    }
    absent(fs, dir, b"probe")?;
    absent(fs, root, b"probedir")?;
    if fs.create(root, b"probedir", FileType::Directory) != Err(VfsError::NoSpace) {
        return Err("a mkdir on a full volume did not fail");
    }
    absent(fs, root, b"probedir")?;
    if fs.stat(dir).map(|s| s.size) != Ok(dir_size) {
        return Err("a failed create changed its directory's size");
    }

    fs.truncate(big, 0)
        .map_err(|_| "freeing the volume failed")?;
    let probe = made(fs.create(dir, b"probe", FileType::Regular))?;
    resolves(fs, dir, b"probe", probe)?;
    let probedir = made(fs.create(root, b"probedir", FileType::Directory))?;
    resolves(fs, root, b"probedir", probedir)
}

#[inline(never)]
fn fill_volume(fs: &Ext2Mount, file: InodeId) -> Check {
    let chunk = [0xa5u8; 1024];
    let mut at = 0u64;
    for _ in 0..IMAGE_BLOCKS * 2 {
        match fs.write(file, at, &chunk) {
            Ok(0) | Err(VfsError::NoSpace) => return Ok(()),
            Ok(n) => at += n as u64,
            Err(_) => return Err("filling the volume failed"),
        }
    }
    Err("the volume never filled")
}

/// Fill the directory's one block with ever shorter names until not even
/// `probe`'s entry fits: growing it would need a block the volume has not got.
#[inline(never)]
fn fill_directory(fs: &Ext2Mount, dir: InodeId) -> Check {
    let mut name = [b'x'; 200];
    let mut serial = 0u32;
    for len in [200usize, 100, 48, 24, 12, 8] {
        loop {
            serial += 1;
            name[0] = b'0' + (serial / 10 % 10) as u8;
            name[1] = b'0' + (serial % 10) as u8;
            match fs.create(dir, &name[..len], FileType::Regular) {
                Ok(_) => {}
                Err(VfsError::NoSpace) => break,
                Err(_) => return Err("filling the directory failed"),
            }
        }
    }
    Ok(())
}

/// A removed directory answers no name it was cached with, and its inode
/// number, handed to a new directory and then to a file, answers only for
/// what it now is.
pub fn test_ext2_dcache_reused_directory_number() -> TestResult {
    on_fresh_mount(reused_directory_body)
}

#[inline(never)]
fn reused_directory_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let old = made(fs.create(root, b"old", FileType::Directory))?;
    let x = made(fs.create(old, b"x", FileType::Regular))?;
    resolves(fs, old, b"x", x)?;
    absent(fs, old, b"y")?;
    fs.unlink(old, b"x").map_err(|_| "the unlink failed")?;
    absent(fs, old, b"x")?;
    fs.rmdir(root, b"old").map_err(|_| "the rmdir failed")?;
    absent(fs, root, b"old")?;
    for name in [b"x", b"y"] {
        if fs.lookup(old, name) != Err(VfsError::NotDirectory) {
            return Err("a removed directory still answered a name");
        }
    }
    if fs.stat(old).map(|s| s.mode) != Ok(0) {
        return Err("a removed directory still read as one");
    }

    let new = made(fs.create(root, b"new", FileType::Directory))?;
    if new != old {
        return Err("the fixture did not hand the freed number to the next directory");
    }
    if stat2(fs, new).map(|s| (s.file_type, s.nlink)) != Ok((FileType::Directory, 2)) {
        return Err("the reused number read as the removed directory");
    }
    absent(fs, new, b"x")?;
    absent(fs, new, b"y")?;
    let y = made(fs.create(new, b"y", FileType::Regular))?;
    resolves(fs, new, b"y", y)?;
    fs.unlink(new, b"y").map_err(|_| "the unlink failed")?;
    absent(fs, new, b"y")?;

    fs.rmdir(root, b"new")
        .map_err(|_| "the second rmdir failed")?;
    let file = made(fs.create(root, b"file", FileType::Regular))?;
    if file != old {
        return Err("the fixture did not hand the freed number to the next file");
    }
    if fs.lookup(file, b"y") != Err(VfsError::NotDirectory) {
        return Err("a file that reused a directory's number answered its names");
    }
    if stat2(fs, file).map(|s| (s.file_type, s.nlink)) != Ok((FileType::Regular, 1)) {
        return Err("the reused number read as the removed directory");
    }
    Ok(())
}

/// A directory renamed over an empty one takes its name, and the one it
/// displaced is freed with every name it was cached with.
pub fn test_ext2_dcache_rename_over_an_empty_directory() -> TestResult {
    on_fresh_mount(rename_over_dir_body)
}

#[inline(never)]
fn rename_over_dir_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let d1 = made(fs.create(root, b"d1", FileType::Directory))?;
    let d2 = made(fs.create(root, b"d2", FileType::Directory))?;
    let src = made(fs.create(d1, b"src", FileType::Directory))?;
    let dst = made(fs.create(d2, b"dst", FileType::Directory))?;
    let inner = made(fs.create(src, b"inner", FileType::Regular))?;
    resolves(fs, d1, b"src", src)?;
    resolves(fs, d2, b"dst", dst)?;
    resolves(fs, src, b"inner", inner)?;
    absent(fs, dst, b"inner")?;
    if stat2(fs, d1).map(|s| s.nlink) != Ok(3) || stat2(fs, d2).map(|s| s.nlink) != Ok(3) {
        return Err("a directory with one subdirectory did not count three links");
    }
    if stat2(fs, dst).map(|s| s.file_type) != Ok(FileType::Directory) {
        return Err("the target directory did not read as one");
    }

    fs.rename(d1, b"src", d2, b"dst")
        .map_err(|_| "the rename over an empty directory failed")?;
    absent(fs, d1, b"src")?;
    resolves(fs, d2, b"dst", src)?;
    resolves(fs, src, b"inner", inner)?;
    resolves(fs, src, b"..", d2)?;
    if fs.stat(d1).map(|s| s.nlink) != Ok(2) || fs.stat(d2).map(|s| s.nlink) != Ok(3) {
        return Err("the parents' link counts did not follow the rename");
    }
    if fs.stat(dst).map(|s| s.mode) != Ok(0) {
        return Err("the displaced directory still read as one");
    }
    if fs.lookup(dst, b"inner") != Err(VfsError::NotDirectory) {
        return Err("the displaced directory still answered a name");
    }
    Ok(())
}

/// A rename over a file that is still open keeps the displaced inode as an
/// orphan: unnamed, unlinked, still a file until it is released.
pub fn test_ext2_dcache_rename_over_an_open_file() -> TestResult {
    on_fresh_mount(rename_over_open_body)
}

#[inline(never)]
fn rename_over_open_body(fs: &'static Ext2Mount) -> Check {
    let root = fs.root_inode();
    let dir = made(fs.create(root, b"d", FileType::Directory))?;
    let a = made(fs.create(dir, b"a", FileType::Regular))?;
    let b = made(fs.create(dir, b"b", FileType::Regular))?;
    if fs.write(b, 0, b"displaced") != Ok(9) {
        return Err("the write failed");
    }
    resolves(fs, dir, b"a", a)?;
    resolves(fs, dir, b"b", b)?;
    if stat2(fs, b).map(|s| (s.nlink, s.size)) != Ok((1, 9)) {
        return Err("the file to displace read wrong");
    }

    if fs.rename_detaching(dir, b"a", dir, b"b") != Ok(Some(b)) {
        return Err("the rename did not answer the displaced orphan");
    }
    absent(fs, dir, b"a")?;
    resolves(fs, dir, b"b", a)?;
    if stat2(fs, b).map(|s| (s.nlink, s.file_type, s.size)) != Ok((0, FileType::Regular, 9)) {
        return Err("the orphan did not read as an unlinked file that kept its bytes");
    }
    if stat2(fs, a).map(|s| s.nlink) != Ok(1) {
        return Err("the renamed file's link count moved");
    }
    fs.release_detached(b)
        .map_err(|_| "the orphan release failed")?;
    if fs.stat(b).map(|s| s.mode) != Ok(0) {
        return Err("a released orphan still read as a file");
    }
    resolves(fs, dir, b"b", a)
}

/// A pool slot handed a different image answers from that image, not from
/// what it cached of the last one.
pub fn test_ext2_dcache_forgets_a_detached_image() -> TestResult {
    let (Some(first), Some(second)) = (
        image(Some((b"alpha", b"first"))),
        image(Some((b"beta", b"second-image"))),
    ) else {
        return TestResult::Skipped;
    };
    let Some(fs) = vfs_ext2_pool_claim() else {
        return slopos_testing::fail!("the ext2 pool handed out no instance");
    };
    let outcome = reattach_body(fs, first, second);
    vfs_ext2_pool_release(fs, false);
    match outcome {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!(msg),
    }
}

#[inline(never)]
fn reattach_body(
    fs: &'static Ext2Mount,
    first: KBox<dyn BlockDevice + Send + Sync>,
    second: KBox<dyn BlockDevice + Send + Sync>,
) -> Check {
    let root = fs.root_inode();
    fs.attach(first, false)
        .map_err(|_| "the first image would not attach")?;
    let file = fs
        .lookup(root, b"alpha")
        .map_err(|_| "the first image's file is missing")?;
    resolves(fs, root, b"alpha", file)?;
    absent(fs, root, b"beta")?;
    if stat2(fs, file).map(|s| s.size) != Ok(5) {
        return Err("the first image's file read with the wrong size");
    }
    if !fs.detach() {
        return Err("the first image would not detach");
    }

    fs.attach(second, false)
        .map_err(|_| "the second image would not attach")?;
    absent(fs, root, b"alpha")?;
    resolves(fs, root, b"beta", file)?;
    if stat2(fs, file).map(|s| s.size) != Ok(12) {
        return Err("stat answered from the detached image");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_dcache_rename_across_directories,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_dcache_unlink_and_create, suite = fs);
slopos_testing::stest!(
    name = test_ext2_dcache_stat_follows_every_record_change,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_dcache_failed_create_stays_absent,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_dcache_forgets_a_detached_image, suite = fs);
slopos_testing::stest!(name = test_ext2_dcache_reused_directory_number, suite = fs);
slopos_testing::stest!(
    name = test_ext2_dcache_rename_over_an_empty_directory,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_dcache_rename_over_an_open_file, suite = fs);
