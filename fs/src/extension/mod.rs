//! Extensions: an image activated beside the base, shown read-only and pinned
//! at `/opt/<name>`. An image is a file of another filesystem, an ext4
//! volume and its SHA-256 tree, and an activation reads it in place through
//! a device that checks each block against the tree ([`device`]), its volume
//! served by an ext4 instance of its own from [`POOL`].
//!
//! Each activation is numbered across every activation and never again
//! within a boot, and the number is the high half of every inode it serves,
//! so an inode names its activation for good. Replacing one moves the mount
//! to the next in one step; the one replaced is freed once nothing holds it,
//! as a retired base generation is, and its instance serves another.

mod device;
mod index;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use slopos_abi::extension::{DIGEST_LEN, Label, MAX_ACTIVATIONS, NAME_MAX};
use slopos_abi::fs::EXTENSION_MAGIC;
use slopos_ext4_core::superblock::{self as sbk, incompat, off as soff, ro_compat};
use slopos_ostd::process::Program;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::{InitFlag, LockClassKey, Mutex, SpinLock, register_class_eagerly};
use slopos_ostd::{KArc, KBox, klog_info, lock_class};
use slopos_verity_core::BLOCK;

pub use self::device::OpenError;
use self::device::VerityFile;
use self::index::Index;
pub use self::index::IndexError;
use crate::basefs::{generation_of, reap};
use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::ext2_vfs::Ext2Mount;
use crate::vfs::mount::{MOUNT_PINNED, MOUNT_RDONLY, mount_replacing, unmount_where};
use crate::vfs::traits::{FsStats, Owner, same_filesystem};
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

/// Every volume's root, as ext4 numbers it.
const ROOT: u32 = 2;

/// The last number an activation may take: a program identity keeps the
/// top bit of an inode for itself.
const LAST_NUMBER: u32 = i32::MAX as u32;

/// Every instance's volume shares one class, which nothing takes twice:
/// none is mounted beneath another, and the file a volume is read from is
/// never an activation's.
const CLASS: &LockClassKey = lock_class!("EXT_ACTIVATION", LOCK_LEVEL_RESOURCE);

pub static POOL: [Activation; MAX_ACTIVATIONS] = [const { Activation::new() }; MAX_ACTIVATIONS];

/// The last activation number issued.
static ISSUED: AtomicU32 = AtomicU32::new(0);

static ACTIVATING: Mutex<()> = Mutex::new((), lock_class!("EXT_ACTIVATING", LOCK_LEVEL_RESOURCE));

static CLASS_REGISTERED: InitFlag = InitFlag::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivateError {
    /// A name the grammar refuses.
    Name,
    /// Every instance is held, or the numbers are spent.
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
    Vacant,
    /// Being built: not yet shown, no reap pass may take it.
    Admitted,
    Shown,
    /// Shown no more; `quiet` once a reap pass found nothing holding it.
    Retired {
        quiet: bool,
    },
    /// Being freed by a reap pass.
    Freeing,
}

struct State {
    phase: Phase,
    label: Option<Label>,
    index: Option<KArc<Index>>,
}

/// One activation: its volume, and the number its inodes carry.
pub struct Activation {
    volume: Ext2Mount,
    /// The activation this instance serves, 0 while it serves none.
    number: AtomicU32,
    /// Calls into the volume in flight, which a reap pass waits out.
    users: AtomicUsize,
    retired: AtomicBool,
    state: SpinLock<State>,
}

/// A call into an activation's volume, counted while it runs.
struct Entered<'a> {
    activation: &'a Activation,
    number: u32,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        self.activation.users.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Entered<'_> {
    fn outer(&self, inner: InodeId) -> InodeId {
        inode_of(self.number, inner as u32)
    }
}

const fn inode_of(number: u32, ino: u32) -> InodeId {
    ((number as u64) << 32) | ino as u64
}

fn inner(inode: InodeId) -> InodeId {
    InodeId::from(inode as u32)
}

