//! Per-address-space typed page-table handle.
//!
//! `VmSpace` is the only handle to a process address space exposed by
//! OSTD: the PML4 frame is private, and the sole mutation path is a
//! [`CursorMut`] held against `&mut VmSpace`.
//!
//! The mutation path ([`CursorMut::map`], [`CursorMut::map_kernel`],
//! [`CursorMut::map_io`], [`CursorMut::unmap`], [`CursorMut::protect`])
//! is machine-checked under Verus in
//! `verification/proofs/vm_space_cursor.rs`, which proves three
//! obligations over every sequence of cursor calls:
//!
//!   * (WF) a present leaf implies its whole intermediate chain (PT, PD,
//!     PDPT) is present and valid, so no walk dereferences a dangling
//!     table;
//!   * (REF) a leaf that owns a reference holds exactly one — `map` /
//!     `map_kernel` leak exactly that one, `unmap` reclaims it, and no
//!     reference is fabricated for a `map_io` leaf that never took one;
//!   * (Inv. 4 + Inv. 5) a present *user-visible* leaf is always an
//!     insensitive frame, carried by `map`'s
//!     [`UFrame<M>`](crate::mm::uframe::UFrame) argument type and by the
//!     `!prop.user` guard on `map_kernel` / `map_io`.
//!
//! The exclusivity model has two tiers: the borrow checker gives one
//! `CursorMut` per `VmSpace` *object*, and the lock that owns a shared
//! object is the sole minter of the `&mut` — `PROCESS_VMS[slot]` per
//! process, `KERNEL_VM_SPACE` for the kernel master. See
//! `verification/STATUS.md` for the gap vs. CortenMM's fine-grained
//! per-PT-page locking.

use core::ops::{ControlFlow, Range};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::arch::x86_64::cr3::{Pcid, write_cr3_pcid};
use crate::mm::frame::{AnyFrameMeta, Frame, FrameAllocOptions, Paddr, PageTableMeta};
use crate::mm::frame_alloc::current_frame_allocator;
use crate::mm::page_property::PageProperty;
use crate::mm::page_size::{PageSize, Size4Kb};
use crate::mm::page_table::{
    PAGE_SIZE_4KB, PAGE_TABLE_ENTRIES, PageTableLevel, Pte, PteFlags, WalkMode, WalkOutcome,
    charge_page_table_frame, entry_in_table, read_leaf, reclaim_leaked_frame,
    refund_page_table_frame, walk_to_leaf,
};
use crate::mm::tlb;
use crate::mm::uframe::{AnyUFrameMeta, UFrame};
use crate::sync::BspToken;

const KERNEL_HALF_START_INDEX: usize = 256;
const KERNEL_HALF_END_INDEX: usize = 512;

/// Lowest canonical higher-half virtual address — PML4 index 256, the
/// first entry of the kernel half.
const HIGHER_HALF_START: u64 = 0xFFFF_8000_0000_0000;

/// Per-address-space page-table handle.
///
/// The kernel half (PML4 indices 256..512) is copied from the registered
/// master once, at construction, and never resynchronised;
/// [`prepopulate_kernel_half`] is what makes that sound. Every deeper
/// kernel-half mutation lands in a table the copy already points at, so
/// every address space sees it immediately.
pub struct VmSpace {
    pml4: Frame<PageTableMeta>,
    /// Whether the PML4 frame carries a page-table charge to give back when
    /// this space drops. [`VmSpace::wrap_existing`] adopts a frame the
    /// bootloader allocated, which nobody charged and nothing frees.
    pml4_charged: bool,
    generation: AtomicU64,
    /// Opaque consumer-defined handle threaded through
    /// [`CursorUnmapHook`] callbacks. `0` ⇒ unset.
    mm_ctx_handle: AtomicU64,
    /// User-half leaves currently present, in 4 KiB pages — the address
    /// space's resident set.
    ///
    /// Counted here because the cursor is the only place a user leaf appears or
    /// disappears; a consumer-maintained count would be a second truth that
    /// drifts.
    resident: AtomicU32,
}

// SAFETY: `Frame<PageTableMeta>` is `Send + Sync` (the `META_SLOTS`
// machinery synchronises ref-count manipulation), and mutation through
// `&mut VmSpace` serialises page-table writes via the cursor.
unsafe impl Send for VmSpace {}
unsafe impl Sync for VmSpace {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Tried to map an already-present slot. Use `unmap` first.
    Overlap,
    /// Cursor moved past `range.end`.
    OutOfBounds,
    /// FrameAlloc returned `None` for an intermediate page-table frame.
    IntermediateAllocFailed,
    /// FrameAlloc not registered, or kernel-master PML4 not registered.
    Uninitialised,
    /// `range` is not page-aligned.
    UnalignedRange,
    /// Internal: a slot lookup failed for a paddr that should have a
    /// META_SLOTS entry.
    PathCorrupt,
    /// Cursor's current vaddr is not aligned to `S::BYTES` for the
    /// requested huge-page operation.
    UnalignedCursor,
    /// Frame's physical address is not aligned to `S::BYTES` for the
    /// requested huge-page operation.
    UnalignedFrame,
    /// `unmap::<S>` / `protect::<S>` invoked on a leaf whose actual
    /// page size differs from `S`.
    SizeMismatch,
    /// A mutating cursor was requested while another handle on the same address
    /// space was outstanding. Transient: release what is held and retry.
    WouldBlock,
    /// A kernel-half entry point ([`CursorMut::map_kernel`],
    /// [`CursorMut::map_io`]) was handed `prop.user`, or a cursor
    /// positioned below the canonical higher half. Neither condition is
    /// recoverable by retrying.
    NotKernelMapping,
}

/// Read-only walking handle over `[range.start, range.end)`.
pub struct Cursor<'a> {
    space: &'a VmSpace,
    range: Range<VirtAddr>,
    cur: VirtAddr,
}

/// Mutating walking handle. Bumps `space.generation` once on `Drop`
/// if any commit happened.
pub struct CursorMut<'a> {
    space: &'a mut VmSpace,
    range: Range<VirtAddr>,
    cur: VirtAddr,
    dirty: bool,
}

/// Snapshot of a cursor's current entry.
#[derive(Debug, Clone, Copy)]
pub struct CursorEntry {
    pub vaddr: VirtAddr,
    pub paddr: Option<Paddr>,
    pub property: PageProperty,
    pub level: PageTableLevel,
}

static KERNEL_MASTER_PML4: AtomicU64 = AtomicU64::new(KERNEL_MASTER_UNINIT);
const KERNEL_MASTER_UNINIT: u64 = u64::MAX;

/// Install the kernel-master PML4 paddr. The `&BspToken<'brand>`
/// witnesses BSP-only init. `paddr` must point to a 4 KiB-aligned,
/// valid PML4 reachable through the kernel HHDM; indices 256..512
/// must describe the canonical kernel mappings (byte-copied by
/// `VmSpace::new` into every fresh address space) and the mapping
/// must persist for the static lifetime of the kernel.
pub fn register_kernel_master_pml4<'brand>(_token: &BspToken<'brand>, paddr: PhysAddr) {
    assert_eq!(
        paddr.as_u64() & (PAGE_SIZE_4KB - 1),
        0,
        "register_kernel_master_pml4: paddr 0x{:x} is not 4 KiB-aligned — \
         a raw CR3 read carries PCID and PWT/PCD in the low bits and must be \
         masked before it names a table base",
        paddr.as_u64(),
    );
    let prev = KERNEL_MASTER_PML4.swap(paddr.as_u64(), Ordering::AcqRel);
    assert_eq!(
        prev, KERNEL_MASTER_UNINIT,
        "register_kernel_master_pml4 called twice"
    );
}

/// The registered kernel-master PML4 base, or `None` before boot registers it.
pub fn kernel_master_pml4() -> Option<PhysAddr> {
    let master = KERNEL_MASTER_PML4.load(Ordering::Acquire);
    (master != KERNEL_MASTER_UNINIT).then(|| PhysAddr::new(master))
}

