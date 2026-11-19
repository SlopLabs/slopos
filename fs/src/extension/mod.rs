//! Extensions: an image activated beside the base, shown read-only and pinned
//! at `/opt/<name>`. An image is a file of another filesystem, an ext4
//! volume and its SHA-256 tree, and an activation reads it in place through
//! a device that checks each block against the tree ([`device`]), its volume
//! served by an ext4 instance made for it from the kernel heap.
//!
//! One filesystem, [`ACTIVATIONS`], serves every activation, each at a mount
//! of its own. Each activation is numbered across every activation and never
//! again within a boot, and the number is the high half of every inode it
//! serves, so an inode names its activation for good and is how a call finds
//! it. Replacing one moves the mount to the next in one step; the one
//! replaced is freed once nothing holds it, as a retired base generation is.
//! What the resident activations hold together is bounded by a budget of
//! memory the activating caller names, as the base's generations are.

mod device;
mod index;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use slopos_abi::extension::{DIGEST_LEN, Label, NAME_MAX};
use slopos_abi::fs::EXTENSION_MAGIC;
use slopos_ext4_core::superblock::{self as sbk, incompat, off as soff, ro_compat};
use slopos_ostd::mm::{AllocError, init_from_owned};
use slopos_ostd::process::Program;
use slopos_ostd::sync::{
    InitFlag, LOCK_LEVEL_REGISTRY, LOCK_LEVEL_RESOURCE, LockClassKey, Mutex, SpinLock,
    SpinLockGuard, register_class_eagerly,
};
use slopos_ostd::{KArc, KBox, KVec, klog_info, lock_class};
use slopos_verity_core::BLOCK;

use self::device::VerityFile;
pub use self::device::{OpenError, reads};
use self::index::Index;
pub use self::index::IndexError;
use crate::basefs::{generation_of, reap};
use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::ext4_vfs::Ext4Mount;
use crate::vfs::mount::{MOUNT_PINNED, MOUNT_RDONLY, mount_replacing, unmount_where};
use crate::vfs::traits::{FsStats, Owner, same_filesystem};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

/// Every volume's root, as ext4 numbers it.
const ROOT: u32 = 2;

/// The last number an activation may take: a program identity keeps the
/// top bit of an inode for itself.
const LAST_NUMBER: u32 = i32::MAX as u32;

/// Every activation's volume shares one class, which nothing takes twice:
/// none is mounted beneath another, and the file a volume is read from is
/// never an activation's.
const CLASS: &LockClassKey = lock_class!("EXT_ACTIVATION", LOCK_LEVEL_RESOURCE);

pub static ACTIVATIONS: Activations = Activations::new();

/// The last activation number issued, moved only under [`ACTIVATING`].
static ISSUED: AtomicU32 = AtomicU32::new(0);

static ACTIVATING: Mutex<()> = Mutex::new((), lock_class!("EXT_ACTIVATING", LOCK_LEVEL_RESOURCE));

