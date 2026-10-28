//! The shipped verified image, mounted from the device the test harness
//! attaches it on.
//!
//! `fs/assets/ext4.img` is the only image that carries a protected verity
//! trailer: the tests image on the root disk is built `VERITY=off` so the
//! suite can write to it. This is the one test that exercises
//! `fs/src/verity.rs` against a trailer a real block device reports, whose
//! capacity counts whole sectors.

use slopos_fs::blockdev::BlockDevice;
use slopos_fs::ext4::cache::{BlockCache, CACHE_ENTRIES_MIN};
use slopos_fs::ext4::{Ext4Error, Ext4Fs};
use slopos_fs::verity::{FsExtent, VerityStatus, build_verified};
use slopos_ostd::KBox;
use slopos_testing::TestResult;
use slopos_testing::{fail, pass};

use crate::block;

/// `scripts/qemu_run.sh`'s test mode attaches the shipped verified image as
/// the first virtio disk, `snapshot=on`.
const VERIFIED: &[u8] = b"vda";

fn extent_of(device: &dyn BlockDevice) -> Result<FsExtent, TestResult> {
    match Ext4Fs::mount_params(device) {
        Ok((sb, geom)) => Ok(FsExtent {
            block_size: geom.block_size(),
            blocks: sb.blocks_count as u64,
        }),
        Err(e) => Err(fail!("verified image superblock unreadable: {:?}", e)),
    }
}

fn claim_verified() -> Result<KBox<dyn BlockDevice + Send + Sync>, TestResult> {
    match block::claim(VERIFIED) {
        Ok(device) => Ok(device),
        Err(block::ClaimError::NoDevice) => Err(fail!(
            "verified image not attached — is fs/assets/ext4.img built?"
        )),
        Err(e) => Err(fail!("could not claim the verified image: {:?}", e)),
    }
}

/// The artifact's trailer must be *reachable* through a sector-granular
/// capacity, and the wrapped device must refuse writes.
pub fn test_verity_artifact_trailer_reachable() -> TestResult {
    let device = match claim_verified() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let cap = device.capacity();
    if cap % 512 != 0 {
        return fail!("virtio capacity {} is not sector-granular", cap);
    }
    let extent = match extent_of(&*device) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let (device, status) = match build_verified(device, extent) {
        Ok(parts) => parts,
        Err(e) => return fail!("build_verified refused the shipped image: {:?}", e),
    };
    let VerityStatus::Verified { blocks, block_size } = status else {
        return fail!(
            "the shipped image mounted with no trailer: the header sits beyond capacity {}",
            cap
        );
    };
    if block_size != 4096 || blocks == 0 {
        return fail!(
            "unexpected trailer geometry: {} blocks of {}",
            blocks,
            block_size
        );
    }
    if !device.write_protected() {
        return fail!("a verified device must be write-protected");
    }
    if device.write_at(0, &[0u8; 512]).is_ok() {
        return fail!("a verified device accepted a write");
    }
    pass!()
}

#[inline(never)]
fn mount_and_probe(device: &(dyn BlockDevice + Send + Sync)) -> TestResult {
    let (sb, geom) = match Ext4Fs::mount_params(device) {
        Ok(v) => v,
        Err(e) => return fail!("mount_params: {:?}", e),
    };
    let mut cache = match BlockCache::new_boxed(geom.block_size(), CACHE_ENTRIES_MIN) {
        Ok(c) => c,
        Err(e) => return fail!("BlockCache::new: {:?}", e),
    };
    let mut fs = Ext4Fs::new(device, &mut cache, sb, geom);
    if !fs.is_read_only() {
        return fail!("ext4 over a verified device must come up read-only");
    }
    match fs.resolve_path(b"/sbin/init") {
        Ok(_) => {}
        Err(e) => return fail!("/sbin/init unreadable through verity: {:?}", e),
    }
    match fs.create_file(2, b"verity_probe") {
        Err(Ext4Error::ReadOnly) => {}
        other => return fail!("want ReadOnly, got {:?}", other.map(|_| ())),
    }
    if fs.dirty_count() != 0 {
        return fail!("the read-only mount dirtied {} blocks", fs.dirty_count());
    }
    pass!()
}

/// End to end: the shipped image mounts read-only, its binaries read through
/// verification, and a create is refused before it dirties anything.
pub fn test_verity_artifact_mounts_read_only() -> TestResult {
    let device = match claim_verified() {
        Ok(d) => d,
        Err(r) => return r,
    };
    let extent = match extent_of(&*device) {
        Ok(e) => e,
        Err(r) => return r,
    };
    let (device, status) = match build_verified(device, extent) {
        Ok(parts) => parts,
        Err(e) => return fail!("build_verified refused the shipped image: {:?}", e),
    };
    if status == VerityStatus::Absent {
        return fail!("the shipped image carries no trailer");
    }
    mount_and_probe(&*device)
}

slopos_testing::stest!(
    name = test_verity_artifact_trailer_reachable,
    suite = verity_artifact
);
slopos_testing::stest!(
    name = test_verity_artifact_mounts_read_only,
    suite = verity_artifact
);
