//! The block layer on real disks: a GPT written through a whole-disk claim,
//! read back by a table re-read into partition nodes and `/dev/disk/by-*`
//! links, and the claims that keep a mounted partition's window from moving.
//!
//! Destructive, so only the scratch disks: the NVMe namespaces, one with
//! 512-byte and one with 4096-byte logical blocks, and for by-id names the
//! virtio disk and the stick. The table takes the first and last blocks; the
//! windows sit below the regions the engine tests write.

use slopos_abi::fs::block_ioctl;
use slopos_boot_core::Guid;
use slopos_boot_core::disk_id::DiskId;
use slopos_fs::blockdev::BlockDevice;
use slopos_fs::devfs::{
    BlockIoctlReply, DEV_NAME_MAX, devfs_block_ioctl, devfs_resolve_block_source,
};
use slopos_fs::partition::PARTUUID_TEXT_MAX;
use slopos_fs::vfs::{FileSystem, FileType, InodeId, VfsError};
use slopos_ostd::KVec;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use super::gpt_fixture;
use crate::block::{self, ClaimError, DiskName, RereadError};

/// The windows, in bytes of the disk: 64 KiB..128 KiB and 128 KiB..256 KiB,
/// so both are whole logical blocks on either block size.
const P1: (u64, u64) = (64 << 10, 64 << 10);
const P2: (u64, u64) = (128 << 10, 128 << 10);

fn show(name: &[u8]) -> &str {
    core::str::from_utf8(name).unwrap_or("?")
}

/// Partition `slot`'s unique GUID: distinct per disk and slot, so one disk's
/// PARTUUID never resolves to the other's partition.
fn unique_guid(disk_tag: u8, slot: u8) -> [u8; 16] {
    core::array::from_fn(|i| match i {
        0 => disk_tag,
        1 => slot,
        _ => 0x40 + i as u8,
    })
}

fn install_gpt(device: &dyn BlockDevice, disk_tag: u8) -> Result<(), &'static str> {
    let entry = |slot: u8, (start, len): (u64, u64)| gpt_fixture::Entry {
        type_guid: Guid([0x11 * slot; 16]),
        unique: Guid(unique_guid(disk_tag, slot)),
        start,
        len,
    };
    gpt_fixture::install(device, Guid([0x5A; 16]), &[entry(1, P1), entry(2, P2)])
}

fn node_capacity(name: &[u8]) -> Option<u64> {
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let inode = fs.lookup(fs.root_inode(), name).ok()?;
    fs.stat(inode).ok().map(|s| s.size)
}

#[inline(never)]
fn partitioned_disk(disk: &[u8], disk_tag: u8, by_id: &[u8]) -> TestResult {
    let Some(device) = block::disk(disk) else {
        return fail!("scratch disk {} not attached", show(disk));
    };
    {
        let whole = match block::claim(disk) {
            Ok(c) => c,
            Err(e) => return fail!("claiming {} failed: {:?}", show(disk), e),
        };
        if let Err(why) = install_gpt(whole.as_ref(), disk_tag) {
            return fail!("could not install the GPT: {}", why);
        }
        assert_test!(
            matches!(block::reread(disk), Err(RereadError::Busy)),
            "a re-read must be refused while the whole disk is claimed"
        );
    }
    if let Err(e) = block::reread(disk) {
        return fail!("re-reading {} failed: {:?}", show(disk), e);
    }

    let base = DiskName::nvme(
        u32::from(disk[4] - b'0'),
        u32::from(disk[disk.len() - 1] - b'0'),
    );
    let (p1, p2) = (base.partition(1), base.partition(2));
    assert_eq_test!(
        node_capacity(p1.as_bytes()),
        Some(P1.1),
        "partition 1's node must be its window"
    );
    assert_eq_test!(
        node_capacity(p2.as_bytes()),
        Some(P2.1),
        "partition 2's node must be its window"
    );

    let verdict = check_links(&p2, disk_tag, by_id);
    if !matches!(verdict, TestResult::Pass) {
        return verdict;
    }
    let verdict = check_claims(disk, &p1, &p2, device.as_ref());
    if !matches!(verdict, TestResult::Pass) {
        return verdict;
    }
    let verdict = check_read_claims(disk, &p1);
    if !matches!(verdict, TestResult::Pass) {
        return verdict;
    }

    let whole = match block::claim(disk) {
        Ok(c) => c,
        Err(e) => return fail!("the whole disk was not released: {:?}", e),
    };
    if let Err(why) = gpt_fixture::wipe(whole.as_ref()) {
        return fail!("{}", why);
    }
    drop(whole);
    if let Err(e) = block::reread(disk) {
        return fail!("the re-read after the wipe failed: {:?}", e);
    }
    assert_test!(
        node_capacity(p1.as_bytes()).is_none(),
        "a wiped table must leave no partition nodes"
    );
    pass!()
}

