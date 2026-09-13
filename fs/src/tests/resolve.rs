//! Path resolution: a working directory, symlink following and its budget,
//! the 255-byte name limit, hard links and `set_times`.

use slopos_abi::fs::UserFsEntry;
use slopos_ostd::KVec;
use slopos_testing::TestResult;

use crate::ext2::Ext2Fs;
use crate::vfs::{
    FileType, RESOLVE_FOLLOW, RESOLVE_NOFOLLOW_FINAL, VfsError, VfsResult, resolve_parent_at,
    resolve_path, resolve_path_at, vfs_link, vfs_list, vfs_mkdir, vfs_open, vfs_open_flags_at,
    vfs_stat, vfs_stat_at, vfs_symlink_at, vfs_unlink, vfs_utimens,
};

use super::{ensure_vfs_ready, phase3_image, with_mounted};

const EXT2_ROOT: u32 = 2;

/// `dir` then `/` then `name`, on the heap: these paths do not belong on a
/// 2 KiB frame.
fn joined(dir: &[u8], name: &[u8]) -> VfsResult<KVec<u8>> {
    let mut out = KVec::with_capacity(dir.len() + 1 + name.len()).map_err(|_| VfsError::NoSpace)?;
    out.extend_from_slice(dir).map_err(|_| VfsError::NoSpace)?;
    out.push(b'/').map_err(|_| VfsError::NoSpace)?;
    out.extend_from_slice(name).map_err(|_| VfsError::NoSpace)?;
    Ok(out)
}

/// A fresh fixture directory: whatever a previous run left is removed first.
#[inline(never)]
fn fixture_dir(name: &[u8]) -> Option<KVec<u8>> {
    let dir = joined(b"/tmp", name).ok()?;
    match vfs_mkdir(dir.as_slice()) {
        Ok(()) | Err(VfsError::AlreadyExists) => Some(dir),
        Err(_) => None,
    }
}

#[inline(never)]
fn seed_file(path: &[u8], payload: &[u8]) -> bool {
    let _ = vfs_unlink(path);
    match vfs_open(path, true) {
        Ok(handle) => handle.write(0, payload).is_ok(),
        Err(_) => false,
    }
}

#[inline(never)]
fn read_back(path: &[u8], want: &[u8]) -> bool {
    let Ok(handle) = vfs_open(path, false) else {
        return false;
    };
    let mut buf = [0u8; 64];
    match handle.read(0, &mut buf) {
        Ok(n) => &buf[..n] == want,
        Err(_) => false,
    }
}

/// `open("src/main.rs")` with a cwd of `/work` must resolve.
pub fn test_resolve_relative_against_cwd() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rel_cwd") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(file) = joined(dir.as_slice(), b"main.rs") else {
        return TestResult::Fail;
    };
    if !seed_file(file.as_slice(), b"fn main() {}") {
        return slopos_testing::fail!("could not seed the fixture file");
    }

    // The task's own cwd slice is NUL-terminated, so the trailing NUL has to
    // be tolerated here exactly as it is on the syscall path.
    let Ok(mut cwd_nul) = joined(b"/tmp", b"rel_cwd") else {
        return TestResult::Fail;
    };
    if cwd_nul.push(0).is_err() {
        return TestResult::Fail;
    }

    let resolved = match resolve_path_at(b"main.rs", cwd_nul.as_slice(), RESOLVE_FOLLOW) {
        Ok(r) => r,
        Err(e) => return slopos_testing::fail!("a relative path did not resolve: {:?}", e),
    };
    let direct = match resolve_path(file.as_slice()) {
        Ok(r) => r,
        Err(e) => return slopos_testing::fail!("the absolute path did not resolve: {:?}", e),
    };
    if resolved.inode != direct.inode {
        return slopos_testing::fail!("the cwd-relative path named a different inode");
    }

    match vfs_open_flags_at(
        b"main.rs",
        dir.as_slice(),
        crate::vfs::VfsOpenFlags::read_only(),
        RESOLVE_FOLLOW,
    ) {
        Ok(handle) => {
            let mut buf = [0u8; 32];
            match handle.read(0, &mut buf) {
                Ok(n) if &buf[..n] == b"fn main() {}" => {}
                _ => return slopos_testing::fail!("the relative open read the wrong bytes"),
            }
        }
        Err(e) => return slopos_testing::fail!("a relative open failed: {:?}", e),
    }

    // `..` from the cwd, which only the rewind can resolve.
    let Ok(parent_probe) = joined(b"..", b"rel_cwd") else {
        return TestResult::Fail;
    };
    let Ok(via_parent) = joined(parent_probe.as_slice(), b"main.rs") else {
        return TestResult::Fail;
    };
    match resolve_path_at(via_parent.as_slice(), dir.as_slice(), RESOLVE_FOLLOW) {
        Ok(r) if r.inode == direct.inode => {}
        other => return slopos_testing::fail!("`..` from a cwd misresolved: {:?}", other.is_ok()),
    }

    let _ = vfs_unlink(file.as_slice());
    TestResult::Pass
}