static CLASS_REGISTERED: InitFlag = InitFlag::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivateError {
    /// A name the grammar refuses.
    Name,
    /// The resident activations would hold more than the budget with it, or
    /// the numbers are spent.
    Busy,
    Image(OpenError),
    /// A volume no activation serves: why.
    Volume(&'static str),
    Index(IndexError),
    Attach(VfsError),
    Mounts(VfsError),
    NoMemory,
    Interrupted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Being built: not yet shown, no reap pass may take it.
    Admitted,
    Shown,
    /// Shown no more; `quiet` once a reap pass found nothing holding it.
    Retired {
        quiet: bool,
    },
}

/// One activation: its volume, its index, and the number its inodes carry.
struct Activation {
    number: u32,
    label: Label,
    volume: KBox<Ext4Mount>,
    index: Index,
    /// What it holds that no reclaim gives back, against the budget.
    held: u64,
    /// Calls into the volume in flight, which a reap pass waits out.
    users: AtomicUsize,
    /// A reap pass is freeing it: no call enters.
    gone: AtomicBool,
}

struct Slot {
    activation: KArc<Activation>,
    phase: Phase,
}

struct Table {
    /// Every resident activation, by number: each number is issued above
    /// every one before it, so an activation joins at the end.
    resident: KVec<Slot>,
}

impl Table {
    fn at(&self, number: u32) -> Option<usize> {
        self.resident
            .as_slice()
            .binary_search_by_key(&number, |slot| slot.activation.number)
            .ok()
    }

    fn held(&self) -> u64 {
        self.resident.iter().map(|slot| slot.activation.held).sum()
    }
}

/// Every activation, served as one filesystem.
pub struct Activations {
    table: SpinLock<Table>,
    /// An activation is retired, so a reap pass has something to look at.
    retired: AtomicBool,
}

/// A call into an activation's volume, counted while it runs.
struct Entered {
    activation: KArc<Activation>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        self.activation.users.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Entered {
    fn outer(&self, inner: InodeId) -> InodeId {
        inode_of(self.activation.number, inner as u32)
    }
}

const fn inode_of(number: u32, ino: u32) -> InodeId {
    ((number as u64) << 32) | ino as u64
}

fn inner(inode: InodeId) -> InodeId {
    InodeId::from(inode as u32)
}

impl Activations {
    const fn new() -> Self {
        Self {
            table: SpinLock::new(
                Table {
                    resident: KVec::new(),
                },
                lock_class!("EXT_ACTIVATIONS", LOCK_LEVEL_REGISTRY),
            ),
            retired: AtomicBool::new(false),
        }
    }

    /// Activation `number`, while it is resident.
    fn find(&self, number: u32) -> Option<KArc<Activation>> {
        let table = self.table.lock();
        let at = table.at(number)?;
        Some(table.resident[at].activation.clone())
    }

    /// A call into the activation `inode` is of: a reap pass frees one only
    /// with none in flight.
    fn enter(&self, inode: InodeId) -> VfsResult<Entered> {
        let activation = self.find(generation_of(inode)).ok_or(VfsError::NotFound)?;
        activation.users.fetch_add(1, Ordering::SeqCst);
        let entered = Entered { activation };
        if entered.activation.gone.load(Ordering::SeqCst) {
            return Err(VfsError::NotFound);
        }
        Ok(entered)
    }

    /// The table, locked with room for one more activation: nothing under
    /// the lock reaches the allocator, which a reclaim pass may be holding.
    fn table_with_room(&self) -> Result<SpinLockGuard<'_, Table>, ActivateError> {
        loop {
            let table = self.table.lock();
            if table.resident.len() < table.resident.capacity() {
                return Ok(table);
            }
            let want = (table.resident.capacity() * 2).max(8);
            drop(table);
            let mut fresh = KVec::with_capacity(want).map_err(|_| ActivateError::NoMemory)?;
            let mut table = self.table.lock();
            if table.resident.capacity() < want {
                for slot in table.resident.drain(..) {
                    let _ = fresh.push(slot);
                }
                core::mem::swap(&mut table.resident, &mut fresh);
            }
            drop(table);
        }
    }

    /// Make `activation` resident, not yet shown, when the resident ones
    /// hold no more than `budget` with it.
    fn admit(&self, activation: KArc<Activation>, budget: u64) -> Result<(), ActivateError> {
        let mut table = self.table_with_room()?;
        if table.held().saturating_add(activation.held) > budget {
            drop(table);
            return Err(ActivateError::Busy);
        }
        let slot = Slot {
            activation,
            phase: Phase::Admitted,
        };
        table
            .resident
            .push(slot)
            .map_err(|_| ActivateError::NoMemory)
    }

    fn note_retired(&self, table: &Table) {
        let retired = table
            .resident
            .iter()
            .any(|slot| matches!(slot.phase, Phase::Retired { .. }));
        self.retired.store(retired, Ordering::Release);
    }

    fn set_phase(&self, number: u32, phase: Phase) {
        let mut table = self.table.lock();
        if let Some(at) = table.at(number) {
            table.resident.as_mut_slice()[at].phase = phase;
        }
        self.note_retired(&table);
    }

    /// Take activation `number` out of the table, its volume detached; what
    /// still holds it keeps only memory.
    fn withdraw(&self, number: u32) {
        let taken = {
            let mut table = self.table.lock();
            let taken = table.at(number).map(|at| table.resident.remove(at));
            self.note_retired(&table);
            taken
        };
        if let Some(slot) = taken {
            slot.activation.volume.detach();
        }
    }

    /// The first retired activation numbered above `after`, and whether a
    /// pass found it quiet.
    fn retired_after(&self, after: u32) -> Option<(KArc<Activation>, bool)> {
        let table = self.table.lock();
        let from = table
            .resident
            .as_slice()
            .partition_point(|slot| slot.activation.number <= after);
        table.resident.as_slice()[from..]
            .iter()
            .find_map(|slot| match slot.phase {
                Phase::Retired { quiet } => Some((slot.activation.clone(), quiet)),
                _ => None,
            })
    }

    /// One reap step: mark a retired activation held or quiet, and free one
    /// found quiet twice with no call in flight.
    fn reap_step(&self, activation: &Activation, quiet: bool) -> reap::Pass {
        let mut pass = reap::Pass::default();
        let number = activation.number;
        if held(number) {
            self.set_phase(number, Phase::Retired { quiet: false });
            return pass;
        }
        if !quiet {
            self.set_phase(number, Phase::Retired { quiet: true });
            pass.marked = true;
            return pass;
        }
        activation.gone.store(true, Ordering::SeqCst);
        if activation.users.load(Ordering::SeqCst) != 0 {
            activation.gone.store(false, Ordering::SeqCst);
            pass.marked = true;
            return pass;
        }
        crate::filemap::forget_where(&ACTIVATIONS, |inode| generation_of(inode) == number);
        if !activation.volume.detach() {
            activation.gone.store(false, Ordering::SeqCst);
            return pass;
        }
        let taken = {
            let mut table = self.table.lock();
            let taken = table.at(number).map(|at| table.resident.remove(at));
            self.note_retired(&table);
            taken
        };
        drop(taken);
        klog_info!("EXT: activation {} freed", number);
        pass.freed = 1;
        pass
    }
}

fn held(number: u32) -> bool {
    let mine = |inode| generation_of(inode) == number;
    crate::vfs::orphan::any_open(&ACTIVATIONS, mine)
        || crate::filemap::any_mapped(&ACTIVATIONS, mine)
        || reap::runs_from(number, true)
}

pub fn is_activation(fs: &dyn FileSystem) -> bool {
    same_filesystem(fs, &ACTIVATIONS)
}

/// Activations resident: shown, and held after being taken away.
pub fn resident() -> usize {
    ACTIVATIONS.table.lock().resident.len()
}

/// The budget `ext_activate` holds the resident activations to: an eighth of
/// usable memory.
pub fn budget() -> u64 {
    u64::from(slopos_mm::page_alloc::get_page_allocator_stats().total) * 4096 / 8
}

/// What the resident activations hold against their budget, in bytes.
pub fn held_bytes() -> u64 {
    ACTIVATIONS.table.lock().held()
}

fn mount_path<'a>(name: &[u8], buf: &'a mut [u8; 5 + NAME_MAX]) -> Option<&'a [u8]> {
    let dir = slopos_abi::extension::DIR.as_bytes();
    let len = dir.len() + 1 + name.len();
    let out = buf.get_mut(..len)?;
    out[..dir.len()].copy_from_slice(dir);
    out[dir.len()] = b'/';
    out[dir.len() + 1..].copy_from_slice(name);
    Some(out)
}