/// Partition 2 resolves by `PARTUUID=`, through its by-partuuid link and
/// through its by-id one, its disk's with `-part2`.
#[inline(never)]
fn check_links(p2: &DiskName, disk_tag: u8, by_id: &[u8]) -> TestResult {
    let text: [u8; PARTUUID_TEXT_MAX] = Guid(unique_guid(disk_tag, 2)).spelling();
    let mut spec = [0u8; 9 + PARTUUID_TEXT_MAX];
    spec[..9].copy_from_slice(b"PARTUUID=");
    spec[9..].copy_from_slice(&text);
    let mut resolved = [0u8; DEV_NAME_MAX];
    match devfs_resolve_block_source(&spec, &mut resolved) {
        Ok(len) if resolved[..len] == *p2.as_bytes() => {}
        other => return fail!("PARTUUID= resolved to {:?}", other),
    }
    let mut link = [0u8; 22 + PARTUUID_TEXT_MAX];
    link[..22].copy_from_slice(b"/dev/disk/by-partuuid/");
    link[22..].copy_from_slice(&text);
    match devfs_resolve_block_source(&link, &mut resolved) {
        Ok(len) if resolved[..len] == *p2.as_bytes() => {}
        other => return fail!("the by-partuuid link resolved to {:?}", other),
    }
    let mut spec = [0u8; 128];
    let parts: [&[u8]; 3] = [b"/dev/disk/by-id/", by_id, b"-part2"];
    let mut len = 0;
    for part in parts {
        spec[len..len + part.len()].copy_from_slice(part);
        len += part.len();
    }
    match devfs_resolve_block_source(&spec[..len], &mut resolved) {
        Ok(at) if resolved[..at] == *p2.as_bytes() => {}
        other => return fail!("the by-id partition link resolved to {:?}", other),
    }

    pass!()
}

/// Two partitions claim side by side; either excludes the whole disk and a
/// re-read; a partition's writes land inside its window.
#[inline(never)]
fn check_claims(
    disk: &[u8],
    p1: &DiskName,
    p2: &DiskName,
    device: &(dyn BlockDevice + Send + Sync),
) -> TestResult {
    let first = match block::claim(p1.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("claiming partition 1 failed: {:?}", e),
    };
    let second = match block::claim(p2.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("partition 2 must be claimable beside partition 1: {:?}", e),
    };
    assert_test!(
        matches!(block::claim(p1.as_bytes()), Err(ClaimError::Busy)),
        "a claimed partition must refuse a second claim"
    );
    assert_test!(
        matches!(block::claim(disk), Err(ClaimError::Busy)),
        "the whole disk must refuse a claim while a partition is claimed"
    );
    assert_test!(
        matches!(block::reread(disk), Err(RereadError::Busy)),
        "a re-read must be refused while a partition is claimed"
    );
    assert_test!(
        matches!(block::reread(p1.as_bytes()), Err(RereadError::NotWholeDisk)),
        "a partition has no table to re-read"
    );

    let Ok(mut stamp) = KVec::<u8>::zeroed(512) else {
        return fail!("stamp alloc");
    };
    stamp.fill(0x9D);
    assert_test!(
        second.write_at(0, &stamp).is_ok(),
        "a write through the partition's claim"
    );
    let Ok(mut parent) = KVec::<u8>::zeroed(512) else {
        return fail!("readback alloc");
    };
    assert_test!(
        device.read_at(P2.0, &mut parent).is_ok() && parent[..] == stamp[..],
        "a partition write must land at the partition's start on the disk"
    );
    assert_test!(
        second.write_at(P2.1, &stamp).is_err(),
        "a write past the window must be refused"
    );
    drop((first, second));

    pass!()
}