/// A symlink to a file is followed by `open`, whether its target is relative
/// or absolute; a symlink to a directory is traversed mid-path.
///
/// Split into phases to stay inside the 2 KiB frame budget.
pub fn test_resolve_follows_symlinks() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rsym") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    match follows_file_links(dir.as_slice()) {
        TestResult::Pass => {}
        other => return other,
    }
    follows_dir_link_midpath(dir.as_slice())
}

#[inline(never)]
fn follows_file_links(dir: &[u8]) -> TestResult {
    let Ok(file) = joined(dir, b"target.txt") else {
        return TestResult::Fail;
    };
    if !seed_file(file.as_slice(), b"followed") {
        return slopos_testing::fail!("could not seed the fixture file");
    }

    let Ok(rel_link) = joined(dir, b"rel.lnk") else {
        return TestResult::Fail;
    };
    let Ok(abs_link) = joined(dir, b"abs.lnk") else {
        return TestResult::Fail;
    };
    let _ = vfs_unlink(rel_link.as_slice());
    let _ = vfs_unlink(abs_link.as_slice());
    if let Err(e) = vfs_symlink_at(b"target.txt", rel_link.as_slice(), b"/") {
        return match e {
            VfsError::NotSupported => TestResult::Skipped,
            other => slopos_testing::fail!("relative symlink creation failed: {:?}", other),
        };
    }
    if let Err(e) = vfs_symlink_at(file.as_slice(), abs_link.as_slice(), b"/") {
        return slopos_testing::fail!("absolute symlink creation failed: {:?}", e);
    }

    if !read_back(rel_link.as_slice(), b"followed") {
        return slopos_testing::fail!("a relative symlink was not followed on open");
    }
    if !read_back(abs_link.as_slice(), b"followed") {
        return slopos_testing::fail!("an absolute symlink was not followed on open");
    }
    match vfs_stat(rel_link.as_slice()) {
        Ok(stat) if stat.file_type == FileType::Regular => {}
        other => return slopos_testing::fail!("stat through a symlink: {:?}", other.is_ok()),
    }
    match vfs_stat_at(rel_link.as_slice(), b"/", RESOLVE_NOFOLLOW_FINAL) {
        Ok(stat) if stat.file_type == FileType::Symlink => TestResult::Pass,
        other => slopos_testing::fail!(
            "NOFOLLOW_FINAL did not answer the link: {:?}",
            other.map(|s| s.file_type)
        ),
    }
}