/// Load the registered kernel-master PML4 into this CPU's CR3, without the
/// `KERNEL_VM_SPACE` lock the scheduler would otherwise take on every switch.
///
/// Nothing [`VmSpace::activate`] reads through `&self` varies for the master:
/// its base is write-once, its `mm_ctx_handle` is permanently zero, and
/// `select_cr3`'s invalid-context arm answers `(PCID 0, flush)` either way.
///
/// No-op before registration, where CR3 already holds the master.
pub fn activate_kernel_master_cr3() {
    let master = KERNEL_MASTER_PML4.load(Ordering::Acquire);
    if master == KERNEL_MASTER_UNINIT {
        return;
    }
    if let Some(hook) = current_cursor_unmap_hook() {
        hook.on_activate(0);
    }
    // SAFETY: `register_kernel_master_pml4`'s contract is exactly
    // `write_cr3_pcid`'s — a 4 KiB-aligned, HHDM-reachable PML4 whose kernel
    // half is canonical and lives for the static lifetime of the kernel — and
    // PCID 0 with `no_flush == false` is the flushing load it asks for.
    unsafe { write_cr3_pcid(PhysAddr::new(master), Pcid::KERNEL, false) };
}

/// Trait the consumer-side TLB / Lazy-User-Flush coordinator implements.
/// `mm_ctx_handle` is an opaque `u64` whose meaning is defined entirely
/// by the consumer — OSTD only stashes it on each `VmSpace` (via
/// [`VmSpace::set_mm_ctx_handle`]) and threads it through these
/// callbacks.
pub trait CursorUnmapHook: Send + Sync {
    /// Fired at the end of [`CursorMut::unmap`] for entries that had
    /// the `USER` bit set, after the entry's clear and before the frame is
    /// handed back. This CPU's invalidation of `vaddr` is complete by the
    /// time the unmapping operation returns, not necessarily by this call.
    fn after_unmap(&self, vaddr: VirtAddr, paddr: PhysAddr, mm_ctx_handle: u64);

    /// Invalidate this CPU's translations in place of a range operation's
    /// per-page local invalidation: at least every one an `invlpg` would
    /// reach, i.e. the context CR3 carries.
    fn flush_local(&self);

    /// Fired at the start of [`VmSpace::activate`].
    fn on_activate(&self, mm_ctx_handle: u64);

    /// Choose the `(pcid, no_flush)` pair for a CR3 load of `mm_ctx_handle`.
    ///
    /// The consumer owns the per-CPU PCID binding, so it is the only thing
    /// that can know whether the tag it hands back still holds this context's
    /// translations. Returning `no_flush == true` asserts that it does; a
    /// consumer that cannot prove it must return `false`, and one that has no
    /// pool at all must return `None` so OSTD falls back to a flushing load
    /// under the kernel PCID.
    ///
    /// `tlb_gen` is the address space's generation counter, bumped by every
    /// page-table mutation. A slot cached at an older generation may hold
    /// stale translations and must be flushed before it is reused.
    fn select_cr3(&self, mm_ctx_handle: u64, tlb_gen: u64) -> Option<(u16, bool)>;
}

static CURSOR_UNMAP_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// One-shot registration of the consumer-side cursor-unmap hook. The
/// `&BspToken<'brand>` witnesses BSP-only init; the underlying
/// `dyn CursorUnmapHook` must be sound for concurrent calls from any
/// CPU.
pub fn register_cursor_unmap_hook<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn CursorUnmapHook,
) {
    let raw = slot as *const &'static dyn CursorUnmapHook as *mut ();
    let prev = CURSOR_UNMAP_HOOK.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::vm_space::register_cursor_unmap_hook called twice"
    );
}

