//! Volume identities read off hand-built superblocks, and the `/dev/disk/by-*`
//! links and source spellings devfs builds on them.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use slopos_ostd::{KArc, klog_info};
use slopos_testing::{TestResult, fail};

use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::devfs::{
    BlockNodeKind, DEV_NAME_MAX, DevFs, devfs_block_contents_changed, devfs_register_block_device,
    devfs_register_block_node, devfs_resolve_block_source, devfs_unregister_block_node,
};
use crate::partition::PartUuid;
use crate::vfs::{FileSystem, FileType, VfsError};
use crate::volume_id::{Unreadable, VolumeId, VolumeKind, probe};

const EXT_UUID: [u8; 16] = [
    0x1b, 0x4e, 0x28, 0xba, 0x2f, 0xa1, 0x11, 0xd2, 0x88, 0x3f, 0x00, 0x16, 0xd3, 0xcc, 0x42, 0x7e,
];

fn probed(device: &MemoryBlockDevice) -> Option<VolumeId> {
    probe(device).ok().flatten()
}

fn device(len: usize, fill: impl FnOnce(&mut [u8])) -> Option<MemoryBlockDevice> {
    let device = MemoryBlockDevice::allocate(len)?;
    device.with_buffer_mut(fill);
    Some(device)
}

fn ext_superblock(buf: &mut [u8], label: &[u8]) {
    let sb = &mut buf[1024..2048];
    sb[56..58].copy_from_slice(&0xEF53u16.to_le_bytes());
    sb[76..80].copy_from_slice(&1u32.to_le_bytes());
    sb[104..120].copy_from_slice(&EXT_UUID);
    sb[120..120 + label.len()].copy_from_slice(label);
}