#[inline(never)]
/// Whether the volume `device` holds is one an activation serves: the
/// extension writer's read-only ext4, filling the image's data exactly, so
/// a shorter tree over the same blocks is no other image.
fn holds_volume(device: &VerityFile) -> Result<(), ActivateError> {
    let mut block = KBox::<[u8; BLOCK]>::zeroed().map_err(|_| ActivateError::NoMemory)?;
    device.read_at(0, &mut block[..]).map_err(|e| match e {
        BlockDeviceError::Interrupted => ActivateError::Interrupted,
        _ => ActivateError::Image(OpenError::Read(VfsError::IoError)),
    })?;
    let sb = &block[1024..2048];
    let refused = if slopos_ext4_core::bytes::le16(sb, soff::MAGIC) != sbk::MAGIC {
        Some("no ext4 superblock")
    } else if slopos_ext4_core::bytes::le32(sb, soff::LOG_BLOCK_SIZE) != 2 {
        Some("blocks other than 4 KiB")
    } else if sbk::blocks_count(sb) != device.data_blocks() {
        Some("a volume that does not fill the image's data")
    } else if slopos_ext4_core::bytes::le32(sb, soff::FEATURE_RO_COMPAT) & ro_compat::READONLY == 0
    {
        Some("a volume not marked read-only")
    } else if slopos_ext4_core::bytes::le32(sb, soff::FEATURE_RO_COMPAT) & ro_compat::METADATA_CSUM
        == 0
    {
        Some("a volume without metadata checksums")
    } else if slopos_ext4_core::bytes::le32(sb, soff::FIRST_INO) != index::FIRST_INO {
        Some("a volume whose first inode is not ext4's")
    } else if slopos_ext4_core::bytes::le32(sb, soff::FEATURE_INCOMPAT) & incompat::RECOVER != 0 {
        Some("a volume whose journal needs recovery")
    } else {
        None
    };
    refused.map_or(Ok(()), |why| Err(ActivateError::Volume(why)))
}