#[inline]
fn current_cursor_unmap_hook() -> Option<&'static dyn CursorUnmapHook> {
    let raw = CURSOR_UNMAP_HOOK.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `raw` was produced by `register_cursor_unmap_hook` from a
    // `&'static &'static dyn CursorUnmapHook`; that storage is `'static`
    // by contract.
    let slot = unsafe { &*(raw as *const &'static dyn CursorUnmapHook) };
    Some(*slot)
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_cursor_unmap_hook_for_test() {
    CURSOR_UNMAP_HOOK.store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_kernel_master_for_test() {
    KERNEL_MASTER_PML4.store(KERNEL_MASTER_UNINIT, Ordering::Release);
}

impl VmSpace {
    /// Allocate a fresh address space: a zeroed PML4 with kernel-half
    /// mappings copied from the registered kernel master.
    pub fn new() -> Result<Self, MapError> {
        let alloc = current_frame_allocator().ok_or(MapError::Uninitialised)?;
        let master = KERNEL_MASTER_PML4.load(Ordering::Acquire);
        if master == KERNEL_MASTER_UNINIT {
            return Err(MapError::Uninitialised);
        }
        let master_phys = PhysAddr::new(master);

        if !charge_page_table_frame() {
            return Err(MapError::IntermediateAllocFailed);
        }
        let Some(pml4_phys) = alloc.alloc(FrameAllocOptions::single().zeroed()) else {
            refund_page_table_frame();
            return Err(MapError::IntermediateAllocFailed);
        };
        let pml4 = Frame::<PageTableMeta>::from_unused(
            pml4_phys,
            PageTableMeta {
                level: PageTableLevel::Four as u8,
                static_borrowed: false,
            },
        )
        .map_err(|_| MapError::PathCorrupt)?;

        copy_kernel_half(master_phys, pml4_phys);

        Ok(Self {
            pml4,
            pml4_charged: true,
            generation: AtomicU64::new(0),
            mm_ctx_handle: AtomicU64::new(0),
            resident: AtomicU32::new(0),
        })
    }

    /// Wrap an already-installed PML4 frame (e.g. the live kernel
    /// master left behind by Limine) as a `VmSpace`. The frame's
    /// contents are preserved — only the META_SLOTS entry transitions
    /// from UNUSED to TYPED to make the frame ref-counted.
    ///
    /// # Safety
    ///
    /// Caller asserts:
    ///
    /// 1. `pml4_phys` is 4 KiB-aligned and reachable via the kernel
    ///    HHDM mapping.
    /// 2. The frame's META_SLOTS entry is currently UNUSED (no other
    ///    `Frame<_>` handle exists for this paddr); the call may
    ///    return [`MapError::PathCorrupt`] otherwise.
    /// 3. PML4 indices 256..512 already contain the canonical
    ///    kernel-half mappings — `wrap_existing` does **not** copy
    ///    from `KERNEL_MASTER_PML4` (the wrapped frame typically
    ///    *is* the kernel master).
    pub unsafe fn wrap_existing(pml4_phys: PhysAddr, _pcid: Pcid) -> Result<Self, MapError> {
        let pml4 = Frame::<PageTableMeta>::from_unused(
            pml4_phys,
            PageTableMeta {
                level: PageTableLevel::Four as u8,
                static_borrowed: true,
            },
        )
        .map_err(|_| MapError::PathCorrupt)?;
        Ok(Self {
            pml4,
            pml4_charged: false,
            generation: AtomicU64::new(0),
            mm_ctx_handle: AtomicU64::new(0),
            resident: AtomicU32::new(0),
        })
    }

    /// Safe wrapper around [`Self::wrap_existing`] gated by a
    /// [`BspToken`](crate::sync::BspToken), used at boot to install the
    /// singleton `KERNEL_VM_SPACE` around the live kernel master PML4
    /// left behind by Limine.
    ///
    /// The token discharges the BSP-only-init clause of `wrap_existing`'s
    /// contract; clauses 1–4 are facts encoded by the boot phase ordering
    /// (meta_slots installed at priority 5, frame_alloc at priority 6,
    /// this call at priority 55).
    pub fn wrap_kernel_master<'brand>(
        _token: &crate::sync::BspToken<'brand>,
        pml4_phys: PhysAddr,
    ) -> Result<Self, MapError> {
        // SAFETY: token + boot ordering jointly discharge the four
        // clauses of `wrap_existing`'s contract (see fn-level docs).
        unsafe { Self::wrap_existing(pml4_phys, Pcid::KERNEL) }
    }

    pub fn pml4_paddr(&self) -> PhysAddr {
        self.pml4.paddr()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Read-only cursor over `range`. `range` must be 4 KiB-aligned.
    pub fn cursor(&self, range: Range<VirtAddr>) -> Result<Cursor<'_>, MapError> {
        check_range_alignment(&range)?;
        Ok(Cursor {
            space: self,
            cur: range.start,
            range,
        })
    }

    /// Mutating cursor over `range`. `range` must be 4 KiB-aligned.
    pub fn cursor_mut(&mut self, range: Range<VirtAddr>) -> Result<CursorMut<'_>, MapError> {
        check_range_alignment(&range)?;
        Ok(CursorMut {
            cur: range.start,
            range,
            space: self,
            dirty: false,
        })
    }

    /// Safe wrapper around [`Self::activate`] for the kernel master
    /// VmSpace: the master maps kernel-half indices 256..512 directly,
    /// so the kernel-half invariant is trivially satisfied. The
    /// scheduler hot path uses [`Self::activate_at_context_switch`].
    pub fn activate_kernel_master(&self) {
        // SAFETY: the kernel master VmSpace always satisfies the
        // kernel-half invariant; CR3 reload to it is sound from any
        // kernel-mode context.
        unsafe { self.activate() }
    }

    /// BSP-token-gated variant for boot-time kernel master CR3 reload;
    /// the token witnesses BSP-init scope (IRQs off, single CPU).
    pub fn activate_kernel_master_bsp<'brand>(&self, _token: &crate::sync::BspToken<'brand>) {
        // SAFETY: BSP-init scope + IRQs off + KERNEL_VM_SPACE just
        // installed jointly discharge the kernel-half invariant the
        // `unsafe fn activate` contract names.
        unsafe { self.activate() };
    }

    /// Safe activate for the scheduler context-switch path: the
    /// scheduler upholds the `activate` contract for every dispatch
    /// (IRQs off, and the kernel half of every address space is a copy
    /// of a master whose top level never changes after
    /// [`prepopulate_kernel_half`]).
    #[inline]
    pub fn activate_at_context_switch(&self) {
        // SAFETY: scheduler invariant — context-switch runs with IRQs
        // disabled on the local CPU, and this VmSpace's kernel half is
        // valid by construction.
        unsafe { self.activate() };
    }

    /// Switch the current CPU to this address space. The only
    /// sanctioned CR3 write path. Fires the registered
    /// [`CursorUnmapHook::on_activate`] callback, if any.
    pub unsafe fn activate(&self) {
        let ctx = self.mm_ctx_handle.load(Ordering::Acquire);
        let hook = current_cursor_unmap_hook();
        if let Some(hook) = hook {
            hook.on_activate(ctx);
        }
        // NOFLUSH is architecturally legal only when `CR4.PCIDE` is
        // enabled; setting it on a platform that never enabled PCIDE
        // yields a #GP.
        let pcide_enabled = (crate::cpu::x86_64::control_regs::read_cr4()
            & crate::cpu::x86_64::control_regs::Cr4Flags::PCIDE.bits())
            != 0;

        // The PCID and the right to skip the flush are one decision, taken by
        // whoever owns the per-CPU binding. A tag chosen without consulting it
        // — a counter masked to 12 bits, say — aliases another address space
        // every 4096 allocations, and NOFLUSH then keeps that address space's
        // translations live under the new one.
        let tlb_gen = self.generation.load(Ordering::Acquire);
        let (pcid, no_flush) = match hook.and_then(|h| h.select_cr3(ctx, tlb_gen)) {
            Some((raw, no_flush)) if pcide_enabled => (Pcid::new(raw), no_flush),
            // No pool, or PCIDE off: PCID 0 with a flushing load is always
            // correct, merely slower.
            _ => (Pcid::KERNEL, false),
        };

        // SAFETY: `self.pml4` is a live page-table frame whose
        // kernel-half indices were copied from the master, whose top
        // level does not change after `prepopulate_kernel_half`; the
        // caller upholds the rest of `write_cr3_pcid`'s contract, and
        // `no_flush` was minted by the pool that owns the binding.
        unsafe { write_cr3_pcid(self.pml4_paddr(), pcid, no_flush) };
    }

    /// Stash an opaque consumer-defined identifier for this address
    /// space. Idempotent for repeated calls with the same value; panics
    /// on conflicting reassignment so a process is never silently
    /// re-bound to a different identity.
    pub fn set_mm_ctx_handle(&self, handle: u64) {
        let prev =
            self.mm_ctx_handle
                .compare_exchange(0, handle, Ordering::AcqRel, Ordering::Acquire);
        match prev {
            Ok(_) => {}
            Err(existing) => {
                assert_eq!(
                    existing, handle,
                    "VmSpace::set_mm_ctx_handle: conflicting reassignment \
                     (existing={existing:#x}, new={handle:#x})"
                );
            }
        }
    }

    /// Read the consumer-set context handle (`0` ⇒ unset).
    pub fn mm_ctx_handle(&self) -> u64 {
        self.mm_ctx_handle.load(Ordering::Acquire)
    }

    /// User-half leaves currently present, in 4 KiB pages: this address
    /// space's resident set.
    pub fn resident_pages(&self) -> u32 {
        self.resident.load(Ordering::Relaxed)
    }

    /// A user leaf appeared (`delta > 0`) or left. Saturating rather than
    /// wrapping: a miscount must not read as four billion pages.
    fn note_resident(&mut self, delta: i32) {
        let resident = self.resident.get_mut();
        *resident = resident.saturating_add_signed(delta);
    }
}

fn check_range_alignment(range: &Range<VirtAddr>) -> Result<(), MapError> {
    if range.start.as_u64() & 0xFFF != 0 || range.end.as_u64() & 0xFFF != 0 {
        return Err(MapError::UnalignedRange);
    }
    if range.start.as_u64() > range.end.as_u64() {
        return Err(MapError::UnalignedRange);
    }
    Ok(())
}

fn copy_kernel_half(master_phys: PhysAddr, dest_phys: PhysAddr) {
    for i in KERNEL_HALF_START_INDEX..KERNEL_HALF_END_INDEX {
        let src = entry_in_table(master_phys, i);
        let dst = entry_in_table(dest_phys, i);
        dst.write(src.read());
    }
}

/// Link a zeroed PDPT under every kernel-half PML4 entry of the
/// registered master that does not have one yet, so all 256 entries are
/// present before any address space is created from it.
///
/// This is what makes the one-shot `copy_kernel_half` in
/// [`VmSpace::new`] correct for the lifetime of the kernel: the only
/// change a kernel-half entry can undergo is absent → present (nothing
/// unlinks one, because [`CursorMut::unmap`] deliberately does not
/// prune), and filling them all in up front removes that transition.
/// Costs 256 page-table frames, once, on the BSP.
///
/// The `&BspToken<'brand>` witnesses BSP-only init: this must run
/// before the first `VmSpace::new` and while nothing else walks the
/// master. Returns the number of entries it linked, or
/// [`MapError::Uninitialised`] if the master or the frame allocator is
/// not registered yet.
pub fn prepopulate_kernel_half<'brand>(_token: &BspToken<'brand>) -> Result<usize, MapError> {
    let alloc = current_frame_allocator().ok_or(MapError::Uninitialised)?;
    let master = KERNEL_MASTER_PML4.load(Ordering::Acquire);
    if master == KERNEL_MASTER_UNINIT {
        return Err(MapError::Uninitialised);
    }
    let master_phys = PhysAddr::new(master);

    let mut linked = 0usize;
    for i in KERNEL_HALF_START_INDEX..KERNEL_HALF_END_INDEX {
        let pte = entry_in_table(master_phys, i);
        if pte.is_present() {
            continue;
        }
        if !charge_page_table_frame() {
            return Err(MapError::IntermediateAllocFailed);
        }
        let Some(pdpt_phys) = alloc.alloc(FrameAllocOptions::single().zeroed()) else {
            refund_page_table_frame();
            return Err(MapError::IntermediateAllocFailed);
        };
        let frame = Frame::<PageTableMeta>::from_unused(
            pdpt_phys,
            PageTableMeta {
                level: PageTableLevel::Three as u8,
                static_borrowed: false,
            },
        )
        .map_err(|_| MapError::PathCorrupt)?;
        // Leak the typed handle into the entry as `step_down` would;
        // these tables, and their charge, live for the lifetime of the kernel.
        let _slot = frame.into_raw();
        pte.set(pdpt_phys, PteFlags::PRESENT | PteFlags::WRITABLE);
        linked += 1;
    }
    Ok(linked)
}

