//! `prof=on`: where the per-process address-space lock is waited for and held,
//! by call site. The lock is a spinlock taken with interrupts off, so a
//! sampling profiler sees its whole cost land on the guard's drop and cannot
//! say which path paid it.

use core::panic::Location;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::lock_class;
use slopos_ostd::sync::LOCK_LEVEL_UNORDERED;
use slopos_ostd::sync::spin::{SpinLock, SpinLockGuard};

use super::ProcessVm;
use crate::lock_sites::{Site, SiteTable};

static ENABLED: AtomicBool = AtomicBool::new(false);

static SITES: SiteTable =
    SiteTable::new(lock_class!("PROCESS_VM_LOCK_SITES", LOCK_LEVEL_UNORDERED));

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn disable() {
    ENABLED.store(false, Ordering::Relaxed);
}

/// Zero every site's counts.
pub fn reset() {
    SITES.reset_counts();
}

/// One slot of the address-space table: the lock and its profile hook.
pub(super) struct VmLock(SpinLock<ProcessVm>);

impl VmLock {
    pub(super) const fn new(lock: SpinLock<ProcessVm>) -> Self {
        Self(lock)
    }

    pub(super) fn raw(&self) -> &SpinLock<ProcessVm> {
        &self.0
    }

    #[track_caller]
    #[inline]
    pub(super) fn lock(&self) -> VmGuard<'_> {
        if !ENABLED.load(Ordering::Relaxed) {
            return VmGuard {
                guard: self.0.lock(),
                profile: None,
            };
        }
        self.lock_profiled(Location::caller())
    }

    #[inline(never)]
    fn lock_profiled(&self, location: &'static Location<'static>) -> VmGuard<'_> {
        let site = SITES.site(location);
        let began = slopos_arch::tsc::rdtsc();
        let guard = self.0.lock();
        let acquired = slopos_arch::tsc::rdtsc();
        let profile = site.map(|site| {
            site.note_acquire(acquired.saturating_sub(began));
            (site, acquired)
        });
        VmGuard { guard, profile }
    }
}

pub(super) struct VmGuard<'a> {
    guard: SpinLockGuard<'a, ProcessVm>,
    profile: Option<(&'static Site, u64)>,
}

impl core::ops::Deref for VmGuard<'_> {
    type Target = ProcessVm;
    fn deref(&self) -> &ProcessVm {
        &self.guard
    }
}

impl core::ops::DerefMut for VmGuard<'_> {
    fn deref_mut(&mut self) -> &mut ProcessVm {
        &mut self.guard
    }
}

impl Drop for VmGuard<'_> {
    fn drop(&mut self) {
        if let Some((site, acquired)) = self.profile {
            site.note_hold(slopos_arch::tsc::rdtsc().saturating_sub(acquired));
        }
    }
}

/// Every call site seen: `(location, acquires, wait cycles, hold cycles)`.
pub fn for_each_site(f: impl FnMut(&'static Location<'static>, u64, u64, u64)) {
    SITES.for_each(f);
}