/// Readers of a partition share it with each other and with a reader of the
/// whole disk, and hold a writer and a re-read off it; a read claim writes
/// nothing.
#[inline(never)]
fn check_read_claims(disk: &[u8], p1: &DiskName) -> TestResult {
    let (first, second, whole) = match (
        block::claim_read(p1.as_bytes()),
        block::claim_read(p1.as_bytes()),
        block::claim_read(disk),
    ) {
        (Ok(first), Ok(second), Ok(whole)) => (first, second, whole),
        other => {
            return fail!(
                "readers must share: {:?}",
                (other.0.err(), other.1.err(), other.2.err())
            );
        }
    };
    assert_test!(
        matches!(block::claim(p1.as_bytes()), Err(ClaimError::Busy)),
        "a partition being read must refuse a writer"
    );
    assert_test!(
        matches!(block::claim(disk), Err(ClaimError::Busy)),
        "a disk being read must refuse a writer"
    );
    assert_test!(
        matches!(block::reread(disk), Err(RereadError::Busy)),
        "a re-read must be refused while a partition is read"
    );
    assert_test!(
        first.write_protected() && first.write_at(0, &[0u8; 512]).is_err(),
        "a read claim must refuse writes"
    );
    drop((first, second, whole));

    let writer = match block::claim(p1.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("the readers did not let go: {:?}", e),
    };
    assert_test!(
        matches!(block::claim_read(p1.as_bytes()), Err(ClaimError::Busy)),
        "a partition being written must refuse a reader"
    );
    assert_test!(
        matches!(block::claim_read(disk), Err(ClaimError::Busy)),
        "a disk with a partition being written must refuse a reader"
    );
    drop(writer);
    pass!()
}

pub fn test_block_partitions_on_512_byte_blocks() -> TestResult {
    partitioned_disk(b"nvme0n2", 0xA5, b"nvme-QEMU_NVMe_Ctrl_slopos-root_2")
}

pub fn test_block_partitions_on_4096_byte_blocks() -> TestResult {
    partitioned_disk(b"nvme1n2", 0x4C, b"nvme-QEMU_NVMe_Ctrl_slopos-4kn_2")
}

/// QEMU's disks are named under `/dev/disk/by-id` by the `serial=` the
/// harness gives each: an NVMe namespace by its controller's model and serial
/// and its namespace, the first namespace by the two alone as well; a virtio
/// disk by its ID string; the stick by INQUIRY's vendor and product, its
/// serial string and its LUN. Each is listed, links to its node, and names it
/// as a mount source.
pub fn test_block_by_id_names() -> TestResult {
    const LINKS: [(&[u8], &[u8]); 7] = [
        (b"nvme-QEMU_NVMe_Ctrl_slopos-root_1", b"nvme0n1"),
        (b"nvme-QEMU_NVMe_Ctrl_slopos-root", b"nvme0n1"),
        (b"nvme-QEMU_NVMe_Ctrl_slopos-root_2", b"nvme0n2"),
        (b"nvme-QEMU_NVMe_Ctrl_slopos-4kn_1", b"nvme1n1"),
        (b"virtio-slopos-verified", b"vda"),
        (b"virtio-slopos-scratch", b"vdb"),
        (b"usb-QEMU_QEMU_HARDDISK_slopos-usb-0:0", b"sda"),
    ];
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let Some(by_id) = by_id_dir(fs) else {
        return fail!("/dev/disk/by-id is missing");
    };
    for (link, node) in LINKS {
        let verdict = by_id_link(fs, by_id, link, node);
        if !matches!(verdict, TestResult::Pass) {
            return verdict;
        }
    }
    assert_eq_test!(
        links_to(fs, by_id, b"nvme0n2"),
        Some(1),
        "namespace 2 has no shorter name"
    );
    pass!()
}

/// A partition of the virtio disk and of the stick is named by its disk's
/// by-id name with `-part<N>`, until a re-read finds it gone.
pub fn test_block_by_id_partition_names() -> TestResult {
    let disks: [(&[u8], &[u8], &[u8]); 2] = [
        (b"vdb", b"virtio-slopos-scratch-part1", b"vdb1"),
        (
            b"sda",
            b"usb-QEMU_QEMU_HARDDISK_slopos-usb-0:0-part1",
            b"sda1",
        ),
    ];
    for (disk, link, node) in disks {
        let verdict = by_id_partition(disk, link, node);
        if !matches!(verdict, TestResult::Pass) {
            return verdict;
        }
    }
    pass!()
}