impl Activation {
    const fn new() -> Self {
        Self {
            volume: Ext2Mount::new_for_checked_image(CLASS),
            number: AtomicU32::new(0),
            users: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
            state: SpinLock::new(
                State {
                    phase: Phase::Vacant,
                    label: None,
                    index: None,
                },
                lock_class!("EXT_ACTIVATION_STATE", LOCK_LEVEL_RESOURCE),
            ),
        }
    }

    /// A call into activation `generation_of(inode)`, while this instance
    /// serves it: a reap pass frees an instance only with none in flight.
    fn enter(&self, inode: InodeId) -> VfsResult<Entered<'_>> {
        self.users.fetch_add(1, Ordering::SeqCst);
        let entered = Entered {
            activation: self,
            number: self.number.load(Ordering::SeqCst),
        };
        if entered.number == 0 || generation_of(inode) != entered.number {
            return Err(VfsError::NotFound);
        }
        Ok(entered)
    }

    fn claim(&self) -> bool {
        let mut state = self.state.lock();
        if state.phase != Phase::Vacant {
            return false;
        }
        state.phase = Phase::Admitted;
        true
    }

    /// Give the instance back once its volume is detached; `false` when it
    /// could not be, which leaves it to the caller.
    fn release(&self) -> bool {
        self.number.store(0, Ordering::SeqCst);
        if !self.volume.detach() {
            return false;
        }
        self.retired.store(false, Ordering::Release);
        let index = {
            let mut state = self.state.lock();
            state.phase = Phase::Vacant;
            state.label = None;
            state.index.take()
        };
        drop(index);
        true
    }

    /// Give back an instance never shown, or leave it to the reaper when its
    /// volume will not detach yet.
    fn abandon(&self) {
        if !self.release() {
            self.set_phase(Phase::Retired { quiet: true });
            reap::wake();
        }
    }

    fn set_phase(&self, phase: Phase) {
        self.state.lock().phase = phase;
        self.retired
            .store(matches!(phase, Phase::Retired { .. }), Ordering::Release);
    }

    fn label(&self, number: u32) -> Option<Label> {
        let state = self.state.lock();
        (self.number.load(Ordering::Acquire) == number && state.phase != Phase::Vacant)
            .then_some(state.label)
            .flatten()
    }
}

pub fn is_activation(fs: &dyn FileSystem) -> bool {
    instance(fs).is_some()
}

fn instance(fs: &dyn FileSystem) -> Option<&'static Activation> {
    POOL.iter()
        .find(|mine| same_filesystem(fs, *mine as &dyn FileSystem))
}

/// The instance serving activation `number`, while it is resident.
fn serving(number: u32) -> Option<&'static Activation> {
    (number != 0)
        .then(|| {
            POOL.iter()
                .find(|a| a.number.load(Ordering::Acquire) == number)
        })
        .flatten()
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
/// activation shown there; answers its number.
pub fn activate(
    name: &[u8],
    digest: [u8; DIGEST_LEN],
    fs: &'static dyn FileSystem,
    inode: InodeId,
) -> Result<u32, ActivateError> {
    let label = Label::new(name, digest).ok_or(ActivateError::Name)?;
    if is_activation(fs) {
        return Err(ActivateError::Image(OpenError::NotImage));
    }
    let _one_at_a_time = ACTIVATING.lock().map_err(|_| ActivateError::Interrupted)?;
    if CLASS_REGISTERED.init_once() {
        register_class_eagerly(CLASS);
    }
    let activation = POOL.iter().find(|a| a.claim()).ok_or(ActivateError::Busy)?;
    let number = match admit(activation, label, fs, inode) {
        Ok(number) => number,
        Err(e) => {
            activation.abandon();
            return Err(e);
        }
    };
    show(activation, number, label.name())?;
    reap::wake();
    Ok(number)
}