impl Cursor<'_> {
    /// Snapshot of the current entry. `paddr == None` ⇒ not present.
    /// Walks the actual leaf (4 KiB / 2 MiB / 1 GiB); the returned
    /// `level` reflects the leaf's real size.
    pub fn query(&self) -> Result<CursorEntry, MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        match walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            false,
            WalkMode::Query,
            PageTableLevel::One,
        )
        .map_err(map_walk_err)?
        {
            outcome @ WalkOutcome::LeafTable { .. } => match read_leaf(&outcome) {
                Some((paddr, property, level)) => Ok(CursorEntry {
                    vaddr: self.cur,
                    paddr: Some(paddr),
                    property,
                    level,
                }),
                None => {
                    // Empty at the leaf level: surface that level so
                    // `entry.level.entry_size()` skips correctly.
                    let leaf_level = match outcome {
                        WalkOutcome::LeafTable { leaf_level, .. } => leaf_level,
                        WalkOutcome::NotPresent { .. } => PageTableLevel::One,
                    };
                    Ok(CursorEntry {
                        vaddr: self.cur,
                        paddr: None,
                        property: PageProperty::default(),
                        level: leaf_level,
                    })
                }
            },
            WalkOutcome::NotPresent { stopped_at } => Ok(CursorEntry {
                vaddr: self.cur,
                paddr: None,
                property: PageProperty::default(),
                // The level whose entry was missing — caller advances
                // by `level.entry_size()` to skip the empty subtree.
                level: stopped_at,
            }),
        }
    }

    pub fn vaddr(&self) -> VirtAddr {
        self.cur
    }

    /// Advance the cursor by one 4 KiB page.
    pub fn next(&mut self) -> Result<(), MapError> {
        self.advance(PAGE_SIZE_4KB)
    }

    /// Advance the cursor by `bytes` (must be a positive multiple of
    /// 4 KiB). Stops at `range.end`; one past the end is allowed,
    /// further advance returns [`MapError::OutOfBounds`].
    pub fn advance(&mut self, bytes: u64) -> Result<(), MapError> {
        if bytes == 0 || bytes & (PAGE_SIZE_4KB - 1) != 0 {
            return Err(MapError::UnalignedRange);
        }
        let next = self
            .cur
            .as_u64()
            .checked_add(bytes)
            .ok_or(MapError::OutOfBounds)?;
        if next > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = VirtAddr::new(next);
        Ok(())
    }

    pub fn seek(&mut self, vaddr: VirtAddr) -> Result<(), MapError> {
        if vaddr.as_u64() & 0xFFF != 0 {
            return Err(MapError::UnalignedRange);
        }
        if vaddr.as_u64() < self.range.start.as_u64() || vaddr.as_u64() > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = vaddr;
        Ok(())
    }
}

impl<'a> CursorMut<'a> {
    pub fn vaddr(&self) -> VirtAddr {
        self.cur
    }

    /// Advance the cursor by one 4 KiB page.
    pub fn next(&mut self) -> Result<(), MapError> {
        self.advance(PAGE_SIZE_4KB)
    }

    /// Advance the cursor by `bytes` (must be a positive multiple of
    /// 4 KiB). Stops at `range.end`; one past the end is allowed,
    /// further advance returns [`MapError::OutOfBounds`].
    pub fn advance(&mut self, bytes: u64) -> Result<(), MapError> {
        if bytes == 0 || bytes & (PAGE_SIZE_4KB - 1) != 0 {
            return Err(MapError::UnalignedRange);
        }
        let next = self
            .cur
            .as_u64()
            .checked_add(bytes)
            .ok_or(MapError::OutOfBounds)?;
        if next > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = VirtAddr::new(next);
        Ok(())
    }

    pub fn seek(&mut self, vaddr: VirtAddr) -> Result<(), MapError> {
        if vaddr.as_u64() & 0xFFF != 0 {
            return Err(MapError::UnalignedRange);
        }
        if vaddr.as_u64() < self.range.start.as_u64() || vaddr.as_u64() > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        self.cur = vaddr;
        Ok(())
    }

    /// Snapshot of the current entry — same data as the read-only
    /// cursor's [`Cursor::query`].
    pub fn query(&self) -> Result<CursorEntry, MapError> {
        let probe = Cursor {
            space: self.space,
            range: self.range.clone(),
            cur: self.cur,
        };
        probe.query()
    }