#[inline(never)]
fn by_id_partition(disk: &[u8], link: &[u8], node: &[u8]) -> TestResult {
    let laid = block::claim(disk)
        .map_err(|_| "claim")
        .and_then(|whole| {
            let entry = gpt_fixture::Entry {
                type_guid: Guid([0x11; 16]),
                unique: Guid([0x1D; 16]),
                start: P1.0,
                len: P1.1,
            };
            gpt_fixture::install(whole.as_ref(), Guid([0x1E; 16]), &[entry])
        })
        .and_then(|()| block::reread(disk).map_err(|_| "re-read"));
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let verdict = match (laid, by_id_dir(fs)) {
        (Ok(()), Some(by_id)) => by_id_link(fs, by_id, link, node),
        (Ok(()), None) => fail!("/dev/disk/by-id is missing"),
        (Err(why), _) => fail!("partitioning {}: {}", show(disk), why),
    };
    let wiped = block::claim(disk)
        .map_err(|_| "claim")
        .and_then(|whole| gpt_fixture::wipe(whole.as_ref()))
        .and_then(|()| block::reread(disk).map_err(|_| "re-read"));
    match (verdict, wiped) {
        (TestResult::Pass, Err(why)) => fail!("wiping {}: {}", show(disk), why),
        (TestResult::Pass, Ok(())) if resolve_by_id(link).is_ok() => {
            fail!("{} outlived its partition", show(link))
        }
        (verdict, _) => verdict,
    }
}

fn by_id_dir(fs: &dyn FileSystem) -> Option<InodeId> {
    let disk = fs.lookup(fs.root_inode(), b"disk").ok()?;
    fs.lookup(disk, b"by-id").ok()
}

/// `link` is listed, links to `node` and names it as a mount source.
#[inline(never)]
fn by_id_link(fs: &dyn FileSystem, by_id: InodeId, link: &[u8], node: &[u8]) -> TestResult {
    let mut listed = false;
    let walked = fs.readdir(by_id, 0, &mut |name, _, kind| {
        listed |= name == link && kind == FileType::Symlink;
        true
    });
    if walked.is_err() || !listed {
        return fail!("/dev/disk/by-id does not list {}", show(link));
    }
    let Ok(inode) = fs.lookup(by_id, link) else {
        return fail!("/dev/disk/by-id/{} is missing", show(link));
    };
    let mut target = [0u8; 32];
    match fs.readlink(inode, &mut target) {
        Ok(len) if target[..len].strip_prefix(b"../../") == Some(node) => {}
        other => return fail!("{} points at {:?}", show(link), other),
    }
    match resolve_by_id(link) {
        Ok(resolved) if resolved.as_bytes() == node => pass!(),
        other => fail!(
            "/dev/disk/by-id/{} resolved to {:?}",
            show(link),
            other.map(|name| name.len)
        ),
    }
}

struct Resolved {
    buf: [u8; DEV_NAME_MAX],
    len: usize,
}

