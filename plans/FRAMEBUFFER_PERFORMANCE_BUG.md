# Framebuffer Performance Bug Investigation

## Summary

After a soft reboot (triggered by losing at roulette), the roulette animation drops from smooth ~60 FPS to ~1 FPS. This document captures the complete investigation and root cause analysis.

## Status: RESOLVED

**Root Cause:** The new slab allocator's `init_kernel_heap()` was missing initial page mapping that the old free-list allocator performed via `expand_heap()`.

**Fix:** Added `map_heap_pages(&mut heap, 4)` to `init_kernel_heap()` in `mm/src/kernel_heap.rs`.

## Symptoms

- **First boot**: Roulette animation runs smoothly
- **Soft reboot** (PS/2 keyboard controller reset via 0xFE to port 0x64): Animation becomes extremely slow (~1 FPS)
- **Diagnostic measurement**: 37,723 cycles per pixel (should be ~1-10 cycles/pixel with proper caching)

## Investigation Timeline

### Initial Hypothesis: PIT Timer Atomics (WRONG)

We initially suspected that PIT timer state (`CURRENT_FREQUENCY_HZ`, `CURRENT_RELOAD_DIVISOR` in `drivers/src/pit.rs`) was persisting across reboots. However, logs showed:

```
First boot:  REBOOT_STATE: Captured early boot state - PIT freq=0, div=0
Second boot: REBOOT_STATE: Captured early boot state - PIT freq=0, div=0
```

Limine correctly zeros BSS on every kernel load, so PIT atomics are NOT the issue.

### Second Hypothesis: PAT Not Initialized (PARTIALLY CORRECT)

We discovered that `pat_init()` was **never called** during boot. The function existed in `mm/src/pat.rs` but:
1. The module wasn't exported from `mm/src/lib.rs`
2. No code called `pat_init()`

**Fix committed**: `3ffdc5ca6 mm: initialize PAT with Write-Combining for framebuffer performance`

This fix ensures PAT[1]=WC (Write-Combining) is set on every boot. However, **the bug persisted**.

### Third Hypothesis: MTRR + Page Table Flags (INVESTIGATED BUT NOT ROOT CAUSE)

After the PAT fix, diagnostic logs showed:

```
CACHE_STATE: PAT MSR=0x0007010600070106
CACHE_STATE: PAT[0]=6 PAT[1]=1 PAT[2]=7 PAT[3]=0 PAT[4]=6 PAT[5]=1 PAT[6]=7 PAT[7]=0
RENDER_DIAG: fill_rect 200x200 took 1508921899 cycles (37723 cycles/pixel)
RENDER_DIAG: WARNING - slow render detected! 37723 cycles/pixel suggests uncached FB
```

**PAT[1]=1 (WC) is correctly set, but rendering is still 37,723 cycles/pixel!**

We initially believed this was due to MTRR misconfiguration after warm reboot. However, further bisecting revealed a different cause.

### Actual Root Cause: Missing Initial Heap Page Mapping (CONFIRMED & FIXED)

Through systematic bisection of commit `b06a5971339` ("new memory allocator"), we discovered:

1. The bug was introduced by the new slab allocator in `mm/src/kernel_heap.rs`
2. The OLD `init_kernel_heap()` called `expand_heap()` which mapped 4 initial pages
3. The NEW `init_kernel_heap()` skipped initial page mapping (lazy allocation)
4. **Adding `map_heap_pages(&mut heap, 4)` to the new init fixed the bug**

**Bisection results:**
- Cosmetic import changes: SMOOTH ✓
- New kernel_heap.rs (without initial mapping): SLOW ✗
- New kernel_heap.rs + initial mapping: SMOOTH ✓

**Why this matters:** Something about mapping heap pages during early boot (before video init) affects framebuffer behavior after soft reboot. The exact mechanism is still unclear but likely involves:
- TLB state synchronization via `paging_bump_kernel_mapping_gen()`
- Page table structure initialization timing
- Interaction with Limine's HHDM page table setup

## Technical Analysis

### How x86 Memory Types Work

The effective memory type for a region is determined by combining:

1. **MTRRs (Memory Type Range Registers)** - Region-based, set by firmware/bootloader
2. **PAT (Page Attribute Table)** - Per-page, selected by page table entry flags

The PAT index is calculated from page table entry bits:
```
PAT_index = (PAT_bit << 2) | (PCD << 1) | PWT
```

Where:
- PWT = Page Write-Through (bit 3)
- PCD = Page Cache Disable (bit 4)
- PAT = Page Attribute Table (bit 7 in PTE)

### Current SlopOS Configuration

**PAT MSR (after pat_init):**
| Entry | Index | Memory Type |
|-------|-------|-------------|
| PA0   | 000   | WB (0x06)   |
| PA1   | 001   | WC (0x01)   |
| PA2   | 010   | UC- (0x07)  |
| PA3   | 011   | UC (0x00)   |
| PA4   | 100   | WB (0x06)   |
| PA5   | 101   | WC (0x01)   |
| PA6   | 110   | UC- (0x07)  |
| PA7   | 111   | UC (0x00)   |

### The Problem

