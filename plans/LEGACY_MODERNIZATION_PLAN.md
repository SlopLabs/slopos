# SlopOS Legacy Modernization Plan

> **Status**: In Progress — Phase 0 (Timer Modernization) **complete**, Phase 0E (PIT Deprecation) complete, Phase 1 (FPU/SIMD State Modernization) **complete**, Phase 2 (Spinlock Modernization) **complete** (2C MCS deferred, `spin` crate fully removed), Phase 3A (PCI Capability List Parsing) **complete**, Phase 3B (MSI Support) **complete**, Phase 3C (MSI-X Support) **complete**, Phase 3D (VirtIO MSI-X Integration) **complete**, Phase 3E (Interrupt-Driven VirtIO Completion) **complete**
> **Target**: Replace all legacy/outdated hardware interfaces and patterns with modern equivalents as SlopOS approaches MVP
> **Scope**: Timers, FPU state, interrupts, spinlocks, PCI, networking, and beyond

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Phase 0: Timer Modernization](#3-phase-0-timer-modernization)
4. [Phase 1: FPU / SIMD State Modernization](#4-phase-1-fpu--simd-state-modernization)
5. [Phase 2: Spinlock Modernization](#5-phase-2-spinlock-modernization)
6. [Phase 3: MSI/MSI-X Interrupt Routing](#6-phase-3-msimsi-x-interrupt-routing)
7. [Phase 4: PCIe ECAM Configuration Space](#7-phase-4-pcie-ecam-configuration-space)
8. [Phase 5: Network Stack Completion](#8-phase-5-network-stack-completion)
9. [Phase 6: PCID / TLB Optimization](#9-phase-6-pcid--tlb-optimization)
10. [Phase 7: Long-Horizon Hardware](#10-phase-7-long-horizon-hardware)
11. [Dependency Graph](#11-dependency-graph)
12. [Blocked Features Reference](#12-blocked-features-reference)

---

## 1. Executive Summary

SlopOS has a remarkably modern foundation — Limine boot, APIC/IOAPIC interrupts, SYSCALL/SYSRET, buddy+slab allocators, higher-half kernel, 4-level paging with NX, SMP TLB shootdown, and proper Ring 0/3 separation. The legacy 8259 PIC has already been sacrificed to the Wheel of Fate.

However, several subsystems still rely on hardware designs from the 1980s–1990s:

| Legacy Component | Era | Modern Replacement | Impact |
|---|---|---|---|
| **PIT (8254)** | 1981 | HPET + LAPIC timer | Scheduler precision, power |
| **FXSAVE/FXRSTOR** | 1999 (Pentium III) | XSAVE/XRSTOR | Blocks AVX, wastes state space |
| **Test-and-set spinlock** | 1970s concept | Ticket / queued locks | SMP starvation, cache thrashing |
| **PCI port I/O (0xCF8/0xCFC)** | 1992 (PCI 2.0) | ECAM (MMIO) | 256B limit, slow, no PCIe |
| **Legacy IRQ lines** | 1981 (ISA) | MSI/MSI-X | Shared IRQs, no per-device vectors |
| **No TCP** | — | TCP state machine | No real networking |
| **PCID detected but unused** | — | Active PCID TLB tagging | Unnecessary TLB flushes |

This plan has **8 phases**, ordered by impact and dependency:

- **Phase 0**: Timer modernization (HPET + LAPIC timer replace PIT) — **highest impact**
- **Phase 1**: XSAVE/XRSTOR replaces FXSAVE — unlocks AVX
- **Phase 2**: Ticket/queued spinlocks — fixes SMP fairness
- **Phase 3**: MSI/MSI-X for VirtIO and PCI devices
- **Phase 4**: PCIe ECAM configuration space via MCFG
- **Phase 5**: TCP in the network stack
- **Phase 6**: PCID-accelerated TLB management
- **Phase 7**: Long-horizon items (USB/xHCI, virtio-gpu, RTC)

Phases 0–2 are self-contained kernel changes. Phase 3–4 build on each other. Phase 5 is independent. Phase 6–7 are stretch goals.

---

## 2. Current State Assessment

### What's Already Modern

| Component | Implementation | Files |
|---|---|---|
| Boot | Limine v8.7.0 | `limine.conf`, `boot/src/limine_protocol.rs` |
| Interrupts | LAPIC + IOAPIC (PIC removed) | `drivers/src/apic/`, `drivers/src/ioapic/` |
| Syscalls | SYSCALL/SYSRET | `core/src/syscall/`, `boot/src/gdt.rs` |
| Memory | Buddy + slab + per-CPU cache | `mm/src/page_alloc.rs`, `mm/src/kernel_heap.rs` |
| Paging | 4-level, NX, higher-half | `mm/src/paging/`, `link.ld` |
| TLB | SMP shootdown via IPI | `mm/src/tlb.rs` |
| Context | Full register save + FPU | `core/context_switch.s` |
| VirtIO | Block + Network drivers | `drivers/src/virtio_blk.rs`, `drivers/src/virtio_net.rs` |
| ACPI | RSDP → XSDT → MADT | `acpi/src/` |
| Userland | Ring 3, fork, exec, pipes, shm, futexes | `core/src/`, `userland/src/` |

### What's Legacy

| Component | Current | File(s) | Why It's Legacy |
|---|---|---|---|
| System timer | PIT at 100Hz | `drivers/src/pit.rs` | 1981 chip, imprecise, wastes IRQs |
| ~~FPU save~~ | ~~FXSAVE (512B fixed)~~ | `core/context_switch.s` | **Modernized** — XSAVE/XRSTOR (mandatory), FXSAVE removed |
| ~~Spinlocks~~ | ~~CAS loop, no fairness~~ | `lib/src/spinlock.rs` | **Modernized** — Ticket lock (FIFO fairness), proportional backoff; MCS locks optional stretch |
| PCI config | Port I/O 0xCF8/0xCFC | `drivers/src/pci.rs`, `lib/src/ports.rs` | 256B config space only, slow |
| IRQ routing | Legacy IOAPIC lines only | `drivers/src/irq.rs` | No MSI/MSI-X, shared IRQ lines |
| Network | UDP/ICMP/ARP only | `drivers/src/net/` | No TCP = no real networking |
| TLB | PCID detected, not used | `mm/src/tlb.rs` | Unnecessary full TLB flushes |
| Input | PS/2 keyboard+mouse | `drivers/src/ps2/` | 1987, but needed for QEMU |
| Serial | UART 16550 on COM1 | `drivers/src/serial.rs` | 1987, but still the standard |

### What Stays (Legacy But Correct)

- **PS/2**: QEMU q35 emulates PS/2 natively. No practical replacement without USB/xHCI (Phase 7). Keep.
- **Serial UART 16550**: Industry standard for kernel debug. Every modern Rust kernel uses it. Keep.
- **`pic_quiesce_disable()`**: 14-line function that masks the 8259 PIC during shutdown. Harmless. Keep or inline.

---

## 3. Phase 0: Timer Modernization

> **The single highest-impact change. Replaces the oldest hardware dependency.**
> **Kernel changes required**: Yes — new HPET driver, LAPIC timer scheduler integration
> **Difficulty**: Medium-High
> **Depends on**: Nothing (self-contained)

### Background

The PIT (Programmable Interval Timer, Intel 8254) is a **1981 chip** running at 1.193182 MHz. SlopOS uses it for:
1. Scheduler preemption ticks (`pit_init(100)` → 100Hz)
2. Busy-wait delays (`pit_poll_delay_ms()`)
3. Sleep implementation (`pit_sleep_ms()`)

The LAPIC timer is already initialized in `drivers/src/apic/mod.rs` (lines 192–214) with `init_timer()`, `set_initial_count()`, `timer_set_divisor()`, and periodic mode support. It's just never wired to the scheduler.

**Problem**: The LAPIC timer's frequency is unknown — it runs off the CPU bus clock, which varies per machine. You need a **reference timer** (HPET or PIT) to calibrate it once at boot.

### 0A: HPET Driver

Parse the ACPI HPET table and implement a minimal HPET driver for system time and LAPIC calibration.

- [x] **0A.1** Add HPET table parsing to `acpi/src/hpet.rs`:
  - Find HPET table signature (`"HPET"`) in XSDT
  - Parse base address (MMIO), minimum tick period, comparator count
  - Export `HpetInfo { base_phys: u64, period_fs: u32, num_comparators: u8 }`
- [x] **0A.2** Create `drivers/src/hpet.rs`:
  - Map HPET MMIO registers via `MmioRegion::map()` (same pattern as IOAPIC)
  - Define register offsets: `GENERAL_CAP` (0x00), `GENERAL_CONFIG` (0x10), `MAIN_COUNTER` (0xF0), `TIMER_N_CONFIG` (0x100+0x20*N), `TIMER_N_COMPARATOR` (0x108+0x20*N)
  - Read capability register to get period (femtoseconds per tick) and number of timers
- [x] **0A.3** Implement `hpet::init()`:
  - Disable legacy replacement mode (clear bit 1 of GENERAL_CONFIG)
  - Enable main counter (set bit 0 of GENERAL_CONFIG)
  - Log: `"HPET: base 0x{:x}, period {} fs, {} comparators"`
- [x] **0A.4** Implement `hpet::read_counter() -> u64`:
  - Read MAIN_COUNTER register (64-bit monotonic)
  - This is the primary precision time source
- [x] **0A.5** Implement `hpet::nanoseconds(ticks: u64) -> u64`:
  - Convert ticks to nanoseconds using period from capability register
  - `ns = ticks * period_fs / 1_000_000`
- [x] **0A.6** Implement `hpet::delay_ns(ns: u64)` and `hpet::delay_ms(ms: u32)`:
  - Spin-wait on main counter for the specified duration
  - Replaces `pit_poll_delay_ms()` for calibration and early-boot delays
- [x] **0A.7** Wire into boot sequence (`boot/src/boot_drivers.rs`):
  - Add `BOOT_STEP_HPET_SETUP` after IOAPIC setup
  - Call `hpet::hpet_init()` using ACPI-discovered base address
- [x] **0A.8** Verify: boot prints HPET info, `hpet_read_counter()` returns increasing values, regression test suite passes (10 tests in `drivers/src/hpet_tests.rs`)

### 0B: LAPIC Timer Calibration

Use HPET (or PIT as fallback) to measure the LAPIC timer frequency.

- [x] **0B.1** Implement `calibrate_lapic_timer() -> u64` in `drivers/src/apic/`:
  - Set LAPIC timer to one-shot mode with a large initial count (e.g., 0xFFFF_FFFF)
  - Set divisor to 16 (already used: `LAPIC_TIMER_DIV_16`)
  - Wait exactly 10ms using `hpet_delay_ns(10_000_000)` (or `pit_poll_delay_ms(10)` as fallback)
  - Read remaining count: `elapsed = initial - current_count`
  - Calculate frequency: `freq_hz = elapsed * 100` (since we measured 10ms)
  - Store in static: `LAPIC_TIMER_FREQ_HZ: AtomicU64`
- [x] **0B.2** Implement `lapic_timer_set_periodic_ms(ms: u32)`:
  - Calculate initial count: `count = freq_hz * ms / 1000`
  - Set periodic mode with the scheduler vector
  - This replaces PIT for scheduling ticks
- [x] **0B.3** Call calibration during boot after HPET init:
  - Log: `"APIC: Timer calibrated: {} Hz (via HPET)"` or `"(via PIT fallback)"`
- [x] **0B.4** Verify: calibration produces a reasonable frequency (~62 MHz with div-16 on QEMU); 7 regression tests in `drivers/src/apic_timer_tests.rs` (425 total tests pass)

### 0C: Scheduler Migration to LAPIC Timer

Replace PIT-driven scheduling ticks with LAPIC timer ticks.

- [x] **0C.1** In `boot/src/boot_drivers.rs`, after LAPIC calibration:
  - Call `lapic_timer_set_periodic_ms(10)` (100Hz, same as current PIT)
  - The LAPIC timer interrupt already routes through the IDT; ensure the scheduler tick handler is called
- [x] **0C.2** Update `drivers/src/irq.rs`:
  - The LAPIC timer fires on a local vector (not through IOAPIC)
  - Ensure the timer ISR calls `scheduler_timer_tick()` (same as PIT currently does)
  - Each CPU gets its own LAPIC timer interrupt — no shared IRQ line
- [x] **0C.3** Disable PIT scheduling role:
  - Stop calling `pit_init()` / `pit_enable_irq()` during boot
  - Keep PIT driver code for fallback calibration only
  - Remove PIT IRQ route from IOAPIC setup
- [x] **0C.4** Update `pit_sleep_ms()` callers:
  - Replace with `hpet_delay_ns()` or a new `timer_sleep_ms()` that uses the scheduler's sleep queue
  - Audit all callers: `pit_sleep_ms`, `pit_poll_delay_ms` across the codebase
- [x] **0C.5** Per-CPU LAPIC timer setup for APs:
  - Each AP must calibrate or inherit the BSP's calibrated frequency
  - Call `lapic_timer_set_periodic_ms()` during AP startup in `boot/src/smp.rs`
- [x] **0C.6** Verify: `just test` passes with LAPIC timer driving scheduling. PIT no longer receives IRQs.

### 0D: High-Resolution System Clock

Expose a monotonic nanosecond clock to the kernel and userland.

- [x] **0D.1** Create `lib/src/clock.rs`:
  - `clock_monotonic_ns() -> u64` — reads HPET main counter, converts to nanoseconds
  - `clock_uptime_ms() -> u64` — wraps monotonic, converts to milliseconds
  - Replaces the tick-counting approach in `irq_get_timer_ticks()`
- [x] **0D.2** Update `SYSCALL_GET_TIME_MS` (39) to use `clock_uptime_ms()` instead of PIT tick counting
- [x] **0D.3** Expose `SYSCALL_CLOCK_GETTIME` (new) for nanosecond precision:
  - `rdi` = clock ID (0 = MONOTONIC)
  - `rsi` = pointer to `{ seconds: u64, nanoseconds: u64 }`
- [x] **0D.4** Update userland `time` command and `uptime` to use nanosecond clock for better precision
- [x] **0D.5** Verify: `uptime` shows correct elapsed time, `time ls` shows sub-millisecond precision

### 0E: PIT Deprecation

Reduce PIT to a calibration-only fallback.  HPET + LAPIC timer are now **mandatory**.

- [x] **0E.1** Make HPET + LAPIC timer mandatory at boot:
  - `boot_step_hpet_setup_fn()` panics if HPET unavailable
  - `boot_step_lapic_calibration_fn()` panics if calibration returns 0 Hz
  - `boot_step_lapic_timer_start_fn()` panics if periodic mode fails
  - PIT retained only as LAPIC calibration polled-delay fallback (dead path)
- [x] **0E.2** Remove `pit_enable_irq()` / `pit_disable_irq()` from the default boot path
- [x] **0E.3** Remove PIT IRQ routing — `enable_pit_timer_fallback()` and handler deleted
- [x] **0E.4** Update `TODO.md` to mark the LAPIC timer item as complete
- [x] **0E.5** Add deprecation comment to `drivers/src/pit.rs`:
  ```rust
  //! Legacy PIT (Intel 8254) driver — **calibration-only fallback**.
  //! The HPET + LAPIC timer is the primary timing source since Phase 0.
  ```
- [x] **0E.6** Remove `delay_ms_or_pit_fallback()` from `hpet.rs` — all callers use `delay_ms()`
- [x] **0E.7** Remove PIT fallback from PlatformServices (`boot_impl.rs`)
- [x] **0E.8** Replace `pit_get_frequency()` in `input_event.rs` with HPET-based timestamp

### Phase 0 Gate

- [x] **GATE**: HPET driver discovers and initializes the timer from ACPI
- [x] **GATE**: LAPIC timer calibrated against HPET (or PIT fallback)
- [x] **GATE**: Scheduler runs on LAPIC timer, not PIT
- [x] **GATE**: Each CPU has its own LAPIC timer tick (no shared IRQ)
- [x] **GATE**: `clock_monotonic_ns()` provides nanosecond precision
- [x] **GATE**: PIT no longer receives interrupts in the default boot path
- [x] **GATE**: `just test` passes
- [x] **GATE**: `just boot` boots and schedules correctly

---

## 4. Phase 1: FPU / SIMD State Modernization

> **Unlocks AVX/AVX-512 support and future-proofs context switching.**
> **Kernel changes required**: Yes — context switch assembly, target JSON, task struct
> **Difficulty**: Medium
> **Depends on**: Nothing (self-contained)

### Background

SlopOS currently uses `fxsave64`/`fxrstor64` (6 occurrences in `core/context_switch.s`) to save/restore FPU state. This saves exactly 512 bytes covering x87, MMX, and SSE registers.

The `XSAVE`/`XRSTOR` family (available since Intel Nehalem, 2008) dynamically sizes the save area based on which CPU features are enabled in `XCR0`. This is **required** for AVX (256-bit), AVX-512, AMX, and any future SIMD extensions.

The target JSON (`targets/x86_64-slos.json`) currently disables AVX with `"-mmx,-avx,-avx2"` — partly because FXSAVE can't handle it.

### 1A: XSAVE Feature Detection

- [x] **1A.1** Add CPUID leaf 0x0D detection to `lib/src/cpu/cpuid.rs`:
  - Query `CPUID.0x0D.0:EBX` for XSAVE area size (with current XCR0)
  - Query `CPUID.0x0D.0:ECX` for maximum XSAVE area size (all features)
  - Query `CPUID.0x0D.0:EAX` for supported XCR0 feature bits
  - Export: `xsave_area_size() -> usize`, `xsave_max_size() -> usize`, `xcr0_supported() -> u64`
- [x] **1A.2** Define XCR0 feature bits in `lib/src/cpu/control_regs.rs`:
  - Bit 0: x87 (always set)
  - Bit 1: SSE (MXCSR + XMM0-15)
  - Bit 2: AVX (YMM0-15 upper halves)
  - Bit 5-7: AVX-512 (opmask, ZMM upper, ZMM16-31)
- [x] **1A.3** Implement `xcr0_read() -> u64` and `xcr0_write(val: u64)`:
  - Uses `xgetbv` / `xsetbv` instructions
  - Only callable after `CR4.OSXSAVE` is set
- [x] **1A.4** Detect XSAVE support: `CPUID.1:ECX[bit 26]` (XSAVE) and `CPUID.1:ECX[bit 27]` (OSXSAVE)
  - Also detect XSAVEC (compact format): `CPUID.0x0D.1:EAX[bit 1]`
  - Prefer XSAVEC when available (smaller save area, no gaps)

### 1B: Enable XSAVE in Boot

- [x] **1B.1** During boot (`boot/src/boot_drivers.rs`):
  - Set `CR4.OSXSAVE` (bit 18) — already defined as `CR4_OSXSAVE` in `lib/src/cpu/control_regs.rs`
  - Write XCR0 with desired feature mask: `x87 | SSE` minimum, `| AVX` if supported
  - Log: `"XSAVE: enabled, area size {} bytes, features 0x{:x}"`
- [x] **1B.2** Store the active XSAVE area size in a global static:
  - `XSAVE_AREA_SIZE: AtomicUsize` — queried by task creation code
  - Minimum 512 (FXSAVE compat), typically 832 with AVX, 2688+ with AVX-512
- [x] **1B.3** Each AP must also set `CR4.OSXSAVE` and write `XCR0` during `ap_entry()` in `boot/src/smp.rs`

### 1C: Update Task FPU State

- [x] **1C.1** Replace fixed `FPU_STATE_SIZE = 512` in `core/src/scheduler/task_struct.rs`:
  - Compile-time maximum of 2688 bytes (covers FXSAVE, XSAVE+AVX, XSAVE+AVX-512)
  - Runtime validation via `validate_fpu_state_size()` panics at boot if hardware exceeds the compile-time max
  - `FXSAVE_AREA_SIZE = 512` constant retained for fallback reference
- [x] **1C.2** Update `FpuState::new()`:
  - XSAVE header (bytes 512–575) explicitly zeroed: `XSTATE_BV` = 0 (init values), `XCOMP_BV` = 0
  - Legacy region defaults unchanged: FCW=0x037F, MXCSR=0x1F80
  - XSAVE header constants defined: `XSAVE_HEADER_OFFSET`, `XSTATE_BV_OFFSET`, `XCOMP_BV_OFFSET`
- [x] **1C.3** Ensure 64-byte alignment for XSAVE area (required by hardware):
  - `#[repr(C, align(64))]` on `FpuState` (was `align(16)`)
  - Compile-time assertions: `align_of::<FpuState>() >= 64`, `FPU_STATE_SIZE % align == 0`
  - Assembly `FPU_STATE_OFFSET` unchanged at 0xD0 (compiler padding naturally satisfied)

### 1D: Update Context Switch Assembly

- [x] **1D.1** Replace all `fxsave64` with `xsave64` (or `xsavec`) in `core/context_switch.s`:
  - `xsave64` / `xrstor64` take an EDX:EAX mask specifying which components to save/restore
  - Set `EDX:EAX = XCR0` value (save all enabled components)
  - 6 sites updated (3 save, 3 restore) via `FPU_SAVE`/`FPU_RESTORE` GAS macros
  - `ACTIVE_XCR0` static exposed to assembly via `#[unsafe(no_mangle)]`
  - [x] **1D.2** ~~Implement lazy XSAVE optimization~~ — **Skipped per plan recommendation**:
  - Eager save/restore is simpler and modern CPUs make XSAVE fast
  - Can be revisited as a future optimisation if profiling shows FPU save/restore overhead
  - [x] **1D.3** ~~Fallback path~~ — **FXSAVE fallback intentionally removed**:
  - XSAVE is now a hard boot requirement — `init()` panics if CPUID reports no XSAVE
  - `XSAVE_ENABLED: AtomicBool` removed — `is_enabled()` unconditionally returns `true`
  - `FPU_SAVE`/`FPU_RESTORE` macros use unconditional `xsave64`/`xrstor64` (no branch)
  - Every x86-64 CPU since 2008 supports XSAVE; QEMU always exposes it

### 1E: Update Target JSON

- [x] **1E.1** Update `targets/x86_64-slos.json`:
  - Removed `-avx` and `-avx2` from disabled features, added `+xsave`
  - Kept `-mmx` disabled (MMX is truly legacy and conflicts with x87)
  - New features string: `"-mmx,+xsave"`
  - The kernel itself doesn't emit AVX instructions, but the compiler is no longer forbidden from using them
- [x] **1E.2** Update userland target `targets/x86_64-slos-userland.json`:
  - Enabled AVX/AVX2 and XSAVE for userland: `"-mmx,+xsave,+avx,+avx2"`
  - Userland code can now use `__m256` intrinsics and AVX instructions without #UD

### Phase 1 Gate

- [x] **GATE**: XSAVE area size detected at boot via CPUID
- [x] **GATE**: `CR4.OSXSAVE` set, `XCR0` configured on BSP and all APs
- [x] **GATE**: Context switch uses `xsave64`/`xrstor64` (or `xsavec`/`xrstorc`)
- [x] **GATE**: Task FPU state area sized dynamically (or compile-time max)
- [x] **GATE**: ~~Fallback to FXSAVE if XSAVE unavailable~~ — FXSAVE fallback removed; XSAVE is a hard boot requirement
- [x] **GATE**: AVX no longer disabled in target JSON
- [x] **GATE**: `just test` passes (437 passed, 0 failed — includes 13 XSAVE regression tests)
- [x] **GATE**: Userland programs can use AVX instructions without #UD


### Phase 1 Test Coverage

A dedicated `xsave` test suite (`tests/src/xsave_tests.rs`, 13 tests) was added to guard
against regressions in the XSAVE/FPU/SIMD modernization:

| # | Test | What It Catches |
|---|------|-----------------|
| 1 | `test_xsave_enabled_matches_cpuid` | XSAVE flag out of sync with CPUID |
| 2 | `test_xsave_area_size_sane` | Save-area size below 512 or exceeding CPUID max |
| 3 | `test_xsave_xcr0_mandatory_bits` | Missing x87/SSE bits in active XCR0 |
| 4 | `test_xsave_features_consistency` | XsaveFeatures struct internally inconsistent |
| 5 | `test_cr4_osxsave_set` | CR4.OSXSAVE not set despite XSAVE enabled |
| 6 | `test_xcr0_matches_active` | Live XCR0 register ≠ xsave::active_xcr0() |
| 7 | `test_xcr0_avx_consistent` | AVX enabled in XCR0 but unsupported by CPU |
| 8 | `test_sse_xsave_xrstor_roundtrip` | SSE register corruption through XSAVE/XRSTOR |
| 9 | `test_avx_xsave_xrstor_roundtrip` | **AVX upper-YMM loss** (would fail under FXSAVE) |
| 10 | `test_sse_multi_register_isolation` | Cross-register bleed across XMM0–XMM7 |
| 11 | `test_xsave_area_size_matches_cpuid` | Runtime vs CPUID area size divergence |
| 12 | `test_xsave_area_size_covers_avx` | Area too small for AVX state components |
| 13 | `test_xsave_variant_flags_consistent` | XSAVEC/XSAVEOPT flags mismatch |

---

## 5. Phase 2: Spinlock Modernization

> **Fixes SMP fairness and eliminates cache line thrashing.**
> **Kernel changes required**: Yes — replace spinlock implementation
> **Difficulty**: Low-Medium
> **Depends on**: Nothing (self-contained)

### Background

The `IrqMutex` in `lib/src/spinlock.rs` previously used a simple `compare_exchange_weak` loop (test-and-set).
On SMP (SlopOS runs with `QEMU_SMP=2`), this had two problems:
1. **No fairness**: CPU 0 can acquire the lock 1000 times while CPU 1 starves
2. **Cache line bouncing**: Every `compare_exchange` invalidates the cache line on the other core, even if the lock is held — wasting memory bandwidth

### 2A: Ticket Lock Implementation

The simplest fair lock. Linux used ticket locks from 2008–2015.

- [x] **2A.1** Implement ticket lock in `IrqMutex` (`lib/src/spinlock.rs`):
  - Replaced `lock: AtomicBool` with `next_ticket: AtomicU16` + `now_serving: AtomicU16`
  - `lock()`: `my_ticket = next_ticket.fetch_add(1, Relaxed)`, spin on `now_serving.load(Acquire) == my_ticket`
  - `unlock()` (Drop): `now_serving.fetch_add(1, Release)`
  - `try_lock()`: CAS `next_ticket` from `now_serving` value to +1 (only succeeds if lock is free)
  - `force_unlock()` / `poison_unlock()`: snap `now_serving` to `next_ticket` (releases lock entirely)
  - `is_locked()`: `next_ticket != now_serving`
  - **FIFO guaranteed** — whoever takes a ticket first gets served first
  - [x] **2A.2** Proportional backoff in the spin loop:
  - `core::hint::spin_loop()` (PAUSE instruction on x86) used in all spin paths
  - Proportional backoff: `distance = my_ticket.wrapping_sub(serving)` → pause 1× per ticket of distance, capped at 64
  - Reduces cache-line traffic dramatically when multiple CPUs are queued
  - [x] **2A.3** Replaced the `IrqMutex` internal lock with ticket lock:
  - The `IrqMutex` API (IRQ disable + preemption disable + lock) stays the same
  - Only the inner locking mechanism changed (`AtomicBool` → `AtomicU16` pair)
  - All existing callers (211 lock sites across 29 files) are unaffected — zero API changes
  - [x] **2A.4** Audited all 211 lock sites across 29 files for hold-time:
  - **All lock holds are short-lived critical sections** — no lock is held across blocking I/O, sleep, or wait
  - `PIPE_STATE` / `FILEIO_STATE` (fs/fileio.rs): correctly scoped — lock acquired in `{}` block, released before any blocking wait
  - `KERNEL_HEAP` (mm/kernel_heap.rs): held during kmalloc/kfree; classic pattern, unavoidable. Per-CPU slab cache already mitigates contention
  - `PAGE_ALLOCATOR` (mm/page_alloc.rs): held during page frame allocation; per-CPU page cache (PCP) already mitigates hot-path contention
  - `VM_MANAGER` (mm/process_vm.rs): held during process VM operations (page table walks but no blocking I/O)
  - **Future `IrqRwLock` candidates** (read-heavy, write-rare): `CONTEXT` (compositor, 17 sites), `INPUT_MANAGER` (input, 18 sites)

### 2A+: Ancillary Lock Cleanup

- [x] **2A+.1** Replace `KLOG_LOCK` test-and-set `AtomicBool` in `drivers/src/serial.rs` with
  PCR-independent ticket lock (`AtomicU16` pair + `cli`/`sti`, no `PreemptGuard` dependency).
  The klog backend must work during AP boot when PCR is unavailable, so `IrqMutex` cannot
  be used — but the old CAS spinlock had no fairness.
- [x] **2A+.2** Upgrade `IrqRwLock` to **writer-preferring** design:
  - Added `writer_waiting: AtomicU32` counter to `IrqRwLock` struct
  - `write()` increments `writer_waiting` before spinning, decrements after acquiring
  - `read()` and `try_read()` yield when `writer_waiting > 0` — prevents writer starvation
  - `try_write()` unchanged (non-blocking, doesn't signal intent)
  - Zero API changes — all existing callers unaffected
- [x] **2A+.3** Remove the external `spin` crate entirely:
  - Implemented native `OnceLock<T>` in `lib/src/once_lock.rs` (124 lines, `AtomicU8` state machine: UNINIT→RUNNING→COMPLETE)
  - Replaced `spin::Once` with `OnceLock` in `drivers/src/apic/mod.rs`, `core/src/scheduler/runtime.rs`, `drivers/src/random.rs`
  - Replaced `spin::Mutex` with `IrqMutex` in `core/src/scheduler/per_cpu.rs` (queue_lock) and `drivers/src/random.rs`
  - Removed `spin` dependency from all 7 Cargo.toml files (workspace root + 6 crate manifests)
  - Zero `spin::` references remain in the codebase — all locking is now kernel-native

### 2B: Test-and-Test-and-Set Optimization

Even before ticket locks, the spin loop can be improved.

- [x] **2B.1** Test-and-test-and-set (TTAS) pattern:
  - The ticket lock inherently has this property: spinning on `now_serving` is a **read-only** operation
  - No CAS in the spin loop — only a `load(Acquire)` that hits the local cache line until the holder releases
  - The `IrqRwLock` spin loops also already use the TTAS pattern (read-only pre-check before CAS)

### 2C: MCS / Queued Locks (Deferred)

Linux's current lock since 2015. Per-CPU queue node eliminates all cache line bouncing.

> **Status**: Explicitly deferred.  Ticket locks + writer-preferring `IrqRwLock` are
> sufficient for SlopOS's current 2-CPU SMP target.  MCS locks should only be
> revisited when (a) SlopOS targets 4+ CPUs, AND (b) profiling shows spinlock
> contention as a measurable bottleneck.  At 2 CPUs with per-CPU page/slab caches
> already absorbing the hot paths, MCS adds complexity with no measurable benefit.

- [x] **2C.1** ~~Implement `McsLock<T>`~~ — **Deferred**: ticket lock sufficient for 2-CPU SMP
- [x] **2C.2** ~~Identify candidates~~ — **Deferred**: per-CPU caches already mitigate contention
- [x] **2C.3** ~~Benchmark~~ — **Deferred**: no contention data warrants MCS complexity

### Phase 2 Gate

- [x] **GATE**: `IrqMutex` uses ticket lock internally (`AtomicU16` pair replaces `AtomicBool`)
- [x] **GATE**: FIFO fairness guaranteed by ticket lock design — `fetch_add` serializes acquisition order
- [x] **GATE**: `spin_loop()` (PAUSE) used in all spin paths with proportional backoff
- [x] **GATE**: No legacy test-and-set `AtomicBool` locks remain anywhere in the codebase
- [x] **GATE**: `IrqRwLock` is writer-preferring — writers cannot be starved by continuous readers
- [x] **GATE**: `KLOG_LOCK` (serial.rs) replaced with PCR-independent ticket lock
- [x] **GATE**: Phase 2C (MCS) explicitly deferred with documented rationale
- [x] **GATE**: `just test` passes
- [x] **GATE**: No deadlocks or livelocks under SMP boot (2 CPUs, full test harness)

---

## 6. Phase 3: MSI/MSI-X Interrupt Routing

> **Eliminates shared IRQ lines, enables per-device interrupt vectors.**
> **Kernel changes required**: Yes — PCI capability parsing, MSI programming
> **Difficulty**: High
> **Depends on**: Phase 4 benefits from this but is not required

### Background

All current interrupt routing goes through IOAPIC with legacy IRQ line numbers:
```rust
// drivers/src/irq.rs
program_ioapic_route(LEGACY_IRQ_TIMER);
program_ioapic_route(LEGACY_IRQ_KEYBOARD);
program_ioapic_route(LEGACY_IRQ_MOUSE);
program_ioapic_route(LEGACY_IRQ_COM1);
```

MSI (Message Signaled Interrupts) writes a message directly to the LAPIC — no IOAPIC involved. Benefits:
- **No shared IRQs**: Each device gets its own vector(s)
- **Lower latency**: Direct write to LAPIC, no IOAPIC redirection table lookup
- **PCIe requirement**: MSI is mandatory for PCIe devices per spec
- **Multi-queue**: MSI-X supports up to 2048 vectors per device (critical for VirtIO multi-queue)

### 3A: PCI Capability List Parsing

MSI/MSI-X are discovered through PCI capability structures.

- [x] **3A.1** Implement PCI capability list walking in `drivers/src/pci.rs`:
  - Read `Status Register` (offset 0x06) bit 4: capabilities list present
  - Read `Capabilities Pointer` (offset 0x34): points to first capability
  - Walk linked list: each entry has `{ cap_id: u8, next_ptr: u8, ... }`
  - Return capability offset for a given ID
  - Implemented as `PciCapabilityIter` struct with `Iterator` trait — idiomatic Rust linked-list traversal
  - Guard counter (48 iterations, matching Linux `PCI_FIND_CAP_TTL`) protects against malformed lists
  - Bottom 2 bits of pointers masked per PCI spec (DWORD alignment)
- [x] **3A.2** Define capability IDs in `drivers/src/pci_defs.rs`:
  - `PCI_CAP_ID_MSI = 0x05`
  - `PCI_CAP_ID_MSIX = 0x11`
  - `PCI_CAP_ID_PCIE = 0x10` (added for Phase 4 readiness)
  - `PCI_CAP_ID_VNDR = 0x09` (already existed, used by VirtIO)
  - `PciCapability { offset: u8, id: u8 }` struct for iterator yield type
- [x] **3A.3** Implement `pci_find_capability(bus, dev, func, cap_id) -> Option<u8>`:
  - Wraps `PciCapabilityIter` with idiomatic `.find().map()` chain
  - Convenience methods on `PciDeviceInfo`: `find_capability()`, `capabilities()`, `has_msi()`, `has_msix()`
  - Used by MSI, MSI-X, and VirtIO modern device detection
- [x] **3A.4** Store discovered capabilities in `PciDeviceInfo`:
  - Added `msi_cap_offset: Option<u8>` and `msix_cap_offset: Option<u8>`
  - Populated during PCI enumeration via single capability list walk
  - All capabilities logged during enumeration: `CAP: 0x{id} ({name}) at offset 0x{off}`
  - `pci_get_msi_capable_devices()` helper for downstream MSI/MSI-X consumers

### 3B: MSI Support

Program basic MSI (1–32 vectors per device).

- [x] **3B.1** Parse MSI capability structure:
  ```
  Offset+0: Cap ID (0x05) | Next
  Offset+2: Message Control (enable, multi-message capable/enable, 64-bit, per-vector masking)
  Offset+4: Message Address (low 32 bits)
  Offset+8: Message Address (high 32 bits, if 64-bit capable)
  Offset+C: Message Data
  ```
- [x] **3B.2** Implement `msi_configure(device, vector, cpu) -> Result<(), Error>`:
  - Message Address: `0xFEE00000 | (cpu_apic_id << 12)`
  - Message Data: `vector | (edge_trigger << 15) | (fixed_delivery << 8)`
  - Set enable bit in Message Control
  - Disable INTx via PCI command register when MSI is active
- [x] **3B.3** Allocate interrupt vectors for MSI:
  - Lock-free bitmap allocator in `core/src/irq.rs` using `AtomicU64` CAS
  - Vectors 32–47 reserved for legacy IOAPIC, 48–223 for MSI allocation (176 vectors)
  - `msi_alloc_vector() -> Option<u8>`, `msi_free_vector(vector: u8)`
- [x] **3B.4** Register MSI handler in IDT:
  - 176 assembly stubs generated via macro in `boot/idt_handlers.s`
  - Address table exported to Rust via `msi_vector_table` in `.rodata`
  - IDT entries installed in `idt_init()` (skipping SYSCALL_VECTOR 0x80)
  - MSI dispatch integrated into `irq_dispatch()` with handler table + EOI + scheduler handoff

### 3C: MSI-X Support

MSI-X is the preferred mechanism (more vectors, per-vector masking, table-based).

- [x] **3C.1** Parse MSI-X capability structure:
  - `msix_read_capability(bus, dev, func, cap_offset) -> MsixCapability`
  - Parses Message Control (table size, function mask, enable), Table Offset/BIR, PBA Offset/BIR
  - `MsixCapability` struct: `cap_offset`, `control`, `table_size` (1–2048), `table_bar`, `table_offset`, `pba_bar`, `pba_offset`
  - Implemented in `drivers/src/msix.rs`
- [x] **3C.2** Map MSI-X table:
  - `msix_map_table(device: &PciDeviceInfo, cap: &MsixCapability) -> Result<MsixTable, MsixError>`
  - Reads BIR to find which BAR contains the table; maps via `MmioRegion::map()`
  - Maps both MSI-X table (16 bytes × table_size) and PBA (⌈table_size/64⌉ × 8 bytes)
  - `MsixTable` struct with `read_vector_control()` and `is_pending()` accessors
  - Each table entry: `{ addr_low: u32, addr_high: u32, data: u32, vector_control: u32 }`
- [x] **3C.3** Implement `msix_configure(table, entry_idx, vector, target_apic_id) -> Result<(), MsixError>`:
  - Masks entry → writes LAPIC message address/data → unmasks entry
  - Same x86 message format as MSI (0xFEE00000 base + APIC ID shift)
  - `msix_mask_entry()` / `msix_unmask_entry()` for per-vector masking
  - `MsixError` enum: `InvalidVector`, `InvalidEntry`, `BarNotAvailable`, `MappingFailed`, `TableNotMapped`
- [x] **3C.4** Implement `msix_enable(device)` / `msix_disable(device)`:
  - `msix_enable()`: sets MSI-X enable bit, clears function mask, disables legacy INTx
  - `msix_disable()`: clears MSI-X enable bit, re-enables legacy INTx
  - `msix_set_function_mask()` / `msix_clear_function_mask()` for atomic bulk reconfiguration
  - `msix_refresh_control()` to re-read Message Control register

### Phase 3C Test Coverage

25 tests in `drivers/src/msix_tests.rs` covering:
- Capability parsing from QEMU VirtIO block (1af4:1042) and net (1af4:1041) devices
- Field consistency: table_size range (1–2048), BIR range (0–5), DWORD-aligned offsets
- Deterministic parsing across multiple reads
- Table mapping: MMIO region creation, accessor bounds checking (read_vector_control, is_pending)
- Entry configuration: valid vector programming, InvalidVector/InvalidEntry error paths
- Mask/unmask operations: per-entry masking with vector control bit verification, out-of-range rejection
- Enable/disable: config-space enable bit toggling, function mask toggling, refresh_control
- MsixCapability helper methods: is_enabled(), is_function_masked()
- Sweep of all MSI-X devices for field validity
- Negative test: SATA controller (8086:2922) correctly reports no MSI-X

### 3D: VirtIO MSI-X Integration

Wire MSI-X into the existing VirtIO drivers.

- [x] **3D.1** Update `drivers/src/virtio_blk.rs`:
  - During probe: check for MSI-X capability
  - If available: allocate vectors, configure MSI-X table entries
  - Map virtqueue interrupts to MSI-X vectors instead of legacy IRQ
- [x] **3D.2** Update `drivers/src/virtio_net.rs`:
  - Same treatment — MSI-X enables separate vectors for RX and TX queues
  - Multi-queue: each queue pair gets its own vector (enables parallel processing)
- [x] **3D.3** MSI-X required, MSI as minimum fallback; legacy polling removed
  - `InterruptMode::None` variant eliminated — probe panics if neither MSI-X nor MSI available
  - `setup_interrupts` returns `Result` — callers handle the error explicitly
  - `irq_mode` field removed from driver state structs (redundant with `msix_state`)

### 3E: Interrupt-Driven VirtIO Completion

Replace busy-wait polling with real MSI-X interrupt-driven I/O completion.

- [x] **3E.1** Remove dead ISR capability code:
  - `VIRTIO_PCI_CAP_ISR_CFG` constant, `isr_cfg` field, PCI cap parsing arm, debug log references
- [x] **3E.2** Add `QueueEvent` completion primitive (`drivers/src/virtio/mod.rs`):
  - `AtomicBool`-backed signal/reset/consume
  - `wait_timeout_ms()` using HPET deadline + x86 `cli; sti; hlt` wakeup pattern
  - Prevents lost-wakeup race: `sti` defers interrupt delivery until after `hlt`
- [x] **3E.3** Wire MSI-X IRQ handlers to signal `QueueEvent`:
  - `virtio-blk`: `BLK_QUEUE_EVENT` static, handler calls `.signal()`
  - `virtio-net`: `NET_RX_EVENT` + `NET_TX_EVENT` statics, handler uses queue-index ctx
- [x] **3E.4** Replace polling with event waits:
  - `poll_used()` removed from `queue.rs`
  - `pop_used()` → non-blocking `try_pop_used()` + `advance_used()`
  - `REQUEST_TIMEOUT_SPINS` (arbitrary spin count) → `REQUEST_TIMEOUT_MS` (5s real-time via HPET)
- [x] **3E.5** Expose `hpet::period_fs()` for deadline computation

### Phase 3 Gate

- [x] **GATE**: PCI capability list walking implemented
- [x] **GATE**: MSI can be configured for at least one device
- [x] **GATE**: MSI-X table mapped and entries programmable
- [x] **GATE**: VirtIO block device works with MSI-X (MSI minimum, legacy polling removed)
- [x] **GATE**: Vector allocator manages the MSI vector space
- [x] **GATE**: `just test` passes (533/533, including 25 MSI-X regression tests + 18 VirtIO MSI-X integration tests + 13 interrupt-driven completion tests)
- [x] **GATE**: Legacy IOAPIC routing still works for PS/2, serial (verified: 533/533 tests pass including IOAPIC-routed keyboard/serial tests)

---

## 7. Phase 4: PCIe ECAM Configuration Space

> **Replaces 1992 port I/O with memory-mapped PCIe config access.**
> **Kernel changes required**: Yes — ACPI MCFG parsing, ECAM MMIO driver
> **Difficulty**: Medium
> **Depends on**: Phase 3 benefits from this (extended config space for MSI-X)

### Background

Current PCI config access uses port I/O:
```rust
PCI_CONFIG_ADDRESS.write(addr);  // 0xCF8
PCI_CONFIG_DATA.read()           // 0xCFC
```

This gives access to only 256 bytes of config space per function. PCIe extended config space is 4096 bytes — needed for:
- Advanced Error Reporting (AER)
- Extended capabilities (some MSI-X features)
- SR-IOV, ACS, ATS, etc.

ECAM (Enhanced Configuration Access Mechanism) maps the entire config space into MMIO. The base address comes from the ACPI MCFG table.

### 4A: ACPI MCFG Table Parsing

- [ ] **4A.1** Add MCFG table parsing to `acpi/src/`:
  - Find `"MCFG"` signature in XSDT
  - Parse entries: each entry has `{ base_address: u64, segment_group: u16, start_bus: u8, end_bus: u8 }`
  - Export: `McfgEntry { base_phys: u64, segment: u16, bus_start: u8, bus_end: u8 }`
- [ ] **4A.2** Store MCFG data in `drivers/src/pci.rs`:
  - Static array of MCFG entries (typically 1 entry covering buses 0–255)
  - `ECAM_BASE: Option<u64>` for the primary segment

### 4B: ECAM MMIO Config Access

- [ ] **4B.1** Map the ECAM region via `MmioRegion::map()`:
  - Size: `(end_bus - start_bus + 1) * 256 * 4096` bytes
  - For buses 0–255: 256 * 256 * 4096 = 256MB (but QEMU often only exposes 0–63)
  - May need multiple mappings or lazy mapping
- [ ] **4B.2** Implement `pci_ecam_read32(bus, dev, func, offset) -> u32`:
  - Address: `ecam_base + (bus << 20) | (dev << 15) | (func << 12) | offset`
  - Use `read_volatile` (same pattern as IOAPIC/APIC MMIO)
  - Works for the full 4096-byte config space (not just 256 bytes)
- [ ] **4B.3** Implement `pci_ecam_write32(bus, dev, func, offset, value)`:
  - Same address calculation, `write_volatile`
- [ ] **4B.4** Create `PciConfigAccess` abstraction:
  ```rust
  enum PciConfigAccess {
      LegacyPortIo,        // 0xCF8/0xCFC (fallback)
      Ecam { base: u64 },  // MMIO
  }
  ```
  - `pci_config_read32()` / `pci_config_write32()` dispatch to the active backend
  - Prefer ECAM when available, fall back to port I/O if MCFG absent

### 4C: Extended Config Space Usage

- [ ] **4C.1** Update PCI enumeration to scan extended capabilities (offset 0x100+):
  - Extended capability list starts at offset 0x100
  - Each entry: `{ cap_id: u16, version: u4, next_offset: u12 }`
  - Only accessible via ECAM (not through port I/O)
- [ ] **4C.2** Implement `pci_find_ext_capability(bus, dev, func, cap_id) -> Option<u16>`
- [ ] **4C.3** Log extended capabilities during PCI enumeration:
  - `"PCI: BDF {}:{}.{} ext_cap 0x{:x} (name) at offset 0x{:x}"`

### Phase 4 Gate

- [ ] **GATE**: MCFG table parsed from ACPI
- [ ] **GATE**: ECAM MMIO region mapped and functional
- [ ] **GATE**: PCI config reads work through ECAM for the full 4096-byte space
- [ ] **GATE**: Legacy port I/O fallback still works when MCFG is absent
- [ ] **GATE**: Extended capability list scanned during enumeration
- [ ] **GATE**: `just test` passes
- [ ] **GATE**: `just boot` — PCI devices discovered correctly through ECAM

---

## 8. Phase 5: Network Stack Completion

> **TCP makes the network stack actually useful.**
> **Kernel changes required**: Yes — TCP state machine, socket layer
> **Difficulty**: Very High
> **Depends on**: Nothing (builds on existing VirtIO net + IPv4/UDP)

### Background

The current network stack has:
- VirtIO network driver (`drivers/src/virtio_net.rs`)
- Ethernet frame handling
- ARP (address resolution)
- IPv4 (packet parsing/construction)
- UDP (connectionless datagrams)
- ICMP (ping)
- DHCP client

Missing: **TCP** — the protocol that powers HTTP, SSH, DNS over TCP, and nearly all "real" networking.

### 5A: TCP State Machine

- [ ] **5A.1** Create `drivers/src/net/tcp.rs`:
  - Define TCP header structure (20 bytes minimum + options)
  - Parse/construct TCP segments with proper field handling
  - Implement checksum calculation (pseudo-header + TCP header + data)
- [ ] **5A.2** Implement the TCP state machine (RFC 793 + RFC 7413):
  - States: `CLOSED`, `LISTEN`, `SYN_SENT`, `SYN_RECEIVED`, `ESTABLISHED`, `FIN_WAIT_1`, `FIN_WAIT_2`, `CLOSE_WAIT`, `CLOSING`, `LAST_ACK`, `TIME_WAIT`
  - Transitions driven by incoming segments and user actions
- [ ] **5A.3** Implement the TCP connection table:
  - Key: `(local_ip, local_port, remote_ip, remote_port)`
  - Store connection state, sequence numbers, window size, retransmit queue
  - Max connections: start with 64
- [ ] **5A.4** Implement three-way handshake (active open):
  - `SYN` → `SYN_SENT` → receive `SYN+ACK` → send `ACK` → `ESTABLISHED`
- [ ] **5A.5** Implement three-way handshake (passive open / listen):
  - `LISTEN` → receive `SYN` → send `SYN+ACK` → `SYN_RECEIVED` → receive `ACK` → `ESTABLISHED`
- [ ] **5A.6** Implement connection teardown:
  - `FIN` handshake with `TIME_WAIT` timeout
  - `RST` handling for aborted connections

### 5B: TCP Data Transfer

- [ ] **5B.1** Implement send buffer:
  - Ring buffer per connection (e.g., 16KB)
  - Track `SND.UNA`, `SND.NXT`, `SND.WND` per RFC 793
  - Segment outgoing data into MSS-sized chunks
- [ ] **5B.2** Implement receive buffer:
  - Ring buffer per connection
  - Track `RCV.NXT`, `RCV.WND`
  - Handle out-of-order segments (simple: drop and let sender retransmit)
- [ ] **5B.3** Implement acknowledgment:
  - Delayed ACK (200ms timer or every other segment)
  - Cumulative ACK
- [ ] **5B.4** Implement retransmission:
  - Retransmission timeout (RTO) with exponential backoff
  - Start with fixed 1s RTO, later implement Karn/Partridge algorithm
- [ ] **5B.5** Implement flow control:
  - Window size advertisement in ACK segments
  - Respect remote window size when sending
  - Zero window probing

### 5C: Socket Abstraction Layer

- [ ] **5C.1** Define socket syscall interface in `abi/src/syscall.rs`:
  - `SYSCALL_SOCKET` — create a socket (AF_INET, SOCK_STREAM / SOCK_DGRAM)
  - `SYSCALL_BIND` — bind to local address/port
  - `SYSCALL_LISTEN` — mark socket as listening
  - `SYSCALL_ACCEPT` — accept incoming connection
  - `SYSCALL_CONNECT` — initiate TCP connection
  - `SYSCALL_SEND` / `SYSCALL_RECV` — transfer data
  - `SYSCALL_CLOSE` — close socket (reuse existing close syscall)
- [ ] **5C.2** Implement socket file descriptors:
  - Sockets are file descriptors (POSIX model)
  - `read()` / `write()` on a socket FD maps to `recv()` / `send()`
  - Integrate with `poll()` for event-driven I/O
- [ ] **5C.3** Implement kernel socket structures in `core/src/net/` (new crate or module):
  - `Socket { domain, sock_type, protocol, state, local_addr, remote_addr, ... }`
  - Per-process socket table (or integrate with file descriptor table)
- [ ] **5C.4** Add userland wrappers in `userland/src/syscall/`:
  - `socket()`, `bind()`, `listen()`, `accept()`, `connect()`, `send()`, `recv()`
  - These mirror the POSIX socket API

### 5D: DNS Client (Optional, Stretch)

- [ ] **5D.1** Implement DNS query construction (UDP port 53)
- [ ] **5D.2** Parse DNS response (A records, CNAME)
- [ ] **5D.3** Simple resolver: `resolve(hostname) -> Option<Ipv4Addr>`

### Phase 5 Gate

- [ ] **GATE**: TCP three-way handshake works (connect to a remote server)
- [ ] **GATE**: TCP data transfer works (send/receive payloads)
- [ ] **GATE**: Socket syscalls implemented (socket, connect, send, recv, close)
- [ ] **GATE**: Simple TCP echo test passes (connect, send, receive echo, close)
- [ ] **GATE**: `just test` passes
- [ ] **GATE**: Can connect to a TCP server from QEMU guest (e.g., netcat)

---

## 9. Phase 6: PCID / TLB Optimization

> **Avoids unnecessary TLB flushes on context switch.**
> **Kernel changes required**: Yes — page table management, context switch
> **Difficulty**: Medium
> **Depends on**: Nothing (self-contained, but best after Phase 0 and 2)

### Background

PCID (Process-Context Identifiers) tags TLB entries with a 12-bit ID, allowing multiple address spaces to coexist in the TLB. Without PCID, every `CR3` write flushes the entire TLB.

SlopOS already detects PCID and INVPCID support in `mm/src/tlb.rs`:
```rust
pcid_supported    // from CPUID
invpcid_supported // from CPUID
```

But these are never used — every context switch does a full TLB flush.

### 6A: PCID Allocation

- [ ] **6A.1** Create PCID allocator in `mm/src/tlb.rs`:
  - 12-bit PCID → 4096 possible values (0 is reserved for kernel)
  - Simple bitmap allocator: `pcid_bitmap: [AtomicU64; 64]` (4096 bits)
  - `alloc_pcid() -> Option<u16>`, `free_pcid(pcid: u16)`
- [ ] **6A.2** Assign PCID to each process:
  - Add `pcid: u16` to the process/task struct
  - Allocate on process creation, free on process exit
  - Kernel always uses PCID 0
- [ ] **6A.3** Handle PCID exhaustion:
  - If all 4095 PCIDs are in use: global TLB flush + reset all PCIDs
  - This is the "PCID generation" approach (used by Linux)

### 6B: PCID-Aware CR3 Writes

- [ ] **6B.1** Modify context switch to set PCID in CR3:
  - CR3 format with PCID: `[bits 63: noflush] [bits 51:12: PML4 phys] [bits 11:0: PCID]`
  - Set bit 63 (noflush) to avoid flushing the TLB on CR3 write
  - The new process's TLB entries are already tagged with its PCID
- [ ] **6B.2** Update `core/context_switch.s` to write PCID-aware CR3:
  - Load `task.cr3` with PCID embedded in low 12 bits
  - Set noflush bit (bit 63) in the value written to CR3
- [ ] **6B.3** INVPCID-based selective flush:
  - Replace `invlpg` with `invpcid` (type 0: individual address + PCID)
  - TLB shootdown sends PCID along with virtual address
  - `invpcid` type 1: flush all entries for a given PCID (process exit)

### 6C: TLB Shootdown Update

- [ ] **6C.1** Update `mm/src/tlb.rs` flush request to include PCID:
  - `FlushRequest { flush_type, vaddr, pcid }` — add PCID field
  - Receiving CPU uses `invpcid` with the PCID to flush only the relevant entries
- [ ] **6C.2** On process exit: flush all TLB entries for that PCID across all CPUs
  - `invpcid` type 1 (single-context invalidation)
- [ ] **6C.3** Fallback: if INVPCID not supported, use `invlpg` + full flush on CR3 switch

### Phase 6 Gate

- [ ] **GATE**: PCID allocated per process, freed on exit
- [ ] **GATE**: Context switch writes PCID-aware CR3 with noflush bit
- [ ] **GATE**: TLB entries survive context switches (measured: fewer TLB misses)
- [ ] **GATE**: TLB shootdown uses INVPCID when available
- [ ] **GATE**: Fallback to non-PCID path works on older CPUs
- [ ] **GATE**: `just test` passes
- [ ] **GATE**: No TLB-related crashes or stale mapping bugs

---

## 10. Phase 7: Long-Horizon Hardware

> **Major new subsystems that complete the hardware story.**
> **These are massive efforts — each could be its own plan document.**
> **Depends on**: Phases 0–4 as foundation

### 7A: USB / xHCI Stack

Replaces PS/2 as the input mechanism and enables USB mass storage, USB networking, etc.

- [ ] **7A.1** Implement xHCI (USB 3.x) host controller driver:
  - Discover xHCI via PCI (class 0x0C, subclass 0x03, progif 0x30)
  - Map MMIO registers (capability, operational, runtime, doorbell)
  - Initialize: reset controller, set up device context base array, configure interrupter
  - Command ring + event ring + transfer ring management
- [ ] **7A.2** Implement USB device enumeration:
  - Address assignment, device descriptor reading
  - Configuration descriptor parsing
  - Interface and endpoint descriptor handling
- [ ] **7A.3** Implement USB HID driver (keyboard + mouse):
  - HID report descriptor parsing (boot protocol as minimum)
  - Interrupt IN endpoint for key events
  - Replace PS/2 keyboard/mouse as primary input
- [ ] **7A.4** Implement USB mass storage driver (optional):
  - Bulk-Only Transport (BOT) protocol
  - SCSI command set (INQUIRY, READ, WRITE)
  - Integrate with VFS as block device

**Estimated effort**: Very High (months of work). Consider as post-MVP.

### 7B: VirtIO GPU (Replace Raw Framebuffer)

- [ ] **7B.1** Implement VirtIO GPU driver:
  - Resource management (create, attach backing, transfer)
  - Scanout configuration
  - Cursor support
  - 2D acceleration (blit, fill)
- [ ] **7B.2** Integrate with compositor:
  - Replace direct framebuffer writes with VirtIO GPU commands
  - Enables hardware-accelerated compositing
  - Double buffering without manual page flipping

### 7C: Hardware RTC (Real-Time Clock)

- [ ] **7C.1** Parse ACPI FADT for RTC info (or use CMOS RTC at ports 0x70/0x71)
- [ ] **7C.2** Read current date/time on boot
- [ ] **7C.3** Combine with HPET monotonic clock for wall-clock time
- [ ] **7C.4** Implement `SYSCALL_CLOCK_GETTIME` with `CLOCK_REALTIME`

### 7D: Power Management

- [ ] **7D.1** Parse ACPI FADT for PM1a/PM1b control block addresses
- [ ] **7D.2** Implement proper ACPI S5 (soft-off) shutdown:
  - Currently uses hardcoded QEMU ports (`0x604`, `0xB004`, `0x4004`)
  - Should parse SLP_TYP from `_S5_` ACPI object and write to PM1a_CNT
- [ ] **7D.3** Implement ACPI S3 (sleep) support (stretch)

### 7E: `pic.rs` Cleanup

- [ ] **7E.1** Inline `pic_quiesce_disable()` into `boot/src/shutdown.rs`
- [ ] **7E.2** Delete `drivers/src/pic.rs` entirely
- [ ] **7E.3** Remove `pub mod pic;` from `drivers/src/lib.rs`
- [ ] **7E.4** The legacy 8259 PIC path is now truly gone — not even a stub remains

---

## 11. Dependency Graph

```
Phase 0: Timers ─────────────┐
  (HPET + LAPIC timer)       │
                              │
Phase 1: XSAVE ──────────────┤ (all independent)
  (FPU/SIMD modernization)   │
                              │
Phase 2: Spinlocks ───────────┤
  (ticket locks)              │
                              │
Phase 3: MSI/MSI-X ──────────┤──→ Phase 4: ECAM ──→ Phase 7A: USB/xHCI
  (PCI cap parsing first)    │     (MCFG parsing)    (needs ECAM + MSI)
                              │
Phase 5: TCP ─────────────────┤ (independent, builds on existing VirtIO net)
                              │
Phase 6: PCID ────────────────┘ (independent, best after Phase 0+2)

Phase 7: Long-horizon
  7A: USB/xHCI      ← needs Phase 3 (MSI) + Phase 4 (ECAM)
  7B: VirtIO GPU    ← needs Phase 3 (MSI)
  7C: RTC           ← independent
  7D: Power Mgmt    ← independent (ACPI parsing exists)
  7E: pic.rs cleanup ← independent (trivial)
```

### Recommended Execution Order

| Order | Phase | Rationale |
|---|---|---|
| 1st | Phase 0 (Timers) | Highest impact, removes biggest legacy dependency |
| 2nd | Phase 2 (Spinlocks) | Low effort, immediate SMP improvement |
| 3rd | Phase 1 (XSAVE) | Medium effort, unlocks AVX for userland |
| 4th | Phase 4 (ECAM) | Foundation for Phase 3 (extended config space) |
| 5th | Phase 3 (MSI/MSI-X) | Requires PCI cap parsing, benefits from ECAM |
| 6th | Phase 6 (PCID) | Performance optimization, best after scheduler is stable |
| 7th | Phase 5 (TCP) | Massive effort, independent track |
| 8th | Phase 7 (Long-horizon) | Post-MVP work |

---

## 12. Blocked Features Reference

Features that **cannot be implemented** until specific phases complete:

| Feature | Blocked By | Phase Required |
|---|---|---|
| Precise sleep/timeout | No high-resolution clock | Phase 0 (HPET) |
| Per-CPU scheduler ticks | PIT is single-IRQ shared | Phase 0 (LAPIC timer) |
| AVX in userland programs | FXSAVE can't save YMM regs | Phase 1 (XSAVE) |
| AVX-512 | FXSAVE + target JSON | Phase 1 (XSAVE) |
| Fair lock acquisition on SMP | Test-and-set has no fairness | Phase 2 (Ticket locks) |
| Per-device interrupt vectors | Legacy shared IRQ lines | Phase 3 (MSI) |
| VirtIO multi-queue networking | Needs per-queue MSI-X vectors | Phase 3 (MSI-X) |
| Full PCIe config space (4KB) | Port I/O limited to 256B | Phase 4 (ECAM) |
| HTTP, SSH, any TCP protocol | No TCP state machine | Phase 5 (TCP) |
| TLB-efficient context switch | PCID unused, full flush every switch | Phase 6 (PCID) |
| USB keyboard/mouse | No xHCI driver | Phase 7A (USB) |
| Hardware-accelerated graphics | No VirtIO GPU driver | Phase 7B |
| Wall-clock time / date | No RTC integration | Phase 7C |
| Clean ACPI shutdown (non-QEMU) | Hardcoded port addresses | Phase 7D |

---

## Progress Tracking

| Phase | Status | Tasks | Done | Blocked |
|---|---|---|---|---|
| **Phase 0**: Timer Modernization | **Complete** | 31 | 31 | — |
| **Phase 1**: XSAVE/XRSTOR | **Complete** | 14 | 14 | — |
| **Phase 2**: Spinlock Modernization | **Complete** (2C MCS deferred, `spin` removed) | 12 | 12 | — |
| **Phase 3**: MSI/MSI-X | **Complete** (3A, 3B, 3C, 3D, 3E all done) | 27 | 27 | — |
| **Phase 4**: PCIe ECAM | Not Started | 9 | 0 | — |
| **Phase 5**: TCP Networking | Not Started | 17 | 0 | — |
| **Phase 6**: PCID / TLB | Not Started | 9 | 0 | — |
| **Phase 7**: Long-Horizon | Not Started | 16 | 0 | Phases 0–4 |
| **Total** | | **116** | **53** | |
