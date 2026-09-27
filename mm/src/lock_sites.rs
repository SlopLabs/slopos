//! `prof=on`: a lock's wait and hold, by the call site that took it. A
//! sampling profiler sees a spinlock's cost land on the guard's drop and a
//! sleeping lock's in the scheduler, so neither can say which path paid it.

use core::panic::Location;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use slopos_ostd::sync::LockClassKey;
use slopos_ostd::sync::spin::SpinLock;

const SITES: usize = 128;

pub struct Site {
    key: AtomicUsize,
    acquires: AtomicU64,
    wait: AtomicU64,
    hold: AtomicU64,
}

impl Site {
    const fn new() -> Self {
        Self {
            key: AtomicUsize::new(0),
            acquires: AtomicU64::new(0),
            wait: AtomicU64::new(0),
            hold: AtomicU64::new(0),
        }
    }

    pub fn note_acquire(&self, wait_cycles: u64) {
        self.acquires.fetch_add(1, Ordering::Relaxed);
        self.wait.fetch_add(wait_cycles, Ordering::Relaxed);
    }

    pub fn note_hold(&self, hold_cycles: u64) {
        self.hold.fetch_add(hold_cycles, Ordering::Relaxed);
    }
}

pub struct SiteTable {
    sites: [Site; SITES],
    /// What each claimed entry names; written once, when it is claimed.
    locations: SpinLock<[Option<&'static Location<'static>>; SITES]>,
}

impl SiteTable {
    pub const fn new(class: &'static LockClassKey) -> Self {
        Self {
            sites: [const { Site::new() }; SITES],
            locations: SpinLock::new([None; SITES], class),
        }
    }

    /// The entry for `location`, claimed on first use; `None` once full.
    pub fn site(&self, location: &'static Location<'static>) -> Option<&Site> {
        let key = core::ptr::from_ref(location) as usize;
        let start = (key >> 3) % SITES;
        for probe in 0..SITES {
            let index = (start + probe) % SITES;
            let site = &self.sites[index];
            match site
                .key
                .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.locations.lock()[index] = Some(location);
                    return Some(site);
                }
                Err(seen) if seen == key => return Some(site),
                Err(_) => {}
            }
        }
        None
    }

    /// Every call site seen: `(location, acquires, wait cycles, hold cycles)`.
    pub fn for_each(&self, mut f: impl FnMut(&'static Location<'static>, u64, u64, u64)) {
        for (index, site) in self.sites.iter().enumerate() {
            let Some(location) = self.locations.lock()[index] else {
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
}