    /// Map `frame` at the cursor's current vaddr with `prop`, at leaf
    /// size `S`. Consumes `frame`; on success its single reference is
    /// leaked into the leaf PTE and held by the page table. Reverse via
    /// [`Self::unmap::<S>`].
    ///
    /// On refusal `frame` comes back in the error, so the caller's page is
    /// never freed by a path that reports failure. VERIFIED:
    /// `verification/proofs/vm_space_cursor.rs`'s (REF-CONSERVE) proves the
    /// offered frame is installed xor handed back, and
    /// `broken_map_failed_drops` is the witness for the `Result<(), MapError>`
    /// shape this replaced — there `?` dropped the frame and freed a page the
    /// caller went on to free again.
    ///
    /// A refusal out of the walk is not a no-op on the tables: the Create-mode
    /// descent links missing intermediates and splits any blocking huge page
    /// before the leaf is reached, and neither is undone. Both are owned by
    /// their parent entry and reclaimed by `VmSpace::drop`, so this is
    /// retained memory, not leaked memory — but a caller that retries sees a
    /// cheaper walk and a permanently split huge page.
    ///
    /// Errors:
    /// * [`MapError::OutOfBounds`] — cursor past `range.end`.
    /// * [`MapError::UnalignedCursor`] — `cur % S::BYTES != 0`.
    /// * [`MapError::UnalignedFrame`] — `frame.paddr() % S::BYTES != 0`.
    /// * [`MapError::Overlap`] — leaf already present.
    /// * [`MapError::IntermediateAllocFailed`] — page-table frame alloc failed.
    pub fn map<S: PageSize, M: AnyUFrameMeta>(
        &mut self,
        frame: UFrame<M>,
        prop: PageProperty,
    ) -> Result<(), (UFrame<M>, MapError)> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err((frame, MapError::OutOfBounds));
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err((frame, MapError::UnalignedCursor));
        }
        // Without this, a 2 MiB map at the last 4 KiB of `range` would
        // silently extend past `range.end`.
        let Some(map_end) = self.cur.as_u64().checked_add(S::BYTES) else {
            return Err((frame, MapError::OutOfBounds));
        };
        if map_end > self.range.end.as_u64() {
            return Err((frame, MapError::OutOfBounds));
        }
        if frame.paddr().as_u64() & (S::BYTES - 1) != 0 {
            return Err((frame, MapError::UnalignedFrame));
        }

        let (leaf_table_phys, leaf_index) = match self.walk_to_leaf_for_map::<S>(prop.user) {
            Ok(leaf) => leaf,
            Err(e) => return Err((frame, e)),
        };
        self.place_leaf::<S, M>(entry_in_table(leaf_table_phys, leaf_index), frame, prop)
    }

    /// Install `frame` in the leaf entry `pte`, which covers the cursor's
    /// vaddr at size `S`.
    fn place_leaf<S: PageSize, M: AnyUFrameMeta>(
        &mut self,
        pte: Pte,
        frame: UFrame<M>,
        prop: PageProperty,
    ) -> Result<(), (UFrame<M>, MapError)> {
        if pte.is_present() {
            // VERIFIED: `verification/proofs/vm_space_cursor.rs`
            // (`broken_double_leak_violates_refcount`) proves this
            // Overlap guard load-bearing for (REF) — a second leak over
            // a present leaf strands a ref. This guard is shared by
            // `map_kernel` and `map_io`; do not remove without
            // re-proving.
            return Err((frame, MapError::Overlap));
        }

        // Leak the UFrame's ref into the PTE: the count stays at 1,
        // owned by the leaf entry. VERIFIED: the `UFrame<M>` argument
        // type is the Inv. 4 + Inv. 5 carrier —
        // `broken_map_sensitive_violates_inv45` proves accepting a raw
        // `Frame` here would let a sensitive frame land in a user PTE.
        let inner = frame.into_frame();
        let paddr = inner.paddr();
        let _slot = inner.into_raw();

        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set(paddr, flags);
        self.dirty = true;
        if prop.user {
            self.space.note_resident(S::BYTES.div_ceil(4096) as i32);
        }
        Ok(())
    }

    /// Map a kernel-owned `frame` at the cursor's current vaddr with
    /// `prop`, at leaf size `S` — the kernel-half sibling of
    /// [`Self::map`], taking a sensitive `Frame<M>` rather than an
    /// untyped `UFrame<M>`. Reverse via
    /// [`Self::unmap_kernel::<S, M>`].
    ///
    /// `M` needs only [`AnyFrameMeta`] because Inv. 4 and Inv. 5 are
    /// scoped to *user-visible* leaves: the `!prop.user` and higher-half
    /// guards below discharge the obligation at run time instead of
    /// through the `UFrame` type carrier that [`Self::map`] uses. Both
    /// are load-bearing, not defensive —
    /// `verification/proofs/vm_space_cursor.rs`'s
    /// `broken_map_kernel_user_violates_inv45` is the machine-checked
    /// statement that dropping the `!prop.user` half violates the
    /// invariant.
    ///
    /// Hands `frame` back on refusal; see [`Self::map`]'s (REF-CONSERVE) note.
    ///
    /// Errors: as [`Self::map`], plus
    /// [`MapError::NotKernelMapping`] when `prop.user` is set or the
    /// cursor sits below the canonical higher half.
    pub fn map_kernel<S: PageSize, M: AnyFrameMeta>(
        &mut self,
        frame: Frame<M>,
        prop: PageProperty,
    ) -> Result<(), (Frame<M>, MapError)> {
        if prop.user || self.cur.as_u64() < HIGHER_HALF_START {
            crate::klog_warn!(
                "vm_space::map_kernel: refused va=0x{:x} user={} -> NotKernelMapping",
                self.cur.as_u64(),
                prop.user,
            );
            return Err((frame, MapError::NotKernelMapping));
        }
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err((frame, MapError::OutOfBounds));
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err((frame, MapError::UnalignedCursor));
        }
        let Some(map_end) = self.cur.as_u64().checked_add(S::BYTES) else {
            return Err((frame, MapError::OutOfBounds));
        };
        if map_end > self.range.end.as_u64() {
            return Err((frame, MapError::OutOfBounds));
        }
        if frame.paddr().as_u64() & (S::BYTES - 1) != 0 {
            return Err((frame, MapError::UnalignedFrame));
        }

        let (leaf_table_phys, leaf_index) = match self.walk_to_leaf_for_map::<S>(false) {
            Ok(leaf) => leaf,
            Err(e) => return Err((frame, e)),
        };
        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if pte.is_present() {
            return Err((frame, MapError::Overlap));
        }

        let paddr = frame.paddr();
        let _slot = frame.into_raw();

        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set(paddr, flags);
        self.dirty = true;
        Ok(())
    }

    /// Install a leaf over physical memory that has **no** `MetaSlot` —
    /// device MMIO apertures, firmware runtime regions, anything outside
    /// the RAM range `META_SLOTS` is sized for. No frame is consumed and
    /// no reference is taken; the leaf carries
    /// [`PageProperty::SOFTWARE_NO_FRAME_REF`] so the unmap path reads
    /// that bit rather than trusting the caller to remember.
    ///
    /// Guarded to supervisor-only leaves (`!prop.user`): a device
    /// aperture reachable from ring 3 is the sensitive-memory exposure
    /// Inv. 4 and Inv. 5 exist to forbid. Unlike [`Self::map_kernel`]
    /// this does **not** additionally require the higher half — firmware
    /// runtime regions are mapped at their physical address, a
    /// supervisor-only leaf in the low half of the kernel master.
    /// Callers own the VA policy.
    ///
    /// Errors: as [`Self::map_kernel`], minus the frame-alignment arm.
    pub fn map_io<S: PageSize>(
        &mut self,
        paddr: Paddr,
        prop: PageProperty,
    ) -> Result<(), MapError> {
        if prop.user {
            crate::klog_warn!(
                "vm_space::map_io: refused user leaf va=0x{:x} -> NotKernelMapping",
                self.cur.as_u64(),
            );
            return Err(MapError::NotKernelMapping);
        }
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedCursor);
        }
        let map_end = self
            .cur
            .as_u64()
            .checked_add(S::BYTES)
            .ok_or(MapError::OutOfBounds)?;
        if map_end > self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if paddr.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedFrame);
        }

        let (leaf_table_phys, leaf_index) = self.walk_to_leaf_for_map::<S>(false)?;
        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if pte.is_present() {
            return Err(MapError::Overlap);
        }

        let prop = PageProperty {
            software: prop.software | PageProperty::SOFTWARE_NO_FRAME_REF,
            ..prop
        };
        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set(paddr, flags);
        self.dirty = true;
        Ok(())
    }

    /// Shared create-mode descent for the three map entry points.
    fn walk_to_leaf_for_map<S: PageSize>(
        &self,
        user_mapping: bool,
    ) -> Result<(Paddr, usize), MapError> {
        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            user_mapping,
            WalkMode::Create,
            S::LEVEL,
        )
        .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            // Create mode never returns NotPresent — it would have
            // allocated.
            crate::klog_warn!(
                "vm_space::map: walk(Create) returned non-leaf va=0x{:x} target_level={:?} \
                 outcome={:?} -> PathCorrupt",
                self.cur.as_u64(),
                S::LEVEL,
                outcome,
            );
            return Err(MapError::PathCorrupt);
        };
        if leaf_level != S::LEVEL {
            // Create mode splits any blocking huge page on the way
            // down, so a wrong level here means corruption.
            crate::klog_warn!(
                "vm_space::map: leaf level mismatch va=0x{:x} got={:?} want={:?} \
                 (blocking huge page not split) -> PathCorrupt",
                self.cur.as_u64(),
                leaf_level,
                S::LEVEL,
            );
            return Err(MapError::PathCorrupt);
        }
        Ok((leaf_table_phys, leaf_index))
    }

    /// Unmap the current entry at leaf size `S`. Returns the freed
    /// `UFrame` (ref count == 1) when one was present.
    ///
    /// Errors:
    /// * [`MapError::OutOfBounds`] — cursor past `range.end`.
    /// * [`MapError::UnalignedCursor`] — `cur % S::BYTES != 0`.
    /// * [`MapError::SizeMismatch`] — leaf is present at a different
    ///   size than `S` (e.g. `unmap::<Size4Kb>` on a 2 MiB leaf).
    pub fn unmap<S: PageSize, M: AnyUFrameMeta>(&mut self) -> Result<Option<UFrame<M>>, MapError> {
        Ok(self.unmap_inner::<S, M>()?.map(UFrame::<M>::from_frame))
    }

    /// Swap the leaf at the cursor's current vaddr for `frame`, returning the
    /// frame the leaf held. The COW write-fault primitive.
    ///
    /// Not `unmap` followed by `map`: between those two the caller holds *two*
    /// owned frames across a fallible step, and a refusal there drops the
    /// displaced one — which `unmap` invalidated on this CPU only, so peers
    /// still translate the page it hands to the allocator. Ordering the pair
    /// cannot fix that; only doing the fallible walk before either frame moves
    /// can, which is what this does.
    ///
    /// The returned frame carries the same obligation as [`Self::unmap`]'s:
    /// hold it until after a cross-CPU shootdown of `va`.
    pub fn replace<S: PageSize, M: AnyUFrameMeta>(
        &mut self,
        frame: UFrame<M>,
        prop: PageProperty,
    ) -> Result<Option<UFrame<M>>, (UFrame<M>, MapError)> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err((frame, MapError::OutOfBounds));
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err((frame, MapError::UnalignedCursor));
        }
        let Some(map_end) = self.cur.as_u64().checked_add(S::BYTES) else {
            return Err((frame, MapError::OutOfBounds));
        };
        if map_end > self.range.end.as_u64() {
            return Err((frame, MapError::OutOfBounds));
        }
        if frame.paddr().as_u64() & (S::BYTES - 1) != 0 {
            return Err((frame, MapError::UnalignedFrame));
        }

        // Everything fallible happens here, before either frame moves. What
        // follows is a `Frame::from_raw_at` over a paddr this leaf owns, then
        // one PTE store.
        let (leaf_table_phys, leaf_index) = match self.walk_to_leaf_for_map::<S>(prop.user) {
            Ok(leaf) => leaf,
            Err(e) => return Err((frame, e)),
        };
        let pte = entry_in_table(leaf_table_phys, leaf_index);

        let displaced_paddr = pte.is_present().then(|| pte.address());
        let displaced = if let Some(paddr) = displaced_paddr {
            if pte.is_huge() != S::HUGE_BIT {
                return Err((frame, MapError::SizeMismatch));
            }
            let owns_no_ref = PageProperty::from_leaf_flags(pte.flags()).software
                & PageProperty::SOFTWARE_NO_FRAME_REF
                != 0;
            if owns_no_ref {
                // A `map_io` leaf: reclaiming it would fabricate a handle over
                // a paddr with no `MetaSlot`. Refuse rather than guess.
                return Err((frame, MapError::NotKernelMapping));
            }
            // SAFETY: the leaf is present and does not carry
            // SOFTWARE_NO_FRAME_REF, so a `map` leaked exactly one ref into it
            // via `Frame::into_raw`; the PTE overwrite below is the only path
            // that held it, and `from_raw_at` re-wraps without bumping.
            match unsafe { Frame::<M>::from_raw_at(paddr) } {
                Ok(f) => Some(UFrame::<M>::from_frame(f)),
                Err(_) => return Err((frame, MapError::PathCorrupt)),
            }
        } else {
            None
        };

        if prop.user && displaced_paddr.is_none() {
            self.space.note_resident(S::BYTES.div_ceil(4096) as i32);
        }

        let inner = frame.into_frame();
        let paddr = inner.paddr();
        let _slot = inner.into_raw();

        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set(paddr, flags);
        flush_leaf_local::<S>(self.cur);
        self.dirty = true;

        // The displaced paddr, captured before the overwrite above.
        if let (Some(old), Some(hook)) = (displaced_paddr, current_cursor_unmap_hook()) {
            hook.after_unmap(
                self.cur,
                old,
                self.space.mm_ctx_handle.load(Ordering::Acquire),
            );
        }
        Ok(displaced)
    }

    /// Kernel-half sibling of [`Self::unmap`], yielding the sensitive
    /// `Frame<M>` that [`Self::map_kernel`] leaked into the leaf.
    /// Dropping it returns the page to the registered allocator.
    ///
    /// Returns `Ok(None)` for a leaf [`Self::map_io`] installed — that
    /// entry owns no reference.
    pub fn unmap_kernel<S: PageSize, M: AnyFrameMeta>(
        &mut self,
    ) -> Result<Option<Frame<M>>, MapError> {
        self.unmap_inner::<S, M>()
    }

    fn unmap_inner<S: PageSize, M: AnyFrameMeta>(&mut self) -> Result<Option<Frame<M>>, MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedCursor);
        }

        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            false,
            WalkMode::Mutate,
            S::LEVEL,
        )
        .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            return Ok(None);
        };
        if leaf_level != S::LEVEL {
            return Err(MapError::SizeMismatch);
        }

        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if !pte.is_present() {
            return Ok(None);
        }
        self.take_leaf::<S, M>(pte, None)
    }

    /// Clear the present leaf `pte`, which maps the cursor's vaddr at size
    /// `S`, and reclaim the reference `map` leaked into it. `Ok(None)` for a
    /// `map_io` leaf, which holds none.
    fn take_leaf<S: PageSize, M: AnyFrameMeta>(
        &mut self,
        pte: Pte,
        range: Option<&mut RangeInvalidation>,
    ) -> Result<Option<Frame<M>>, MapError> {
        // A size mismatch would already have failed the caller's
        // `S::LEVEL` check.
        debug_assert_eq!(pte.is_huge(), S::HUGE_BIT);

        let paddr = pte.address();
        let was_user = pte.flags().contains(PteFlags::USER);
        let owns_no_ref = PageProperty::from_leaf_flags(pte.flags()).software
            & PageProperty::SOFTWARE_NO_FRAME_REF
            != 0;
        pte.clear();
        // Local invalidation only; cross-CPU shootdown is the
        // consumer's responsibility.
        match range {
            Some(range) => range.invalidate(self.cur),
            None => flush_leaf_local::<S>(self.cur),
        }
        self.dirty = true;

        if owns_no_ref {
            // A `map_io` leaf: its physical range has no `MetaSlot`, so
            // `from_raw_at` below would either fail `OutOfRange` or —
            // on a machine whose RAM reaches past the aperture —
            // succeed against a slot naming unrelated memory and hand a
            // device window to the frame allocator.
            return Ok(None);
        }

        if was_user {
            self.space.note_resident(-(S::BYTES.div_ceil(4096) as i32));
            if let Some(hook) = current_cursor_unmap_hook() {
                hook.after_unmap(
                    self.cur,
                    paddr,
                    self.space.mm_ctx_handle.load(Ordering::Acquire),
                );
            }
        }

        // VERIFIED: `verification/proofs/vm_space_cursor.rs` (REF)
        // proves `unmap` of a present leaf reclaims exactly one ref and
        // that the not-present guard in every caller prevents a double-free.
        // SAFETY: at `map` time we leaked exactly one ref to this slot
        // via `Frame::into_raw`; clearing the PTE above removes the only
        // path that held it, and `from_raw_at` re-wraps without bumping
        // the count.
        let frame: Frame<M> =
            unsafe { Frame::<M>::from_raw_at(paddr).map_err(|_| MapError::PathCorrupt)? };
        Ok(Some(frame))
    }

    /// Update the access/cache properties of the current entry at
    /// leaf size `S` without remapping. No-op when no mapping is
    /// present (returns `Ok(())`).
    ///
    /// Errors:
    /// * [`MapError::OutOfBounds`] — cursor past `range.end`.
    /// * [`MapError::UnalignedCursor`] — `cur % S::BYTES != 0`.
    /// * [`MapError::SizeMismatch`] — present leaf is a different size than `S`.
    pub fn protect<S: PageSize>(&mut self, prop: PageProperty) -> Result<(), MapError> {
        if self.cur.as_u64() >= self.range.end.as_u64() {
            return Err(MapError::OutOfBounds);
        }
        if self.cur.as_u64() & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedCursor);
        }
        let outcome = walk_to_leaf(
            self.space.pml4_paddr(),
            self.cur,
            prop.user,
            WalkMode::Mutate,
            S::LEVEL,
        )
        .map_err(map_walk_err)?;
        let WalkOutcome::LeafTable {
            leaf_table_phys,
            leaf_index,
            leaf_level,
        } = outcome
        else {
            return Ok(());
        };
        let pte = entry_in_table(leaf_table_phys, leaf_index);
        if !pte.is_present() {
            return Ok(());
        }
        if leaf_level != S::LEVEL {
            return Err(MapError::SizeMismatch);
        }
        self.rewrite_leaf::<S>(pte, prop, None);
        Ok(())
    }

    /// Rewrite the present leaf `pte`, which maps the cursor's vaddr at size
    /// `S`, to `prop`, keeping its frame.
    fn rewrite_leaf<S: PageSize>(
        &mut self,
        pte: Pte,
        prop: PageProperty,
        range: Option<&mut RangeInvalidation>,
    ) {
        let mut flags = prop.to_leaf_flags();
        if !flags.contains(PteFlags::PRESENT) {
            flags |= PteFlags::PRESENT;
        }
        if S::HUGE_BIT {
            flags |= PteFlags::HUGE;
        }
        pte.set_flags_only(flags);
        match range {
            Some(range) => range.invalidate(self.cur),
            None => flush_leaf_local::<S>(self.cur),
        }
        self.dirty = true;
    }
}

