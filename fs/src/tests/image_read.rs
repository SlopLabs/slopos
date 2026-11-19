//! What a file's pages cost to fault in from an image, against the same file
//! on the filesystem the image is a file of: one report line, graded by
//! `run_tests gate fsperf`.
//!
//! ```text
//! FSIMG[tests]: bytes=N coldroot=N coldimage=N warmroot=N warmimage=N
//! ```
//!
//! The data file and the image are written to the root disk, sealed and
//! synced, and the disk mounted again, so the first pass over each reads the
//! device; the second finds the page sets the first one left, as a program
//! started again does. Both are taken on the same disk in the same run, so
//! their quotient describes the image's read path rather than the host.

use slopos_abi::extension::DIGEST_LEN;
use slopos_abi::fs::S_IFREG;
use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_kernel_services::clock::monotonic_ns;
use slopos_ostd::KVec;
use slopos_ostd::authority::{Capability, Seal, check_mask};
use slopos_ostd::process::AccountId;
use slopos_testing::{TestResult, fail};

use super::extensions::image_parts;
use super::suite_disks::{TestDisk, suite_disk};
use crate::extension::{self, deactivate};
use crate::filemap;
use crate::vfs::{
    resolve_path, vfs_ext4_mount_named, vfs_ext4_unmount_named, vfs_mkdir, vfs_open, vfs_rmdir,
    vfs_set_sealed, vfs_stat, vfs_unlink,
};

const MOUNT: &[u8] = b"/fsimgroot";
const DATA: &[u8] = b"/fsimgroot/fsimg.dat";
const IMAGE: &[u8] = b"/fsimgroot/fsimg.ext";
const NAME: &[u8] = b"fsimg";
const SHOWN: &[u8] = b"/opt/fsimg/data";

/// The file each pass faults in: the most an image built in one allocation
/// holds, its volume under 1 MiB.
const BYTES: usize = 768 * 1024;
const PAGE: usize = 4096;
const PAGES: u32 = (BYTES / PAGE) as u32;

/// Bytes no block of which is zeros, which the tree's hash answers without
/// hashing.
#[inline(never)]
fn data() -> Option<KVec<u8>> {
    let mut bytes = KVec::<u8>::zeroed(BYTES).ok()?;
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add((i / PAGE) as u8) | 1;
    }
    Some(bytes)
}

/// Write `parts` to `path` back to back.
fn write_file(path: &[u8], parts: &[&[u8]]) -> Result<(), &'static str> {
    let file = vfs_open(path, true).map_err(|_| "a measured file could not be made")?;
    let mut at = 0u64;
    for part in parts {
        let mut done = 0;
        while done < part.len() {
            match file.write(at + done as u64, &part[done..]) {
                Ok(n) if n > 0 => done += n,
                _ => return Err("a measured file could not be written"),
            }
        }
        at += part.len() as u64;
    }
    Ok(())
}

/// The data file and an image holding it as `data`, sealed and synced;
/// answers the image's root. What a boot that died mid-test left goes first.
#[inline(never)]
fn stage(bytes: &[u8]) -> Result<[u8; DIGEST_LEN], &'static str> {
    unstage();
    let (volume, tree, root) =
        image_parts(&[("data", S_IFREG | 0o755, bytes)]).ok_or("no room for the image")?;
    write_file(DATA, &[bytes])?;
    volume.with_buffer_mut(|vol| write_file(IMAGE, &[vol, tree.as_slice()]))?;
    vfs_set_sealed(IMAGE).map_err(|_| "the image could not be sealed")?;
    crate::vfs::vfs_sync_all().map_err(|_| "the root could not be synced")?;
    Ok(root)
}

/// Fault every page of `path` in through a page set, as a mapping of it
/// would, and answer the nanoseconds; then hold the pages to `want`.
#[inline(never)]
fn fault_in(path: &[u8], want: &[u8]) -> Result<u64, &'static str> {
    let resolved = resolve_path(path).map_err(|_| "a measured file did not resolve")?;
    let start = monotonic_ns();
    let map = filemap::reserve_range(
        resolved.fs,
        resolved.inode,
        0,
        PAGES,
        false,
        AccountId::NONE,
    )
    .map_err(|_| "a measured file could not be mapped")?;
    let mut taken = 0;
    for page in 0..u64::from(PAGES) {
        if filemap::fault_page_in_set(map, page, true).is_err() {
            break;
        }
        taken += 1;
    }
    let ns = monotonic_ns().saturating_sub(start);
    let same = read_back(resolved.fs, resolved.inode, want);
    filemap::release(map, PAGES + taken);
    filemap::drain_pending();
    if taken != PAGES {
        return Err("a page of a measured file did not fault in");
    }
    same?;
    Ok(ns)
}