/// A symlink named by an intermediate component, and a `resolve_parent` that
/// follows it while leaving the final name alone.
#[inline(never)]
fn follows_dir_link_midpath(dir: &[u8]) -> TestResult {
    let Ok(inner_dir) = joined(dir, b"real") else {
        return TestResult::Fail;
    };
    let _ = vfs_mkdir(inner_dir.as_slice());
    let Ok(inner_file) = joined(inner_dir.as_slice(), b"inner.txt") else {
        return TestResult::Fail;
    };
    if !seed_file(inner_file.as_slice(), b"midpath") {
        return slopos_testing::fail!("could not seed the mid-path fixture");
    }
    let Ok(dir_link) = joined(dir, b"dir.lnk") else {
        return TestResult::Fail;
    };
    let _ = vfs_unlink(dir_link.as_slice());
    if let Err(e) = vfs_symlink_at(b"real", dir_link.as_slice(), b"/") {
        return slopos_testing::fail!("directory symlink creation failed: {:?}", e);
    }
    let Ok(through_link) = joined(dir_link.as_slice(), b"inner.txt") else {
        return TestResult::Fail;
    };
    if !read_back(through_link.as_slice(), b"midpath") {
        return slopos_testing::fail!("a symlinked directory was not traversed mid-path");
    }

    match resolve_parent_at(through_link.as_slice(), b"/") {
        Ok((parent, name)) => {
            if name.as_bytes() != b"inner.txt" {
                return slopos_testing::fail!("resolve_parent lost the final component");
            }
            let Ok(real) = resolve_path(inner_dir.as_slice()) else {
                return TestResult::Fail;
            };
            if parent.inode != real.inode {
                return slopos_testing::fail!("resolve_parent did not follow the link");
            }
            TestResult::Pass
        }
        Err(e) => slopos_testing::fail!("resolve_parent through a link: {:?}", e),
    }
}

/// A link pointing at itself must be `ELOOP`, not a hang.
pub fn test_resolve_symlink_loop_is_eloop() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rloop") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(link) = joined(dir.as_slice(), b"self.lnk") else {
        return TestResult::Fail;
    };
    let _ = vfs_unlink(link.as_slice());
    if let Err(e) = vfs_symlink_at(b"self.lnk", link.as_slice(), b"/") {
        return match e {
            VfsError::NotSupported => TestResult::Skipped,
            other => slopos_testing::fail!("self-link creation failed: {:?}", other),
        };
    }
    match resolve_path(link.as_slice()) {
        Err(VfsError::TooManySymlinks) => TestResult::Pass,
        Err(other) => slopos_testing::fail!("want TooManySymlinks, got {:?}", other),
        Ok(_) => slopos_testing::fail!("a self-referential symlink resolved"),
    }
}

/// Names in a chain fixture: `l0` … `l<n>`.
fn chain_name(index: usize, out: &mut [u8; 4]) -> &[u8] {
    out[0] = b'l';
    let tens = index / 10;
    if tens > 0 {
        out[1] = b'0' + (tens % 10) as u8;
        out[2] = b'0' + (index % 10) as u8;
        &out[..3]
    } else {
        out[1] = b'0' + (index % 10) as u8;
        &out[..2]
    }
}

/// `links` links in a row, the last pointing at a regular file holding
/// `payload`. Every target is relative, so the whole chain lives in `dir`.
#[inline(never)]
fn build_chain(dir: &[u8], links: usize, payload: &[u8]) -> VfsResult<KVec<u8>> {
    let target = joined(dir, b"t")?;
    if vfs_open(target.as_slice(), true)
        .and_then(|h| h.write(0, payload))
        .is_err()
    {
        return Err(VfsError::IoError);
    }
    for i in (0..links).rev() {
        let mut link_buf = [0u8; 4];
        let mut next_buf = [0u8; 4];
        let link = joined(dir, chain_name(i, &mut link_buf))?;
        let next: &[u8] = if i + 1 == links {
            b"t"
        } else {
            chain_name(i + 1, &mut next_buf)
        };
        let _ = vfs_unlink(link.as_slice());
        vfs_symlink_at(next, link.as_slice(), b"/")?;
    }
    let mut head_buf = [0u8; 4];
    joined(dir, chain_name(0, &mut head_buf))
}