/// Local-CPU invalidation for a leaf at size `S`: one INVLPG per 4 KiB
/// page in the leaf.
#[inline]
fn flush_leaf_local<S: PageSize>(start: VirtAddr) {
    let mut offset: u64 = 0;
    while offset < S::BYTES {
        tlb::flush_local(VirtAddr::new(start.as_u64() + offset));
        offset += PAGE_SIZE_4KB;
    }
}

/// This CPU's invalidation across a range: an `invlpg` per leaf until that
/// costs more than invalidating the whole context once, which [`Self::finish`]
/// then does. Past that point a leaf's clear is only fenced, which is all the
/// unmap hook needs of the invalidation: the clear ordered before its reads.
struct RangeInvalidation {
    leaves: usize,
    hook: Option<&'static dyn CursorUnmapHook>,
}

impl RangeInvalidation {
    /// Where one `invlpg` per leaf starts to cost more than a context flush.
    const LEAF_LIMIT: usize = 32;

    fn new() -> Self {
        Self {
            leaves: 0,
            hook: current_cursor_unmap_hook(),
        }
    }

    fn invalidate(&mut self, vaddr: VirtAddr) {
        self.leaves += 1;
        if self.leaves <= Self::LEAF_LIMIT || self.hook.is_none() {
            tlb::flush_local(vaddr);
        } else {
            core::sync::atomic::fence(Ordering::SeqCst);
        }
    }

