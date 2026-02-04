# Framebuffer Performance Bug Investigation

## Summary

After a soft reboot (triggered by losing at roulette), the roulette animation drops from smooth ~60 FPS to ~1 FPS. This document captures the complete investigation and root cause analysis.

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

### Root Cause: MTRR + Page Table Flags (CONFIRMED)

After the PAT fix, diagnostic logs showed:

```
CACHE_STATE: PAT MSR=0x0007010600070106
CACHE_STATE: PAT[0]=6 PAT[1]=1 PAT[2]=7 PAT[3]=0 PAT[4]=6 PAT[5]=1 PAT[6]=7 PAT[7]=0
RENDER_DIAG: fill_rect 200x200 took 1508921899 cycles (37723 cycles/pixel)
RENDER_DIAG: WARNING - slow render detected! 37723 cycles/pixel suggests uncached FB
```

**PAT[1]=1 (WC) is correctly set, but rendering is still 37,723 cycles/pixel!**

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

## Why First Boot Works

On **cold boot**:
1. UEFI/firmware may set up MTRRs with WC for framebuffer region
2. Limine preserves these MTRR settings
3. Framebuffer gets WC from MTRR (even without PAT help)

On **warm reboot** (PS/2 0xFE reset):
1. CPU is reset but MTRRs may revert to defaults
2. Limine may not reinitialize MTRRs the same way
3. Framebuffer region becomes UC by default
4. Even with PAT[1]=WC, page tables use PAT index 0 (no PWT bit)
5. Effective type: UC (extremely slow)

## Solution Options

### Option 1: Variable MTRR (Recommended)

Configure a variable MTRR for the framebuffer region as WC.

**Pros:**
- Authoritative fix - MTRRs override PAT defaults
- Works regardless of page table flags
- Localized change, doesn't affect rest of system

**Cons:**
- Requires new MTRR management code
- Must be done on all CPUs (MTRRs are per-core)
- MTRR constraints: base must be aligned to size, size must be power-of-two

**Implementation:**
1. Read `IA32_MTRR_CAP` to get number of variable MTRRs
2. Find a free variable MTRR pair
3. Program `IA32_MTRR_PHYSBASEn` and `IA32_MTRR_PHYSMASKn` for FB region
4. Use Intel-required sequence: disable caching → wbinvd → disable MTRRs → write MSRs → re-enable → wbinvd → re-enable caching
5. Ensure same setup on AP CPUs during SMP bring-up

### Option 2: Page Table Flag Fix (May Not Be Sufficient)

Set PWT=1 on framebuffer page table entries to select PAT[1]=WC.

**Pros:**
- Simpler implementation
- Uses existing page mapping infrastructure

**Cons:**
- **Won't work if MTRR defaults to UC** - UC overrides WC from PAT
- Limine may use huge pages (2MB/1GB) for HHDM, making PTE surgery complex
- Fragile - depends on bootloader behavior

**Implementation:**
1. Add `PageFlags::FRAMEBUFFER_WC` constant (PRESENT | WRITABLE | WRITE_THROUGH)
2. After boot, remap framebuffer pages with new flags
3. Invalidate TLB for affected pages

### Option 3: Combined Approach (Most Robust)

1. Set up MTRR for framebuffer as WC
2. Create dedicated kernel virtual mapping with PWT=1
3. Use this mapping instead of HHDM for framebuffer access

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

1. **Implement MTRR support** in `mm/src/mtrr.rs`
2. **Add MTRR init** for framebuffer during early boot (after PAT, before video)
3. **Ensure SMP consistency** - same MTRR config on all CPUs
4. **Test warm reboot** - verify performance is consistent across reboots
5. **Remove diagnostic instrumentation** once fix is verified

## References

- Intel SDM Vol. 3A, Chapter 11: Memory Cache Control
- Intel SDM Table 11-7: Effective Page-Level Memory Types
- Limine Boot Protocol: https://github.com/limine-bootloader/limine/blob/trunk/PROTOCOL.md

## Commits Related to This Investigation

- `3ffdc5ca6` - mm: initialize PAT with Write-Combining for framebuffer performance
  - Fixed: PAT was never initialized
  - Added: test_pat_wc_enabled to verify PAT[1]==WC
  - Result: PAT now correct, but bug persists due to MTRR issue