/// The budget is the whole path's: `MAX_SYMLINK_FOLLOWS` expansions resolve,
/// and one more is `ELOOP`.
pub fn test_resolve_symlink_budget_is_whole_path() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let budget = crate::MAX_SYMLINK_FOLLOWS as usize;
    let Some(ok_dir) = fixture_dir(b"rchain_ok") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let head = match build_chain(ok_dir.as_slice(), budget, b"chained") {
        Ok(head) => head,
        Err(VfsError::NotSupported) => return TestResult::Skipped,
        Err(e) => return slopos_testing::fail!("could not build a {}-link chain: {:?}", budget, e),
    };
    if !read_back(head.as_slice(), b"chained") {
        return slopos_testing::fail!("a chain of {} links did not resolve", budget);
    }

    let Some(over_dir) = fixture_dir(b"rchain_over") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let over = match build_chain(over_dir.as_slice(), budget + 1, b"chained") {
        Ok(head) => head,
        Err(e) => {
            return slopos_testing::fail!("could not build a {}-link chain: {:?}", budget + 1, e);
        }
    };
    match resolve_path(over.as_slice()) {
        Err(VfsError::TooManySymlinks) => TestResult::Pass,
        Err(other) => slopos_testing::fail!("want TooManySymlinks, got {:?}", other),
        Ok(_) => slopos_testing::fail!("a chain of {} links resolved", budget + 1),
    }
}

/// A name of exactly `MAX_NAME_LEN` is creatable, listable and openable; one
/// byte more is `ENAMETOOLONG`.
pub fn test_resolve_max_name_roundtrips() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rname") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(name) = KVec::filled(b'n', crate::MAX_NAME_LEN) else {
        return TestResult::Fail;
    };
    let Ok(path) = joined(dir.as_slice(), name.as_slice()) else {
        return TestResult::Fail;
    };
    let _ = vfs_unlink(path.as_slice());
    if !seed_file(path.as_slice(), b"at-the-limit") {
        return slopos_testing::fail!("a {}-byte name was refused", crate::MAX_NAME_LEN);
    }
    if !read_back(path.as_slice(), b"at-the-limit") {
        return slopos_testing::fail!("a name at the limit did not re-open");
    }

    let Ok(mut entries) = KVec::filled(UserFsEntry::new(), 8) else {
        return TestResult::Fail;
    };
    let count = match vfs_list(dir.as_slice(), &mut entries) {
        Ok(n) => n,
        Err(e) => return slopos_testing::fail!("listing the fixture failed: {:?}", e),
    };
    let mut listed = false;
    for entry in entries.iter().take(count) {
        if entry.name_str().as_bytes() == name.as_slice() {
            listed = true;
        }
    }
    if !listed {
        return slopos_testing::fail!("a name at the limit listed truncated or not at all");
    }

    let Ok(mut over) = joined(dir.as_slice(), name.as_slice()) else {
        return TestResult::Fail;
    };
    if over.push(b'n').is_err() {
        return TestResult::Fail;
    }
    let outcome = match vfs_open(over.as_slice(), true) {
        Err(VfsError::NameTooLong) => TestResult::Pass,
        Err(other) => slopos_testing::fail!("want NameTooLong, got {:?}", other),
        Ok(_) => slopos_testing::fail!("a name past the limit was created"),
    };
    let _ = vfs_unlink(path.as_slice());
    outcome
}