    fn finish(self) {
        if self.leaves > Self::LEAF_LIMIT
            && let Some(hook) = self.hook
        {
            hook.flush_local();
        }
    }
}

impl CursorMut<'_> {
    /// Map a sequence of consecutive `S`-sized leaves starting at the
    /// cursor's current vaddr, advancing the cursor by `S::BYTES` after
    /// each successful map. Stops on the first error and returns the
    /// number of mappings that were installed (the cursor is left at
    /// the position immediately after the last installed leaf).
    ///
    /// Requires the cursor's `range` to extend at least
    /// `frames.len() * S::BYTES` bytes from the starting `cur`.
    ///
    /// The refused frame comes back in the error beside the count, so a caller
    /// that owns its frames can free exactly the one that did not land. The
    /// iterator's *untouched* remainder is the caller's: it still holds the
    /// `I`, and every frame this consumed is either mapped (counted) or
    /// returned here.
    pub fn map_range<S, M, I>(
        &mut self,
        frames: I,
        prop: PageProperty,
    ) -> Result<usize, (Option<UFrame<M>>, usize, MapError)>
    where
        S: PageSize,
        M: AnyUFrameMeta,
        I: IntoIterator<Item = UFrame<M>>,
    {
        let mut count = 0usize;
        for frame in frames {
            if let Err((frame, e)) = self.map::<S, M>(frame, prop) {
                return Err((Some(frame), count, e));
            }
            if let Err(e) = self.advance(S::BYTES) {
                return Err((None, count + 1, e));
            }
            count += 1;
        }
        Ok(count)
    }

    /// Apply [`Self::protect::<S>`] across `len_bytes` of leaves
    /// starting at the cursor's current vaddr. `len_bytes` must be a
    /// positive multiple of `S::BYTES`. Skips not-present entries
    /// (consistent with the per-entry `protect`); returns the first
    /// error encountered.
    pub fn protect_range<S>(&mut self, len_bytes: u64, prop: PageProperty) -> Result<(), MapError>
    where
        S: PageSize,
    {
        if len_bytes == 0 || len_bytes & (S::BYTES - 1) != 0 {
            return Err(MapError::UnalignedRange);
        }
        let mut remaining = len_bytes;
        while remaining > 0 {
            self.protect::<S>(prop)?;
            self.advance(S::BYTES)?;
            remaining -= S::BYTES;
        }
        Ok(())
    }

    /// Unmap every present 4 KiB leaf from the cursor to `range.end`,
    /// descending once per leaf table, and hand each frame to `each`, with the
    /// property its leaf carried, in the state [`Self::unmap`] returns it in:
    /// gone from this CPU's TLB only. Huge leaves stay mapped. On error the
    /// cursor rests on the leaf that failed, and every leaf before it is gone.
    pub fn unmap_present<M: AnyUFrameMeta>(
        &mut self,
        mut each: impl FnMut(VirtAddr, PageProperty, UFrame<M>),
    ) -> Result<(), MapError> {
        let mut local = RangeInvalidation::new();
        let unmapped = self.unmap_present_into(&mut local, &mut each);
        local.finish();
        unmapped
    }

    fn unmap_present_into<M: AnyUFrameMeta>(
        &mut self,
        local: &mut RangeInvalidation,
        each: &mut impl FnMut(VirtAddr, PageProperty, UFrame<M>),
    ) -> Result<(), MapError> {
        while let Some((table, first, last)) = self.next_leaf_table()? {
            let base = self.cur.as_u64() - first as u64 * PAGE_SIZE_4KB;
            for index in first..last {
                let pte = entry_in_table(table, index);
                if !pte.is_present() {
                    continue;
                }
                self.cur = VirtAddr::new(base + index as u64 * PAGE_SIZE_4KB);
                let property = PageProperty::from_leaf_flags(pte.flags());
                if let Some(frame) = self.take_leaf::<Size4Kb, M>(pte, Some(local))? {
                    each(self.cur, property, UFrame::from_frame(frame));
                }
            }
            self.cur = VirtAddr::new(base + last as u64 * PAGE_SIZE_4KB);
        }
        Ok(())
    }

    /// Offer every present 4 KiB leaf from the cursor to `range.end` to `f`,
    /// descending once per leaf table; `Continue(Some(prop))` rewrites the
    /// leaf as [`Self::protect`] would. Huge leaves are not offered.
    /// `Ok(false)` when `f` broke off, with the cursor on the leaf it refused.
    pub fn update_present(
        &mut self,
        mut f: impl FnMut(CursorEntry) -> ControlFlow<(), Option<PageProperty>>,
    ) -> Result<bool, MapError> {
        let mut local = RangeInvalidation::new();
        let completed = self.update_present_into(&mut local, &mut f);
        local.finish();
        completed
    }

    fn update_present_into(
        &mut self,
        local: &mut RangeInvalidation,
        f: &mut impl FnMut(CursorEntry) -> ControlFlow<(), Option<PageProperty>>,
    ) -> Result<bool, MapError> {
        while let Some((table, first, last)) = self.next_leaf_table()? {
            let base = self.cur.as_u64() - first as u64 * PAGE_SIZE_4KB;
            for index in first..last {
                let pte = entry_in_table(table, index);
                if !pte.is_present() {
                    continue;
                }
                self.cur = VirtAddr::new(base + index as u64 * PAGE_SIZE_4KB);
                let entry = CursorEntry {
                    vaddr: self.cur,
                    paddr: Some(pte.address()),
                    property: PageProperty::from_leaf_flags(pte.flags()),
                    level: PageTableLevel::One,
                };
                match f(entry) {
                    ControlFlow::Break(()) => return Ok(false),
                    ControlFlow::Continue(Some(prop)) => {
                        self.rewrite_leaf::<Size4Kb>(pte, prop, Some(local))
                    }
                    ControlFlow::Continue(None) => {}
                }
            }
            self.cur = VirtAddr::new(base + last as u64 * PAGE_SIZE_4KB);
        }
        Ok(true)
    }

    /// Map each `(vaddr, frame, prop)` as [`Self::map::<Size4Kb>`] would at
    /// `vaddr`, descending once per leaf table while consecutive vaddrs share
    /// one. Returns how many were mapped. A refusal hands back the frame and
    /// the vaddr it was refused at; an iterator passed by `&mut` keeps the
    /// items not reached.
    pub fn map_each<M, I>(&mut self, items: I) -> Result<usize, (UFrame<M>, VirtAddr, MapError)>
    where
        M: AnyUFrameMeta,
        I: IntoIterator<Item = (VirtAddr, UFrame<M>, PageProperty)>,
    {
        // The 2 MiB block last descended into, its leaf table, and whether
        // that descent made the path user-reachable.
        let mut last: Option<(u64, Paddr, bool)> = None;
        let mut count = 0usize;
        for (vaddr, frame, prop) in items {
            if let Err(e) = self.seek(vaddr) {
                return Err((frame, vaddr, e));
            }
            if vaddr.as_u64() >= self.range.end.as_u64() {
                return Err((frame, vaddr, MapError::OutOfBounds));
            }
            let block = vaddr.as_u64() & PageTableLevel::Two.align_mask();
            let table = match last {
                Some((b, table, user)) if b == block && (user || !prop.user) => table,
                _ => match self.walk_to_leaf_for_map::<Size4Kb>(prop.user) {
                    Ok((table, _)) => {
                        last = Some((block, table, prop.user));
                        table
                    }
                    Err(e) => return Err((frame, vaddr, e)),
                },
            };
            let pte = entry_in_table(table, PageTableLevel::One.index_of(vaddr));
            self.place_leaf::<Size4Kb, M>(pte, frame, prop)
                .map_err(|(frame, e)| (frame, vaddr, e))?;
            count += 1;
        }
        Ok(count)
    }

    /// Descend from the cursor to the next 4 KiB leaf table under the range,
    /// stepping over absent subtrees and huge leaves whole. Returns the table
    /// and the index span `[first, last)` of it the range covers, with the
    /// cursor on `first`; `None`, with the cursor at `range.end`, once there is
    /// none.
    fn next_leaf_table(&mut self) -> Result<Option<(Paddr, usize, usize)>, MapError> {
        let end = self.range.end.as_u64();
        while self.cur.as_u64() < end {
            let skip = match walk_to_leaf(
                self.space.pml4_paddr(),
                self.cur,
                false,
                WalkMode::Mutate,
                PageTableLevel::One,
            )
            .map_err(map_walk_err)?
            {
                WalkOutcome::LeafTable {
                    leaf_table_phys,
                    leaf_index,
                    leaf_level: PageTableLevel::One,
                } => {
                    let span = ((PAGE_TABLE_ENTRIES - leaf_index) as u64 * PAGE_SIZE_4KB)
                        .min(end - self.cur.as_u64());
                    let last = leaf_index + (span / PAGE_SIZE_4KB) as usize;
                    return Ok(Some((leaf_table_phys, leaf_index, last)));
                }
                WalkOutcome::LeafTable { leaf_level, .. } => leaf_level,
                WalkOutcome::NotPresent { stopped_at } => stopped_at,
            };
            self.cur = match (self.cur.as_u64() & skip.align_mask()).checked_add(skip.entry_size())
            {
                Some(next) if next < end => VirtAddr::new(next),
                _ => self.range.end,
            };
        }
        Ok(None)
    }
}

