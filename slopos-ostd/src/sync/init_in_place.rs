//! One-shot in-place initialiser for a static too large to build on a stack.
//!
//! The value starts as the constant it was declared with; [`InitInPlace::init_once`]
//! writes it where it lies, and [`InitInPlace::get`] reads it once that has
//! returned.

use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct InitInPlace<T> {
    cell: SyncUnsafeCell<T>,
    gate: AtomicBool,
    done: AtomicBool,
}

// SAFETY: the gate admits one writer, on whichever thread wins it, and `get`
// hands out `&T` only once `done` says that writer's `&mut T` has ended, so no
// `&T` overlaps a `&mut T`.
unsafe impl<T: Send + Sync> Sync for InitInPlace<T> {}

impl<T> InitInPlace<T> {
    pub const fn new(value: T) -> Self {
        Self {
            cell: SyncUnsafeCell::new(value),
            gate: AtomicBool::new(false),
            done: AtomicBool::new(false),
        }
    }

    /// Run `init` exactly once on the value where it lies. Returns `true` if
    /// this call ran it; `false` if another call did or is running it, so
    /// [`Self::get`] may still answer `None`.
    pub fn init_once(&self, init: impl FnOnce(&mut T)) -> bool {
        if self.gate.swap(true, Ordering::SeqCst) {
            return false;
        }
        // SAFETY: the swap returned `false` (was uninit), so this thread holds
        // exclusive write access, and `get` reads nothing until `done`.
        let slot = unsafe { &mut *self.cell.get() };
        init(slot);
        self.done.store(true, Ordering::Release);
        true
    }

    /// The value once [`Self::init_once`] has returned, after which nothing
    /// can reach it mutably.
    #[inline]
    pub fn get(&self) -> Option<&T> {
        if !self.done.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `done` is published only after the one `&mut` reborrow has
        // ended, and the gate refuses every later one.
        Some(unsafe { &*self.cell.get() })
    }
}