/// A path near `MAX_PATH_LEN` resolves; past it is `ENAMETOOLONG`. `..`
/// above the root is absorbed rather than escaping it.
pub fn test_resolve_path_length_boundary() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rlong") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(file) = joined(dir.as_slice(), b"f") else {
        return TestResult::Fail;
    };
    if !seed_file(file.as_slice(), b"long-path") {
        return slopos_testing::fail!("could not seed the fixture file");
    }

    // `/tmp/rlong/./././…/f`, a few bytes short of the limit: the length is
    // the *input*'s, which canonicalisation then collapses.
    let Ok(mut long) = joined(b"/tmp", b"rlong") else {
        return TestResult::Fail;
    };
    while long.len() + 4 < crate::MAX_PATH_LEN {
        if long.extend_from_slice(b"/.").is_err() {
            return TestResult::Fail;
        }
    }
    if long.extend_from_slice(b"/f").is_err() {
        return TestResult::Fail;
    }
    if long.len() > crate::MAX_PATH_LEN {
        return slopos_testing::fail!("the probe path overshot the limit");
    }
    if !read_back(long.as_slice(), b"long-path") {
        return slopos_testing::fail!("a {}-byte path did not resolve", long.len());
    }

    let mut over = long;
    while over.len() <= crate::MAX_PATH_LEN {
        if over.push(b'x').is_err() {
            return TestResult::Fail;
        }
    }
    match vfs_stat(over.as_slice()) {
        Err(VfsError::NameTooLong) => {}
        other => {
            return slopos_testing::fail!(
                "a path past {} bytes was not refused: {:?}",
                crate::MAX_PATH_LEN,
                other.is_ok()
            );
        }
    }

    // Absorbed, not escaped: `/../..` names the root.
    let Ok(root) = resolve_path(b"/") else {
        return TestResult::Fail;
    };
    match resolve_path(b"/../../..") {
        Ok(r) if r.inode == root.inode => {}
        other => return slopos_testing::fail!("`..` above the root escaped: {:?}", other.is_ok()),
    }
    let Ok(above) = joined(b"/..", b"tmp") else {
        return TestResult::Fail;
    };
    let Ok(above) = joined(above.as_slice(), b"rlong/f") else {
        return TestResult::Fail;
    };
    if !read_back(above.as_slice(), b"long-path") {
        return slopos_testing::fail!("`..` above the root broke a path that follows it");
    }

    let _ = vfs_unlink(file.as_slice());
    TestResult::Pass
}

/// `link(2)` refuses what it must: a second filesystem, and a directory.
pub fn test_vfs_link_refuses_cross_mount_and_directories() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rlink") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(file) = joined(dir.as_slice(), b"src") else {
        return TestResult::Fail;
    };
    if !seed_file(file.as_slice(), b"linked") {
        return slopos_testing::fail!("could not seed the fixture file");
    }

    // `/tmp` is its own RamFs instance, so a link from it to the root crosses
    // filesystems whatever the root happens to be.
    match vfs_link(file.as_slice(), b"/rlink_probe", b"/") {
        Err(VfsError::CrossDevice) => {}
        other => {
            let _ = vfs_unlink(b"/rlink_probe");
            return slopos_testing::fail!("want CrossDevice, got {:?}", other);
        }
    }

    let Ok(dir_link) = joined(dir.as_slice(), b"dirlink") else {
        return TestResult::Fail;
    };
    match vfs_link(dir.as_slice(), dir_link.as_slice(), b"/") {
        Err(VfsError::PermissionDenied) => {}
        other => {
            let _ = vfs_unlink(dir_link.as_slice());
            return slopos_testing::fail!("want PermissionDenied for a directory, got {:?}", other);
        }
    }

    let _ = vfs_unlink(file.as_slice());
    TestResult::Pass
}

/// A hard link is a second name for one inode, and the first `unlink` takes
/// the name without taking the file.
pub fn test_ext2_hard_link_shares_the_inode() -> TestResult {
    let Some(device) = phase3_image(b"orig.txt", b"hardlinked") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, hard_link_inner) {
        Ok(()) => TestResult::Pass,
        Err(e) => slopos_testing::fail!("{}", e),
    }
}