/// A FAT32 boot sector with serial `1234-ABCD` and boot-sector label
/// `BOOTLABEL`, and, where `buf` reaches it, a root directory whose first
/// cluster holds `root_label` as a volume-label entry when given.
fn fat32(buf: &mut [u8], root_label: Option<&[u8; 11]>) {
    const SECTOR: usize = 512;
    const RESERVED: usize = 4;
    const FATS: usize = 2;
    const FAT_SECTORS: usize = 8;
    buf[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    buf[13] = 1;
    buf[14..16].copy_from_slice(&(RESERVED as u16).to_le_bytes());
    buf[16] = FATS as u8;
    buf[21] = 0xF8;
    buf[36..40].copy_from_slice(&(FAT_SECTORS as u32).to_le_bytes());
    buf[44..48].copy_from_slice(&2u32.to_le_bytes());
    buf[66] = 0x29;
    buf[67..71].copy_from_slice(&0x1234_ABCDu32.to_le_bytes());
    buf[71..82].copy_from_slice(b"BOOTLABEL  ");
    buf[82..90].copy_from_slice(b"FAT32   ");
    buf[510] = 0x55;
    buf[511] = 0xAA;
    let root = (RESERVED + FATS * FAT_SECTORS) * SECTOR;
    let Some(dir) = buf.get_mut(root..root + 64) else {
        return;
    };
    dir[..11].copy_from_slice(b"LFN ENTRY  ");
    dir[11] = 0x0F;
    if let Some(label) = root_label {
        dir[32..43].copy_from_slice(label);
        dir[43] = 0x08;
    }
}

pub fn test_volume_id_ext() -> TestResult {
    let Some(dev) = device(4096, |b| ext_superblock(b, b"rootfs")) else {
        return fail!("no memory for the fixture");
    };
    let Some(id) = probed(&dev) else {
        return fail!("an ext superblock was not recognised");
    };
    if id.kind != VolumeKind::Ext {
        return fail!("probed as {:?}", id.kind);
    }
    if id.uuid() != Some(&b"1b4e28ba-2fa1-11d2-883f-0016d3cc427e"[..]) {
        return fail!("ext UUID spelled {:?}", id.uuid().map(core::str::from_utf8));
    }
    if id.label() != Some(&b"rootfs"[..]) {
        return fail!("ext label read as {:?}", id.label());
    }
    TestResult::Pass
}

pub fn test_volume_id_vfat() -> TestResult {
    let Some(dev) = device(64 * 1024, |b| fat32(b, None)) else {
        return fail!("no memory for the fixture");
    };
    match probed(&dev) {
        Some(id)
            if id.kind == VolumeKind::Vfat
                && id.uuid() == Some(&b"1234-ABCD"[..])
                && id.label() == Some(&b"BOOTLABEL"[..]) => {}
        Some(id) => {
            return fail!(
                "FAT32 read as {:?} uuid {:?} label {:?}",
                id.kind,
                id.uuid(),
                id.label()
            );
        }
        None => return fail!("a FAT32 boot sector was not recognised"),
    }
    // `fatlabel` rewrites the root directory's entry; that one wins.
    let Some(dev) = device(64 * 1024, |b| fat32(b, Some(b"SLOPOS-ESP "))) else {
        return fail!("no memory for the fixture");
    };
    match probed(&dev).and_then(|id| id.label().map(|l| l == b"SLOPOS-ESP")) {
        Some(true) => {}
        other => return fail!("the root-directory label did not win: {:?}", other),
    }
    // A protective MBR carries 0xAA55 too, and is no FAT volume.
    let Some(dev) = device(64 * 1024, |b| {
        b[446 + 4] = 0xEE;
        b[510] = 0x55;
        b[511] = 0xAA;
    }) else {
        return fail!("no memory for the fixture");
    };
    match probed(&dev) {
        None => TestResult::Pass,
        Some(id) => fail!("a protective MBR probed as {:?}", id.kind),
    }
}

/// A root directory past the device's end leaves the boot sector's label.
pub fn test_volume_id_vfat_root_past_the_end() -> TestResult {
    let Some(dev) = device(8 * 1024, |b| fat32(b, None)) else {
        return fail!("no memory for the fixture");
    };
    match probe(&dev) {
        Ok(Some(id)) if id.label() == Some(&b"BOOTLABEL"[..]) => TestResult::Pass,
        other => fail!(
            "a truncated FAT32 volume probed as {:?}",
            other.map(|i| i.map(|i| i.kind))
        ),
    }
}

pub fn test_volume_id_btrfs() -> TestResult {
    let Some(dev) = device(128 * 1024, |b| {
        let sb = &mut b[64 * 1024..];
        sb[0x20..0x30].copy_from_slice(&EXT_UUID);
        sb[0x40..0x48].copy_from_slice(b"_BHRfS_M");
        sb[0x12B..0x12B + 6].copy_from_slice(b"cachy1");
    }) else {
        return fail!("no memory for the fixture");
    };
    match probed(&dev) {
        Some(id)
            if id.kind == VolumeKind::Btrfs
                && id.uuid() == Some(&b"1b4e28ba-2fa1-11d2-883f-0016d3cc427e"[..])
                && id.label() == Some(&b"cachy1"[..]) =>
        {
            TestResult::Pass
        }
        Some(id) => fail!(
            "btrfs read as {:?} {:?} {:?}",
            id.kind,
            id.uuid(),
            id.label()
        ),
        None => fail!("a btrfs superblock was not recognised"),
    }
}

fn node(fill: impl FnOnce(&mut [u8])) -> Option<KArc<dyn BlockDevice + Send + Sync>> {
    let dev = KArc::try_new(device(4096, fill)?).ok()?;
    Some(dev)
}

/// A label with a space and a slash is a link named with udev's escapes and
/// resolves raw from `LABEL=`; a partition resolves by PARTUUID; a disk with
/// a table names no volume of its own; and a withdrawn node resolves nowhere.
pub fn test_devfs_disk_links() -> TestResult {
    klog_info!("VOLID_TEST: /dev/disk links");
    let (Some(labelled), Some(part), Some(table)) = (
        node(|b| ext_superblock(b, b"my disk/1")),
        node(|_| {}),
        node(|b| ext_superblock(b, b"stale")),
    ) else {
        return fail!("no memory for the fixture");
    };
    let uuid = PartUuid::Mbr {
        signature: 0x0BAD_F00D,
        number: 3,
    };
    let names: [&[u8]; 3] = [b"volidtest0", b"volidtest1", b"volidtest2"];
    let registered = [
        devfs_register_block_node(
            names[0],
            labelled,
            BlockNodeKind::Disk { partitioned: false },
        ),
        devfs_register_block_node(names[1], part, BlockNodeKind::Partition { uuid }),
        devfs_register_block_node(names[2], table, BlockNodeKind::Disk { partitioned: true }),
    ];
    let verdict = if registered.iter().all(Result::is_ok) {
        links_body(names)
    } else {
        fail!("registration failed: {:?}", registered)
    };
    for name in names {
        let _ = devfs_unregister_block_node(name);
    }
    verdict
}

#[inline(never)]
fn links_body(names: [&[u8]; 3]) -> TestResult {
    let mut out = [0u8; DEV_NAME_MAX];
    let mut resolve =
        |spec: &[u8]| devfs_resolve_block_source(spec, &mut out).map(|len| out[..len] == *names[0]);
    if resolve(b"LABEL=my disk/1") != Ok(true) {
        return fail!("LABEL= with the raw label did not resolve");
    }
    if resolve(br"/dev/disk/by-label/my\x20disk\x2f1") != Ok(true) {
        return fail!("the escaped by-label path did not resolve");
    }
    if resolve(b"LABEL=stale") != Err(VfsError::NotFound) {
        return fail!("a partitioned disk's stale label resolved");
    }
    if resolve(b"LABEL=") != Err(VfsError::InvalidArgument) {
        return fail!("an empty LABEL= must be invalid");
    }
    if resolve(b"not-a-device") != Err(VfsError::InvalidArgument) {
        return fail!("a name no device can have must be invalid");
    }
    match devfs_resolve_block_source(b"PARTUUID=0BADF00D-03", &mut out) {
        Ok(len) if out[..len] == *names[1] => {}
        other => return fail!("PARTUUID= resolved to {:?}", other),
    }

    let fs = DevFs::new();
    let Ok(disk) = fs.lookup(fs.root_inode(), b"disk") else {
        return fail!("/dev/disk missing");
    };
    let Ok(by_partuuid) = fs.lookup(disk, b"by-partuuid") else {
        return fail!("/dev/disk/by-partuuid missing");
    };
    let Ok(link) = fs.lookup(by_partuuid, b"0badf00d-03") else {
        return fail!("the partition's link is missing");
    };
    match fs.stat(link) {
        Ok(stat) if stat.file_type == FileType::Symlink && stat.size == 16 => {}
        other => {
            return fail!(
                "the link stats as {:?}",
                other.map(|s| (s.file_type, s.size))
            );
        }
    }
    let mut target = [0u8; 32];
    match fs.readlink(link, &mut target) {
        Ok(len) if target[..len] == *b"../../volidtest1" => {}
        other => return fail!("readlink gave {:?}", other),
    }
    if fs.read(link, 0, &mut target).is_ok() {
        return fail!("a link read as a device");
    }
    let _ = devfs_unregister_block_node(names[1]);
    if fs.stat(link).is_ok() || fs.lookup(by_partuuid, b"0badf00d-03").is_ok() {
        return fail!("a withdrawn node's link outlived it");
    }
    TestResult::Pass
}

struct CountingReads {
    inner: MemoryBlockDevice,
    reads: AtomicUsize,
}

impl BlockDevice for CountingReads {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.inner.logical_block_size()
    }
}

