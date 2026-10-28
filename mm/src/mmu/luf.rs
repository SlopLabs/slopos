//! Lazy Unmap Flush (LUF) — the local half of tearing down a user mapping.
//!
//! The unmapping CPU's own invalidation belongs to the cursor: each entry's
//! clear is ordered before the unmap hook runs, and the local invalidation is
//! complete by the time the range operation returns. Holding the frame back
//! until no peer still caches it belongs to [`super::quiesce`]; no shootdown
//! IPI is sent.

use core::sync::atomic::{AtomicU64, Ordering};

/// Record that peers have *not* been told `mm_ctx_handle` lost a translation.
///
/// Runs once per page a range teardown clears, so a stamp already this new is
/// only read: the RMW would bounce a line every unmapping CPU shares.
pub fn queue_unmap(mm_ctx_handle: u64) {
    let epoch = super::quiesce::note_deferred_unmap();
    let slot = &CTX_DEFERRED[ctx_slot(mm_ctx_handle)];
    if slot.load(Ordering::Acquire) < epoch {
        slot.fetch_max(epoch, Ordering::AcqRel);
    }
}

/// Address spaces tracked individually; context handles that share a slot
/// share its epoch, which only ever makes a flush more likely.
const CTX_SLOTS: usize = 1024;

/// The newest epoch in which an address space hashing to each slot unmapped a
/// page lazily. Zero: none ever did.
static CTX_DEFERRED: [AtomicU64; CTX_SLOTS] = [const { AtomicU64::new(0) }; CTX_SLOTS];

fn ctx_slot(mm_ctx_handle: u64) -> usize {
    (mm_ctx_handle % CTX_SLOTS as u64) as usize
}

/// Whether any CPU may still cache a translation `mm_ctx_handle` tore down
/// lazily. A page mapped where nothing was ever mapped needs no invalidation
/// — x86 caches no not-present translation — so a fresh mapping owes a
/// shootdown only while a lazily unmapped predecessor may linger somewhere.
///
/// Serialised against [`queue_unmap`] by the per-process lock both run under.
pub fn may_hold_stale(mm_ctx_handle: u64) -> bool {
    let deferred = CTX_DEFERRED[ctx_slot(mm_ctx_handle)].load(Ordering::Acquire);
    deferred != 0 && !super::quiesce::all_acked_after(deferred)
}
