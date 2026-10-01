//! The block layer every block driver registers its disks with: their names,
//! partition tables, `/dev` nodes, and the claims a mount or a raw write
//! takes.
//!
//! Names follow Linux: `vda` in virtio probe order, `nvme<C>n<N>` for
//! namespace `N` of the `C`-th NVMe controller, and a partition appended as
//! `vda1` or, after a name ending in a digit, `nvme0n1p1`. Probe order is not
//! stable across machines, so `/dev/disk/by-*` and `PARTUUID=`, `UUID=` and
//! `LABEL=` are the spellings that are.
//!
//! A claim covers what it names: a partition's excludes the whole disk and
//! itself, the whole disk's excludes every partition. A write claim is
//! exclusive; read claims, which read-only mounts take, share with each other
//! and exclude a writer to what they cover. The table is re-read only while
//! nothing on the disk is claimed.

pub mod engine;

mod disk;

pub use disk::EngineDisk;

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_boot_core::Guid;
use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, WriteTicket};
use slopos_fs::devfs::{
    BlockNodeKind, DEV_NAME_MAX, devfs_block_contents_changed, devfs_register_block_node,
    devfs_set_block_partitioned, devfs_unregister_block_node,
};
use slopos_fs::partition::{
    PartitionDevice, PartitionEntry, PartitionError, PartitionKind, PartitionScheme, probe,
};
use slopos_fs::vfs::{BlockLayerOps, VfsError, VfsResult};
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, Mutex, MutexGuard};
use slopos_ostd::{KArc, KBox, KVec, klog_info, lock_class};

const MAX_DISKS: usize = 32;

/// A disk's or a partition's kernel name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DiskName {
    buf: [u8; DEV_NAME_MAX],
    len: usize,
}

impl core::fmt::Display for DiskName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(core::str::from_utf8(self.as_bytes()).unwrap_or("?"))
    }
}