/// Activate the image in `inode` of `fs`, whose root the caller vouches is
/// `digest`, as `name`, and show it at `/opt/<name>` in place of the
/// activation shown there, when the resident activations hold no more than
/// `budget` bytes with it; answers its number.
pub fn activate(
    name: &[u8],
    digest: [u8; DIGEST_LEN],
    fs: &'static dyn FileSystem,
    inode: InodeId,
    budget: u64,
) -> Result<u32, ActivateError> {
    let label = Label::new(name, digest).ok_or(ActivateError::Name)?;
    if is_activation(fs) {
        return Err(ActivateError::Image(OpenError::NotImage));
    }
    let _one_at_a_time = ACTIVATING.lock().map_err(|_| ActivateError::Interrupted)?;
    if CLASS_REGISTERED.init_once() {
        register_class_eagerly(CLASS);
    }
    let number = ISSUED
        .load(Ordering::Relaxed)
        .checked_add(1)
        .filter(|&n| n <= LAST_NUMBER)
        .ok_or(ActivateError::Busy)?;
    ACTIVATIONS.admit(build(label, number, fs, inode)?, budget)?;
    ISSUED.store(number, Ordering::Relaxed);
    show(number, label.name())?;
    reap::wake();
    Ok(number)
}

/// The image read into a volume of its own and indexed, as activation
/// `number`.
fn build(
    label: Label,
    number: u32,
    fs: &'static dyn FileSystem,
    inode: InodeId,
) -> Result<KArc<Activation>, ActivateError> {
    let volume = volume()?;
    let (bytes, tree) = attach(&volume, label.digest, fs, inode)?;
    let index = Index::build(&*volume, ROOT, bytes).map_err(ActivateError::Index)?;
    let state = volume
        .held_bytes()
        .map_err(|_| ActivateError::Interrupted)?;
    let fixed = (size_of::<Activation>() + size_of::<Ext4Mount>()) as u64;
    let held = fixed + state + tree + index.bytes();
    KArc::try_new(Activation {
        number,
        label,
        volume,
        index,
        held,
        users: AtomicUsize::new(0),
        gone: AtomicBool::new(false),
    })
    .map_err(|_| ActivateError::NoMemory)
}

#[inline(never)]
fn volume() -> Result<KBox<Ext4Mount>, ActivateError> {
    let volume = init_from_owned::<_, AllocError>(Ext4Mount::new_for_checked_image(CLASS));
    KBox::try_init(volume).map_err(|_| ActivateError::NoMemory)
}

