//! XSAVE / XRSTOR initialisation and runtime queries.
//!
//! Phase 1B of the Legacy Modernisation Plan: enable the XSAVE instruction
//! family on the BSP during boot and on every AP during SMP bring-up.
//!
//! The module exposes three pieces of global state that later phases (1C, 1D)
//! depend on:
//!
//! * **`XSAVE_AREA_SIZE`** — runtime-detected save-area size (bytes).
//! * **`XSAVE_ENABLED`** — whether XSAVE is the active FPU save mechanism.
//! * **`ACTIVE_XCR0`** — the XCR0 value written to every CPU.
//!
//! The `init()` entry point is called once on the BSP (via a boot step at
//! priority 42, before SMP).  Each AP then calls `enable_on_current_cpu()` to
//! replicate the same CR4 + XCR0 configuration.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::control_regs::{Cr4Flags, Xcr0Flags, read_cr4, write_cr4, xcr0_write};
use super::cpuid::XsaveFeatures;
use crate::{klog_debug, klog_info};

// ---------------------------------------------------------------------------
// Global state (set by BSP `init`, read by APs + task creation)
// ---------------------------------------------------------------------------

/// Active XSAVE area size in bytes.  Defaults to 512 (FXSAVE-compat) until
/// `init()` runs.  Task creation code should read this to allocate FPU state.
static XSAVE_AREA_SIZE: AtomicUsize = AtomicUsize::new(512);

/// `true` once XSAVE has been successfully enabled on the BSP.
static XSAVE_ENABLED: AtomicBool = AtomicBool::new(false);

/// XCR0 value computed by the BSP — every AP writes the same mask.
static ACTIVE_XCR0: AtomicU64 = AtomicU64::new(0);

/// `true` when `XSAVEC` is available (compact save format, no gaps).
static XSAVEC_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// `true` when `XSAVEOPT` is available (optimised — only writes dirty state).
static XSAVEOPT_AVAILABLE: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Public queries
// ---------------------------------------------------------------------------

/// Active XSAVE (or FXSAVE-fallback) area size in bytes.
///
/// Before `init()` this returns 512 (FXSAVE size).  After `init()` it
/// reflects the hardware-reported size for the features enabled in XCR0.
#[inline]
pub fn area_size() -> usize {
    XSAVE_AREA_SIZE.load(Ordering::Relaxed)
}

/// Whether XSAVE is the active FPU save/restore mechanism.
#[inline]
pub fn is_enabled() -> bool {
    XSAVE_ENABLED.load(Ordering::Relaxed)
}

/// The XCR0 value written to every CPU, or 0 if XSAVE is not enabled.
#[inline]
pub fn active_xcr0() -> u64 {
    ACTIVE_XCR0.load(Ordering::Relaxed)
}

/// Whether the compact `XSAVEC` instruction is available.
#[inline]
pub fn has_xsavec() -> bool {
    XSAVEC_AVAILABLE.load(Ordering::Relaxed)
}

/// Whether the optimised `XSAVEOPT` instruction is available.
#[inline]
pub fn has_xsaveopt() -> bool {
    XSAVEOPT_AVAILABLE.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// BSP initialisation (called once from boot step)
// ---------------------------------------------------------------------------

/// Detect XSAVE support, compute the XCR0 mask, enable CR4.OSXSAVE, write
/// XCR0, and record the resulting save-area size.
///
/// Returns `0` on success (including graceful FXSAVE fallback when the CPU
/// does not support XSAVE).
///
/// # Contract
/// * Must be called **once**, on the BSP, **before** SMP AP startup.
/// * Interrupts should be disabled (boot context).
pub fn init() -> i32 {
    let features = XsaveFeatures::detect();

    if !features.supported {
        // CPU does not support XSAVE — keep FXSAVE defaults.
        klog_info!("XSAVE: not supported by CPU, using FXSAVE (512 B)");
        // XSAVE_AREA_SIZE already initialised to 512.
        return 0;
    }

    // ------------------------------------------------------------------
    // 1. Compute the XCR0 mask: x87 + SSE are mandatory, AVX if available,
    //    AVX-512 if all three component bits are supported.
    // ------------------------------------------------------------------
    let supported = features.xcr0_supported;
    let mut xcr0 = Xcr0Flags::X87 | Xcr0Flags::SSE;

    if (supported & Xcr0Flags::AVX.bits()) != 0 {
        xcr0 |= Xcr0Flags::AVX;
    }

    // AVX-512 requires all three sub-components to be present.
    let avx512_bits =
        Xcr0Flags::OPMASK.bits() | Xcr0Flags::ZMM_HI256.bits() | Xcr0Flags::HI16_ZMM.bits();
    if (supported & avx512_bits) == avx512_bits {
        xcr0 |= Xcr0Flags::OPMASK | Xcr0Flags::ZMM_HI256 | Xcr0Flags::HI16_ZMM;
    }

    // Store for AP replication — must be visible before SMP startup.
    ACTIVE_XCR0.store(xcr0.bits(), Ordering::Release);

    // ------------------------------------------------------------------
    // 2. Enable XSAVE on the BSP: CR4.OSXSAVE then XCR0 write.
    // ------------------------------------------------------------------
    let cr4 = read_cr4() | Cr4Flags::OSXSAVE.bits();
    write_cr4(cr4);
    xcr0_write(xcr0.bits());

    // ------------------------------------------------------------------
    // 3. Re-query CPUID for the actual save-area size *after* XCR0 is set.
    //    CPUID.0Dh.0:EBX reflects the currently-enabled components.
    // ------------------------------------------------------------------
    let area_size = super::cpuid::xsave_area_size();
    XSAVE_AREA_SIZE.store(area_size, Ordering::Release);

    // ------------------------------------------------------------------
    // 4. Record instruction variants for later context-switch selection.
    // ------------------------------------------------------------------
    XSAVEC_AVAILABLE.store(features.xsavec, Ordering::Release);
    XSAVEOPT_AVAILABLE.store(features.xsaveopt, Ordering::Release);
    XSAVE_ENABLED.store(true, Ordering::Release);

    // ------------------------------------------------------------------
    // 5. Log.
    // ------------------------------------------------------------------
    klog_info!(
        "XSAVE: enabled, area size {} bytes, features 0x{:x}",
        area_size,
        xcr0.bits(),
    );
    klog_debug!(
        "XSAVE: supported XCR0 0x{:x}, max area {} B, XSAVEC={}, XSAVEOPT={}, XSAVES={}",
        supported,
        features.area_size_max,
        features.xsavec,
        features.xsaveopt,
        features.xsaves,
    );

    0
}

// ---------------------------------------------------------------------------
// Per-CPU enablement (called on each AP from ap_entry)
// ---------------------------------------------------------------------------

/// Replicate the BSP's XSAVE configuration on the current CPU.
///
/// Sets CR4.OSXSAVE and writes the same XCR0 mask that `init()` computed.
/// If XSAVE was not enabled (CPU doesn't support it), this is a no-op.
///
/// # Contract
/// * `init()` must have been called first (on the BSP).
/// * Interrupts should be disabled.
pub fn enable_on_current_cpu() {
    if !XSAVE_ENABLED.load(Ordering::Acquire) {
        return;
    }

    let xcr0 = ACTIVE_XCR0.load(Ordering::Acquire);
    if xcr0 == 0 {
        return;
    }

    // Set CR4.OSXSAVE on this AP.
    let cr4 = read_cr4() | Cr4Flags::OSXSAVE.bits();
    write_cr4(cr4);

    // Write the identical XCR0 mask.
    xcr0_write(xcr0);
}
