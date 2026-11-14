//! The disks `scripts/qemu_run.sh` attaches in test mode, found by the
//! identity it gives each one and never by node name: on a real machine
//! `nvme0n2`, `vdb` or `sda` is somebody's disk.

use core::fmt;

use slopos_boot_core::disk_id::{DISK_ID_MAX, DiskId};

use crate::devfs::{DEV_NAME_MAX, devfs_disks_with_identity};

/// The model QEMU's NVMe controller identifies as.
const QEMU_NVME: &[u8] = b"QEMU NVMe Ctrl";

/// A disk the suite attaches. Only [`TestDisk::scratch`] ones are blank each
/// run and may be written by any test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestDisk {
    /// The tests image the boot mounts as `/`: namespace 1 of `slopos-root`.
    Root,
    /// The labelled `slopos-media` volume, 4096-byte blocks.
    Media,
    /// The shipped verified image, attached `snapshot=on`.
    Verified,
    /// Namespace 2 of `slopos-root`, 512-byte blocks.
    NvmeScratch,
    /// Namespace 2 of `slopos-4kn`, 4096-byte blocks.
    NvmeScratch4kn,
    /// The only namespace of the `slopos-spare` controller.
    NvmeSpare,
    VirtioScratch,
    UsbScratch,
}

impl TestDisk {
    pub const ALL: [TestDisk; 8] = [
        TestDisk::Root,
        TestDisk::Media,
        TestDisk::Verified,
        TestDisk::NvmeScratch,
        TestDisk::NvmeScratch4kn,
        TestDisk::NvmeSpare,
        TestDisk::VirtioScratch,
        TestDisk::UsbScratch,
    ];

    pub fn scratch(self) -> bool {
        !matches!(self, TestDisk::Root | TestDisk::Media | TestDisk::Verified)
    }

    fn id(self) -> Option<DiskId> {
        match self {
            TestDisk::Root => DiskId::nvme(QEMU_NVME, b"slopos-root", 1),
            TestDisk::Media => DiskId::nvme(QEMU_NVME, b"slopos-4kn", 1),
            TestDisk::Verified => DiskId::virtio(b"slopos-verified"),
            TestDisk::NvmeScratch => DiskId::nvme(QEMU_NVME, b"slopos-root", 2),
            TestDisk::NvmeScratch4kn => DiskId::nvme(QEMU_NVME, b"slopos-4kn", 2),
            TestDisk::NvmeSpare => DiskId::nvme(QEMU_NVME, b"slopos-spare", 1),
            TestDisk::VirtioScratch => DiskId::virtio(b"slopos-scratch"),
            TestDisk::UsbScratch => DiskId::usb(b"QEMU", b"QEMU HARDDISK", Some(b"slopos-usb"), 0),
        }
    }

    /// Its `/dev/disk/by-id` name, into `out`; its length.
    pub fn by_id(self, out: &mut [u8; DISK_ID_MAX]) -> usize {
        self.id()
            .and_then(|id| id.write(false, None, out))
            .unwrap_or(0)
    }
}

/// The node a registered disk is published as, what a claim or a mount
/// source names it by.
#[derive(Clone, Copy)]
pub struct DiskNode {
    name: [u8; DEV_NAME_MAX],
    len: usize,
}

impl DiskNode {
    pub fn as_bytes(&self) -> &[u8] {
        &self.name[..self.len]
    }
}

impl fmt::Display for DiskNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(core::str::from_utf8(self.as_bytes()).unwrap_or("?"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No registered disk carries the identity.
    Absent(TestDisk),
    /// More than one does, so neither is known to be the suite's.
    Ambiguous(TestDisk),
    /// Asked for as scratch, a disk the suite needs kept.
    NotScratch(TestDisk),
    /// A by-id name the suite attaches no disk as.
    Unknown,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (disk, why) = match *self {
            Refusal::Absent(disk) => (disk, "is not attached"),
            Refusal::Ambiguous(disk) => (disk, "names more than one disk"),
            Refusal::NotScratch(disk) => (disk, "is no scratch disk"),
            Refusal::Unknown => {
                return f.write_str("no disk the suite attaches has that by-id name");
            }
        };
        let mut by_id = [0u8; DISK_ID_MAX];
        let len = disk.by_id(&mut by_id);
        let name = core::str::from_utf8(&by_id[..len]).unwrap_or("?");
        write!(f, "the suite's disk {} {}", name, why)
    }
}

/// The node of the disk the suite attached as `disk`, when exactly one
/// registered disk carries its identity.
pub fn suite_disk(disk: TestDisk) -> Result<DiskNode, Refusal> {
    let id = disk.id().ok_or(Refusal::Absent(disk))?;
    let mut node = DiskNode {
        name: [0; DEV_NAME_MAX],
        len: 0,
    };
    match devfs_disks_with_identity(&id, &mut node.name) {
        0 => Err(Refusal::Absent(disk)),
        1 => {
            node.len = node
                .name
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(DEV_NAME_MAX);
            Ok(node)
        }
        _ => Err(Refusal::Ambiguous(disk)),
    }
}

/// [`suite_disk`] for a disk a test may destroy.
pub fn scratch_disk(disk: TestDisk) -> Result<DiskNode, Refusal> {
    if !disk.scratch() {
        return Err(Refusal::NotScratch(disk));
    }
    suite_disk(disk)
}

/// [`scratch_disk`] by `/dev/disk/by-id` name.
pub fn scratch_disk_by_id(by_id: &[u8]) -> Result<DiskNode, Refusal> {
    let mut name = [0u8; DISK_ID_MAX];
    let disk = TestDisk::ALL
        .into_iter()
        .find(|disk| {
            let len = disk.by_id(&mut name);
            name[..len] == *by_id
        })
        .ok_or(Refusal::Unknown)?;
    scratch_disk(disk)
}