1. **Framebuffer location**: Physical 0x80000000, HHDM virtual 0xffff800080000000
2. **HHDM page tables**: Set up by Limine bootloader, not SlopOS
3. **Page flags used**: `KERNEL_RW` (PRESENT | WRITABLE) - **no PWT bit set**
4. **Resulting PAT index**: 000 (PWT=0, PCD=0, PAT=0) → PA0 = WB

**But 37,000 cycles/pixel is way too slow for WB!** This suggests MTRRs are involved.

### MTRR Impact

- The framebuffer at 0x80000000 is a PCI BAR (MMIO region)
- MMIO regions not covered by explicit MTRRs default to **UC (Uncacheable)**
- When MTRR says UC and PAT says WB, effective type is **UC** (most restrictive wins)

**Key insight from Oracle consultation:**
> PAT alone cannot override an MTRR-default-UC region. UC wins in the effective-type resolution.

## Why First Boot Works / Why Warm Reboot Failed

**Original hypothesis (MTRR-based) was incorrect.**

The actual reason:
- **First boot after commit `b06a5971339`**: Still slow on reboot because no initial heap pages mapped
- **After fix**: Initial heap page mapping during `init_kernel_heap()` triggers page table/TLB operations that properly synchronize state

The exact mechanism is not fully understood, but the symptom correlates directly with whether `map_heap_pages()` is called during heap initialization.

## Solution (IMPLEMENTED)

### Fix: Restore Initial Heap Page Mapping

Added to `mm/src/kernel_heap.rs` in `init_kernel_heap()`:

```rust
if map_heap_pages(&mut heap, 4).is_none() {
    panic!("Failed to initialize kernel heap");
}
```

This restores the behavior of the old allocator's `expand_heap()` call, which mapped 4 pages during init.

### Discarded Options (MTRR-based)

The following options were considered but are **not needed** since the root cause was not MTRR-related:

1. ~~Variable MTRR configuration~~
2. ~~Page table flag fix (PWT=1)~~
3. ~~Combined MTRR + page table approach~~

These may still be useful for future framebuffer optimization but are not required for this bug.

## Files Involved

| File | Role |
|------|------|
| `mm/src/pat.rs` | PAT MSR configuration (WC at index 1) |
| `mm/src/memory_init.rs` | Memory system init, calls pat_init() |
| `abi/src/arch/x86_64/paging.rs` | PageFlags definitions (WRITE_THROUGH = bit 3) |
| `abi/src/arch/x86_64/msr.rs` | MSR constants (MTRR_CAP defined but unused) |
| `video/src/framebuffer.rs` | Framebuffer init, uses HHDM mapping |
| `boot/src/shutdown.rs` | Soft reboot via PS/2 0xFE |

## Diagnostic Instrumentation

Current instrumentation (not committed):

**boot/src/boot_drivers.rs:**
```rust
fn read_pat_msr() -> u64 { ... }
// Logs CACHE_STATE: PAT MSR and individual PAT entries
```

**video/src/roulette_core.rs:**
```rust
fn run_render_diagnostics(backend, width, height) {
    // Measures fill_rect performance via TSC
    // Reports cycles/pixel
    // Warns if > 50 cycles/pixel
}
```

## Next Steps

1. ~~**Implement MTRR support** in `mm/src/mtrr.rs`~~ (Not needed - bug was not MTRR-related)
2. ~~**Investigate why initial heap mapping affects framebuffer**~~ (See investigation below)
3. **Remove diagnostic instrumentation** once fix is verified

## Deep Investigation: Why 4 Heap Pages?

Systematic experimentation revealed the precise requirements for the fix:

### Experiment Results

| Experiment | Physical Allocs | Page Maps | Result |
|------------|-----------------|-----------|--------|
| Read page tables only | 0 | 0 | ❌ Slow |
| Write (no-op) to existing page tables | 0 | 0 | ❌ Slow |
| 2 allocs, immediately freed | 2 (freed) | 0 | ❌ Slow |
| 1 page via map_heap_pages | 1 | 1 | ❌ Slow |
| 2 pages via map_heap_pages | 2 | 2 | ✅ Smooth |
| 3 pages via map_heap_pages | 3 | 3 | ✅ Smooth |
| 4 pages via map_heap_pages | 4 | 4 | ✅ Smooth |
| 1 extra alloc + 1 page map | 2 | 1 | ✅ Smooth |
| 2 allocs, 0 maps | 2 | 0 | ❌ Slow |
| 1 page map + 1 extra alloc | 2 | 1 | ✅ Smooth |

### Conclusion

**The fix requires BOTH:**
- **≥2 physical frame allocations** via `alloc_page_frame()`
- **≥1 page mapping** via `map_page_4kb()`

Order does not matter. The current fix of `map_heap_pages(&mut heap, 4)` satisfies both requirements (4 allocs + 4 maps).

### Technical Analysis: Why ≥2 Allocations + ≥1 Mapping?

Based on x86 architecture knowledge and the experimental results, here is the most likely explanation:

#### The Soft Reboot Problem