impl DiskName {
    fn empty() -> Self {
        Self {
            buf: [0; DEV_NAME_MAX],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let end = (self.len + bytes.len()).min(DEV_NAME_MAX);
        self.buf[self.len..end].copy_from_slice(&bytes[..end - self.len]);
        self.len = end;
    }

    fn push_decimal(&mut self, mut value: u32) {
        let mut digits = [0u8; 10];
        let mut at = digits.len();
        loop {
            at -= 1;
            digits[at] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        self.push(&digits[at..]);
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// `vd` and the `index`-th letter name: `a`..`z`, then `aa`, `ab`, …
    pub fn virtio(index: usize) -> Self {
        let mut letters = [0u8; 8];
        let mut at = letters.len();
        let mut n = index + 1;
        while n > 0 && at > 0 {
            n -= 1;
            at -= 1;
            letters[at] = b'a' + (n % 26) as u8;
            n /= 26;
        }
        let mut name = Self::empty();
        name.push(b"vd");
        name.push(&letters[at..]);
        name
    }

    pub fn nvme(controller: u32, nsid: u32) -> Self {
        let mut name = Self::empty();
        name.push(b"nvme");
        name.push_decimal(controller);
        name.push(b"n");
        name.push_decimal(nsid);
        name
    }

    fn ends_in_digit(&self) -> bool {
        self.as_bytes().last().is_some_and(u8::is_ascii_digit)
    }

    /// Partition `number` of this disk.
    pub fn partition(&self, number: u8) -> Self {
        let mut name = *self;
        if self.ends_in_digit() {
            name.push(b"p");
        }
        name.push_decimal(u32::from(number));
        name
    }

    /// The partition number `name` gives this disk, if it names one of its
    /// partitions: the canonical spelling only.
    fn partition_number(&self, name: &[u8]) -> Option<u8> {
        let mut rest = name.strip_prefix(self.as_bytes())?;
        if self.ends_in_digit() {
            rest = rest.strip_prefix(b"p")?;
        }
        if rest.is_empty() || rest[0] == b'0' || !rest.iter().all(u8::is_ascii_digit) {
            return None;
        }
        core::str::from_utf8(rest).ok()?.parse::<u8>().ok()
    }
}

/// At most one writer, or any number of readers.
#[derive(Clone, Copy, Default)]
struct Holders {
    writer: bool,
    readers: u32,
}

impl Holders {
    fn any(&self) -> bool {
        self.writer || self.readers > 0
    }

    fn take(&mut self, access: Access) {
        match access {
            Access::Write => self.writer = true,
            Access::Read => self.readers += 1,
        }
    }

    fn release(&mut self, access: Access) {
        match access {
            Access::Write => self.writer = false,
            Access::Read => self.readers = self.readers.saturating_sub(1),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Write,
    Read,
}

struct Partition {
    entry: PartitionEntry,
    holders: Holders,
}

struct Disk {
    name: DiskName,
    device: KArc<EngineDisk>,
    scheme: PartitionScheme,
    whole: Holders,
    /// Set while a table is read with the registry unlocked: nothing on the
    /// disk may be claimed until the table it names is the one in place.
    scanning: bool,
    partitions: KVec<Partition>,
}

impl Disk {
    fn held(&self) -> bool {
        self.whole.any() || self.partitions.iter().any(|p| p.holders.any())
    }

    fn written(&self) -> bool {
        self.whole.writer || self.partitions.iter().any(|p| p.holders.writer)
    }
}

/// Registration order is probe order: `root=auto`'s disk0 is the first.
/// A sleeping lock, since registering and re-reading allocate under it; no
/// I/O ever runs under it.
static DISKS: Mutex<KVec<Disk>> =
    Mutex::new(KVec::new(), lock_class!("BLOCK_DISKS", LOCK_LEVEL_REGISTRY));

/// A killed task still releases what it claimed, and nothing under the lock
/// blocks, so its acquire spins where a live task's sleeps.
fn disks() -> MutexGuard<'static, KVec<Disk>> {
    match DISKS.lock() {
        Ok(guard) => guard,
        Err(_) => loop {
            if let Some(guard) = DISKS.try_lock() {
                break guard;
            }
            core::hint::spin_loop();
        },
    }
}

/// What a claim covers, so dropping it releases exactly that.
#[derive(Clone, Copy)]
enum Target {
    Whole,
    Partition(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// No disk or partition has that name.
    NoDevice,
    /// Something already holds it, or a table re-read is in progress.
    Busy,
    NoMemory,
    /// The partition's window does not fit the disk.
    Unusable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RereadError {
    NoDevice,
    /// The name is a partition's: only a whole disk has a table.
    NotWholeDisk,
    Busy,
    Table,
    NoMemory,
}

/// A disk's view for its `/dev` nodes: it reads and flushes, and a write
/// through a node takes a claim for that one call instead.
struct DiskReader(KArc<EngineDisk>);

impl BlockDevice for DiskReader {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.0.read_at(offset, buffer)
    }

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::WriteProtected)
    }

    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.0.logical_block_size()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.0.flush()
    }
}

/// Register a disk a driver probed, read its partition table and publish its
/// `/dev` nodes.
pub fn register_disk(name: DiskName, device: KArc<EngineDisk>) -> bool {
    {
        let mut disks = disks();
        if disks.len() >= MAX_DISKS || disks.iter().any(|d| d.name == name) {
            klog_info!("BLOCK: cannot register {}", name);
            return false;
        }
        let entry = Disk {
            name,
            device: KArc::clone(&device),
            scheme: PartitionScheme::None,
            whole: Holders::default(),
            scanning: true,
            partitions: KVec::new(),
        };
        if disks.push(entry).is_err() {
            return false;
        }
    }
    let reader: KArc<dyn BlockDevice + Send + Sync> = match KArc::try_new(DiskReader(device)) {
        Ok(reader) => reader,
        Err(_) => {
            let removed = {
                let mut disks = disks();
                disks
                    .iter()
                    .position(|d| d.name == name)
                    .map(|at| disks.remove(at))
            };
            drop(removed);
            return false;
        }
    };
    if let Err(e) = devfs_register_block_node(
        name.as_bytes(),
        KArc::clone(&reader),
        BlockNodeKind::Disk { partitioned: false },
    ) {
        klog_info!("BLOCK: /dev/{} not published: {:?}", name, e);
    }
    let (scheme, partitions) = scan(&name, reader.as_ref()).unwrap_or_else(|e| {
        klog_info!("BLOCK: {} partition table unusable: {:?}", name, e);
        (PartitionScheme::None, KVec::new())
    });
    publish_partitions(&name, &reader, &partitions);
    finish_scan(&name, scheme, partitions);
    true
}

/// The table on `device` and the partitions it holds; none when it has no
/// table.
fn scan(
    name: &DiskName,
    device: &(dyn BlockDevice + Send + Sync),
) -> Result<(PartitionScheme, KVec<Partition>), PartitionError> {
    let mut partitions = KVec::new();
    let table = probe(device)?;
    if table.scheme == PartitionScheme::None {
        return Ok((table.scheme, partitions));
    }
    for entry in table.entries.iter() {
        if partitions
            .push(Partition {
                entry: *entry,
                holders: Holders::default(),
            })
            .is_err()
        {
            klog_info!("BLOCK: {} has more partitions than memory for them", name);
            break;
        }
    }
    klog_info!(
        "BLOCK: {} holds a {:?} table of {} partition(s)",
        name,
        table.scheme,
        partitions.len()
    );
    Ok((table.scheme, partitions))
}

fn publish_partitions(
    name: &DiskName,
    reader: &KArc<dyn BlockDevice + Send + Sync>,
    partitions: &[Partition],
) {
    for part in partitions {
        let entry = &part.entry;
        let part_name = name.partition(entry.number);
        let window = match PartitionDevice::try_new(KArc::clone(reader), entry.start, entry.len) {
            Ok(window) => window,
            Err(e) => {
                klog_info!("BLOCK: {} unusable: {:?}", part_name, e);
                continue;
            }
        };
        let Ok(window) = KArc::try_new(window) else {
            continue;
        };
        let window: KArc<dyn BlockDevice + Send + Sync> = window;
        if let Err(e) = devfs_register_block_node(
            part_name.as_bytes(),
            window,
            BlockNodeKind::Partition { uuid: entry.uuid },
        ) {
            klog_info!("BLOCK: /dev/{} not published: {:?}", part_name, e);
        }
    }
}

/// Install a scanned table and let claims in again. The table it replaces
/// drops here, with the registry unlocked.
fn finish_scan(name: &DiskName, scheme: PartitionScheme, partitions: KVec<Partition>) {
    let _ = devfs_set_block_partitioned(name.as_bytes(), !partitions.is_empty());
    let replaced = {
        let mut disks = disks();
        let Some(disk) = disks.iter_mut().find(|d| d.name == *name) else {
            return;
        };
        disk.scanning = false;
        disk.scheme = scheme;
        core::mem::replace(&mut disk.partitions, partitions)
    };
    drop(replaced);
}

pub fn disk_count() -> usize {
    disks().len()
}

/// The `index`-th disk registered, in probe order.
pub fn disk_name(index: usize) -> Option<DiskName> {
    disks().get(index).map(|d| d.name)
}

/// A partition of a registered disk and its window on that disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub disk: DiskName,
    pub partition: DiskName,
    pub start: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocateError {
    /// No registered disk's GPT names that disk GUID.
    NoDisk,
    /// The disk holds no partition of that type.
    NoPartition,
    /// Two disks carry the GUID, or the disk two partitions of the type.
    Ambiguous,
}

/// The one partition of type `type_guid` on the one disk whose GPT names it
/// `disk`.
pub fn locate_partition(disk: Guid, type_guid: Guid) -> Result<Located, LocateError> {
    let disks = disks();
    let mut holders = disks
        .iter()
        .filter(|d| d.scheme == PartitionScheme::Gpt { disk });
    let holder = holders.next().ok_or(LocateError::NoDisk)?;
    if holders.next().is_some() {
        return Err(LocateError::Ambiguous);
    }
    let mut typed = holder
        .partitions
        .iter()
        .filter(|p| p.entry.kind == PartitionKind::Gpt { type_guid });
    let part = typed.next().ok_or(LocateError::NoPartition)?;
    if typed.next().is_some() {
        return Err(LocateError::Ambiguous);
    }
    Ok(Located {
        disk: holder.name,
        partition: holder.name.partition(part.entry.number),
        start: part.entry.start,
        len: part.entry.len,
    })
}

/// Where `name` points: the disk's index and what on it.
fn locate(disks: &KVec<Disk>, name: &[u8]) -> Option<(usize, Target)> {
    let name = name.strip_prefix(b"/dev/").unwrap_or(name);
    disks.iter().enumerate().find_map(|(i, d)| {
        if d.name.as_bytes() == name {
            return Some((i, Target::Whole));
        }
        d.name
            .partition_number(name)
            .map(|n| (i, Target::Partition(n)))
    })
}

/// An exclusive writable window onto the disk or partition `name` names.
/// Dropping it releases the claim.
pub fn claim(name: &[u8]) -> Result<KBox<dyn BlockDevice + Send + Sync>, ClaimError> {
    claim_as(name, Access::Write)
}

/// A read-only window onto what `name` names, shared with other readers and
/// held against any writer to it, a table re-read included.
pub fn claim_read(name: &[u8]) -> Result<KBox<dyn BlockDevice + Send + Sync>, ClaimError> {
    claim_as(name, Access::Read)
}

fn claim_as(
    name: &[u8],
    access: Access,
) -> Result<KBox<dyn BlockDevice + Send + Sync>, ClaimError> {
    let (owner, device, target, window) = {
        let mut disks = disks();
        let (index, target) = locate(&disks, name).ok_or(ClaimError::NoDevice)?;
        let disk = &mut disks[index];
        if disk.scanning {
            return Err(ClaimError::Busy);
        }
        let window = match target {
            Target::Whole => {
                let busy = match access {
                    Access::Write => disk.held(),
                    Access::Read => disk.written(),
                };
                if busy {
                    return Err(ClaimError::Busy);
                }
                disk.whole.take(access);
                None
            }
            Target::Partition(number) => {
                let whole = disk.whole;
                let part = disk
                    .partitions
                    .iter_mut()
                    .find(|p| p.entry.number == number)
                    .ok_or(ClaimError::NoDevice)?;
                let busy = match access {
                    Access::Write => whole.any() || part.holders.any(),
                    Access::Read => whole.writer || part.holders.writer,
                };
                if busy {
                    return Err(ClaimError::Busy);
                }
                part.holders.take(access);
                Some((part.entry.start, part.entry.len))
            }
        };
        (disk.name, KArc::clone(&disk.device), target, window)
    };
    let guard = ClaimGuard {
        disk: owner,
        target,
        access,
        wrote: AtomicBool::new(false),
    };
    let device: KArc<dyn BlockDevice + Send + Sync> = device;
    let inner: KBox<dyn BlockDevice + Send + Sync> = match window {
        None => KBox::try_new(slopos_fs::partition::SharedBlockDevice(device))
            .map_err(|_| ClaimError::NoMemory)?,
        Some((start, len)) => {
            let window =
                PartitionDevice::try_new(device, start, len).map_err(|_| ClaimError::Unusable)?;
            KBox::try_new(window).map_err(|_| ClaimError::NoMemory)?
        }
    };
    KBox::try_new(Claimed { inner, guard })
        .map(|claimed| claimed as KBox<dyn BlockDevice + Send + Sync>)
        .map_err(|_| ClaimError::NoMemory)
}

/// Releases its claim when dropped, whether or not the window around it was
/// ever built.
struct ClaimGuard {
    disk: DiskName,
    target: Target,
    access: Access,
    wrote: AtomicBool,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        // What a writer left may be a new filesystem with a new identity,
        // and no claim may resolve by the old one once this one is gone.
        if self.wrote.load(Ordering::Acquire) {
            devfs_block_contents_changed();
        }
        {
            let mut disks = disks();
            let Some(disk) = disks.iter_mut().find(|d| d.name == self.disk) else {
                return;
            };
            match self.target {
                Target::Whole => disk.whole.release(self.access),
                Target::Partition(number) => {
                    if let Some(part) = disk
                        .partitions
                        .iter_mut()
                        .find(|p| p.entry.number == number)
                    {
                        part.holders.release(self.access);
                    }
                }
            }
        }
    }
}

struct Claimed {
    inner: KBox<dyn BlockDevice + Send + Sync>,
    guard: ClaimGuard,
}

impl Claimed {
    fn begin_write(&self) -> Result<(), BlockDeviceError> {
        match self.guard.access {
            Access::Write => {
                self.guard.wrote.store(true, Ordering::Release);
                Ok(())
            }
            Access::Read => Err(BlockDeviceError::WriteProtected),
        }
    }
}

impl BlockDevice for Claimed {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.begin_write()?;
        self.inner.write_at(offset, buffer)
    }

    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.begin_write()?;
        self.inner.write_vectored(offset, segs)
    }

    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        self.begin_write()?;
        self.inner.submit_write(offset, segs)
    }

    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        self.inner.complete_write(ticket)
    }

    fn write_depth(&self) -> usize {
        self.inner.write_depth()
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.inner.logical_block_size()
    }

    fn write_protected(&self) -> bool {
        self.guard.access == Access::Read || self.inner.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.inner.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.inner.checkpoint()
    }
}

