//! Lazy Unmap Flush (LUF) — the local half of tearing down a user mapping.
//!
//! The unmapping CPU stops resolving the address here, with one `invlpg`.
//! Holding the frame back until no peer still caches it belongs to
//! [`super::quiesce`]; no shootdown IPI is sent.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::VirtAddr;
use slopos_arch::pcr::MAX_CPUS;

/// Tear down the calling CPU's cached translation for `vaddr`, and record that
/// peers have *not* been told.
///
/// Unconditional: a `munmap` and a later dereference on the same CPU must
/// fault. `INVLPG` targets whatever PCID is currently loaded, so it is a no-op
/// when this CPU is running another address space; the quiesce epoch covers
/// that CPU when it next acks.
pub fn queue_unmap(vaddr: VirtAddr, mm_ctx_handle: u64) {
    slopos_arch::cpu::tlb::invlpg(vaddr.as_u64());
    let epoch = super::quiesce::note_deferred_unmap();
    CTX_DEFERRED[ctx_slot(mm_ctx_handle)].fetch_max(epoch, Ordering::AcqRel);
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

/// Per-CPU `mm_ctx_handle` of the address space currently installed in CR3.
/// `0` is the "no context bound" sentinel, matching `MmContextId::INVALID`.
/// Each CPU writes its own slot; cross-CPU reads are diagnostics only.
static ACTIVE_MM_CTX_HANDLE: [AtomicU64; MAX_CPUS] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_CPUS]
};

/// Record the address space the current CPU is installing; called from
/// `LufHook::on_activate`, before `VmSpace::activate` writes CR3.
#[inline]
pub fn current_cpu_set_active_mm_ctx(handle: u64) {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        ACTIVE_MM_CTX_HANDLE[cpu].store(handle, Ordering::Release);
    }
}

/// Read the address space handle this CPU last activated, or `0` if no
/// VmSpace has been activated on this CPU yet.
#[inline]
pub fn current_cpu_active_mm_ctx() -> u64 {
    let cpu = slopos_arch::pcr::get_current_cpu();
    if cpu < MAX_CPUS {
        ACTIVE_MM_CTX_HANDLE[cpu].load(Ordering::Acquire)
    } else {
        0
    }
}