#[inline(never)]
fn hard_link_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let orig = fs
        .resolve_path(b"/orig.txt")
        .map_err(|_| "resolve source")?;
    fs.link_entry(EXT2_ROOT, b"second.txt", orig)
        .map_err(|_| "link_entry refused a regular file")?;

    let second = fs
        .resolve_path(b"/second.txt")
        .map_err(|_| "the new name does not resolve")?;
    if second != orig {
        return Err("the link named a different inode");
    }
    if fs.read_inode(orig).map_err(|_| "read inode")?.links_count != 2 {
        return Err("links_count did not reach 2");
    }

    // A directory is refused, and so is a name that already exists.
    if fs.link_entry(EXT2_ROOT, b"dirlink", EXT2_ROOT).is_ok() {
        return Err("a directory was hard-linked");
    }
    if fs.link_entry(EXT2_ROOT, b"second.txt", orig).is_ok() {
        return Err("a duplicate name was accepted");
    }

    fs.unlink_entry(EXT2_ROOT, b"orig.txt")
        .map_err(|_| "unlink the first name")?;
    if fs.resolve_path(b"/orig.txt").is_ok() {
        return Err("the unlinked name still resolves");
    }
    let mut buf = [0u8; 32];
    let read = fs
        .read_file(second, 0, &mut buf)
        .map_err(|_| "the surviving name lost its contents")?;
    if &buf[..read] != b"hardlinked" {
        return Err("the surviving name read the wrong bytes");
    }
    if fs.read_inode(orig).map_err(|_| "read inode")?.links_count != 1 {
        return Err("links_count did not fall back to 1");
    }
    Ok(())
}

/// `set_times` is observable through `stat`, on the RAM root and on ext2.
pub fn test_set_times_is_observable_through_stat() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rtimes") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(file) = joined(dir.as_slice(), b"stamped") else {
        return TestResult::Fail;
    };
    if !seed_file(file.as_slice(), b"times") {
        return slopos_testing::fail!("could not seed the fixture file");
    }

    if let Err(e) = vfs_utimens(
        file.as_slice(),
        b"/",
        Some(1_000_000),
        Some(2_000_000),
        RESOLVE_FOLLOW,
    ) {
        return slopos_testing::fail!("utimens on a RAM file: {:?}", e);
    }
    let stat = match vfs_stat(file.as_slice()) {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("stat after utimens: {:?}", e),
    };
    if stat.atime != 1_000_000 || stat.mtime != 2_000_000 {
        return slopos_testing::fail!(
            "times did not round-trip: atime {}, mtime {}",
            stat.atime,
            stat.mtime
        );
    }

    // `None` is `UTIME_OMIT`: the other field must not move.
    if vfs_utimens(file.as_slice(), b"/", None, Some(3_000_000), RESOLVE_FOLLOW).is_err() {
        return slopos_testing::fail!("a partial utimens was refused");
    }
    let stat = match vfs_stat(file.as_slice()) {
        Ok(s) => s,
        Err(e) => return slopos_testing::fail!("stat after a partial utimens: {:?}", e),
    };
    if stat.atime != 1_000_000 || stat.mtime != 3_000_000 {
        return slopos_testing::fail!("an omitted field was overwritten");
    }

    let _ = vfs_unlink(file.as_slice());
    TestResult::Pass
}

/// The ext2 half, through the filesystem rather than the VFS, so the on-disk
/// inode fields are what is checked.
pub fn test_ext2_set_times_roundtrips() -> TestResult {
    let Some(device) = phase3_image(b"stamped.txt", b"times") else {
        return TestResult::Skipped;
    };
    match with_mounted(&device, set_times_inner) {
        Ok(()) => TestResult::Pass,
        Err(e) => slopos_testing::fail!("{}", e),
    }
}

#[inline(never)]
fn set_times_inner(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs
        .resolve_path(b"/stamped.txt")
        .map_err(|_| "resolve the fixture")?;
    fs.set_times(ino, Some(1_700_000_000), Some(1_700_000_001))
        .map_err(|_| "set_times refused")?;
    let inode = fs.read_inode(ino).map_err(|_| "read inode")?;
    if inode.atime != 1_700_000_000 || inode.mtime != 1_700_000_001 {
        return Err("the times did not reach the inode");
    }

    fs.set_times(ino, None, Some(1_700_000_002))
        .map_err(|_| "a partial set_times refused")?;
    let inode = fs.read_inode(ino).map_err(|_| "read inode")?;
    if inode.atime != 1_700_000_000 || inode.mtime != 1_700_000_002 {
        return Err("an omitted field was overwritten");
    }
    // Past what a 32-bit ext2 timestamp holds: refused, not truncated.
    if fs
        .set_times(ino, Some(u64::from(u32::MAX) + 1), None)
        .is_ok()
    {
        return Err("an out-of-range time was accepted");
    }
    Ok(())
}

