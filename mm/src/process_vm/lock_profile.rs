//! `prof=on`: where the per-process address-space lock is waited for and held,
//! by call site. The lock is a spinlock taken with interrupts off, so a
//! sampling profiler sees its whole cost land on the guard's drop and cannot
//! say which path paid it.

use core::panic::Location;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use slopos_ostd::lock_class;
use slopos_ostd::sync::LOCK_LEVEL_UNORDERED;
use slopos_ostd::sync::spin::{SpinLock, SpinLockGuard};

use super::ProcessVm;

static ENABLED: AtomicBool = AtomicBool::new(false);

const SITES: usize = 128;

struct Site {
    key: AtomicUsize,
    acquires: AtomicU64,
    wait: AtomicU64,
    hold: AtomicU64,
}

static TABLE: [Site; SITES] = {
    const EMPTY: Site = Site {
        key: AtomicUsize::new(0),
        acquires: AtomicU64::new(0),
        wait: AtomicU64::new(0),
        hold: AtomicU64::new(0),
    };
    [EMPTY; SITES]
};

/// What each claimed entry of `TABLE` names; written once, when it is claimed.
static LOCATIONS: SpinLock<[Option<&'static Location<'static>>; SITES]> = SpinLock::new(
    [None; SITES],
    lock_class!("PROCESS_VM_LOCK_SITES", LOCK_LEVEL_UNORDERED),
);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// The table entry for `location`, claimed on first use; `None` once full.
fn site(location: &'static Location<'static>) -> Option<&'static Site> {
    let key = core::ptr::from_ref(location) as usize;
    let start = (key >> 3) % SITES;
    for probe in 0..SITES {
        let index = (start + probe) % SITES;
        let site = &TABLE[index];
        match site
            .key
            .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                LOCATIONS.lock()[index] = Some(location);
                return Some(site);
            }
            Err(seen) if seen == key => return Some(site),
            Err(_) => {}
        }
    }
    None
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
        let site = site(location);
        let began = slopos_arch::tsc::rdtsc();
        let guard = self.0.lock();
        let acquired = slopos_arch::tsc::rdtsc();
        let profile = site.map(|site| {
            site.acquires.fetch_add(1, Ordering::Relaxed);
            site.wait
                .fetch_add(acquired.saturating_sub(began), Ordering::Relaxed);
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
            let held = slopos_arch::tsc::rdtsc().saturating_sub(acquired);
            site.hold.fetch_add(held, Ordering::Relaxed);
        }
    }
}

/// Every call site seen: `(location, acquires, wait cycles, hold cycles)`.
pub fn for_each_site(mut f: impl FnMut(&'static Location<'static>, u64, u64, u64)) {
    for (index, site) in TABLE.iter().enumerate() {
        let Some(location) = LOCATIONS.lock()[index] else {
            continue;
        };
        f(
            location,
            site.acquires.load(Ordering::Relaxed),
            site.wait.load(Ordering::Relaxed),
            site.hold.load(Ordering::Relaxed),
        );
    }
}