/// What the page set holds of the file, held to `want`.
#[inline(never)]
fn read_back(
    fs: &'static dyn crate::vfs::FileSystem,
    inode: crate::vfs::InodeId,
    want: &[u8],
) -> Result<(), &'static str> {
    let mut page = KVec::<u8>::zeroed(PAGE).map_err(|_| "no room to read back")?;
    for (k, expected) in want.chunks(PAGE).enumerate() {
        let at = (k * PAGE) as u64;
        let got = crate::vfs_file_ops::read_chunk(fs, inode, at, page.as_mut_slice())
            .map_err(|_| "a faulted page did not read back")?;
        if page.as_slice()[..got] != *expected {
            return Err("a page faulted in from the image holds other bytes than the file");
        }
    }
    Ok(())
}

/// Cold then warm, root then image, each its own pass.
#[inline(never)]
fn measure(bytes: &[u8]) -> Result<[u64; 4], &'static str> {
    let coldroot = fault_in(DATA, bytes)?;
    let coldimage = fault_in(SHOWN, bytes)?;
    let warmroot = fault_in(DATA, bytes)?;
    let warmimage = fault_in(SHOWN, bytes)?;
    Ok([coldroot, coldimage, warmroot, warmimage])
}

/// Activate the image the root carries, measure, and take it away again.
#[inline(never)]
fn activated(root: [u8; DIGEST_LEN], bytes: &[u8]) -> Result<[u64; 4], &'static str> {
    let image = resolve_path(IMAGE).map_err(|_| "the image did not resolve")?;
    extension::activate(NAME, root, image.fs, image.inode, extension::budget())
        .map_err(|_| "the image did not activate")?;
    let measured = measure(bytes);
    let _ = deactivate(NAME);
    for _ in 0..2 {
        extension::pass();
    }
    measured
}

/// Take the seal off the image and remove both files, so the next boot of the
/// root starts where this one did.
#[inline(never)]
fn unstage() {
    let holder = ();
    if let (Ok(image), Ok(seal)) = (
        resolve_path(IMAGE),
        check_mask::<Seal, _>(&holder, Capability::Seal.bit()),
    ) {
        let flags = image.fs.inode_flags(image.inode).unwrap_or(0);
        let _ = image
            .fs
            .set_inode_flags(image.inode, flags & !FS_IMMUTABLE_FL, Some(&seal));
    }
    let _ = vfs_unlink(IMAGE);
    let _ = vfs_unlink(DATA);
}

#[inline(never)]
fn body(disk: &[u8], bytes: &[u8]) -> Result<[u64; 4], &'static str> {
    let root = stage(bytes)?;
    vfs_ext4_unmount_named(MOUNT).map_err(|_| "the root disk could not be unmounted")?;
    vfs_ext4_mount_named(disk, MOUNT, false).map_err(|_| "the root disk did not mount again")?;
    activated(root, bytes)
}

/// A file's pages faulted in from an image cost about what the same file's
/// do from the root it is a file of, cold and warm.
pub fn test_fsperf_image_read_cost() -> TestResult {
    let Some(bytes) = data() else {
        return fail!("no room for the measured file");
    };
    let disk = match suite_disk(TestDisk::Root) {
        Ok(disk) => disk,
        Err(why) => return fail!("{}", why),
    };
    if vfs_mkdir(MOUNT).is_err() && vfs_stat(MOUNT).is_err() {
        return fail!("could not create the mount point for the root disk");
    }
    if let Err(e) = vfs_ext4_mount_named(disk.as_bytes(), MOUNT, false) {
        return fail!("could not mount the root disk: {:?}", e);
    }
    let measured = body(disk.as_bytes(), bytes.as_slice());
    filemap::drop_idle_sets();
    unstage();
    if let Err(e) = vfs_ext4_unmount_named(MOUNT) {
        // The boot's own `fs init` step claims the disk after the tests.
        return fail!("could not unmount the root disk: {:?}", e);
    }
    let _ = vfs_rmdir(MOUNT);
    let [coldroot, coldimage, warmroot, warmimage] = match measured {
        Ok(measured) => measured,
        Err(why) => return fail!("{}", why),
    };
    crate::fsreport::record_image_cost(&crate::fsreport::ImageCost {
        bytes: BYTES as u64,
        coldroot,
        coldimage,
        warmroot,
        warmimage,
    });
    TestResult::Pass
}

slopos_testing::stest!(name = test_fsperf_image_read_cost, suite = fs);