/// `..` names the parent of the directory the walk actually reached, so a
/// symlinked component is traversed rather than erased: resolved lexically,
/// `link/../probe` would name a `probe` beside the *link*.
pub fn test_resolve_dotdot_is_taken_against_the_symlink_target() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rdotdot") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    match build_dotdot_fixture(dir.as_slice()) {
        TestResult::Pass => {}
        other => return other,
    }
    dotdot_reads_beside_the_target(dir.as_slice())
}

/// `<dir>/real/inner` reached through `<dir>/link`, with a decoy `probe`
/// beside the link and the real one beside the target's parent.
#[inline(never)]
fn build_dotdot_fixture(dir: &[u8]) -> TestResult {
    let Ok(real) = joined(dir, b"real") else {
        return TestResult::Fail;
    };
    let _ = vfs_mkdir(real.as_slice());
    let Ok(inner) = joined(real.as_slice(), b"inner") else {
        return TestResult::Fail;
    };
    let _ = vfs_mkdir(inner.as_slice());

    let Ok(beside_target) = joined(real.as_slice(), b"probe.txt") else {
        return TestResult::Fail;
    };
    let Ok(beside_link) = joined(dir, b"probe.txt") else {
        return TestResult::Fail;
    };
    if !seed_file(beside_target.as_slice(), b"beside-target")
        || !seed_file(beside_link.as_slice(), b"beside-link")
    {
        return slopos_testing::fail!("could not seed the probe files");
    }

    let Ok(rel_link) = joined(dir, b"link") else {
        return TestResult::Fail;
    };
    let Ok(abs_link) = joined(dir, b"alink") else {
        return TestResult::Fail;
    };
    let _ = vfs_unlink(rel_link.as_slice());
    let _ = vfs_unlink(abs_link.as_slice());
    if let Err(e) = vfs_symlink_at(b"real/inner", rel_link.as_slice(), b"/") {
        return match e {
            VfsError::NotSupported => TestResult::Skipped,
            other => slopos_testing::fail!("relative symlink creation failed: {:?}", other),
        };
    }
    if let Err(e) = vfs_symlink_at(inner.as_slice(), abs_link.as_slice(), b"/") {
        return slopos_testing::fail!("absolute symlink creation failed: {:?}", e);
    }
    TestResult::Pass
}

#[inline(never)]
fn dotdot_reads_beside_the_target(dir: &[u8]) -> TestResult {
    // `link` -> `real/inner`, so `link/..` is `real` and the probe beside it
    // is `real/probe.txt`, not the one beside the link.
    for link in [b"link".as_slice(), b"alink".as_slice()] {
        let Ok(base) = joined(dir, link) else {
            return TestResult::Fail;
        };
        let Ok(probe) = joined(base.as_slice(), b"../probe.txt") else {
            return TestResult::Fail;
        };
        if !read_back(probe.as_slice(), b"beside-target") {
            return slopos_testing::fail!(
                "`..` after a symlink resolved lexically, erasing the link"
            );
        }
    }

    // The other direction: a `..` cannot excuse a component that is not
    // there. Lexically `real/missing/..` collapses to `real` and succeeds.
    let Ok(through_missing) = joined(dir, b"real/missing/..") else {
        return TestResult::Fail;
    };
    match vfs_stat(through_missing.as_slice()) {
        Err(VfsError::NotFound) => {}
        other => {
            return slopos_testing::fail!(
                "a `..` past a missing component resolved: {:?}",
                other.is_ok()
            );
        }
    }

    // A trailing `..` is not a name to remove: lexically it named the
    // grandparent.
    let Ok(trailing) = joined(dir, b"real/inner/..") else {
        return TestResult::Fail;
    };
    if crate::vfs::vfs_rmdir(trailing.as_slice()).is_ok() {
        return slopos_testing::fail!("rmdir accepted a trailing `..`");
    }
    let Ok(real) = joined(dir, b"real") else {
        return TestResult::Fail;
    };
    match vfs_stat(real.as_slice()) {
        Ok(stat) if stat.file_type == FileType::Directory => TestResult::Pass,
        _ => slopos_testing::fail!("rmdir of a trailing `..` removed the parent"),
    }
}