impl Drop for CursorMut<'_> {
    fn drop(&mut self) {
        if self.dirty {
            self.space.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn map_walk_err(e: crate::mm::page_table::WalkError) -> MapError {
    use crate::mm::page_table::WalkError;
    match e {
        WalkError::AllocUninitialised => MapError::Uninitialised,
        WalkError::AllocFailed => MapError::IntermediateAllocFailed,
        WalkError::PathCorrupt => MapError::PathCorrupt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_error_eq() {
        assert_eq!(MapError::Overlap, MapError::Overlap);
        assert_ne!(MapError::Overlap, MapError::OutOfBounds);
    }

    #[test]
    fn vm_space_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<VmSpace>();
        assert_sync::<VmSpace>();
    }

    #[test]
    fn check_range_alignment_rejects_unaligned() {
        let r = VirtAddr::new(0x1000)..VirtAddr::new(0x2001);
        assert_eq!(check_range_alignment(&r), Err(MapError::UnalignedRange));
    }

    #[test]
    fn check_range_alignment_rejects_inverted() {
        let r = VirtAddr::new(0x2000)..VirtAddr::new(0x1000);
        assert_eq!(check_range_alignment(&r), Err(MapError::UnalignedRange));
    }

    #[test]
    fn check_range_alignment_accepts_empty_aligned() {
        let r = VirtAddr::new(0x1000)..VirtAddr::new(0x1000);
        assert!(check_range_alignment(&r).is_ok());
    }
}

// A VmSpace drops only when its `KArc` refcount hits zero, so no CPU is
// using it: the user-half walk needs no per-page TLB flush, and
// consumers issue a single up-front `tlb_shootdown(All)` before the last
// `KArc` drops. The kernel half (256..512) is skipped — those entries
// point into the shared kernel master, which `KERNEL_VM_SPACE` owns and
// never drops. Recursion depth is fixed at 3 (PML4 → PDPT → PD → PT),
// well under the 2 KiB stack threshold.

impl Drop for VmSpace {
    fn drop(&mut self) {
        drop_user_half_tree(self.pml4.paddr());
        if self.pml4_charged {
            refund_page_table_frame();
        }
    }
}

fn drop_user_half_tree(pml4_phys: Paddr) {
    for i in 0..KERNEL_HALF_START_INDEX {
        let pte = entry_in_table(pml4_phys, i);
        if !pte.is_present() {
            continue;
        }
        debug_assert!(
            !pte.is_huge(),
            "PML4 huge entry at index {i} — architecturally invalid",
        );
        let child = pte.address();
        // SAFETY: every present, non-huge PML4 entry holds exactly one
        // `Frame<PageTableMeta>` ref leaked by `step_down` in
        // `WalkMode::Create`, and the VmSpace's refcount is zero (we are
        // inside its Drop), so no CPU is walking this tree concurrently.
        recursively_reclaim_subtree(child, PageTableLevel::Three);
    }
}

fn recursively_reclaim_subtree(table_phys: Paddr, level: PageTableLevel) {
    use crate::mm::page_table::PAGE_TABLE_ENTRIES;
    for i in 0..PAGE_TABLE_ENTRIES {
        let pte = entry_in_table(table_phys, i);
        if !pte.is_present() {
            continue;
        }
        let child = pte.address();
        if pte.is_huge() {
            // `Frame<M>` carries no per-page size, so `dealloc` sees
            // `size_pages = 1`; the trailing pages of a huge region come
            // back only because the consumer's `FrameAlloc` tracks the
            // allocation size itself.
            // SAFETY: see `drop_user_half_tree`.
            unsafe { reclaim_leaked_frame(child) };
        } else if level == PageTableLevel::One {
            // SAFETY: see `drop_user_half_tree`.
            unsafe { reclaim_leaked_frame(child) };
        } else {
            let next_level = level
                .next_lower()
                .expect("recursively_reclaim_subtree called with leaf level");
            recursively_reclaim_subtree(child, next_level);
        }
    }
    // SAFETY: every intermediate page-table frame was leaked into its
    // parent PTE by `step_down`, and no CPU is walking the tree
    // (VmSpace refcount is zero).
    unsafe { reclaim_leaked_frame(table_phys) };
    refund_page_table_frame();
}