/// Re-read the partition table of the disk `name` names, replacing its
/// partition nodes. Refused while anything on the disk is claimed: a mounted
/// partition's window must not move under it.
pub fn reread(name: &[u8]) -> Result<(), RereadError> {
    let (whole, device, old) = {
        let mut disks = disks();
        let (index, target) = locate(&disks, name).ok_or(RereadError::NoDevice)?;
        if let Target::Partition(_) = target {
            return Err(RereadError::NotWholeDisk);
        }
        let disk = &mut disks[index];
        if disk.scanning || disk.held() {
            return Err(RereadError::Busy);
        }
        disk.scanning = true;
        let mut old = KVec::new();
        for part in disk.partitions.iter() {
            if old.push(disk.name.partition(part.entry.number)).is_err() {
                disk.scanning = false;
                return Err(RereadError::NoMemory);
            }
        }
        (disk.name, KArc::clone(&disk.device), old)
    };
    for part in old.iter() {
        let _ = devfs_unregister_block_node(part.as_bytes());
    }
    let reader: KArc<dyn BlockDevice + Send + Sync> = match KArc::try_new(DiskReader(device)) {
        Ok(reader) => reader,
        Err(_) => {
            finish_scan(&whole, PartitionScheme::None, KVec::new());
            return Err(RereadError::NoMemory);
        }
    };
    let (scheme, partitions) = match scan(&whole, reader.as_ref()) {
        Ok(table) => table,
        Err(e) => {
            klog_info!("BLOCK: {} re-read: table unusable: {:?}", whole, e);
            finish_scan(&whole, PartitionScheme::None, KVec::new());
            return Err(RereadError::Table);
        }
    };
    publish_partitions(&whole, &reader, &partitions);
    finish_scan(&whole, scheme, partitions);
    Ok(())
}