/// A volume's identity is read once, not on every lookup, and read again
/// once a writer has let go of a device.
pub fn test_devfs_identity_read_once_per_change() -> TestResult {
    const NAME: &[u8] = b"volidcache0";
    let Some(image) = device(4096, |b| ext_superblock(b, b"before")) else {
        return fail!("no memory for the fixture");
    };
    let Ok(counted) = KArc::try_new(CountingReads {
        inner: image,
        reads: AtomicUsize::new(0),
    }) else {
        return fail!("no memory for the fixture");
    };
    let node: KArc<dyn BlockDevice + Send + Sync> = counted.clone();
    if let Err(e) = devfs_register_block_device(NAME, node) {
        return fail!("registration failed: {:?}", e);
    }
    let verdict = identity_cache_body(&counted);
    let _ = devfs_unregister_block_node(NAME);
    verdict
}

#[inline(never)]
fn identity_cache_body(device: &CountingReads) -> TestResult {
    let mut out = [0u8; DEV_NAME_MAX];
    if devfs_resolve_block_source(b"LABEL=before", &mut out).is_err() {
        return fail!("the fixture's label did not resolve");
    }
    let reads = device.reads.load(Ordering::Relaxed);
    let again = devfs_resolve_block_source(b"LABEL=before", &mut out);
    if again.is_err() || device.reads.load(Ordering::Relaxed) != reads {
        return fail!("a second lookup read the device again: {:?}", again);
    }
    device
        .inner
        .with_buffer_mut(|b| b[1024 + 120..1024 + 126].copy_from_slice(b"after\0"));
    devfs_block_contents_changed();
    if devfs_resolve_block_source(b"LABEL=after", &mut out).is_err() {
        return fail!("the new label did not resolve after the change");
    }
    match devfs_resolve_block_source(b"LABEL=before", &mut out) {
        Err(VfsError::NotFound) => TestResult::Pass,
        other => fail!("the old label still resolved: {:?}", other),
    }
}