impl Resolved {
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

fn resolve_by_id(link: &[u8]) -> Result<Resolved, VfsError> {
    const DIR: &[u8] = b"/dev/disk/by-id/";
    let mut spec = [0u8; 128];
    let len = DIR.len() + link.len();
    spec[..DIR.len()].copy_from_slice(DIR);
    spec[DIR.len()..len].copy_from_slice(link);
    let mut resolved = Resolved {
        buf: [0; DEV_NAME_MAX],
        len: 0,
    };
    resolved.len = devfs_resolve_block_source(&spec[..len], &mut resolved.buf)?;
    Ok(resolved)
}

/// How many of `/dev/disk/by-id`'s links point at `node`.
#[inline(never)]
fn links_to(fs: &dyn FileSystem, by_id: InodeId, node: &[u8]) -> Option<usize> {
    let mut links = [0; 64];
    let mut found = 0;
    fs.readdir(by_id, 0, &mut |_, inode, kind| {
        if kind == FileType::Symlink && found < links.len() {
            links[found] = inode;
            found += 1;
        }
        true
    })
    .ok()?;
    let mut target = [0u8; 32];
    let points = |inode: &&InodeId| {
        fs.readlink(**inode, &mut target)
            .is_ok_and(|len| target[..len].strip_prefix(b"../../") == Some(node))
    };
    Some(links[..found].iter().filter(points).count())
}

/// The labelled volume the harness attaches resolves by label and by UUID,
/// and `/dev/disk/by-label` lists it as a link to its node.
pub fn test_block_volume_links() -> TestResult {
    let mut resolved = [0u8; DEV_NAME_MAX];
    match devfs_resolve_block_source(b"LABEL=slopos-media", &mut resolved) {
        Ok(len) if resolved[..len] == *b"nvme1n1" => {}
        other => return fail!("LABEL=slopos-media resolved to {:?}", other),
    }
    let Some(media) = block::disk(b"nvme1n1") else {
        return fail!("the media disk is not attached");
    };
    let mut sb = [0u8; 136];
    assert_test!(media.read_at(1024, &mut sb).is_ok(), "superblock read");
    let mut uuid = [0u8; 5 + 36];
    uuid[..5].copy_from_slice(b"UUID=");
    let mut at = 5;
    for (i, byte) in sb[104..120].iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            uuid[at] = b'-';
            at += 1;
        }
        uuid[at..at + 2].copy_from_slice(&[
            b"0123456789ABCDEF"[usize::from(byte >> 4)],
            b"0123456789ABCDEF"[usize::from(byte & 0xF)],
        ]);
        at += 2;
    }
    match devfs_resolve_block_source(&uuid, &mut resolved) {
        Ok(len) if resolved[..len] == *b"nvme1n1" => {}
        other => return fail!("an uppercase UUID= resolved to {:?}", other),
    }

    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let lookup = |parent, name: &[u8]| fs.lookup(parent, name);
    let Ok(disk_dir) = lookup(fs.root_inode(), b"disk") else {
        return fail!("/dev/disk is missing");
    };
    let Ok(by_label) = lookup(disk_dir, b"by-label") else {
        return fail!("/dev/disk/by-label is missing");
    };
    let mut listed = None;
    let walked = fs.readdir(by_label, 0, &mut |name, inode, kind| {
        if name == b"slopos-media" {
            listed = Some((inode, kind));
        }
        true
    });
    let Some((inode, FileType::Symlink)) = listed.filter(|_| walked.is_ok()) else {
        return fail!("by-label does not list slopos-media as a link");
    };
    assert_eq_test!(
        lookup(by_label, b"slopos-media").ok(),
        Some(inode),
        "the listed link must be what a lookup finds"
    );
    let mut target = [0u8; 32];
    match fs.readlink(inode, &mut target) {
        Ok(len) if target[..len] == *b"../../nvme1n1" => pass!(),
        other => fail!("the link points at {:?}", other),
    }
}

/// A block node answers its size, its block size and whether it refuses
/// writes; an unknown request, or an inode that is no block node, gets none.
pub fn test_block_ioctls() -> TestResult {
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let Ok(inode) = fs.lookup(fs.root_inode(), b"nvme1n2") else {
        return fail!("/dev/nvme1n2 is missing");
    };
    let Some(disk) = block::disk(b"nvme1n2") else {
        return fail!("nvme1n2 is not registered");
    };
    let ask = |request| devfs_block_ioctl(inode, request);
    assert_eq_test!(
        ask(block_ioctl::BLKSSZGET),
        Some(Ok(BlockIoctlReply::Int(4096))),
        "BLKSSZGET"
    );
    assert_eq_test!(
        ask(block_ioctl::BLKGETSIZE64),
        Some(Ok(BlockIoctlReply::U64(disk.capacity()))),
        "BLKGETSIZE64"
    );
    assert_eq_test!(
        ask(block_ioctl::BLKGETSIZE),
        Some(Ok(BlockIoctlReply::U64(disk.capacity() / 512))),
        "BLKGETSIZE counts 512-byte sectors"
    );
    assert_eq_test!(
        ask(block_ioctl::BLKROGET),
        Some(Ok(BlockIoctlReply::Int(0))),
        "BLKROGET"
    );
    assert_eq_test!(
        ask(0x1234),
        Some(Err(VfsError::NotSupported)),
        "an unknown request"
    );
    assert_test!(
        devfs_block_ioctl(fs.root_inode(), block_ioctl::BLKSSZGET).is_none(),
        "a non-block inode is not answered"
    );
    pass!()
}

pub fn test_block_disk_names() -> TestResult {
    let virtio = |i| DiskName::virtio(i);
    assert_test!(virtio(0).as_bytes() == b"vda", "vda");
    assert_test!(virtio(25).as_bytes() == b"vdz", "vdz");
    assert_test!(virtio(26).as_bytes() == b"vdaa", "vdaa follows vdz");
    assert_test!(virtio(701).as_bytes() == b"vdzz", "vdzz");
    assert_test!(virtio(702).as_bytes() == b"vdaaa", "vdaaa follows vdzz");
    let nvme = DiskName::nvme(1, 12);
    assert_test!(nvme.as_bytes() == b"nvme1n12", "nvme1n12");
    assert_test!(
        nvme.partition(3).as_bytes() == b"nvme1n12p3",
        "a name ending in a digit takes a p"
    );
    assert_test!(
        virtio(1).partition(128).as_bytes() == b"vdb128",
        "a name ending in a letter does not"
    );
    assert_test!(DiskName::scsi(0).as_bytes() == b"sda", "sda");
    assert_test!(DiskName::scsi(27).as_bytes() == b"sdab", "sdab");
    assert_test!(DiskName::scsi(0).partition(1).as_bytes() == b"sda1", "sda1");
    pass!()
}