fn vfs_error(e: ClaimError) -> VfsError {
    match e {
        ClaimError::NoDevice => VfsError::NotFound,
        ClaimError::Busy => VfsError::Busy,
        ClaimError::NoMemory => VfsError::NoSpace,
        ClaimError::Unusable => VfsError::IoError,
    }
}

fn vfs_claim(name: &[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    claim(name).map_err(vfs_error)
}

fn vfs_claim_read(name: &[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    claim_read(name).map_err(vfs_error)
}

fn vfs_reread(name: &[u8]) -> VfsResult<()> {
    reread(name).map_err(|e| match e {
        RereadError::NoDevice => VfsError::NotFound,
        RereadError::NotWholeDisk => VfsError::InvalidArgument,
        RereadError::Busy => VfsError::Busy,
        RereadError::Table => VfsError::IoError,
        RereadError::NoMemory => VfsError::NoSpace,
    })
}

/// What boot hands the VFS, which cannot name a driver.
pub static VFS_OPS: BlockLayerOps = BlockLayerOps {
    claim: vfs_claim,
    claim_read: vfs_claim_read,
    reread: vfs_reread,
};

/// The disk registered as `name`, for tests that drive its engine directly.
#[cfg(feature = "test-hooks")]
pub fn disk(name: &[u8]) -> Option<KArc<EngineDisk>> {
    let disks = disks();
    disks
        .iter()
        .find(|d| d.name.as_bytes() == name)
        .map(|d| KArc::clone(&d.device))
}