/// Attach the volume of the image in `inode` of `fs`, under `root`, to
/// `volume`; answers how many bytes it is and the tree blocks its device
/// holds.
#[inline(never)]
fn attach(
    volume: &Ext4Mount,
    root: [u8; DIGEST_LEN],
    fs: &'static dyn FileSystem,
    inode: InodeId,
) -> Result<(u64, u64), ActivateError> {
    let device = VerityFile::open(fs, inode, root).map_err(ActivateError::Image)?;
    holds_volume(&device)?;
    let bytes = device.capacity();
    let tree = device.held_bytes();
    if !device.reads_direct() {
        klog_info!("EXT: image read through its filesystem, not off its disk");
    }
    let device: KBox<dyn BlockDevice + Send + Sync> = device;
    let info = volume.attach(device, true).map_err(ActivateError::Attach)?;
    if !info.read_only {
        return Err(ActivateError::Attach(VfsError::ReadOnly));
    }
    Ok((bytes, tree))
}

/// Mount activation `number` read-only and pinned at `/opt/<name>`, in place
/// of an activation shown there, which is retired; withdrawn when it cannot
/// be. `/opt` is held where it is, so the walk to the mount passes through
/// no link.
#[inline(never)]
fn show(number: u32, name: &[u8]) -> Result<(), ActivateError> {
    let mut path = [0; 5 + NAME_MAX];
    let shown = mount_path(name, &mut path)
        .ok_or(VfsError::InvalidPath)
        .and_then(|path| {
            crate::vfs::init::hold_dir(slopos_abi::extension::DIR.as_bytes())?;
            mount_replacing(
                path,
                &ACTIVATIONS,
                inode_of(number, ROOT),
                MOUNT_RDONLY | MOUNT_PINNED,
                is_activation,
            )
        });
    let replaced = match shown {
        Ok(replaced) => replaced.map(|mounted| generation_of(mounted.root)),
        Err(e) => {
            ACTIVATIONS.withdraw(number);
            return Err(ActivateError::Mounts(e));
        }
    };
    {
        let mut table = ACTIVATIONS.table.lock();
        for slot in table.resident.iter_mut() {
            if slot.activation.number == number {
                slot.phase = Phase::Shown;
            } else if Some(slot.activation.number) == replaced {
                slot.phase = Phase::Retired { quiet: false };
            }
        }
        ACTIVATIONS.note_retired(&table);
    }
    klog_info!(
        "EXT: activation {} is {}",
        number,
        core::str::from_utf8(name).unwrap_or("?")
    );
    Ok(())
}

/// Take the activation shown at `/opt/<name>` away; what holds it keeps it.
pub fn deactivate(name: &[u8]) -> Result<(), ActivateError> {
    if !slopos_abi::extension::valid_name(name) {
        return Err(ActivateError::Name);
    }
    let mut path = [0; 5 + NAME_MAX];
    let path = mount_path(name, &mut path).ok_or(ActivateError::Name)?;
    let _one_at_a_time = ACTIVATING.lock().map_err(|_| ActivateError::Interrupted)?;
    let taken = unmount_where(path, is_activation).map_err(ActivateError::Mounts)?;
    klog_info!(
        "EXT: {} deactivated",
        core::str::from_utf8(name).unwrap_or("?")
    );
    ACTIVATIONS.set_phase(generation_of(taken.root), Phase::Retired { quiet: false });
    reap::wake();
    Ok(())
}

/// The label of the activation `program` is of, while it is resident.
pub fn label_of(program: Program) -> Option<Label> {
    let Program::Extension(inode) = program else {
        return None;
    };
    Some(ACTIVATIONS.find(generation_of(inode.get()))?.label)
}

/// Write the absolute path of activation `number`'s `inode`,
/// `/opt/<name>/<path>`, into `buf` when it fits, and answer its length
/// either way; `NotFound` once the activation is gone.
pub fn path_of(number: u32, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
    let activation = ACTIVATIONS.find(number).ok_or(VfsError::NotFound)?;
    let mut at = [0; 5 + NAME_MAX];
    let at = mount_path(activation.label.name(), &mut at).ok_or(VfsError::NotFound)?;
    let rest = activation
        .index
        .path_of(inode as u32, buf.get_mut(at.len()..).unwrap_or(&mut []))?;
    if rest + at.len() <= buf.len() {
        buf[..at.len()].copy_from_slice(at);
    }
    Ok(at.len() + rest)
}