/// A USB disk takes the lowest free `sd` letter, gives it back when its
/// device leaves, and a claim on the disk that left never releases its
/// successor's; its by-id link leaves and comes back with it; it reports the
/// write protection its device does, and is never the disk `root=auto` takes.
pub fn test_block_usb_disks_come_and_go() -> TestResult {
    const LINK: &[u8] = b"usb-Test_Come_And_Go_comego-0:0";
    let Some(scratch) = block::disk(b"nvme1n2") else {
        return fail!("nvme1n2 is not registered");
    };
    let view = || {
        let ns = scratch.namespace();
        slopos_ostd::KArc::try_new(
            block::EngineDisk::new(
                slopos_ostd::KArc::clone(scratch.engine()),
                ns.nsid,
                1 << ns.block_shift,
                scratch.capacity(),
                false,
            )
            .protected()
            .identified(DiskId::usb(b"Test", b"Come And Go", Some(b"comego"), 0)),
        )
    };
    let named = |name: DiskName| resolve_by_id(LINK).is_ok_and(|r| r.as_bytes() == name.as_bytes());
    let Ok(first) = view() else {
        return fail!("no memory for a disk");
    };
    let Some(name) = block::register_usb_disk(first) else {
        return fail!("the USB disk was not registered");
    };
    assert_test!(name.as_bytes().starts_with(b"sd"), "a USB disk is sd");
    assert_test!(named(name), "its by-id link names it");
    assert_test!(
        block::first_fixed_disk().is_some_and(|d| d.as_bytes() == b"nvme0n1"),
        "root=auto's disk stays nvme0n1"
    );
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let Ok(inode) = fs.lookup(fs.root_inode(), name.as_bytes()) else {
        return fail!("/dev/{} is missing", name);
    };
    assert_eq_test!(
        devfs_block_ioctl(inode, block_ioctl::BLKROGET),
        Some(Ok(BlockIoctlReply::Int(1))),
        "BLKROGET reports the device's protection"
    );
    let held = match block::claim(name.as_bytes()) {
        Ok(held) => held,
        Err(e) => return fail!("claiming {} failed: {:?}", name, e),
    };
    assert_test!(held.write_protected(), "a write claim reports it too");
    assert_test!(block::unregister_disk(name), "unregister");
    assert_test!(
        fs.lookup(fs.root_inode(), name.as_bytes()).is_err(),
        "its node is gone"
    );
    assert_test!(
        matches!(resolve_by_id(LINK), Err(VfsError::NotFound)),
        "its by-id link is gone"
    );
    let Ok(second) = view() else {
        return fail!("no memory for a disk");
    };
    let again = block::register_usb_disk(second);
    assert_test!(again == Some(name), "the letter is reused");
    assert_test!(named(name), "its by-id link is back");
    let successor = match block::claim(name.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("claiming the successor failed: {:?}", e),
    };
    drop(held);
    assert_test!(
        matches!(block::claim_read(name.as_bytes()), Err(ClaimError::Busy)),
        "the departed disk's claim released its successor's"
    );
    drop(successor);
    assert_test!(block::unregister_disk(name), "unregister the successor");
    assert_test!(!block::unregister_disk(name), "a disk leaves once");
    pass!()
}

slopos_testing::stest!(name = test_block_disk_names, suite = block_layer);
slopos_testing::stest!(
    name = test_block_partitions_on_512_byte_blocks,
    suite = block_layer
);
slopos_testing::stest!(
    name = test_block_partitions_on_4096_byte_blocks,
    suite = block_layer
);
slopos_testing::stest!(name = test_block_volume_links, suite = block_layer);
slopos_testing::stest!(name = test_block_by_id_names, suite = block_layer);
slopos_testing::stest!(name = test_block_by_id_partition_names, suite = block_layer);
slopos_testing::stest!(name = test_block_ioctls, suite = block_layer);
slopos_testing::stest!(name = test_block_usb_disks_come_and_go, suite = block_layer);