/// Read the image into `activation`'s volume and index it, under the next
/// number.
fn admit(
    activation: &'static Activation,
    label: Label,
    fs: &'static dyn FileSystem,
    inode: InodeId,
) -> Result<u32, ActivateError> {
    let bytes = attach(activation, label.digest, fs, inode)?;
    let number = ISSUED
        .load(Ordering::Relaxed)
        .checked_add(1)
        .filter(|&n| n <= LAST_NUMBER)
        .ok_or(ActivateError::Busy)?;
    ISSUED.store(number, Ordering::Relaxed);
    let index = index(activation, bytes)?;
    activation.number.store(number, Ordering::SeqCst);
    let mut state = activation.state.lock();
    state.label = Some(label);
    state.index = Some(index);
    Ok(number)
}

/// Attach the volume of the image in `inode` of `fs`, under `root`, to
/// `activation`; answers how many bytes it is.
#[inline(never)]
fn attach(
    activation: &'static Activation,
    root: [u8; DIGEST_LEN],
    fs: &'static dyn FileSystem,
    inode: InodeId,
) -> Result<u64, ActivateError> {
    let device = VerityFile::open(fs, inode, root).map_err(ActivateError::Image)?;
    holds_volume(&device)?;
    let bytes = device.capacity();
    let device: KBox<dyn BlockDevice + Send + Sync> = device;
    let info = activation
        .volume
        .attach(device, true)
        .map_err(ActivateError::Attach)?;
    if !info.read_only {
        return Err(ActivateError::Attach(VfsError::ReadOnly));
    }
    Ok(bytes)
}

#[inline(never)]
fn index(activation: &'static Activation, bytes: u64) -> Result<KArc<Index>, ActivateError> {
    let index = Index::build(&activation.volume, ROOT, bytes).map_err(ActivateError::Index)?;
    KArc::try_new(index).map_err(|_| ActivateError::NoMemory)
}

