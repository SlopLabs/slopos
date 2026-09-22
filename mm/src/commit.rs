//! The commit ceiling: how many pages of private memory the machine will
//! promise. Derived from usable RAM once the buddy is seeded, held on the
//! root account's `CommitPages` row, and read back for `sys_info`.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::quota::{NO_LIMIT_SENTINEL, ResourceKind};
use slopos_ostd::process::quota::{root, set_limit, stats};

/// Share of usable frames the ledger may promise, in percent. The kernel's
/// own consumers draw from the same buddy under caps of their own, so at 100
/// a promise can still meet an empty buddy; that road stays `SIGBUS`.
pub const DEFAULT_COMMIT_PERCENT: u32 = 100;

static COMMIT_PERCENT: AtomicU32 = AtomicU32::new(DEFAULT_COMMIT_PERCENT);

/// `mem.commit=<percent>`; `0` leaves the ledger measuring with no ceiling.
pub fn set_commit_percent(percent: u32) {
    COMMIT_PERCENT.store(percent.min(400), Ordering::Release);
}

pub fn commit_percent() -> u32 {
    COMMIT_PERCENT.load(Ordering::Acquire)
}

/// The ceiling `usable_frames` and the configured share derive.
pub fn commit_limit_for(usable_frames: u32, percent: u32) -> u32 {
    if percent == 0 {
        return NO_LIMIT_SENTINEL;
    }
    let pages = (usable_frames as u64 * percent as u64) / 100;
    u32::try_from(pages)
        .unwrap_or(NO_LIMIT_SENTINEL - 1)
        .min(NO_LIMIT_SENTINEL - 1)
}

/// Install the ceiling measured off `usable_frames`. Returns the limit.
pub fn install(usable_frames: u32) -> u32 {
    let limit = commit_limit_for(usable_frames, commit_percent());
    set_limit(root(), ResourceKind::CommitPages, limit);
    limit
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CommitStats {
    /// Pages the machine will promise; `u32::MAX` when no ceiling is set.
    pub limit: u32,
    pub committed: u32,
    pub peak: u32,
    pub denials: u32,
}

pub fn commit_stats() -> CommitStats {
    match stats(root(), ResourceKind::CommitPages) {
        Some(s) => CommitStats {
            limit: s.limit,
            committed: s.used,
            peak: s.peak,
            denials: s.denials,
        },
        None => CommitStats::default(),
    }
}