/// Run `f` over every volume an activation reads, for the reclaimer: none
/// while the table is held elsewhere, since a reclaim pass may not wait.
pub(crate) fn for_each_volume(f: &mut dyn FnMut(&Ext4Mount)) {
    let Some(table) = ACTIVATIONS.table.try_lock() else {
        return;
    };
    for slot in table.resident.iter() {
        if slot.activation.volume.is_initialized() {
            f(&slot.activation.volume);
        }
    }
}

pub(crate) fn any_retired() -> bool {
    ACTIVATIONS.retired.load(Ordering::Acquire)
}

/// One pass over the retired activations.
pub(crate) fn pass() -> reap::Pass {
    let Ok(_one_at_a_time) = reap::PASSING.lock() else {
        return reap::Pass::default();
    };
    let mut pass = reap::Pass::default();
    let mut after = 0;
    while let Some((activation, quiet)) = ACTIVATIONS.retired_after(after) {
        after = activation.number;
        pass = pass.and(ACTIVATIONS.reap_step(&activation, quiet));
    }
    pass
}

impl FileSystem for Activations {
    fn name(&self) -> &'static str {
        "extension"
    }

    /// No activation's: each is mounted at its own root.
    fn root_inode(&self) -> InodeId {
        inode_of(0, ROOT)
    }

    /// Answered from the index, without the volume's lock, but for what it
    /// leaves to the volume.
    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        let entered = self.enter(parent)?;
        let activation = &entered.activation;
        if let Some(found) = activation.index.lookup(inner(parent) as u32, name) {
            return found
                .map(|ino| entered.outer(InodeId::from(ino)))
                .ok_or(VfsError::NotFound);
        }
        activation
            .volume
            .lookup(inner(parent), name)
            .map(|found| entered.outer(found))
    }

    /// Answered from the index, without the volume's lock, but for an inode
    /// it does not hold.
    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        let entered = self.enter(inode)?;
        let activation = &entered.activation;
        let mut stat = match activation.index.stat(inner(inode) as u32) {
            Some(stat) => stat,
            None => activation.volume.stat(inner(inode))?,
        };
        stat.inode = inode;
        stat.uid = 0;
        stat.gid = 0;
        stat.sealed = true;
        Ok(stat)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let entered = self.enter(inode)?;
        entered.activation.volume.read(inner(inode), offset, buf)
    }

    fn read_pages(&self, inode: InodeId, offset: u64, pages: &mut [&mut [u8]]) -> VfsResult<usize> {
        let entered = self.enter(inode)?;
        entered
            .activation
            .volume
            .read_pages_unkept(inner(inode), offset, pages)
    }

    fn write(&self, _inode: InodeId, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::ReadOnly)
    }

    fn create_owned(
        &self,
        _parent: InodeId,
        _name: &[u8],
        _file_type: FileType,
        _owner: Owner,
        _mode: u16,
    ) -> VfsResult<InodeId> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, _parent: InodeId, _name: &[u8]) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let entered = self.enter(inode)?;
        entered
            .activation
            .volume
            .readdir(inner(inode), offset, &mut |name, found, kind| {
                callback(name, entered.outer(found), kind)
            })
    }

    fn readdir_cookie(
        &self,
        inode: InodeId,
        cookie: u64,
        callback: &mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<u64> {
        let entered = self.enter(inode)?;
        entered.activation.volume.readdir_cookie(
            inner(inode),
            cookie,
            &mut |next, name, found, kind| callback(next, name, entered.outer(found), kind),
        )
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        let entered = self.enter(inode)?;
        entered.activation.volume.readlink(inner(inode), buf)
    }

    fn set_sealed(&self, _inode: InodeId) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self, inode: InodeId) -> VfsResult<FsStats> {
        let entered = self.enter(inode)?;
        let mut stats = entered.activation.volume.statfs(inner(inode))?;
        stats.magic = EXTENSION_MAGIC;
        stats.read_only = true;
        Ok(stats)
    }
}