/// Mount activation `number` read-only and pinned at `/opt/<name>`, in place
/// of an activation shown there, which is retired; given back when it
/// cannot be. `/opt` is held where it is, so the walk to the mount passes
/// through no link.
#[inline(never)]
fn show(activation: &'static Activation, number: u32, name: &[u8]) -> Result<(), ActivateError> {
    let mut path = [0; 5 + NAME_MAX];
    let shown = mount_path(name, &mut path)
        .ok_or(VfsError::InvalidPath)
        .and_then(|path| {
            crate::vfs::init::hold_dir(slopos_abi::extension::DIR.as_bytes())?;
            let root = inode_of(number, ROOT);
            mount_replacing(
                path,
                activation,
                root,
                MOUNT_RDONLY | MOUNT_PINNED,
                is_activation,
            )
        });
    let replaced = match shown {
        Ok(replaced) => replaced.and_then(instance),
        Err(e) => {
            activation.abandon();
            return Err(ActivateError::Mounts(e));
        }
    };
    activation.set_phase(Phase::Shown);
    if let Some(replaced) = replaced {
        replaced.set_phase(Phase::Retired { quiet: false });
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
    if let Some(activation) = instance(taken) {
        klog_info!(
            "EXT: {} deactivated",
            core::str::from_utf8(name).unwrap_or("?")
        );
        activation.set_phase(Phase::Retired { quiet: false });
        reap::wake();
    }
    Ok(())
}

/// The label of the activation `program` is of, while it is resident.
pub fn label_of(program: Program) -> Option<Label> {
    let Program::Extension(inode) = program else {
        return None;
    };
    let number = generation_of(inode.get());
    serving(number)?.label(number)
}

/// Write the absolute path of activation `number`'s `inode`,
/// `/opt/<name>/<path>`, into `buf` when it fits, and answer its length
/// either way; `NotFound` once the activation is gone.
pub fn path_of(number: u32, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
    let activation = serving(number).ok_or(VfsError::NotFound)?;
    let (label, index) = {
        let state = activation.state.lock();
        if activation.number.load(Ordering::Acquire) != number {
            return Err(VfsError::NotFound);
        }
        (state.label, state.index.clone())
    };
    let (label, index) = label.zip(index).ok_or(VfsError::NotFound)?;
    let mut at = [0; 5 + NAME_MAX];
    let at = mount_path(label.name(), &mut at).ok_or(VfsError::NotFound)?;
    let rest = index.path_of(inode as u32, buf.get_mut(at.len()..).unwrap_or(&mut []))?;
    if rest + at.len() <= buf.len() {
        buf[..at.len()].copy_from_slice(at);
    }
    Ok(at.len() + rest)
}

/// Run `f` over every volume an activation reads, for the reclaimer.
pub(crate) fn for_each_volume(f: &mut dyn FnMut(&'static Ext2Mount)) {
    for activation in POOL.iter() {
        if activation.volume.is_initialized() {
            f(&activation.volume);
        }
    }
}

pub(crate) fn any_retired() -> bool {
    POOL.iter().any(|a| a.retired.load(Ordering::Acquire))
}

impl Activation {
    fn held(&'static self, number: u32) -> bool {
        let mine = |inode| generation_of(inode) == number;
        crate::vfs::orphan::any_open(self, mine)
            || crate::filemap::any_mapped(self, mine)
            || reap::runs_from(number, true)
    }

    /// One reap step: mark a retired activation held or quiet, and free one
    /// found quiet twice with no call in flight.
    fn reap_step(&'static self) -> reap::Pass {
        let mut pass = reap::Pass::default();
        let number = self.number.load(Ordering::Acquire);
        let quiet = match self.state.lock().phase {
            Phase::Retired { quiet } => quiet,
            _ => return pass,
        };
        if self.held(number) {
            self.set_phase(Phase::Retired { quiet: false });
            return pass;
        }
        if !quiet {
            self.set_phase(Phase::Retired { quiet: true });
            pass.marked = true;
            return pass;
        }
        self.set_phase(Phase::Freeing);
        self.number.store(0, Ordering::SeqCst);
        if self.users.load(Ordering::SeqCst) != 0 {
            self.number.store(number, Ordering::SeqCst);
            self.set_phase(Phase::Retired { quiet: true });
            pass.marked = true;
            return pass;
        }
        crate::filemap::forget_where(self, |inode| generation_of(inode) == number);
        if !self.release() {
            self.number.store(number, Ordering::SeqCst);
            self.set_phase(Phase::Retired { quiet: true });
            return pass;
        }
        klog_info!("EXT: activation {} freed", number);
        pass.freed = 1;
        pass
    }
}

/// One pass over the retired activations.
pub(crate) fn pass() -> reap::Pass {
    let Ok(_one_at_a_time) = reap::PASSING.lock() else {
        return reap::Pass::default();
    };
    POOL.iter()
        .fold(reap::Pass::default(), |pass, a| pass.and(a.reap_step()))
}

impl FileSystem for Activation {
    fn name(&self) -> &'static str {
        "extension"
    }

    fn root_inode(&self) -> InodeId {
        inode_of(self.number.load(Ordering::Acquire), ROOT)
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        let entered = self.enter(parent)?;
        self.volume
            .lookup(inner(parent), name)
            .map(|found| entered.outer(found))
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        let _entered = self.enter(inode)?;
        let mut stat = self.volume.stat(inner(inode))?;
        stat.inode = inode;
        stat.uid = 0;
        stat.gid = 0;
        stat.sealed = true;
        Ok(stat)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let _entered = self.enter(inode)?;
        self.volume.read(inner(inode), offset, buf)
    }

    fn read_pages(&self, inode: InodeId, offset: u64, pages: &mut [&mut [u8]]) -> VfsResult<usize> {
        let _entered = self.enter(inode)?;
        self.volume.read_pages(inner(inode), offset, pages)
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
        self.volume
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
        self.volume
            .readdir_cookie(inner(inode), cookie, &mut |next, name, found, kind| {
                callback(next, name, entered.outer(found), kind)
            })
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        let _entered = self.enter(inode)?;
        self.volume.readlink(inner(inode), buf)
    }

    fn set_sealed(&self, _inode: InodeId) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self) -> VfsResult<FsStats> {
        let _entered = self.enter(self.root_inode())?;
        let mut stats = self.volume.statfs()?;
        stats.magic = EXTENSION_MAGIC;
        stats.read_only = true;
        Ok(stats)
    }
}
