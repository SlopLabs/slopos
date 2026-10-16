//! Extensions: an archive activated beside the base, shown read-only and
//! pinned at `/opt/<name>`. Each activation is an instance of its own from
//! [`POOL`], serving one generation numbered across every activation, so an
//! inode's high half names the activation for good. Replacing one moves the
//! mount to the next in one step; the one replaced is freed, as a retired
//! base generation is, once nothing holds it.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::extension::{DIGEST_LEN, Label, MAX_ACTIVATIONS, NAME_MAX};
use slopos_ostd::process::Program;
use slopos_ostd::sync::Mutex;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::{klog_info, lock_class};

use super::index::ROOT;
use super::{ActivateError, BaseFs, Source, generation_of, inode_of, reap, stage};
use crate::vfs::mount::{MOUNT_PINNED, MOUNT_RDONLY, mount_replacing, unmount_where};
use crate::vfs::traits::same_filesystem;
use crate::vfs::{FileSystem, InodeId, VfsError, VfsResult};

pub static POOL: [BaseFs; MAX_ACTIVATIONS] = [const { BaseFs::new() }; MAX_ACTIVATIONS];

/// The last activation number issued; numbers are never reused within a boot.
static ISSUED: AtomicU32 = AtomicU32::new(0);

/// Usable memory's share every activation's copy may take together, in
/// percent; 0 until the boot sets it, which reads as an eighth.
static MEMORY_PERCENT: AtomicU32 = AtomicU32::new(0);

static ACTIVATING: Mutex<()> = Mutex::new((), lock_class!("EXT_ACTIVATING", LOCK_LEVEL_RESOURCE));

/// `ext.memory=<percent>` of usable memory for every activation together.
pub fn set_memory_percent(percent: u32) {
    MEMORY_PERCENT.store(percent, Ordering::Relaxed);
}

/// What every activation's copy may take together out of `usable` bytes.
pub fn budget(usable: u64) -> u64 {
    match MEMORY_PERCENT.load(Ordering::Relaxed) {
        0 => usable / 8,
        percent => usable / 100 * u64::from(percent),
    }
}

pub fn is_activation(fs: &dyn FileSystem) -> bool {
    instance(fs).is_some()
}

fn instance(fs: &dyn FileSystem) -> Option<&'static BaseFs> {
    POOL.iter()
        .find(|mine| same_filesystem(fs, *mine as &dyn FileSystem))
}

/// The instance serving activation `number`, while it is resident.
fn serving(number: u32) -> Option<&'static BaseFs> {
    POOL.iter().find(|fs| fs.generation(number).is_ok())
}

impl BaseFs {
    pub(crate) fn vacant(&self) -> bool {
        self.table.lock().slots.iter().all(Option::is_none)
    }

    pub(crate) fn resident(&self) -> u64 {
        let table = self.table.lock();
        table
            .slots
            .iter()
            .flatten()
            .map(|slot| slot.generation.store.len())
            .sum()
    }

    fn label(&self, number: u32) -> Option<Label> {
        self.generation(number).ok()?.label
    }
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

/// Build an activation named `name` from the archive `source` reads, at most
/// `budget` bytes of it, and show it at `/opt/<name>` in place of the one
/// shown there, the copies resident with it taking at most `budget`
/// together; answers its number.
pub fn activate(
    name: &[u8],
    digest: [u8; DIGEST_LEN],
    source: &mut Source<'_>,
    budget: u64,
) -> Result<u32, ActivateError> {
    let label = Label::new(name, digest).ok_or(ActivateError::Name)?;
    let _one_at_a_time = ACTIVATING.lock().map_err(|_| ActivateError::Interrupted)?;
    if !POOL.iter().any(BaseFs::vacant) {
        return Err(ActivateError::Busy);
    }
    let staged = stage(source, budget).map_err(ActivateError::Stage)?;
    let (fs, number) = admit(staged, budget, label)?;
    show(fs, number, label.name())?;
    reap::wake();
    Ok(number)
}

/// Give `staged` a vacant instance and the next activation number.
#[inline(never)]
fn admit(
    staged: super::Staged,
    budget: u64,
    label: Label,
) -> Result<(&'static BaseFs, u32), ActivateError> {
    let fs = POOL
        .iter()
        .find(|fs| fs.vacant())
        .ok_or(ActivateError::Busy)?;
    let elsewhere = POOL.iter().map(BaseFs::resident).sum();
    let number = ISSUED
        .load(Ordering::Relaxed)
        .checked_add(1)
        .ok_or(ActivateError::Busy)?;
    ISSUED.store(number, Ordering::Relaxed);
    let number = fs.admit_as(staged, budget, elsewhere, Some(number), Some(label))?;
    Ok((fs, number))
}

/// Mount activation `number` of `fs` read-only and pinned at `/opt/<name>`,
/// in place of an activation shown there, which is retired; withdrawn when
/// it cannot be. `/opt` is held where it is, so the walk to the mount passes
/// through no link.
#[inline(never)]
fn show(fs: &'static BaseFs, number: u32, name: &[u8]) -> Result<(), ActivateError> {
    let mut path = [0; 5 + NAME_MAX];
    let shown = mount_path(name, &mut path)
        .ok_or(VfsError::InvalidPath)
        .and_then(|path| {
            crate::vfs::init::hold_dir(slopos_abi::extension::DIR.as_bytes())?;
            let root = inode_of(number, ROOT);
            mount_replacing(path, fs, root, MOUNT_RDONLY | MOUNT_PINNED, is_activation)
        });
    let replaced = match shown {
        Ok(replaced) => replaced.and_then(instance),
        Err(e) => {
            fs.withdraw(number);
            return Err(ActivateError::Mounts(e));
        }
    };
    fs.make_current(number);
    if let Some(replaced) = replaced {
        replaced.retire();
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
    if let Some(fs) = instance(taken) {
        klog_info!(
            "EXT: {} deactivated",
            core::str::from_utf8(name).unwrap_or("?")
        );
        fs.retire();
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
pub(super) fn path_of(number: u32, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
    let fs = serving(number).ok_or(VfsError::NotFound)?;
    let label = fs.label(number).ok_or(VfsError::NotFound)?;
    let mut at = [0; 5 + NAME_MAX];
    let at = mount_path(label.name(), &mut at).ok_or(VfsError::NotFound)?;
    let rest = fs.path_of(inode, buf.get_mut(at.len()..).unwrap_or(&mut []))?;
    if rest + at.len() <= buf.len() {
        buf[..at.len()].copy_from_slice(at);
    }
    Ok(at.len() + rest)
}
