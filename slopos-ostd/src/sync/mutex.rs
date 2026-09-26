//! Sleeping mutex built on top of [`SpinLock`] + [`WaitQueue`].
//!
//! A contended locker spins for a bounded [`SPIN_CYCLES`] and then blocks on
//! the wait queue, so the lock may be held across long-running operations — and
//! must never be taken from an interrupt handler, which would block the CPU.
//!
//! Until a [`WaitQueueBackend`](super::wait_queue::WaitQueueBackend) is
//! registered, `lock()` falls back to spin-acquiring the inner spinlock.

use crate::sync::lock_tracking::LockClassKey;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering, fence};

use crate::mm::AllocError;
use crate::mm::init::{Init, init_from_closure, init_from_owned};
use crate::sync::wait_queue::{WaitAbort, WaitQueue, WaitResult};

/// How long a contended `lock` spins before it sleeps: about 20 µs at the clock
/// rates this runs at. Most holds are shorter than a block, a context switch and
/// a wake, which is what sleeping at once pays for each of them; a longer hold
/// costs the spinner no more than this before it sleeps as it always did.
const SPIN_CYCLES: u64 = 60_000;

pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: WaitQueue,
    data: UnsafeCell<T>,
}

// SAFETY: synchronisation through `locked` + the wait queue.
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// `class` names the inner wait queue, which is the tracked lock here: the
    /// `Mutex` itself sleeps and so cannot live on the per-CPU held stack.
    pub const fn new(data: T, class: &'static LockClassKey) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: WaitQueue::new(class),
            data: UnsafeCell::new(data),
        }
    }

    /// Place an already-owned `data` directly into the destination:
    /// `KArc::try_new(Mutex::new(big, class))` would stage `big` through two
    /// stack frames. The error type is fixed rather than generic so consumer
    /// crates never have to name `AllocError`, which is
    /// `allocator_api`-unstable.
    pub fn init_owned(data: T, class: &'static LockClassKey) -> impl Init<Self, AllocError> {
        Self::init_with(class, init_from_owned::<T, AllocError>(data))
    }

    /// In-place [`Init`] recipe, so a large `T` never materialises on the
    /// caller's stack between allocation and construction.
    pub fn init_with<E>(
        class: &'static LockClassKey,
        data_init: impl Init<T, E>,
    ) -> impl Init<Self, E>
    where
        E: From<AllocError>,
    {
        // SAFETY: the closure writes every field of `slot`; `locked` and
        // `waiters` are built in place so no `Self` rvalue exists, and
        // `data_init` writes the inner `T` into the same heap slot.
        unsafe {
            init_from_closure(move |slot: *mut Self| -> Result<(), E> {
                addr_of_mut!((*slot).locked).write(AtomicBool::new(false));
                addr_of_mut!((*slot).waiters).write(WaitQueue::new(class));
                let data_ptr = addr_of_mut!((*slot).data) as *mut T;
                data_init.__init(data_ptr)?;
                Ok(())
            })
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(MutexGuard { mutex: self })
        } else {
            None
        }
    }

    /// Acquire the lock, blocking the current task if necessary.
    ///
    /// Fails with [`WaitAbort::Killed`] when the current task is marked for
    /// death while contending: the guard is released only by its `Drop`, so a
    /// task abandoned here would hold the lock forever. Killable rather than
    /// interruptible — a `SIGINT` must not abandon a filesystem operation
    /// midway.
    ///
    /// With no wait-queue backend registered (the pre-scheduler device-probe
    /// paths) the acquire degrades to busy-waiting on the flag.
    #[must_use = "an unacquired lock guards nothing"]
    pub fn lock(&self) -> WaitResult<MutexGuard<'_, T>> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        if let Some(guard) = self.spin_for_unlock() {
            return Ok(guard);
        }
        loop {
            if let Some(guard) = self.try_lock() {
                return Ok(guard);
            }

            match self
                .waiters
                .wait_event(|| !self.locked.load(Ordering::Acquire))
            {
                Ok(()) => {}
                Err(abort @ (WaitAbort::Killed | WaitAbort::Interrupted)) => return Err(abort),
                // `Timeout` cannot arise on an untimed wait; spinning rather
                // than panicking keeps this path panic-free.
                Err(_) => core::hint::spin_loop(),
            }
        }
    }

    fn spin_for_unlock(&self) -> Option<MutexGuard<'_, T>> {
        let start = crate::arch::x86_64::tsc::rdtsc();
        loop {
            if !self.locked.load(Ordering::Relaxed)
                && let Some(guard) = self.try_lock()
            {
                return Some(guard);
            }
            if crate::arch::x86_64::tsc::rdtsc().wrapping_sub(start) > SPIN_CYCLES {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock, so access is exclusive.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock, so access is exclusive.
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
        // Pairs with the fence `wait_core` takes between queueing a waiter and
        // re-reading `locked`: either this sees the waiter, or it sees the lock
        // free and never sleeps.
        fence(Ordering::SeqCst);
        if self.mutex.waiters.has_waiters() {
            self.mutex.waiters.wake_one();
        }
    }
}