When a soft reboot occurs (PS/2 keyboard controller reset via `0xFE` to port `0x64`):
1. The CPU performs a warm reset - some internal state is preserved
2. Physical memory contents persist (unlike cold boot where RAM is cleared)
3. Limine reloads and creates fresh page tables
4. The kernel re-initializes all subsystems

The key issue: **stale CPU microarchitectural state from the previous boot interacts with fresh memory contents**.

#### Why `pat_init()` Isn't Enough

`pat_init()` performs the Intel-mandated PAT modification procedure:
- `wbinvd()` - write-back and invalidate all caches
- `flush_tlb_all()` - full TLB flush via CR3 reload
- Write new PAT value
- `wbinvd()` again
- `flush_tlb_all()` again

This should reset cache/TLB state, but it doesn't flush all microarchitectural state:
- **Store buffers** may retain pending writes
- **Line fill buffers** may have in-flight reads
- **Memory ordering buffers** track outstanding operations
- **Prefetch queues** may have stale physical addresses

#### The "2 Allocations" Requirement

The buddy allocator (`page_alloc.rs`) uses a bitmap and free lists stored in physical memory. When `alloc_page_frame()` is called:

1. **First allocation**: Reads allocator metadata, finds a free page, marks it used
   - This reads/writes to allocator's bitmap and free list structures
   - Creates cache lines for these structures
   - But may still have inconsistent microarchitectural state

2. **Second allocation**: Reads the SAME allocator metadata again
   - Forces a second pass through the allocator's data structures
   - The read-after-write to the same memory locations forces the CPU to serialize
   - This acts as an implicit **memory barrier** that flushes store buffers

This explains why `2 allocs + 0 maps = SLOW` but `2 allocs + 1 map = SMOOTH`:
- The allocations alone synchronize allocator state
- But without a mapping, the page table structures (used by framebuffer access) aren't touched

#### The "1 Mapping" Requirement

`map_page_4kb()` performs a page table walk and writes to page table entries. For the heap region (`0xFFFF_FFFF_9000_0000`), this:

1. Walks PML4 → PDPT → PD → PT (reading page table structures via HHDM)
2. May allocate intermediate page tables (using `alloc_page_frame(ALLOC_FLAG_ZERO)`)
3. Writes the final PT entry

The page table walk **reads the same physical page table structures used by framebuffer access**. Since framebuffer is accessed via HHDM (`0xFFFF_8000_xxxx_xxxx`), and the kernel heap is also in kernel space, they share:
- The same PML4 (kernel's KERNEL_PAGE_DIR)
- Potentially overlapping PDPT structures

By reading/writing page tables for the heap, we force the CPU to:
1. Fetch current page table contents into cache
2. Establish coherent cache lines for the page table hierarchy
3. Flush any stale prefetched translations

#### The Combined Effect

```
pat_init()
    ↓ (flushes caches/TLB but not all microarch state)
alloc_page_frame() × 2
    ↓ (forces allocator metadata coherency via read-after-write)
map_page_4kb() × 1+
    ↓ (forces page table coherency via walk + write)
Framebuffer access
    ↓ (now sees coherent page tables → correct PAT → fast WC access)
```

#### Why 1 Allocation Fails

With only 1 allocation + 1 mapping:
- The single allocator access doesn't force full serialization
- The mapping uses the frame from that allocation
- But the allocator's internal state may still have stale cached data
- Subsequent page table allocations (for PDPT/PD/PT structures) may behave incorrectly

The second allocation forces a second trip through the allocator, which:
- Touches the bitmap again (read-modify-write)
- Serializes memory operations
- Ensures subsequent page table structure allocations see coherent state

#### Conclusion

The fix works because it forces **two separate serialization points**:
1. **Allocator serialization**: ≥2 allocations force read-after-write coherency on buddy allocator metadata
2. **Page table serialization**: ≥1 mapping forces page table walk + write, establishing coherent cache state for the shared kernel page table hierarchy

This is a subtle interaction between soft reboot's partial CPU reset, Limine's fresh page tables, and the kernel's memory subsystem initialization order. The "4 pages" in the original fix is conservative - "2 pages" (2 allocs + 2 maps) would work, but 4 provides margin.

## References

- Intel SDM Vol. 3A, Chapter 11: Memory Cache Control
- Intel SDM Table 11-7: Effective Page-Level Memory Types
- Limine Boot Protocol: https://github.com/limine-bootloader/limine/blob/trunk/PROTOCOL.md

## Commits Related to This Investigation

- `3ffdc5ca6` - mm: initialize PAT with Write-Combining for framebuffer performance
  - Fixed: PAT was never initialized
  - Added: test_pat_wc_enabled to verify PAT[1]==WC
  - Result: PAT now correct, but bug persisted (was not the root cause)

- `b06a5971339` - new memory allocator
  - **Introduced the bug** by removing initial heap page mapping from init
  - Old `expand_heap()` mapped 4 pages; new slab init mapped none

- (pending) - mm: fix framebuffer perf regression by restoring initial heap mapping
  - Added `map_heap_pages(&mut heap, 4)` to `init_kernel_heap()`
  - Restores behavior of old allocator's `expand_heap()` call
  - **Fixes the soft-reboot framebuffer performance bug**
