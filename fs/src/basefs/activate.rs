//! Making a new generation the base the base directories show.
//!
//! Everything that can fail runs first — reading and indexing the archive,
//! checking its interface, admitting it and finding each mounted base
//! directory in it — and then one pass under the mount table's lock moves
//! every base mount to it, or none.

use slopos_abi::fs::BASE_DIRS;
use slopos_abi::{BASE_INTERFACE, BASE_INTERFACE_MIN};
use slopos_ostd::sync::Mutex;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::{KVec, klog_info, lock_class};

use super::{BASE_FS, BaseFs, Source, StageError, is_base, monitor, reap, stage};
use crate::vfs::mount::{mount_at, switch_roots};
use crate::vfs::traits::same_filesystem;
use crate::vfs::{InodeId, VfsError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivateError {
    Stage(StageError),
    /// The archive names an interface this kernel does not serve, or none.
    Unserved(Option<u32>),
    /// A directory a mount shows that the archive lacks.
    Missing,
    /// Every generation slot is held, or the copies resident beside this
    /// one would take more than the budget.
    Busy,
    NoMemory,
    Mounts(VfsError),
    Interrupted,
}

static ACTIVATING: Mutex<()> =
    Mutex::new((), lock_class!("BASEFS_ACTIVATING", LOCK_LEVEL_RESOURCE));

/// Build a generation from the archive `source` reads, at most `budget`
/// bytes of it, and make it the base, the copies resident with it taking at
/// most `budget` together; answers its number.
pub fn activate(source: &mut Source<'_>, budget: u64) -> Result<u32, ActivateError> {
    let mut mounts = [(&b""[..], &b""[..]); BASE_DIRS.len()];
    for (mount, dir) in mounts.iter_mut().zip(BASE_DIRS) {
        *mount = (dir.as_bytes(), dir.as_bytes());
    }
    activate_on(&BASE_FS, &mounts, source, budget)
}

/// [`activate`] for `fs`, whose mounts are each `(path, directory)`: the
/// mount at `path` shows `directory` of the generation.
pub(crate) fn activate_on(
    fs: &'static BaseFs,
    mounts: &[(&[u8], &[u8])],
    source: &mut Source<'_>,
    budget: u64,
) -> Result<u32, ActivateError> {
    let _one_at_a_time = ACTIVATING.lock().map_err(|_| ActivateError::Interrupted)?;
    let staged = stage(source, budget).map_err(ActivateError::Stage)?;
    let interface = staged
        .interface()
        .filter(|n| (BASE_INTERFACE_MIN..=BASE_INTERFACE).contains(n))
        .ok_or(ActivateError::Unserved(staged.interface()))?;
    let number = fs.admit(staged, budget)?;
    if let Err(e) = switch(fs, number, mounts) {
        fs.withdraw(number);
        return Err(e);
    }
    fs.make_current(number);
    if is_base(fs) {
        klog_info!(
            "BASE: generation {} is the base, interface {}",
            number,
            interface
        );
        monitor::changed();
        reap::wake();
    }
    Ok(number)
}

fn switch(
    fs: &'static BaseFs,
    number: u32,
    mounts: &[(&[u8], &[u8])],
) -> Result<(), ActivateError> {
    let mut roots: KVec<(&[u8], InodeId)> =
        KVec::with_capacity(mounts.len()).map_err(|_| ActivateError::NoMemory)?;
    for &(path, dir) in mounts {
        if !mount_at(path).is_some_and(|m| same_filesystem(m.fs, fs)) {
            continue;
        }
        let root = fs.directory(number, dir).ok_or(ActivateError::Missing)?;
        roots
            .push((path, root))
            .map_err(|_| ActivateError::NoMemory)?;
    }
    switch_roots(fs, &roots).map_err(ActivateError::Mounts)
}