/// Each valid UTF-8 sequence of a label is kept and every other byte escaped,
/// as udev names the link; a label a directory entry cannot carry has no
/// link, though `LABEL=` still finds it.
pub fn test_devfs_link_names_follow_udev() -> TestResult {
    let names: [&[u8]; 2] = [b"volidudev0", b"volidudev1"];
    let (Some(mixed), Some(dots)) = (
        node(|b| ext_superblock(b, b"caf\xc3\xa9\xff")),
        node(|b| ext_superblock(b, b"..")),
    ) else {
        return fail!("no memory for the fixture");
    };
    let registered = [
        devfs_register_block_device(names[0], mixed),
        devfs_register_block_device(names[1], dots),
    ];
    let verdict = if registered.iter().all(Result::is_ok) {
        udev_names_body(names)
    } else {
        fail!("registration failed: {:?}", registered)
    };
    for name in names {
        let _ = devfs_unregister_block_node(name);
    }
    verdict
}

#[inline(never)]
fn udev_names_body(names: [&[u8]; 2]) -> TestResult {
    let mut out = [0u8; DEV_NAME_MAX];
    let mut resolve = |spec: &[u8]| devfs_resolve_block_source(spec, &mut out);
    match resolve(b"/dev/disk/by-label/caf\xc3\xa9\\xff") {
        Ok(len) if len == names[0].len() => {}
        other => return fail!("the mixed label's link resolved to {:?}", other),
    }
    if resolve(b"/dev/disk/by-label/..") != Err(VfsError::NotFound) {
        return fail!("a `..` label became a link");
    }
    if resolve(b"LABEL=..").is_err() {
        return fail!("LABEL= did not find the `..` label");
    }
    let fs = DevFs::new();
    let Ok(disk) = fs.lookup(fs.root_inode(), b"disk") else {
        return fail!("/dev/disk missing");
    };
    let Ok(by_label) = fs.lookup(disk, b"by-label") else {
        return fail!("/dev/disk/by-label missing");
    };
    let mut dotdots = 0;
    let listed = fs.readdir(by_label, 0, &mut |name, _, _| {
        dotdots += usize::from(name == b"..");
        true
    });
    match listed {
        Ok(_) if dotdots == 1 => TestResult::Pass,
        other => fail!("by-label listed `..` {} times: {:?}", dotdots, other),
    }
}

/// A disk that cannot be read names no volume, without being remembered as
/// carrying none: once a writer lets go of it, it is read again.
pub fn test_devfs_identity_of_an_unreadable_disk() -> TestResult {
    const NAME: &[u8] = b"volidflaky0";
    let Some(image) = device(4096, |b| ext_superblock(b, b"flaky")) else {
        return fail!("no memory for the fixture");
    };
    let Ok(flaky) = KArc::try_new(FailingReads {
        inner: image,
        failing: AtomicBool::new(true),
    }) else {
        return fail!("no memory for the fixture");
    };
    if !matches!(probe(flaky.as_ref()), Err(Unreadable)) {
        return fail!("a failed read probed as an answer");
    }
    let node: KArc<dyn BlockDevice + Send + Sync> = flaky.clone();
    if let Err(e) = devfs_register_block_device(NAME, node) {
        return fail!("registration failed: {:?}", e);
    }
    let mut out = [0u8; DEV_NAME_MAX];
    let hidden = devfs_resolve_block_source(b"LABEL=flaky", &mut out);
    flaky.failing.store(false, Ordering::Relaxed);
    devfs_block_contents_changed();
    let found = devfs_resolve_block_source(b"LABEL=flaky", &mut out);
    let _ = devfs_unregister_block_node(NAME);
    match (hidden, found) {
        (Err(VfsError::NotFound), Ok(len)) if out[..len] == *NAME => TestResult::Pass,
        other => fail!(
            "an unreadable disk then a readable one resolved as {:?}",
            other
        ),
    }
}

struct FailingReads {
    inner: MemoryBlockDevice,
    failing: AtomicBool,
}

impl BlockDevice for FailingReads {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if self.failing.load(Ordering::Relaxed) {
            return Err(BlockDeviceError::DeviceFault);
        }
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.inner.logical_block_size()
    }
}

slopos_testing::stest!(name = test_volume_id_ext, suite = fs);
slopos_testing::stest!(name = test_volume_id_vfat, suite = fs);
slopos_testing::stest!(name = test_volume_id_vfat_root_past_the_end, suite = fs);
slopos_testing::stest!(name = test_volume_id_btrfs, suite = fs);
slopos_testing::stest!(name = test_devfs_disk_links, suite = fs);
slopos_testing::stest!(name = test_devfs_link_names_follow_udev, suite = fs);
slopos_testing::stest!(name = test_devfs_identity_of_an_unreadable_disk, suite = fs);
slopos_testing::stest!(name = test_devfs_identity_read_once_per_change, suite = fs);