/// A hard link over a sealed name is refused at the VFS, as every other
/// mutation of a sealed name is. `link` checked only its source, so a second,
/// unsealed inode could be installed at a sealed name.
pub fn test_vfs_link_refuses_a_sealed_destination() -> TestResult {
    if !ensure_vfs_ready() {
        return TestResult::Fail;
    }
    let Some(dir) = fixture_dir(b"rlinkseal") else {
        return slopos_testing::fail!("could not build the fixture directory");
    };
    let Ok(source) = joined(dir.as_slice(), b"src") else {
        return TestResult::Fail;
    };
    let Ok(sealed) = joined(dir.as_slice(), b"sealed") else {
        return TestResult::Fail;
    };
    if !seed_file(source.as_slice(), b"linked") {
        return slopos_testing::fail!("could not seed the link source");
    }
    // Sealing is one-way, so this name is spent for the rest of the boot —
    // which is why it is its own name and not the source.
    if !seed_file(sealed.as_slice(), b"sealed") {
        return slopos_testing::fail!("could not seed the seal target");
    }
    if let Err(e) = crate::vfs::vfs_set_sealed(sealed.as_slice()) {
        return match e {
            VfsError::NotSupported => TestResult::Skipped,
            other => slopos_testing::fail!("could not seal the fixture: {:?}", other),
        };
    }

    match vfs_link(source.as_slice(), sealed.as_slice(), b"/") {
        Err(VfsError::PermissionDenied) => {}
        other => return slopos_testing::fail!("want PermissionDenied, got {:?}", other),
    }
    // Still one inode behind the sealed name, and still the sealed one.
    match vfs_stat(sealed.as_slice()) {
        Ok(stat) if stat.sealed => {}
        other => return slopos_testing::fail!("the seal did not survive: {:?}", other.is_ok()),
    }
    if !read_back(sealed.as_slice(), b"sealed") {
        return slopos_testing::fail!("the sealed name reads other bytes");
    }

    let _ = vfs_unlink(source.as_slice());
    TestResult::Pass
}

slopos_testing::stest!(name = test_resolve_relative_against_cwd, suite = fs);
slopos_testing::stest!(name = test_resolve_follows_symlinks, suite = fs);
slopos_testing::stest!(name = test_resolve_symlink_loop_is_eloop, suite = fs);
slopos_testing::stest!(name = test_resolve_symlink_budget_is_whole_path, suite = fs);
slopos_testing::stest!(name = test_resolve_max_name_roundtrips, suite = fs);
slopos_testing::stest!(name = test_resolve_path_length_boundary, suite = fs);
slopos_testing::stest!(
    name = test_vfs_link_refuses_cross_mount_and_directories,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_hard_link_shares_the_inode, suite = fs);
slopos_testing::stest!(name = test_set_times_is_observable_through_stat, suite = fs);
slopos_testing::stest!(name = test_ext2_set_times_roundtrips, suite = fs);
slopos_testing::stest!(
    name = test_resolve_dotdot_is_taken_against_the_symlink_target,
    suite = fs
);
slopos_testing::stest!(
    name = test_vfs_link_refuses_a_sealed_destination,
    suite = fs
);
