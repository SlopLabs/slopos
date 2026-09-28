//! A process's filesystem context: its working directory.
//!
//! One [`FsContext`] is shared through `CLONE_FS` (every thread of a process),
//! so a `chdir` from any sharer moves all; `fork` and spawn copy it. The path
//! is immutable once published and replaced whole, so a racing reader sees
//! the old path or the new one, never a mix.

#[cfg(any(test, feature = "test-helpers"))]
use core::sync::atomic::{AtomicBool, Ordering};

use crate::sync::RcuArcSlot;
use crate::{AllocError, KArc, KVec};

/// Capacity of a working-directory path, NUL terminator included.
pub const CWD_MAX: usize = slopos_abi::fs::USER_PATH_MAX;

/// A published working directory, NUL-terminated.
pub struct CwdPath {
    bytes: KVec<u8>,
}

impl CwdPath {
    fn try_new(path: &[u8]) -> Result<KArc<Self>, AllocError> {
        if path.len() + 1 > CWD_MAX {
            return Err(AllocError);
        }
        #[cfg(any(test, feature = "test-helpers"))]
        if CWD_ALLOC_FAILS.swap(false, Ordering::AcqRel) {
            return Err(AllocError);
        }
        let mut bytes = KVec::<u8>::zeroed(path.len() + 1)?;
        bytes[..path.len()].copy_from_slice(path);
        KArc::try_new(Self { bytes })
    }
}

/// Forces the next working-directory allocation to fail, so the refusal path
/// is reachable without exhausting the heap. Consumed by the refusal it causes.
#[cfg(any(test, feature = "test-helpers"))]
static CWD_ALLOC_FAILS: AtomicBool = AtomicBool::new(false);

#[cfg(any(test, feature = "test-helpers"))]
pub fn fail_next_cwd_alloc_for_test() {
    CWD_ALLOC_FAILS.store(true, Ordering::Release);
}

/// The filesystem state `CLONE_FS` shares. An empty slot is `/`.
pub struct FsContext {
    cwd: RcuArcSlot<CwdPath>,
}

impl FsContext {
    /// A context at `path`, or at `/` for `None`.
    pub fn try_new(path: Option<&[u8]>) -> Result<KArc<Self>, AllocError> {
        let cwd = match path {
            Some(path) => Some(CwdPath::try_new(path)?),
            None => None,
        };
        let mut fresh = Self {
            cwd: RcuArcSlot::empty(),
        };
        let _ = fresh.cwd.replace_exclusive(cwd);
        KArc::try_new(fresh)
    }

    /// A private context starting where this one is now, for `fork`, spawn
    /// and a clone without `CLONE_FS`. The path object itself is shared: it
    /// never changes, only the slot that names it does.
    pub fn try_copy(&self) -> Result<KArc<Self>, AllocError> {
        let mut fresh = Self {
            cwd: RcuArcSlot::empty(),
        };
        let _ = fresh.cwd.replace_exclusive(self.cwd.load());
        KArc::try_new(fresh)
    }

    /// Move every sharer to `path`. False if `path` does not fit with its
    /// NUL, or the new path could not be allocated; the old one stays.
    pub fn set_cwd(&self, path: &[u8]) -> bool {
        match CwdPath::try_new(path) {
            Ok(fresh) => {
                self.cwd.store(Some(fresh));
                true
            }
            Err(AllocError) => false,
        }
    }

    /// Call `f` with the working directory, NUL included.
    pub fn with_cwd<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        match self.cwd.load() {
            Some(path) => f(&path.bytes),
            None => f(b"/\0"),
        }
    }
}
