# SlopOS TTY Overhaul Plan

> **Status**: Phases 1–11 Complete · **Phases 12–14 Planned** (Defaults, ABI Cleanup, Responsibility Split) · Phase 15 Verify & Test
> **Target**: Replace the global singleton TTY with a proper per-terminal TTY subsystem comparable to Linux N_TTY / RedoxOS
> **Current**: `drivers/src/tty/` module directory — clean per-TTY API, no backward-compatible shims, `TtyServices` takes `TtyIndex` for per-TTY operations, compositor focus split from POSIX foreground, `check_read()` as sole read gate
> **Bugs Addressed**: Double-typing on PS/2 keyboard, nc immediate termination, dual input delivery, blocked-reader wakeup regression (PS/2/TTY reads)

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Current State Assessment](#2-current-state-assessment)
3. [Bug Analysis](#3-bug-analysis)
4. [Target Architecture](#4-target-architecture)
5. [Phase 1: TTY Core Abstraction](#5-phase-1-tty-core-abstraction)
6. [Phase 2: Enhanced Line Discipline](#6-phase-2-enhanced-line-discipline)
7. [Phase 3: Input Pipeline Cleanup](#7-phase-3-input-pipeline-cleanup)
8. [Phase 4: Session & Process Group Management](#8-phase-4-session--process-group-management)
9. [Phase 5: FD Integration](#9-phase-5-fd-integration)
10. [Phase 6: Control-Plane Correctness](#10-phase-6-control-plane-correctness)
11. [Phase 7: Lifecycle & Hangup Semantics](#11-phase-7-lifecycle--hangup-semantics)
12. [Phase 8: Per-TTY Locking & Performance](#12-phase-8-per-tty-locking--performance)
13. [Phase 9: Rust Idioms & Termios Completion](#13-phase-9-rust-idioms--termios-completion)
14. [Phase 10: Job Control Correctness](#14-phase-10-job-control-correctness)
15. [Phase 11: Non-Canonical Timing Fix](#15-phase-11-non-canonical-timing-fix)
16. [Phase 12: Sane Defaults & Output Column Tracking ✅](#16-phase-12-sane-defaults--output-column-tracking)
17. [Phase 13: ABI Signal Constant Unification](#17-phase-13-abi-signal-constant-unification)
18. [Phase 14: Responsibility Split — PTY Foundation](#18-phase-14-responsibility-split--pty-foundation)
19. [Phase 15: Verify & Test](#19-phase-15-verify--test)
20. [File Inventory](#20-file-inventory)
21. [Future: PTY Support](#21-future-pty-support)

---

## 1. Executive Summary

The SlopOS TTY subsystem is currently a **global singleton line discipline** behind a single `IrqMutex`, with an ad-hoc focus system using bare atomics. It works for single-terminal use but has architectural issues that cause:

- **Double-typing bug**: Each PS/2 keystroke must be typed twice for one character to register
- **nc immediate termination**: Foreground child processes get killed unexpectedly on Enter
- **No multi-terminal support**: Single global `LINE_DISC`, single `TTY_WAIT_QUEUE`
- **Missing POSIX TTY semantics**: No sessions, no controlling terminal, no job control signals beyond SIGINT

This plan replaces the singleton with a proper **per-terminal TTY subsystem** modeled after Linux's `tty_struct` + `n_tty` line discipline, adapted for SlopOS's `no_std` Rust environment.

### Summary of changes

| Phase | What | Files Modified | New Files |
|-------|------|---------------|-----------|
| 1 | TTY core structs | `drivers/src/tty.rs` (deleted), `drivers/src/line_disc.rs` (deleted), `drivers/src/lib.rs` | `drivers/src/tty/mod.rs`, `tty/driver.rs`, `tty/table.rs`, `tty/ldisc.rs`, `tty/session.rs`, `drivers/src/tty_tests.rs` | **DONE** |
| 1b | Shim removal | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/ps2/keyboard.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `core/src/syscall/core_handlers.rs`, `core/src/syscall/ui_handlers.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `fs/src/fileio.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 2 | Line discipline | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 3 | Input pipeline | `drivers/src/ps2/keyboard.rs`, `drivers/src/input_event.rs` | — | **DONE** |
| 4 | Sessions/pgrps | `drivers/src/tty/session.rs`, `drivers/src/tty/mod.rs`, `abi/src/syscall.rs`, `lib/`, `core/`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 5 | FD integration | `fs/src/fileio.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 6 | Control-plane correctness | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `drivers/src/tty/ldisc.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `core/src/syscall/ui_handlers.rs`, `core/src/syscall/core_handlers.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 7 | Lifecycle & hangup | `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty/ldisc.rs`, `core/src/scheduler/task.rs`, `core/src/scheduler/task_struct.rs`, `core/src/syscall/process_handlers.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs`, `core/src/syscall/tests.rs` | — | **DONE** |
| 8 | Per-TTY locking & perf | `drivers/src/tty/table.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 9 | Rust idioms & termios | `drivers/src/tty/mod.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/syscall_services_init.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/driver_runtime.rs`, `core/src/driver_hooks.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 10 | Job control correctness | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `abi/src/syscall.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
| 11 | Non-canonical timing fix | `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — | **DONE** |
|| 12 | Sane defaults & column tracking | `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — |
|| 13 | ABI signal unification | `abi/src/syscall.rs`, `abi/src/signal.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/mod.rs` | — |
|| 14 | Responsibility split (PTY prep) | `drivers/src/tty/mod.rs`, `drivers/src/tty/driver.rs`, `drivers/src/tty/session.rs`, `drivers/src/tty/ldisc.rs` | — |
|| 15 | Verification & testing | — | — |

---

## 2. Current State Assessment

### What Exists

| Component | Location | Lines | Description |
|-----------|----------|-------|-------------|
| TTY driver | `drivers/src/tty.rs` | 373 | Global singleton, wait queues, focus system, `tty_read_cooked`, `tty_handle_input_char` |
| Line discipline | `drivers/src/line_disc.rs` | 183 | Basic `LineDisc` struct with edit buffer, cooked ring buffer, ICANON/ECHO/ISIG |
| TTY syscall glue | `lib/src/kernel_services/syscall_services/tty.rs` | ~50 | Thin wrappers calling `drivers::tty::*` |
| Userland TTY syscalls | `userland/src/syscall/tty.rs` | ~30 | `tcgetattr`, `tcsetattr`, `set_foreground_pgrp` |
| ABI types | `abi/src/syscall.rs:567-636` | ~70 | `UserTermios`, `UserWinsize`, TCGETS/TCSETS/TIOCGPGRP/etc, cc indices |
| Keyboard driver | `drivers/src/ps2/keyboard.rs` | 328 | PS/2 scancode handling, calls BOTH `input_route_key_event` AND `tty_handle_input_char` |
| Input event system | `drivers/src/input_event.rs` | 437 | Compositor/Wayland-style per-task input queues |
| Serial driver | `drivers/src/serial.rs` | 260 | Polling-based UART, `INPUT_BUFFER` ring, no IRQ handler |
| FD console routing | `fs/src/fileio.rs:495-543` | ~50 | Console FDs (0,1,2) bootstrap, read/write/poll routing |
| Poll/ioctl handlers | `core/src/syscall/fs/poll_ioctl_handlers.rs` | ~200 | TCGETS/TCSETS/TIOCGPGRP dispatch |

### What's Missing vs Linux/RedoxOS

| Feature | Linux | RedoxOS | SlopOS |
|---------|-------|---------|--------|
| Per-terminal state | `struct tty_struct` | `ptyd` daemon | ❌ Global singleton |
| Driver abstraction | `struct tty_driver` + ops | scheme:// URIs | ❌ Hardcoded serial output |
| Line discipline | `n_tty.c` (2800 lines) | In `ptyd` | ⚠️ Basic (183 lines) |
| Output processing | OPOST, ONLCR, OCRNL, etc. | Yes | ❌ None |
| Input flags | ICRNL, INLCR, IGNCR, ISTRIP, IXON/IXOFF | Yes | ❌ None |
| Echo modes | ECHO, ECHOCTL, ECHOKE, ECHOPRT | Yes | ⚠️ ECHO, ECHOE, ECHOK only |
| VMIN/VTIME | Full non-canonical timing | Yes | ✅ Enforced (all 4 POSIX cases) |
| Controlling terminal | Per-process `/dev/tty` | Per-process scheme | ✅ `/dev/tty` resolves to controlling terminal |
| Sessions | `setsid()`, session leader | Scheme-based | ❌ None |
| Job control signals | SIGTTIN, SIGTTOU, SIGTSTP | Yes | ❌ Only SIGINT |
| PTY | Full master/slave | `ptyd` | ❌ None |
| Multiple terminals | VT switching, unlimited PTYs | Arbitrary schemes | ❌ Single terminal |

---

## 3. Bug Analysis

### 3.1 Double-Typing Bug (PS/2 Keyboard via QEMU Graphical Window)

**Symptoms**: Each character must be typed twice to register once when using the QEMU graphical window (PS/2 keyboard input).

**Root Cause Analysis**: The keyboard interrupt handler in `drivers/src/ps2/keyboard.rs:202-297` has a **dual delivery** architecture:

```
PS/2 Interrupt
    ├── Line 227: input_route_key_event()  →  Input event queue (compositor)
    │                                          Shell drains via poll_batch()
    │                                          but IGNORES keyboard events
    │
    └── Line 294: tty_handle_input_char()  →  LINE_DISC cooked buffer
                                               Shell reads via fd 0
```

While the shell correctly ignores keyboard events from the input queue, the **focus system** (`tty_task_has_focus` at `tty.rs:196-203`) creates a race:

1. `tty_read_cooked` checks `tty_task_has_focus(task_id)` before reading
2. Focus is determined by `TTY_FOCUSED_TASK_ID` (set lazily) and `TTY_FOREGROUND_PGRP` (compares pgrp ID with task ID — semantically wrong)
3. When the shell spawns a child (like nc), focus may not transfer correctly
4. The `tty_wait_for_focus()` call blocks the reader, causing the next poll cycle to miss the character

Additionally, `tty_drain_hw_input()` is called from multiple contexts (poll, read, block_until_ready) which creates redundant drain cycles. Combined with the single global `TTY_WAIT_QUEUE`, this means a `tty_notify_input_ready()` wakeup may reach the wrong blocked task.

**Fix**: Phases 1 + 3 + 4 eliminate all of these issues by:
- Per-TTY wait queues (no cross-terminal wakeup confusion)
- Clean single input path (keyboard → TTY only)
- Proper session/pgrp-based foreground determination

### 3.2 NC Immediate Termination

**Symptoms**: nc connects to host, user types "hello" + Enter, nc immediately dies with `Terminating task 'nc' (ID 9)` — no nc error message, no data sent to host.

**Root Cause Analysis**: Two contributing factors:

1. **Signal delivery**: When Enter is pressed, `process_raw_char` feeds `'\n'` to the line discipline. If the TTY is still in canonical mode (the `tcsetattr` from nc may race with the first keystroke), `'\n'` triggers `flush_edit_to_cooked`. But more critically, the ad-hoc focus system may route the wakeup to the shell's blocked `waitpid` reader instead of nc, causing the shell to think nc has exited.

2. **TCP dst_mac bug** (separate from TTY, noted here): `drivers/src/net/socket.rs:830` hardcodes `let dst_mac = [0xff; 6]` (broadcast MAC) for TCP segments, while the rest of the IP stack uses proper ARP neighbor resolution (`drivers/src/net/ipv4.rs:275`). This means even if nc successfully sends data, the TCP data frames use broadcast destination MAC instead of the resolved next-hop MAC.

**Fix**: Phase 4 (proper session management) ensures child processes correctly inherit the controlling terminal and foreground group, preventing spurious termination. The TCP dst_mac fix is out of scope for this plan but should be addressed separately.

---

## 4. Target Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         USERLAND                                │
│  ┌────────┐  ┌──────┐  ┌────────┐  ┌─────────────────────────┐ │
│  │ Shell  │  │  nc  │  │ editor │  │  Future: PTY clients    │ │
│  └───┬────┘  └──┬───┘  └───┬────┘  └────────────┬────────────┘ │
│      │ read/write/ioctl(fd 0,1,2)                │              │
└──────┼──────────┼───────────┼────────────────────┼──────────────┘
       │  SYSCALL │           │                    │
┌──────▼──────────▼───────────▼────────────────────▼──────────────┐
│  fileio.rs — per-process FD table                                │
│  FileDescriptor.tty_index → indexes into TTY_TABLE               │
│      │                                                           │
│  ┌───▼───────────────────────────────────────────────────────┐   │
│  │  TTY Subsystem  (drivers/src/tty/)                        │   │
│  │                                                           │   │
│  │  ┌──────────────────┐  ┌──────────────────┐               │   │
│  │  │  Tty[0]          │  │  Tty[1]          │  ...          │   │
│  │  │  ┌────────────┐  │  │  ┌────────────┐  │               │   │
│  │  │  │  LineDisc   │  │  │  │  LineDisc   │  │               │   │
│  │  │  │  - termios  │  │  │  │  - termios  │  │               │   │
│  │  │  │  - edit_buf │  │  │  │  - edit_buf │  │               │   │
│  │  │  │  - cooked   │  │  │  │  - cooked   │  │               │   │
│  │  │  └────────────┘  │  │  └────────────┘  │               │   │
│  │  │  session_id      │  │  session_id      │               │   │
│  │  │  fg_pgrp         │  │  fg_pgrp         │               │   │
│  │  │  wait_queue      │  │  wait_queue      │               │   │
│  │  │  ┌────────────┐  │  │  ┌────────────┐  │               │   │
│  │  │  │ Driver:    │  │  │  │ Driver:    │  │               │   │
│  │  │  │ SerialCon  │  │  │  │ VConsole   │  │               │   │
│  │  │  │ (COM1)     │  │  │  │ (PS/2+FB)  │  │               │   │
│  │  │  └────────────┘  │  │  └────────────┘  │               │   │
│  │  └──────────────────┘  └──────────────────┘               │   │
│  │                                                           │   │
│  │  TTY_TABLE: [Option<Tty>; MAX_TTYS]                       │   │
│  └───────────────────────────────────────────────────────────┘   │
│                                                                  │
│  Input Event System (unchanged — compositor/mouse only)          │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │  input_event.rs — per-task queues for pointer/window      │   │
│  │  (keyboard events NO LONGER routed here)                  │   │
│  └───────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### Key Design Decisions

1. **Per-TTY `LineDisc`**: Each `Tty` owns its own line discipline instance. No global singleton.

2. **`TtyDriver` trait**: Abstracts the hardware backend. Two initial implementations:
   - `SerialConsoleDriver` — wraps COM1 UART (polling-based, as today)
   - `VConsoleDriver` — wraps PS/2 keyboard + framebuffer output

3. **Single input path**: The keyboard interrupt handler feeds the **active TTY** only. `input_route_key_event()` is NO LONGER called for keyboard input — it remains for pointer/window events only.

4. **Per-TTY wait queue**: Each `Tty` has its own `WaitQueue`. `tty_notify_input_ready()` wakes only tasks blocked on that specific TTY.

5. **Session-based foreground**: Foreground process group is stored per-TTY. `tty_read_cooked` checks the calling process's session and pgrp against the TTY's foreground pgrp.

6. **Controlling terminal per-process**: Each process stores a `controlling_tty: Option<TtyIndex>`. Set on first open or inherited from parent via fork.

---

## 5. Phase 1: TTY Core Abstraction ✅ COMPLETED

**Status**: Completed. All 26 TTY regression tests pass. Build clean. `just test` passes (944/944).

**Post-Phase-1 Hotfix (Read/Wakeup Regression)**: Completed. `tty::read()` now blocks on per-TTY wait queues using `WaitQueue::wait_event(...)` instead of raw `block_current_task()`. Input arrival (`push_input`/`notify_input_ready`) now wakes the matching per-TTY wait queue, and `set_focus()` wakes blocked readers so focus handoff no longer relies on idle-loop reschedule side effects.

**Implementation note (lock ordering)**: In SlopOS, per-TTY wait queues are stored in a separate static array (`TTY_INPUT_WAITERS`) indexed by `TtyIndex`, while line discipline/session state remains in `TTY_TABLE`. This matches existing SlopOS socket wait-queue patterns and avoids sleeping while holding the `TTY_TABLE` lock.

**Operational note (until Phase 4/5)**: Read-side focus gating is temporarily relaxed so stdin readers on console FDs are not hard-blocked by compositor focus state while FD routing is still fixed to TTY 0. Proper job-control/session enforcement remains Phase 4/5 work.

**Phase 1b (Shim Removal)**: All backward-compatible shims removed. TtyServices now takes `tty_index: u8` for per-TTY operations. Focus state moved from global atomics into per-TTY `TtySession.focused_task_id`. Compat wait queue deleted. Keyboard driver calls `push_input(active_tty(), c)` directly. `TTY_WINSIZE` global in `poll_ioctl_handlers.rs` replaced with per-TTY `get_winsize`/`set_winsize` via TtyServices.

**Goal**: Replace the global singleton with `Tty` struct, `TtyDriver` trait, and `TTY_TABLE`.

### 5.1 New file structure

Convert `drivers/src/tty.rs` (single file) → `drivers/src/tty/` (module directory):

```
drivers/src/tty/
├── mod.rs           # Public API: tty_read, tty_write, tty_poll, tty_ioctl
├── driver.rs        # TtyDriver trait + SerialConsoleDriver + VConsoleDriver
├── table.rs         # TTY_TABLE: global array of Tty instances
├── ldisc.rs         # Enhanced LineDisc (Phase 2, but file created here)
├── session.rs       # Session/pgrp management (Phase 4, but file created here)
└── wait_queue.rs    # Per-TTY WaitQueue (extracted from current tty.rs)
```

### 5.2 Core types

```rust
// drivers/src/tty/mod.rs

/// Index into the global TTY table.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TtyIndex(pub u8);

pub const MAX_TTYS: usize = 8;

/// The central TTY structure — one per terminal.
pub struct Tty {
    /// Which TTY slot this is (0 = serial console, 1 = virtual console, etc.)
    pub index: TtyIndex,

    /// The line discipline owned by this TTY.
    pub ldisc: LineDisc,

    /// Hardware driver backend.
    pub driver: TtyDriverKind,

    /// Session ID that owns this TTY (0 = no session).
    pub session_id: u32,

    /// Foreground process group ID.
    pub fg_pgrp: u32,

    /// Wait queue for tasks blocked on input.
    pub input_waiters: WaitQueue,

    /// Window size (for TIOCGWINSZ / TIOCSWINSZ).
    pub winsize: UserWinsize,

    /// Whether this TTY is active/allocated.
    pub active: bool,
}
```

### 5.3 TtyDriver trait

```rust
// drivers/src/tty/driver.rs

/// Backend driver operations for a TTY.
pub trait TtyDriver {
    /// Write output bytes to the terminal hardware (serial port, framebuffer, etc.)
    fn write_output(&self, buf: &[u8]);

    /// Poll for pending hardware input. Returns bytes available.
    /// Called by tty_drain_hw_input.
    fn drain_input(&self, out: &mut [u8]) -> usize;

    /// Optional: called when termios changes (e.g., baud rate).
    fn set_termios(&self, _termios: &UserTermios) {}
}

/// Enum dispatch to avoid trait objects (no_std, no alloc).
pub enum TtyDriverKind {
    SerialConsole(SerialConsoleDriver),
    VConsole(VConsoleDriver),
    None,
}
```

### 5.4 Migration steps

1. Create `drivers/src/tty/` directory and move `tty.rs` → `tty/mod.rs`
2. Create `driver.rs` with `TtyDriver` trait and two implementations
3. Create `table.rs` with `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>`
4. Create `wait_queue.rs` — extract `TtyWaitQueue` from current code
5. Create `ldisc.rs` — move `line_disc.rs` content (will be enhanced in Phase 2)
6. Create `session.rs` — stub for Phase 4
7. Update `drivers/src/lib.rs` to use the module instead of single file
8. Update all call sites: `tty::tty_read_cooked` → `tty::read(tty_index, ...)`
9. Initialize TTY 0 (serial console) and TTY 1 (virtual console) at boot
10. `tty_handle_input_char` → `tty::push_input(tty_index, c)`

### 5.5 Compatibility shim

During migration, provide backward-compatible public functions that delegate to TTY 0 (serial console):

```rust
// Temporary shim — will be removed in Phase 5
pub fn tty_read_cooked(buffer: *mut u8, max: usize, nonblock: bool) -> isize {
    read(TtyIndex(0), buffer, max, nonblock)
}

pub fn tty_handle_input_char(c: u8) {
    push_input(TtyIndex(0), c);
}
```

### 5.6 Files modified (actual)

| File | Change |
|------|--------|
| `drivers/src/tty.rs` | **Deleted** — replaced by `drivers/src/tty/mod.rs` |
| `drivers/src/line_disc.rs` | **Deleted** — moved to `drivers/src/tty/ldisc.rs` |
| `drivers/src/lib.rs` | Removed `pub mod line_disc`, added `#[cfg(feature = "itests")] pub mod tty_tests` |
| `drivers/src/tty/mod.rs` | **New** — `Tty` struct, `TtyIndex`, per-TTY API, backward-compatible shims |
| `drivers/src/tty/driver.rs` | **New** — `TtyDriver` trait, `TtyDriverKind` enum, `SerialConsoleDriver`, `VConsoleDriver` |
| `drivers/src/tty/table.rs` | **New** — `TTY_TABLE` global, `tty_table_init()`, `with_tty()` helpers |
| `drivers/src/tty/ldisc.rs` | **New** — `LineDisc` (moved from `line_disc.rs`, identical logic) |
| `drivers/src/tty/session.rs` | **New** — `TtySession` stub for Phase 4 |
| `drivers/src/tty_tests.rs` | **New** — 26 regression tests (LineDisc, TtyIndex, TtyDriverKind, TTY table, compat shims) |
| `drivers/src/ps2/keyboard.rs` | Unchanged — `crate::tty::tty_handle_input_char` path still valid |
| `drivers/src/syscall_services_init.rs` | Unchanged — `crate::tty` path still valid |
| `lib/src/kernel_services/syscall_services/tty.rs` | Unchanged — no import changes needed |
| `fs/src/fileio.rs` | Unchanged — calls `tty::read_cooked` via kernel services layer |

---

## 6. Phase 2: Enhanced Line Discipline ✅ COMPLETED

**Status**: Completed. All 962 tests pass (14 new Phase 2 regression tests). Build clean. `just test` passes.

**Implementation summary**: `LineDisc` rewritten from 216 lines to ~590 lines with:
- Input flag processing (`c_iflag`): ICRNL, INLCR, IGNCR, ISTRIP, IXON flow control
- Output flag processing (`c_oflag`): OPOST, ONLCR, OCRNL, ONOCR, ONLRET via `OutputAction` enum
- Additional signal generation: SIGQUIT (Ctrl+\\), SIGTSTP (Ctrl+Z) alongside existing SIGINT
- Echo modes: ECHOCTL (^X for control chars), ECHOKE (kill with newline), column tracking
- Canonical editing: VWERASE (Ctrl+W word erase), VREPRINT (Ctrl+R redisplay via `ReprintLine` action), VLNEXT (Ctrl+V literal next)
- Flow control: IXON with VSTOP/VSTART (Ctrl+S / Ctrl+Q)
- Non-canonical: VMIN/VTIME parsed in ABI but timing enforcement deferred
- New ABI constants in `abi/src/syscall.rs`: 12 c_iflag bits, 5 c_oflag bits, 6 c_lflag bits, 6 new c_cc indices
- `write()` in `mod.rs` now applies `c_oflag` output processing
- `push_input`/`process_raw_char_for` handle `InputAction::ReprintLine`
- Default termios unchanged (`c_iflag: 0`, `c_oflag: 0`) to preserve existing behavior; userland can enable OPOST+ONLCR via `tcsetattr`

**Goal**: Bring `LineDisc` to feature parity with Linux's `n_tty` (simplified but complete).

### 6.1 Current gaps

The current `LineDisc` (`line_disc.rs`, 183 lines) supports:
- ✅ Canonical mode (ICANON): edit buffer → cooked on newline/EOF
- ✅ ECHO: basic character echo
- ✅ ECHOE: backspace echo (BS-SPACE-BS)
- ✅ ECHOK: kill-line echo (newline)
- ✅ ISIG: SIGINT on Ctrl+C (VINTR)
- ✅ VEOF, VERASE, VKILL

Missing:
- ❌ **Input flags** (`c_iflag`): ICRNL, INLCR, IGNCR, ISTRIP, IXON, IXOFF, IUTF8
- ❌ **Output flags** (`c_oflag`): OPOST, ONLCR, OCRNL, ONOCR, ONLRET
- ❌ **Echo modes**: ECHOCTL (echo ^C for control chars), ECHOKE (kill with BS sequence), ECHOPRT
- ❌ **Non-canonical timing**: VMIN/VTIME — parsed in ABI but never enforced
- ❌ **Signals**: VQUIT (SIGQUIT), VSUSP (SIGTSTP), VDSUSP
- ❌ **Flow control**: VSTOP (Ctrl+S), VSTART (Ctrl+Q)
- ❌ **Word erase**: VWERASE (Ctrl+W in canonical mode)
- ❌ **Reprint**: VREPRINT (Ctrl+R to redisplay line)
- ❌ **Literal next**: VLNEXT (Ctrl+V to insert next char literally)

### 6.2 Enhanced `LineDisc` struct

```rust
pub struct LineDisc {
    termios: UserTermios,

    // Canonical mode buffers
    edit_buf: [u8; EDIT_BUF_SIZE],
    edit_len: usize,

    // Cooked output ring buffer (ready for userland read)
    cooked: [u8; COOKED_BUF_SIZE],
    cooked_head: usize,
    cooked_tail: usize,
    cooked_count: usize,

    // Non-canonical mode state
    raw_count: usize,          // bytes available for VMIN check

    // Echo state
    echo_pending: bool,        // deferred echo after output processing

    // Flow control
    stopped: bool,             // output stopped via XOFF (Ctrl+S)
    literal_next: bool,        // next char is literal (after Ctrl+V)

    // Columns tracking (for ECHOKE/ECHOPRT)
    column: usize,
}
```

### 6.3 New ABI constants

Add to `abi/src/syscall.rs`:

```rust
// c_iflag bits
pub const IGNBRK:  u32 = 0x001;
pub const BRKINT:  u32 = 0x002;
pub const IGNPAR:  u32 = 0x004;
pub const PARMRK:  u32 = 0x008;
pub const INPCK:   u32 = 0x010;
pub const ISTRIP:  u32 = 0x020;
pub const INLCR:   u32 = 0x040;
pub const IGNCR:   u32 = 0x080;
pub const ICRNL:   u32 = 0x100;
pub const IXON:    u32 = 0x400;
pub const IXOFF:   u32 = 0x1000;
pub const IUTF8:   u32 = 0x4000;

// c_oflag bits
pub const OPOST:   u32 = 0x01;
pub const ONLCR:   u32 = 0x04;
pub const OCRNL:   u32 = 0x08;
pub const ONOCR:   u32 = 0x10;
pub const ONLRET:  u32 = 0x20;

// c_lflag bits (in addition to existing ISIG, ICANON, ECHO, ECHOE, ECHOK, ECHONL)
pub const ECHOCTL: u32 = 0x200;
pub const ECHOPRT: u32 = 0x400;
pub const ECHOKE:  u32 = 0x800;
pub const NOFLSH:  u32 = 0x80;
pub const TOSTOP:  u32 = 0x100;
pub const IEXTEN:  u32 = 0x8000;

// c_cc indices (in addition to existing VINTR..VEOL)
pub const VQUIT:   usize = 1;  // already exists
pub const VSUSP:   usize = 10;
pub const VWERASE: usize = 14;
pub const VREPRINT: usize = 12;
pub const VLNEXT:  usize = 15;
pub const VSTOP:   usize = 9;
pub const VSTART:  usize = 8;
```

### 6.4 Input processing pipeline

```rust
pub fn input_char(&mut self, c: u8) -> InputAction {
    let iflag = self.termios.c_iflag;
    let lflag = self.termios.c_lflag;

    // 1. Input flag processing (c_iflag)
    let c = self.process_input_flags(c, iflag);

    // 2. Literal-next mode (Ctrl+V was pressed)
    if self.literal_next {
        self.literal_next = false;
        return self.insert_char(c);
    }

    // 3. Signal generation (ISIG)
    if (lflag & ISIG) != 0 {
        if c == self.cc(VINTR) { return InputAction::Signal(SIGINT); }
        if c == self.cc(VQUIT) { return InputAction::Signal(SIGQUIT); }
        if c == self.cc(VSUSP) { return InputAction::Signal(SIGTSTP); }
    }

    // 4. Flow control (IXON)
    if (iflag & IXON) != 0 {
        if c == self.cc(VSTOP) { self.stopped = true; return InputAction::None; }
        if c == self.cc(VSTART) { self.stopped = false; return InputAction::None; }
    }

    // 5. Canonical vs non-canonical
    if (lflag & ICANON) != 0 {
        self.canonical_input(c)
    } else {
        self.raw_input(c)
    }
}
```

### 6.5 Output processing

```rust
/// Process a byte through c_oflag before sending to the driver.
pub fn process_output(&self, c: u8) -> OutputAction {
    let oflag = self.termios.c_oflag;
    if (oflag & OPOST) == 0 {
        return OutputAction::Write(&[c]);
    }
    match c {
        b'\n' if (oflag & ONLCR) != 0 => OutputAction::Write(&[b'\r', b'\n']),
        b'\r' if (oflag & OCRNL) != 0 => OutputAction::Write(&[b'\n']),
        b'\r' if (oflag & ONOCR) != 0 && self.column == 0 => OutputAction::Suppress,
        _ => OutputAction::Write(&[c]),
    }
}
```

### 6.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Complete rewrite (was `line_disc.rs`) |
| `abi/src/syscall.rs` | Add c_iflag, c_oflag, c_lflag constants, new c_cc indices |

---

## 7. Phase 3: Input Pipeline Cleanup ✅ COMPLETED

**Goal**: Establish a single, clean input path: keyboard → TTY. Remove dual delivery.

### 7.1 Current dual delivery

In `drivers/src/ps2/keyboard.rs:handle_scancode()`:

```rust
// Line 227: Sends to compositor input queue (ALL events, press + release)
input_event::input_route_key_event(make_code, ascii, is_press, timestamp_ms);

// Lines 279, 294: Sends to TTY line discipline (press only, non-zero ASCII)
tty_handle_input_char(ascii);
```

The compositor input queue receives keyboard events that the shell **ignores** (only processes mouse events in `poll_batch`). This is wasted work and architectural confusion.

### 7.2 New input routing

```
PS/2 Keyboard Interrupt
    │
    ├── Key Press (ASCII != 0)
    │   └── tty::push_input(active_tty_index, ascii)
    │       └── LineDisc::input_char(ascii) → cooked buffer → wake readers
    │
    ├── Key Press (extended: arrows, delete, etc.)
    │   └── tty::push_input(active_tty_index, extended_key)
    │
    └── Key Release
        └── (nothing — release events are ignored)

PS/2 Mouse Interrupt
    └── input_event::input_route_pointer_*()  (unchanged)
```

### 7.3 Changes to keyboard handler

```rust
// drivers/src/ps2/keyboard.rs — handle_scancode()

pub fn handle_scancode(scancode: u8) {
    let mut state = STATE.lock();

    if scancode == 0xE0 {
        state.extended_code = true;
        return;
    }

    let is_press = !is_break_code(scancode);
    let make_code = get_make_code(scancode);

    state.scancode_buffer.push_overwrite(scancode);

    let ascii = translate_scancode(scancode, &state.modifiers);
    // REMOVED: input_event::input_route_key_event(...)
    // Keyboard events no longer go to the compositor input queue.

    if matches!(make_code, 0x2A | 0x36 | 0x1D | 0x38 | 0x3A) {
        handle_modifier(&mut state.modifiers, make_code, is_press);
        return;
    }

    if state.extended_code {
        state.extended_code = false;
        if !is_press { return; }
        let extended_key = match make_code { /* ... */ };
        if extended_key != 0 {
            drop(state);
            tty::push_input(tty::active_tty(), extended_key);
            request_reschedule_from_interrupt();
        }
        return;
    }

    if !is_press { return; }

    if ascii != 0 {
        drop(state);
        tty::push_input(tty::active_tty(), ascii);
        request_reschedule_from_interrupt();
    }
}
```

### 7.4 Serial input integration

Currently `tty_drain_hw_input()` polls the UART and drains `keyboard::char_buffer` (which is never written to). Simplify:

```rust
// In Tty::drain_hw_input() — called from read/poll
fn drain_hw_input(&mut self) {
    let mut scratch = [0u8; 64];
    let count = self.driver.drain_input(&mut scratch);
    for i in 0..count {
        let c = self.ldisc.process_input_flags_byte(scratch[i]);
        self.ldisc_input(c);
    }
}
```

The `SerialConsoleDriver::drain_input` wraps the existing `serial_poll_receive` + `INPUT_BUFFER` drain logic.
The `VConsoleDriver::drain_input` returns 0 (PS/2 input comes via interrupt, not polling).

### 7.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/ps2/keyboard.rs` | Remove `input_route_key_event` for keyboard, call `tty::push_input` |
| `drivers/src/tty/mod.rs` | Add `push_input(TtyIndex, u8)`, `active_tty() -> TtyIndex` |
| `drivers/src/tty/driver.rs` | `SerialConsoleDriver::drain_input` wraps UART polling |
| `drivers/src/input_event.rs` | No changes — still used for mouse/pointer events |

---

## 8. Phase 4: Session & Process Group Management ✅ COMPLETED

**Status**: Completed. All 988 tests pass (21 new Phase 4 regression tests). Build clean. `just test` passes.

**Implementation summary**: `TtySession` rewritten from 35-line stub to 202-line full implementation with:
- `TtySession` struct with `session_leader`, `session_id`, `fg_pgrp`, `focused_task_id` fields
- `ForegroundCheck` enum: `Allowed`, `NoSession`, `BackgroundRead`, `BackgroundWrite`
- `check_read()` / `check_write()` for POSIX foreground access control
- `task_has_access()` transitional helper combining compositor focus + session-based checks
- `set_fg_pgrp_checked()` with same-session validation
- `attach()` / `detach()` for session lifecycle management
- Sentinel constants: `NO_SESSION`, `NO_FOREGROUND_PGRP`
- Cross-crate wiring: `TIOCGSID` ioctl, `current_task_pgid/sid` runtime services, `setsid()` detaches controlling TTY, `TIOCSPGRP` validates caller session
- 21 new regression tests covering session attach/detach, foreground checks, read/write access control, per-TTY API round-trips

**Goal**: Replace ad-hoc `TTY_FOCUSED_TASK_ID` / `TTY_FOREGROUND_PGRP` with proper POSIX-like sessions.

### 8.1 Current focus system problems

| Issue | Location | Problem |
|-------|----------|---------|
| `TTY_FOCUSED_TASK_ID` | `tty.rs:42` | Global atomic. Set lazily on first read. Never properly transferred between parent/child. |
| `TTY_FOREGROUND_PGRP` | `tty.rs:43` | Compares pgrp with task_id — semantically wrong. Process group ≠ task ID. |
| `tty_task_has_focus` | `tty.rs:196-203` | OR's two unrelated checks. Focus and foreground pgrp are conflated. |
| `tty_ensure_focus_for_task` | `tty.rs:205-209` | CAS-free set. Race-prone. |
| `tty_wait_for_focus` | `tty.rs:211-225` | Blocks on global `TTY_FOCUS_QUEUE`. No per-TTY granularity. |

### 8.2 New session model

```rust
// drivers/src/tty/session.rs

/// Per-TTY session/foreground state.
pub struct TtySession {
    /// Session leader's process ID (0 = no session).
    pub session_leader: u32,

    /// Session ID (typically == session_leader's PID).
    pub session_id: u32,

    /// Foreground process group ID.
    pub fg_pgrp: u32,
}
```

### 8.3 Foreground check in read/write

```rust
// In tty::read()
fn check_foreground(&self, caller_pgrp: u32) -> Result<(), TtyError> {
    if self.session.fg_pgrp == 0 {
        return Ok(()); // No session control, allow
    }
    if caller_pgrp == self.session.fg_pgrp {
        return Ok(()); // Caller is in foreground group
    }
    // Background process trying to read → SIGTTIN
    Err(TtyError::BackgroundRead)
}
```

### 8.4 Per-process controlling terminal

Add to the process/task structure (in `core/src/scheduler/task.rs` or equivalent):

```rust
/// Controlling terminal index (None = no controlling terminal).
pub controlling_tty: Option<TtyIndex>,

/// Process group ID.
pub pgrp: u32,

/// Session ID.
pub session_id: u32,
```

These are:
- Inherited from parent on `fork()`
- Set on `setsid()` (new session, no controlling terminal)
- Acquired on first open of a TTY (if no controlling terminal and process is session leader)

### 8.5 ioctl integration

| ioctl | Action |
|-------|--------|
| `TIOCGPGRP` | Return `tty.session.fg_pgrp` |
| `TIOCSPGRP` | Set `tty.session.fg_pgrp` (caller must be in same session) |
| `TIOCGSID` | Return `tty.session.session_id` |

### 8.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/session.rs` | New: `TtySession` struct, foreground checks |
| `drivers/src/tty/mod.rs` | Use `TtySession` in `Tty` struct |
| `core/src/scheduler/task.rs` | Add `controlling_tty`, `pgrp`, `session_id` fields |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Update TIOCGPGRP/TIOCSPGRP to use per-TTY session |

---

## 9. Phase 5: FD Integration ✅ COMPLETED

**Status**: Completed. All 997 tests pass (9 new Phase 5 regression tests). Build clean. `just test` passes.

**Goal**: Wire the new TTY subsystem into the file descriptor layer.

### 9.1 FileDescriptor changes

Currently `FileDescriptor` has a boolean `console` flag. Replace with an optional TTY index:

```rust
// fs/src/fileio.rs
struct FileDescriptor {
    // ... existing fields ...
    console: bool,        // REMOVE
    tty_index: Option<TtyIndex>,  // ADD: None = not a TTY, Some(i) = TTY i
}
```

### 9.2 Console FD bootstrap

```rust
fn bootstrap_console_fds(table: &mut FileTableSlot, tty: TtyIndex) {
    // FD 0 = stdin
    table.descriptors[0] = FileDescriptor {
        flags: FILE_OPEN_READ,
        valid: true,
        tty_index: Some(tty),
        ..FileDescriptor::new()
    };
    // FD 1 = stdout
    table.descriptors[1] = FileDescriptor {
        flags: FILE_OPEN_WRITE,
        valid: true,
        tty_index: Some(tty),
        ..FileDescriptor::new()
    };
    // FD 2 = stderr
    table.descriptors[2] = FileDescriptor {
        flags: FILE_OPEN_WRITE,
        valid: true,
        tty_index: Some(tty),
        ..FileDescriptor::new()
    };
}
```

### 9.3 Read/write/poll routing

```rust
// file_read_fd — console path
if let Some(tty_idx) = desc.tty_index {
    let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
    drop(guard);
    return tty::read(tty_idx, buffer as *mut u8, count, is_nonblock);
}

// file_write_fd — console path
if let Some(tty_idx) = desc.tty_index {
    drop(guard);
    let bytes = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
    return tty::write(tty_idx, bytes) as ssize_t;
}

// file_poll_fd — console path
if let Some(tty_idx) = desc.tty_index {
    let mut revents = 0u16;
    if (events & POLLIN) != 0 && tty::has_data(tty_idx) {
        revents |= POLLIN;
    }
    if (events & POLLOUT) != 0 {
        revents |= POLLOUT;
    }
    drop(guard);
    return revents;
}
```

### 9.4 Write with output processing

Currently console writes go directly to `serial_write_bytes`. With the new system:

```rust
// tty::write(tty_index, bytes)
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    let mut table = TTY_TABLE.lock();
    let tty = match table[idx.0 as usize].as_mut() {
        Some(t) => t,
        None => return 0,
    };

    for &c in data {
        match tty.ldisc.process_output(c) {
            OutputAction::Write(bytes) => tty.driver.write_output(bytes),
            OutputAction::Suppress => {}
        }
    }
    data.len()
}
```

This means `\n` → `\r\n` conversion (ONLCR) is finally handled by the TTY layer, not by userland.

### 9.5 ioctl dispatch

Update `core/src/syscall/fs/poll_ioctl_handlers.rs`:

```rust
fn syscall_ioctl(fd: i32, request: u64, arg: u64) -> i64 {
    let tty_idx = match get_tty_index_for_fd(fd) {
        Some(idx) => idx,
        None => return -1, // ENOTTY
    };

    match request {
        TCGETS => tty::get_termios(tty_idx, arg as *mut UserTermios),
        TCSETS | TCSETSW | TCSETSF => tty::set_termios(tty_idx, arg as *const UserTermios),
        TIOCGPGRP => tty::get_foreground_pgrp(tty_idx, arg as *mut u32),
        TIOCSPGRP => tty::set_foreground_pgrp(tty_idx, arg as *const u32),
        TIOCGWINSZ => tty::get_winsize(tty_idx, arg as *mut UserWinsize),
        TIOCSWINSZ => tty::set_winsize(tty_idx, arg as *const UserWinsize),
        _ => -1, // EINVAL
    }
}
```

### 9.6 Files modified (planned)

| File | Change |
|------|--------|
| `fs/src/fileio.rs` | Replace `console: bool` with `tty_index: Option<TtyIndex>`, update read/write/poll |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Route ioctl to per-TTY, add TIOCGWINSZ/TIOCSWINSZ |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update to call `tty::*` with TTY index |

### 9.7 Implementation Notes

**Actual files modified:**

| File | Change |
|------|--------|
| `fs/src/fileio.rs` | Replaced `console: bool` with `tty_index: Option<u8>` in `FileDescriptor`; routed `file_read_fd` through `tty::read_cooked(tty_idx, ...)`, `file_write_fd` through `tty::write_bytes(tty_idx, ...)`, `file_poll_fd` through `tty::has_cooked_data(tty_idx)`; added `file_get_tty_index(process_id, fd) -> Option<u8>` for ioctl dispatch; updated `bootstrap_console_fds` to set `tty_index: Some(0)` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Rewrote `syscall_ioctl` to resolve TTY index from FD via `file_get_tty_index(pid, fd)` instead of hardcoded `0`; all TCGETS/TCSETS/TIOCGWINSZ/TIOCSWINSZ/TIOCGPGRP/TIOCSPGRP/TIOCGSID now use the resolved `tty_idx` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added `write_bytes(tty_index: u8, buf: *const u8, len: usize) -> usize` to `TtyServices` |
| `drivers/src/syscall_services_init.rs` | Added `tty_write_bytes_adapter` function and registered it in `TTY_SERVICES` struct |
| `drivers/src/tty_tests.rs` | Added 9 Phase 5 regression tests (output processing, raw passthrough, invalid index, per-TTY termios/winsize/pgrp/data/session isolation, invalid read) |

**Key design decisions:**
- Used `Option<u8>` for `tty_index` instead of `Option<TtyIndex>` to match existing per-TTY API signatures across the codebase
- Added `write_bytes` as a new `TtyServices` function rather than modifying existing `write_char`, keeping backward compatibility
- `file_get_tty_index` is a standalone public function in `fileio.rs` to allow ioctl handlers to resolve TTY index without full FD lock
---

## 10. Phase 6: Control-Plane Correctness ✅ COMPLETED

> **Priority**: P0 — Must fix before any new feature work.
> **Rationale**: The deep architectural review (comparing against Linux `tty_struct` + `n_tty` and RedoxOS) identified that the biggest risks in SlopOS's TTY are **not** parsing/processing logic (which is solid) but **control-plane correctness**: compositor focus is conflated with POSIX session foreground, the transitional `task_has_access()` permanently bypasses session control, and the `TtyIndex` type leaks to raw `u8` at the FD boundary.

**Goal**: Establish correct POSIX-like control semantics by separating compositor focus from session foreground, making `check_read()` the authoritative gate, and enforcing type safety across crate boundaries.

**Status**: Completed. All tests pass (58 suites, 0 failures). Build clean. `just test` passes.

**Implementation summary**:
- **10.1 (Compositor focus split)**: `set_focus()` → `set_compositor_focus()` / `get_focus()` → `get_compositor_focus()` — only sets `focused_task_id`, never touches `fg_pgrp`. Updated `TtyServices` and `ui_handlers.rs` call sites.
- **10.2 (check_read as sole gate)**: Removed `task_has_access()` from `session.rs`. `tty::read()` now uses `check_read()` directly as the sole read-side gate. Background reads send `SIGTTIN` signal to the calling process.
- **10.3 (TtyIndex type safety)**: Moved `TtyIndex` newtype to `abi/src/syscall.rs` (`#[repr(transparent)]` wrapper around `u8`) for cross-crate visibility. Changed `FileDescriptor.tty_index` from `Option<u8>` to `Option<TtyIndex>`. Updated all `TtyServices` signatures and adapter functions to accept `TtyIndex` directly.
- **10.4 (Signal constants)**: Added `SIGINT=2`, `SIGQUIT=3`, `SIGTSTP=20`, `SIGTTIN=21` constants to `abi/src/syscall.rs`. Replaced all magic numbers in `ldisc.rs` signal generation with named constants.
- **Regression tests**: Replaced 4 `task_has_access` tests with 3 `check_read` tests. Renamed `test_focus` → `test_compositor_focus`. Added 4 new Phase 6 regression tests: `test_tty_index_abi_type`, `test_signal_constants`, `test_set_compositor_focus_does_not_set_fg_pgrp`, `test_check_read_sole_gate_background`.

### 10.1 Split compositor focus from POSIX foreground

**Problem**: `set_focus(task_id)` in `mod.rs` sets **both** `focused_task_id` AND `fg_pgrp` to the same value.  A task ID is not a process group ID.  Compositor window focus and POSIX foreground group are independent concepts.

**Current (broken)**:
```rust
pub fn set_focus(task_id: u32) -> i32 {
    // ...
    tty.session.focused_task_id = task_id;
    tty.session.fg_pgrp = task_id;  // ← WRONG: task_id != pgid
}
```

**Fix**:
- `set_focus()` → renamed `set_compositor_focus()`, sets **only** `focused_task_id`
- `fg_pgrp` is modified **only** via `TIOCSPGRP` / `set_foreground_pgrp_checked()`
- Update `core/src/syscall/ui_handlers.rs` to call the renamed function
- Add `set_compositor_focus` / `get_compositor_focus` to `TtyServices`

### 10.2 Replace `task_has_access()` with proper `check_read()` gating

**Problem**: `task_has_access()` in `session.rs` is labeled "transitional" but is the primary read gate.  It OR's compositor focus with session foreground, meaning a background process with compositor focus can bypass POSIX session control.

**Current (broken)**:
```rust
pub fn task_has_access(&self, task_id: u32, caller_pgid: u32) -> bool {
    // Compositor focus takes priority (breaks POSIX)
    if self.focused_task_id != 0 && self.focused_task_id == task_id {
        return true;
    }
    // Session check (correct)
    if self.fg_pgrp != NO_FOREGROUND_PGRP && self.fg_pgrp == caller_pgid {
        return true;
    }
    // ...
}
```

**Fix**:
- Make `check_read(caller_pgid, caller_sid)` the **sole** read-side gate in `tty::read()`
- Use compositor focus **only** for the "no session attached yet" bootstrap path (first reader before `setsid()`)
- Remove or deprecate `task_has_access()`
- Update `tty::read()` and `auto_attach_session()` to use `check_read()` directly
- Add `BackgroundRead` → `SIGTTIN` signal delivery in `read()` when `check_read()` returns `BackgroundRead`

### 10.3 Type safety: `Option<TtyIndex>` end-to-end

**Problem**: `fs/src/fileio.rs` uses `tty_index: Option<u8>` and `TtyServices` bridge functions accept raw `u8`.  Any integer can be silently passed as a TTY index without compile-time checking.

**Fix**:
- Change `FileDescriptor.tty_index` from `Option<u8>` to `Option<TtyIndex>` (re-export `TtyIndex` in `abi/` or `lib/`)
- Update `TtyServices` function signatures to accept `TtyIndex` (or a newtype `TtyHandle(u8)` that lives in `abi/`)
- Update `file_get_tty_index()` to return `Option<TtyIndex>`
- Update all ioctl dispatch in `poll_ioctl_handlers.rs` to pass `TtyIndex`
- Update `drivers/src/syscall_services_init.rs` adapters

### 10.4 Replace hardcoded signal numbers with ABI constants

**Problem**: `ldisc.rs` uses magic numbers `2` (SIGINT), `3` (SIGQUIT), `20` (SIGTSTP) in `InputAction::Signal()`.

**Fix**:
- Add `SIGINT`, `SIGQUIT`, `SIGTSTP` constants to `abi/src/syscall.rs` (or a new `abi/src/signal.rs`)
- Replace all hardcoded signal numbers in `ldisc.rs` with named constants
- Update test assertions to use the same constants

### 10.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Rename `set_focus`→`set_compositor_focus`, replace `task_has_access` calls with `check_read`, update `read()` gating |
| `drivers/src/tty/session.rs` | Deprecate/remove `task_has_access()`, add SIGTTIN delivery helper |
| `drivers/src/tty/ldisc.rs` | Replace signal magic numbers with ABI constants |
| `fs/src/fileio.rs` | Change `tty_index: Option<u8>` → `Option<TtyIndex>` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update `TtyServices` signatures to use `TtyIndex` |
| `drivers/src/syscall_services_init.rs` | Update adapter functions |
| `core/src/syscall/ui_handlers.rs` | Call `set_compositor_focus` instead of `set_focus` |
| `abi/src/syscall.rs` | Add `SIGINT`/`SIGQUIT`/`SIGTSTP` constants |
| `drivers/src/tty_tests.rs` | Update tests for renamed APIs, add `check_read()`-based gating tests, signal constant tests |

---

## 11. Phase 7: Lifecycle & Hangup Semantics ✅ COMPLETED

> **Priority**: P1 — Must fix before PTY implementation or proper shell exit.
> **Rationale**: Linux's biggest TTY complexity is lifecycle management, and it exists for a reason.  Without open counts and hangup signaling, dead processes can hold TTY resources, PTY pairs can't clean up, and shell exit doesn't notify children.  This is the single biggest "pain later" item if deferred.

**Goal**: Add TTY lifecycle management (open/close tracking, hangup signaling, controlling terminal acquisition) modeled after Linux's `tty_port` + `kref` pattern, adapted for SlopOS's static table.

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- Added lifecycle state to TTYs (`open_count`, `hung_up`) and wired robust hangup semantics (`hangup()`, wake blocked readers, EOF/EIO behavior) in `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, and `drivers/src/tty/ldisc.rs`.
- Integrated full FD reference counting for TTY-backed descriptors across bootstrap, dup/fork cloning, and close/reset paths in `fs/src/fileio.rs`; exposed hangup via poll (`POLLHUP`).
- Implemented controlling-terminal acquisition via `TIOCSCTTY` with session-leader checks in `core/src/syscall/fs/poll_ioctl_handlers.rs`, added `controlling_tty` task state in scheduler structs, and wired session-leader exit hangup in `core/src/scheduler/task.rs`.
- Extended ABI/services for lifecycle operations (`TIOCSCTTY`, `SIGHUP`, `SIGCONT`, `open_ref`, `close_ref`, `hangup`, `is_hung_up`) across `abi/src/syscall.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, and `drivers/src/syscall_services_init.rs`.
- Added regression coverage in `drivers/src/tty_tests.rs` (flush-all, open-count lifecycle, hangup flag/detach, hangup read semantics) and `core/src/syscall/tests.rs` (`TIOCSCTTY` leader/non-leader behavior).

### 11.1 Add open count to `Tty`

**What**: Track how many file descriptors reference each TTY.  This enables "last close" detection for cleanup.

```rust
pub struct Tty {
    // ... existing fields ...

    /// Number of open file descriptors referencing this TTY.
    pub open_count: u16,
}
```

- Increment on `open()` / FD bootstrap / `dup()` / `fork()` when FD has `tty_index`
- Decrement on `close()` / process exit when FD has `tty_index`
- On `open_count == 0`: trigger cleanup (flush buffers, optionally deallocate for PTY slots)

### 11.2 Implement `tty_hangup()`

**What**: When a session leader exits or carrier is lost, the controlling TTY must signal the session.

```rust
/// Hangup a TTY: send SIGHUP + SIGCONT to the session's foreground group,
/// flush buffers, wake all blocked readers (they get EOF / -EIO).
pub fn hangup(idx: TtyIndex) {
    let (fg_pgrp, session_id) = {
        let mut table = TTY_TABLE.lock();
        let tty = match table.get_mut(idx.0 as usize) {
            Some(Some(t)) => t,
            _ => return,
        };
        let fg = tty.session.fg_pgrp;
        let sid = tty.session.session_id;
        tty.ldisc.flush_all();  // New: clear edit + cooked buffers
        tty.session.detach();   // Clear session state
        (fg, sid)
    };

    // Signal outside the lock to avoid deadlock.
    if fg_pgrp != 0 {
        let _ = signal_process_group(fg_pgrp, SIGHUP);
        let _ = signal_process_group(fg_pgrp, SIGCONT);
    }

    // Wake all blocked readers — they will see EOF or -EIO.
    TTY_INPUT_WAITERS[idx.0 as usize].wake_all();
}
```

### 11.3 Wire hangup into process/session leader exit

- In `core/src/scheduler/task.rs` (or process exit path): when a session leader exits, find its controlling TTY and call `tty::hangup()`
- In `setsid()`: already calls `detach_session_by_id()` — verify it also handles hangup if old session had a controlling terminal

### 11.4 Controlling terminal acquisition (`TIOCSCTTY`)

**What**: Implement the `TIOCSCTTY` ioctl so a session leader can explicitly acquire a controlling terminal.

- Only session leaders with no existing controlling TTY may call this
- Set `tty.session.attach(caller_sid, caller_pgid)`
- Set `controlling_tty` in the process's task struct
- Add `TIOCSCTTY` constant to `abi/src/syscall.rs` and dispatch in `poll_ioctl_handlers.rs`

### 11.5 Add `flush_all()` to `LineDisc`

```rust
/// Clear both edit and cooked buffers (used during hangup/close).
pub fn flush_all(&mut self) {
    self.edit_len = 0;
    self.cooked_head = 0;
    self.cooked_tail = 0;
    self.cooked_count = 0;
    self.stopped = false;
    self.literal_next = false;
    self.column = 0;
}
```

### 11.6 Blocked reader behavior on hangup

- Readers blocked in `TTY_INPUT_WAITERS[idx].wait_event(...)` are woken by `hangup()`
- On wakeup, `read()` re-checks: if session is detached and no data, return 0 (EOF)
- Non-blocking reads return `-EIO` if TTY is hung up
- Add a `hung_up: bool` flag to `Tty` to track post-hangup state

### 11.7 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Added `open_count`/`hung_up`; implemented `open_ref`, `close_ref`, `hangup`, `is_hung_up`; added EOF/EIO read behavior after hangup |
| `drivers/src/tty/table.rs` | Initialized lifecycle fields in `Tty::new()` |
| `drivers/src/tty/ldisc.rs` | Added `flush_all()` |
| `fs/src/fileio.rs` | Wired TTY refcount lifecycle through bootstrap/dup/fork/close and exposed hangup through `POLLHUP` |
| `core/src/scheduler/task_struct.rs` | Added `controlling_tty: Option<TtyIndex>` to task state |
| `core/src/scheduler/task.rs` | Wired session-leader exit path to `tty::hangup()` |
| `core/src/syscall/process_handlers.rs` | Cleared `controlling_tty` in `setsid()` |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Added `TIOCSCTTY` dispatch and session-leader ownership checks |
| `lib/src/kernel_services/syscall_services/tty.rs` | Added lifecycle service methods (`attach_session`, `open_ref`, `close_ref`, `hangup`, `is_hung_up`) |
| `drivers/src/syscall_services_init.rs` | Added adapters and service wiring for new lifecycle methods |
| `abi/src/syscall.rs` | Added `TIOCSCTTY`, `SIGHUP`, and `SIGCONT` constants |
| `drivers/src/tty_tests.rs` | Added hangup/open-count/flush regression tests |
| `core/src/syscall/tests.rs` | Added `TIOCSCTTY` regression tests (leader success, non-leader reject) |

---

## 12. Phase 8: Per-TTY Locking & Performance ✅ COMPLETED

> **Priority**: P1 — Must fix before multiple active TTYs or PTY support.
> **Rationale**: The single `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>` lock protected **all** 8 TTY slots.  Any operation on TTY 0 blocked all operations on TTY 1–7.  `write()` held the lock for the entire byte-by-byte serial output loop (~86μs/byte at 115200 baud).  A 1 KB write held the global lock for ~86 ms.  Linux uses per-tty mutexes with a global lock only for lookup/registration.

**Goal**: Move to per-TTY locking so that operations on different TTYs are fully independent, and release the lock before slow driver I/O.

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass (1016/1016 tests, 0 failures, 7 new Phase 8 regression tests).

**Implementation summary**:
- **12.1 (Per-TTY lock architecture)**: Replaced `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>` (single global lock) with `TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS]` (per-slot independent locks).  Matches existing `UDP_RX_QUEUES` pattern in the socket module.  All ~39 `TTY_TABLE.lock()` call sites in `mod.rs` rewritten to per-slot `TTY_SLOTS[slot].lock()`.
- **12.2 (Split-write pattern)**: `write()` now processes output through the line discipline under the per-TTY lock into a 256-byte stack buffer, copies a lightweight `DriverId` enum, drops the lock, then writes the buffered bytes to hardware via `write_driver_unlocked()`.  Slow serial I/O no longer blocks other TTYs.
- **12.3 (Merged drain+read)**: `read()` now performs foreground check + `drain_hw_input()` + `ldisc.read()` in a single per-TTY lock acquisition per loop iteration, reducing from 5–6 separate lock/unlock cycles to 1.  Deferred signal delivery (e.g. Ctrl+C on serial) happens after dropping the lock.
- **12.4 (Idle callback iterates all TTYs)**: `input_available_cb()` now iterates all `MAX_TTYS` slots, draining hardware input and waking blocked readers on each active TTY (previously only checked TTY 0).
- **12.5 (Lock ordering documented)**: Comprehensive lock ordering rules documented in `table.rs`: never hold two per-TTY locks simultaneously; `TTY_INPUT_WAITERS` is separate from `TTY_SLOTS` to avoid lock-order violations during blocking waits.
- **12.6 (DriverId for lock-free I/O)**: Added `DriverId` enum (`SerialConsole`, `VConsole`, `None`) with `#[derive(Clone, Copy, PartialEq, Eq, Debug)]` and `TtyDriverKind::id()` method for the split-write pattern.  `write_driver_unlocked(DriverId, &[u8])` performs hardware I/O without any TTY lock.
- **Regression tests**: 7 new Phase 8 tests: `test_phase8_per_tty_lock_independence` (slots lockable simultaneously), `test_phase8_driver_id_round_trip` (DriverId matches driver kind), `test_phase8_split_write_returns_input_len` (split-write correctness), `test_phase8_idle_cb_iterates_all_ttys` (idle callback all-TTY check), `test_phase8_merged_drain_read` (single-lock drain+read), `test_phase8_with_tty_per_slot` (per-slot with_tty correctness), `test_phase8_driver_id_traits` (Copy/Clone/Eq).

### 12.1 Per-TTY lock architecture

**Previous**:
```rust
pub static TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>;  // One lock for everything
```

**New**:
```rust
/// Per-TTY locked slots.  Each element is independently locked.
pub static TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS] = [const { IrqMutex::new(None) }; MAX_TTYS];
```

- No global table lock — each slot is independently locked
- `Tty` struct name kept (not renamed to `TtyInner`) to minimize churn
- `with_tty()` and `with_tty_ref()` helpers updated to lock individual slots

### 12.2 Split-write via DriverId

```rust
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    // Phase 1: Process under per-TTY lock (fast — pure computation).
    let driver_id;
    let mut out_buf = [0u8; 256];
    let mut out_len = 0;
    { let mut guard = TTY_SLOTS[slot].lock(); /* process ldisc */ driver_id = tty.driver.id(); }
    // Phase 2: Driver I/O without lock (slow — hardware).
    write_driver_unlocked(driver_id, &out_buf[..out_len]);
}
```

### 12.3 Merged drain+read

Single lock acquisition in `read()` loop body combines: foreground check, `drain_hw_input()`, `ldisc.read()`, and deferred signal extraction.

### 12.4 Idle callback iterates all TTYs

`input_available_cb()` loops over `0..MAX_TTYS`, locking and releasing each slot individually.

### 12.5 Lock ordering rules

Documented in `table.rs` module doc:
1. **`TTY_SLOTS[i]`** — per-TTY, held for ldisc/session/termios.  **Never hold two simultaneously.**
2. **`TTY_INPUT_WAITERS[i]`** — separate static, condition closure may transiently re-acquire same slot.
3. Rule: **Never acquire `TTY_SLOTS[j]` while holding `TTY_SLOTS[i]`** (i ≠ j).

### 12.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/table.rs` | Replaced `TTY_TABLE` with `TTY_SLOTS: [IrqMutex<Option<Tty>>; MAX_TTYS]`; added comprehensive lock ordering documentation |
| `drivers/src/tty/mod.rs` | Rewrote all ~39 `TTY_TABLE.lock()` call sites to per-slot locking; implemented split-write pattern; merged drain+read; idle callback iterates all TTYs |
| `drivers/src/tty/driver.rs` | Added `DriverId` enum, `TtyDriverKind::id()`, `write_driver_unlocked()` |
| `drivers/src/tty_tests.rs` | Migrated `TTY_TABLE` → `TTY_SLOTS` in existing tests; added 7 Phase 8 regression tests |
---

## 13. Phase 9: Rust Idioms & Termios Completion ✅ COMPLETED

> **Priority**: P2 — Improves code quality and enables realistic userspace.
> **Rationale**: The current codebase uses C-style error patterns (`isize`, `-1`, raw pointers) internally where Rust `Result` types and slices would catch bugs at compile time.  Additionally, `VMIN/VTIME` is parsed but not enforced — terminal applications like readline and curses depend on this for responsive input.

**Status**: Completed. Build clean. `cargo fmt --all`, `just build`, and `just test` pass (1026/1026 tests, 0 failures, 10 new Phase 9 regression tests).

**Implementation summary**:
- **13.1 (Result-based error handling)**: Added `TtyError` enum with 7 variants (`InvalidIndex`, `NotAllocated`, `BackgroundRead`, `BackgroundWrite`, `HungUp`, `WouldBlock`, `PermissionDenied`). Converted all 14 public API functions (`read`, `write`, `get_termios`, `set_termios`, `get_winsize`, `set_winsize`, `get_foreground_pgrp`, `set_foreground_pgrp`, `set_foreground_pgrp_checked`, `get_session_id`, `open_ref`, `close_ref`, `set_compositor_focus`, `get_compositor_focus`) to return `Result<T, TtyError>`. Void/bool helpers (`push_input`, `hangup`, `attach_session`, etc.) left unchanged.
- **13.2 (Slice-based internal API)**: `read()` now accepts `&mut [u8]`, `write()` accepts `&[u8]` internally. Raw pointer conversion happens only at the syscall adapter boundary in `syscall_services_init.rs` (16 adapter functions updated).
- **13.3 (VMIN/VTIME enforcement)**: Implemented all 4 POSIX non-canonical read cases using `wait_event_timeout` from `lib/src/waitqueue.rs`. Added `vmin_vtime()` and `is_canonical()` helpers to `ldisc.rs`. Case matrix: VMIN=0/VTIME=0 (immediate return), VMIN=0/VTIME>0 (timed wait), VMIN>0/VTIME=0 (block until VMIN bytes), VMIN>0/VTIME>0 (inter-byte timeout).
- **13.4 (`/dev/tty` support)**: Added intercept in `fs/src/fileio.rs::file_open_for_process()` that resolves `/dev/tty` to the calling process's controlling terminal via `current_task_controlling_tty()` from `DriverRuntimeServices`. Returns `-ENXIO` if no controlling terminal. Wired through `core/src/driver_hooks.rs` reading the task struct's `controlling_tty: Option<TtyIndex>` field.
- **13.5 (ABI boundary preserved)**: The `TtyServices` trait in `lib/` was NOT modified — it retains raw pointer signatures for ABI stability. Result/slice conversion happens entirely in the adapter layer (`syscall_services_init.rs`). Error mapping: `WouldBlock → -11 (EAGAIN)`, `HungUp → -5 (EIO)`, others `→ -1`.
- **Regression tests**: 10 new Phase 9 tests: `test_phase9_tty_error_variants` (all 7 error variants exist), `test_phase9_read_returns_result` (read returns Ok), `test_phase9_read_invalid_index_error` (out-of-range index), `test_phase9_read_not_allocated_error` (unallocated slot), `test_phase9_write_returns_result` (write returns Ok with count), `test_phase9_get_termios_returns_result` (termios retrieval), `test_phase9_vmin0_vtime0_immediate_return` (immediate non-canonical read), `test_phase9_vmin_enforcement` (VMIN byte count enforcement), `test_phase9_set_fg_pgrp_checked_permission_denied` (permission error on non-member pgrp), `test_phase9_hangup_read_returns_hung_up` (hung-up TTY returns HungUp error).

**Goal**: Adopt Rust-idiomatic error handling internally, enforce VMIN/VTIME in non-canonical mode, and add `/dev/tty` support for POSIX compliance.

### 13.1 Internal `Result<usize, Errno>` error handling

**What**: Replace C-style `isize` returns with `Result` internally, converting at the syscall ABI boundary.

```rust
/// Kernel-internal error type for TTY operations.
#[derive(Debug, Clone, Copy)]
pub enum TtyError {
    InvalidIndex,
    NotAllocated,
    BackgroundRead,   // → SIGTTIN
    BackgroundWrite,  // → SIGTTOU
    HungUp,           // → -EIO
    WouldBlock,       // → -EAGAIN
}

// Internal API returns Result:
pub fn read(idx: TtyIndex, buf: &mut [u8], nonblock: bool) -> Result<usize, TtyError> { ... }

// Syscall boundary converts:
fn tty_read_adapter(idx: u8, buf: *mut u8, max: usize, nonblock: bool) -> isize {
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, max) };
    match tty::read(TtyIndex(idx), slice, nonblock) {
        Ok(n) => n as isize,
        Err(TtyError::WouldBlock) => -11, // EAGAIN
        Err(TtyError::HungUp) => -5,      // EIO
        Err(_) => -1,
    }
}
```

### 13.2 Accept slices internally (not raw pointers)

**What**: Change internal functions from `read(idx, buffer: *mut u8, max: usize)` to `read(idx, buf: &mut [u8])`.  Keep raw pointers only at the syscall adapter boundary.

### 13.3 VMIN/VTIME enforcement

**What**: Implement non-canonical read timing:

- **VMIN > 0, VTIME == 0**: Block until VMIN bytes available (current behavior, but enforce count)
- **VMIN == 0, VTIME > 0**: Return immediately if data; else wait up to VTIME deciseconds
- **VMIN > 0, VTIME > 0**: Block until VMIN bytes OR VTIME inter-byte timeout
- **VMIN == 0, VTIME == 0**: Return immediately with available data (pure non-blocking)

Requires timer integration (scheduler or PIT-based timeout) for VTIME.

### 13.4 `/dev/tty` support

**What**: Allow a process to refer to "my controlling terminal" generically, needed for `isatty()`, `ttyname()`, and POSIX utilities.

- When opening `/dev/tty`, resolve to the process's `controlling_tty` from its task struct
- Return `-ENXIO` if no controlling terminal
- Wire into VFS/devfs if present, or handle as a special path in `open()`

### 13.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Change signatures to `Result<usize, TtyError>`, accept `&mut [u8]` slices, add `TtyError` enum |
| `drivers/src/tty/ldisc.rs` | Add VMIN/VTIME enforcement in `read()`, add timer-based timeout |
| `drivers/src/tty/session.rs` | Return `TtyError` variants instead of booleans |
| `drivers/src/syscall_services_init.rs` | Convert `Result` to `isize` at adapter boundary |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update `TtyServices` to match new signatures |
| `fs/src/fileio.rs` | Add `/dev/tty` resolution in `file_open_fd` |
| `drivers/src/tty_tests.rs` | Update all tests for `Result` returns, add VMIN/VTIME tests, `/dev/tty` tests |

---

## 14. Phase 10: Job Control Correctness ✅ COMPLETED

> **Priority**: P0 — Must fix before any real shell or interactive program works correctly.
> **Rationale**: The deep architectural review (comparing against Linux `tty_struct` + `n_tty` and RedoxOS) found that **write-side foreground enforcement is completely missing** — `write()` never calls `check_write()`, `SIGTTOU` is never delivered, and `check_read()` permits cross-session reads with a TODO comment. No job-control-aware shell (bash, zsh) can work correctly without these. Issues #2, #10, #12 from the review.

**Status**: Completed. All tests pass (11 suites, 0 failures). Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- **14.1 (Wire `check_write()` into `write()`)**: Added foreground check at the start of `write()` in `drivers/src/tty/mod.rs`. When `TOSTOP` is set in `c_lflag`, the write path reads the caller's task ID, looks up session/termios state from the locked TTY slot, and calls `check_write(caller_pgid, tostop)`. If `BackgroundWrite` is returned, `SIGTTOU` is delivered to the caller's process group (outside the lock) and `Err(TtyError::BackgroundWrite)` is returned. Kernel tasks (task_id == 0) are exempted to allow early-boot writes.
- **14.2 (Deliver SIGTTOU)**: Added `pub const SIGTTOU: u8 = 22` to `abi/src/syscall.rs` (matching the existing definition in `abi/src/signal.rs`). Imported `SIGTTOU` and `TOSTOP` in `drivers/src/tty/mod.rs` for use in the write-side foreground check.
- **14.3 (Tighten `check_read()`)**: Replaced the permissive cross-session TODO block in `session.rs::check_read()` with proper rejection — `caller_sid != 0 && caller_sid != self.session_id` now returns `ForegroundCheck::NoSession` (maps to `-EIO` in read). Kernel tasks (sid == 0) remain exempted for early-boot bootstrap.
- **Regression tests**: Added 8 Phase 10 tests: `test_phase10_sigttou_constant`, `test_phase10_check_write_tostop_blocks_background`, `test_phase10_check_write_no_tostop_allows_background`, `test_phase10_check_write_tostop_allows_foreground`, `test_phase10_check_read_cross_session_rejected`, `test_phase10_check_read_same_session_foreground`, `test_phase10_check_read_kernel_task_allowed`, `test_phase10_tty_write_foreground_with_tostop`.

**Goal**: Wire write-side foreground gating, deliver SIGTTOU for background writes, tighten read-side session enforcement, and close the remaining job-control gaps.

### 14.1 Wire `check_write()` into `write()`

**Problem**: `session.rs` defines `check_write()` returning `ForegroundCheck::BackgroundWrite`, but `mod.rs::write()` never calls it. Background processes can freely write to the terminal, overwriting foreground output.

**Fix**:
```rust
// In tty::write() — add foreground check before output processing
pub fn write(idx: TtyIndex, data: &[u8]) -> Result<usize, TtyError> {
    let slot = idx.0 as usize;
    // ... lock slot ...
    
    // Check TOSTOP: if set, background writers get SIGTTOU
    if (tty.ldisc.termios().c_lflag & TOSTOP) != 0 {
        match tty.session.check_write(caller_pgid, caller_sid) {
            ForegroundCheck::Allowed => {}
            ForegroundCheck::BackgroundWrite => {
                let pgid = caller_pgid;
                drop(guard);
                signal_process_group(pgid, SIGTTOU);
                return Err(TtyError::BackgroundWrite);
            }
            _ => {}
        }
    }
    
    // ... existing output processing ...
}
```

### 14.2 Deliver SIGTTOU for background writes

**Problem**: The `SIGTTOU` constant exists in `abi/src/signal.rs` (value 22) but is never used anywhere in the TTY subsystem. Linux sends SIGTTOU to background process groups that attempt to write when TOSTOP is set.

**Fix**:
- Add SIGTTOU delivery in `write()` when `check_write()` returns `BackgroundWrite`
- Ensure `SIGTTOU` constant is accessible from `drivers/src/tty/mod.rs`
- Match Linux behavior: only enforce when `TOSTOP` is set in `c_lflag`

### 14.3 Tighten `check_read()` to reject cross-session access

**Problem**: `session.rs::check_read()` currently allows reads from processes in a different session with a TODO comment ("Phase 5 will enforce"). This was never closed. Any process on the system can read any TTY regardless of session membership.

**Fix**:
```rust
pub fn check_read(&self, caller_pgid: u64, caller_sid: u64) -> ForegroundCheck {
    // No session → allow (bootstrap path)
    if self.session_id == NO_SESSION {
        return ForegroundCheck::Allowed;
    }
    
    // Different session → reject (POSIX requirement)
    if caller_sid != self.session_id as u64 {
        return ForegroundCheck::NoSession;  // -EIO in read()
    }
    
    // Same session, foreground group → allow
    if self.fg_pgrp == NO_FOREGROUND_PGRP || caller_pgid == self.fg_pgrp as u64 {
        return ForegroundCheck::Allowed;
    }
    
    // Same session, background group → SIGTTIN
    ForegroundCheck::BackgroundRead
}
```

### 14.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Add `check_write()` call in `write()`, deliver SIGTTOU, return `BackgroundWrite` error |
| `drivers/src/tty/session.rs` | Tighten `check_read()` to reject cross-session, remove permissive TODO |
| `abi/src/syscall.rs` | Add `SIGTTOU` constant (value 22) |
| `drivers/src/tty_tests.rs` | Add 8 Phase 10 regression tests: background write blocked with TOSTOP, cross-session read rejected, SIGTTOU constant, foreground write allowed, kernel task exempted |
---

## 15. Phase 11: Non-Canonical Timing Fix ✅ COMPLETED

> **Priority**: P0 — Breaks interactive serial tools and terminal programs.
> **Rationale**: POSIX specifies that when both VMIN > 0 and VTIME > 0, the timer starts **after the first byte arrives**, not when `read()` is called. The current implementation starts the timeout at call entry, meaning slow typists or serial devices get premature timeouts. Programs like minicom, picocom, and readline in raw mode depend on correct inter-byte timing.

**Goal**: Fix the VMIN > 0 / VTIME > 0 case to use inter-byte timeout semantics per POSIX.

**Status**: Completed. All tests pass (1039/1039, 5 new Phase 11 regression tests). Build clean. `cargo fmt --all`, `just build`, and `just test` pass.

**Implementation summary**:
- **15.1 (Two-phase wait for VMIN>0/VTIME>0)**: Rewrote the `(_, _)` match arm in `drivers/src/tty/mod.rs::read()` to implement correct POSIX inter-byte timeout semantics. When `total == 0` (no bytes received yet), `wait_timeout_ms` remains `None` so the read blocks indefinitely via `wait_event()` until the first byte arrives. Once `total > 0` (at least one byte received), `wait_timeout_ms` is set to `vtime_ms` so subsequent waits use `wait_event_timeout()` with the inter-byte timer. This matches the POSIX specification: the timer starts after the first byte, not at `read()` entry.
- **15.2 (No ldisc changes needed)**: The `vmin_vtime()` helper and `is_canonical()` methods in `ldisc.rs` were already correct and required no modifications.
- **Regression tests**: 5 new Phase 11 tests: `test_phase11_vmin_vtime_enough_data_returns_immediately` (VMIN bytes available returns immediately), `test_phase11_vmin_vtime_partial_nonblock` (partial data with nonblock returns available bytes), `test_phase11_vmin_vtime_no_data_nonblock` (no data returns WouldBlock), `test_phase11_vmin_vtime_interbyte_timeout_returns_partial` (blocking read with 1 byte returns after inter-byte timeout instead of blocking indefinitely), `test_phase11_ldisc_vmin_vtime_helper` (vmin_vtime accessor correctness).

### 15.1 Previous behavior (incorrect)

```rust
// In read() — VMIN>0, VTIME>0 case
// Timer started at read() call entry — WRONG
should_wait = true;
wait_timeout_ms = Some(vtime_ms); // ← always set, even with no bytes yet
```

### 15.2 Correct POSIX behavior (implemented)

```rust
// VMIN>0, VTIME>0: inter-byte timeout.
// The timer starts after the first byte arrives,
// NOT when read() is called.
should_wait = true;
// Phase 1: no bytes yet — wait indefinitely for the first byte.
// Phase 2: at least one byte received — start inter-byte timer.
if total > 0 {
    wait_timeout_ms = Some(vtime_ms);
}
// else: wait_timeout_ms remains None (indefinite)
```

### 15.3 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Rewrote `(_, _)` match arm in `read()` with two-phase wait: indefinite for first byte, inter-byte timeout after |
| `drivers/src/tty/ldisc.rs` | No changes needed (`vmin_vtime()` helper already correct) |
| `drivers/src/tty_tests.rs` | Added 5 Phase 11 regression tests (enough data immediate return, partial nonblock, no data WouldBlock, inter-byte timeout partial return, vmin_vtime helper) |


## 16. Phase 12: Sane Defaults & Output Column Tracking ✅ COMPLETED

> **Priority**: P1 — Without this, every terminal program looks broken out of the box.
> **Rationale**: Default `c_iflag: 0` and `c_oflag: 0` means `printf("hello\n")` doesn't produce a carriage return — the cursor advances to the next line but stays at the current column. Every userland program must manually `tcsetattr()` to enable ICRNL and ONLCR. Linux defaults have both on. Additionally, column tracking only happens in the input echo path, not the output path, breaking tab expansion and accurate line-erase.

**Goal**: Set Linux-compatible default termios flags and implement bidirectional column tracking.

### 16.1 Set sane default termios

```rust
// In LineDisc::new() or Tty::new()
let default_termios = UserTermios {
    c_iflag: ICRNL,                    // CR → NL on input
    c_oflag: OPOST | ONLCR,           // NL → CRNL on output
    c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE,
    c_cflag: 0,                        // unchanged
    c_cc: default_cc_array(),          // existing defaults
};
```

### 16.2 Track columns in output path

**Problem**: `process_output_byte()` in `ldisc.rs` doesn't update `self.column`. Only the input echo path tracks columns. This means ECHOKE can't accurately erase lines, and tab expansion can't compute stop positions.

**Fix**:
```rust
pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
    let oflag = self.termios.c_oflag;
    if (oflag & OPOST) == 0 {
        return OutputAction::Write(c);
    }
    
    match c {
        b'\n' if (oflag & ONLCR) != 0 => {
            self.column = 0;
            OutputAction::WritePair(b'\r', b'\n')
        }
        b'\r' => {
            self.column = 0;
            if (oflag & OCRNL) != 0 { OutputAction::Write(b'\n') }
            else if (oflag & ONOCR) != 0 && self.column == 0 { OutputAction::Suppress }
            else { OutputAction::Write(b'\r') }
        }
        b'\t' => {
            let spaces = 8 - (self.column % 8);
            self.column += spaces;
            OutputAction::Tab(spaces as u8)
        }
        b'\x08' => {  // Backspace
            if self.column > 0 { self.column -= 1; }
            OutputAction::Write(c)
        }
        c if c >= 0x20 && c < 0x7F => {
            self.column += 1;
            OutputAction::Write(c)
        }
        _ => OutputAction::Write(c),
    }
}
```

### 16.3 Add `OutputAction::Tab` variant

The existing `OutputAction` enum needs a new variant for tab expansion:
```rust
pub enum OutputAction {
    Write(u8),
    WritePair(u8, u8),
    Tab(u8),       // New: expand to N spaces
    Suppress,
}
```

### 16.4 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/ldisc.rs` | Update default termios, add column tracking to `process_output_byte()`, add `OutputAction::Tab`, tab expansion |
| `drivers/src/tty/mod.rs` | Handle `OutputAction::Tab` in `write()`, update default `Tty::new()` termios |
| `drivers/src/tty/mod.rs` | Handle `OutputAction::Tab` in `write()`, buffer threshold relaxed to `OUT_BUF_CAP - 8` |
| `drivers/src/tty_tests.rs` | Fixed 3 existing tests for new defaults; added 10 Phase 12 regression tests |

### 16.5 Implementation summary

**Completed**: All three sub-tasks implemented and verified.

1. **Sane defaults** (`LineDisc::new()`): `c_iflag: ICRNL`, `c_oflag: OPOST | ONLCR`, `c_lflag` gains `ECHOK | ECHOCTL | ECHOKE`.
2. **Output column tracking** (`process_output_byte()`): Rewritten with full column accounting for printable chars (+1), NL/CR (→0), tab (8-stop expansion), backspace (−1), and control chars (no change). Non-OPOST path tracks via `update_column_raw()`. Proper ONOCR (suppress CR at col 0), ONLRET (NL resets col), OCRNL+ONLRET interaction.
3. **Tab expansion**: New `OutputAction::Tab(u8)` variant returns the number of spaces; `Tty::write()` emits N spaces. Buffer split threshold relaxed from `OUT_BUF_CAP - 2` to `OUT_BUF_CAP - 8`.

**Tests** (10 new, 3 fixed):
- `test_phase12_default_termios_has_icrnl`
- `test_phase12_default_termios_has_opost_onlcr`
- `test_phase12_default_termios_has_full_lflag`
- `test_phase12_output_column_tracking_printable`
- `test_phase12_output_column_tracking_newline`
- `test_phase12_output_column_tracking_cr`
- `test_phase12_output_column_tracking_tab`
- `test_phase12_output_column_tracking_backspace`
- `test_phase12_onocr_at_column_zero`
- `test_phase12_default_onlcr_newline_expands`

All 55 test suites pass (0 failures). `cargo fmt` clean.
---

## 17. Phase 13: ABI Signal Constant Unification

> **Priority**: P2 — Maintenance hazard, not a runtime bug yet.
> **Rationale**: Signal constants (`SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`, `SIGCONT`) are defined in **both** `abi/src/syscall.rs` and `abi/src/signal.rs`. The TTY subsystem imports from both: `mod.rs` uses `slopos_abi::signal::*` while `ldisc.rs` uses `slopos_abi::syscall::*`. If the values drift between files, signal delivery will silently break.

**Goal**: Establish `abi/src/signal.rs` as the single canonical source for all signal constants. Remove duplicates from `abi/src/syscall.rs`.

### 17.1 Remove duplicate signal constants from `syscall.rs`

**Currently duplicated**:
- `SIGINT` (2), `SIGQUIT` (3), `SIGHUP` (1), `SIGTSTP` (20), `SIGTTIN` (21), `SIGTTOU` (22), `SIGCONT` (18)

**Action**: Delete these from `abi/src/syscall.rs`. Keep only in `abi/src/signal.rs`.

### 17.2 Update imports across the codebase

```rust
// BEFORE (ldisc.rs):
use slopos_abi::syscall::{SIGINT, SIGQUIT, SIGTSTP};

// AFTER (ldisc.rs):
use slopos_abi::signal::{SIGINT, SIGQUIT, SIGTSTP};
```

### 17.3 Re-export from `abi/src/lib.rs` if needed

If other crates can't directly access `abi::signal::*`, add a re-export:
```rust
// abi/src/lib.rs
pub mod signal;  // ensure public
```

### 17.4 Files modified

| File | Change |
|------|--------|
| `abi/src/syscall.rs` | Remove duplicate `SIGINT`, `SIGQUIT`, `SIGHUP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU`, `SIGCONT` |
| `abi/src/signal.rs` | Verify all signal constants are present (already should be) |
| `abi/src/lib.rs` | Ensure `pub mod signal` is exported |
| `drivers/src/tty/ldisc.rs` | Update imports: `slopos_abi::syscall::SIG*` → `slopos_abi::signal::SIG*` |
| `drivers/src/tty/mod.rs` | Verify imports use `slopos_abi::signal::*` (already does) |
| Any other files importing signal constants from `syscall.rs` | Update imports |

---

## 18. Phase 14: Responsibility Split — PTY Foundation

> **Priority**: P2 — Required before PTY implementation, not blocking current functionality.
> **Rationale**: The current `Tty` struct (919 lines in `mod.rs`) mixes core state, I/O processing, session management, idle callbacks, and signal dispatch in one module. Linux separates these into `tty_io.c`, `tty_jobctrl.c`, `tty_port.c`, and the swappable `tty_ldisc` interface. Adding PTY support to the current monolithic `Tty` would compound coupling — PTY master needs raw passthrough (no line discipline), while PTY slave uses full N_TTY processing. Without a swappable line discipline abstraction, this can't be cleanly expressed.

**Goal**: Split internal responsibilities to prepare for PTY master/slave pairs without redesigning the static slot model or public API.

### 18.1 Extract session policy from `Tty`

Move all session/job-control logic from `mod.rs` into `session.rs`:
- `auto_attach_session()` → `session.rs`
- `detach_session_by_id()` → `session.rs`
- Foreground check wrappers → `session.rs`

`Tty` retains a `session: TtySession` field but delegates all policy decisions.

### 18.2 Add line discipline abstraction

```rust
// drivers/src/tty/ldisc.rs

/// Line discipline operations — swappable per-TTY.
pub enum LdiscKind {
    /// Full N_TTY processing (canonical, echo, signals, etc.)
    NTty(LineDisc),
    /// Raw passthrough (for PTY master, future SLIP/PPP)
    Raw,
}

impl LdiscKind {
    pub fn process_input(&mut self, c: u8) -> InputAction { ... }
    pub fn process_output(&mut self, c: u8) -> OutputAction { ... }
    pub fn read(&mut self, buf: &mut [u8]) -> usize { ... }
    pub fn has_data(&self) -> bool { ... }
}
```

### 18.3 Prepare `TtyDriverKind` for PTY

```rust
pub enum TtyDriverKind {
    SerialConsole(SerialConsoleDriver),
    VConsole(VConsoleDriver),
    PtyMaster { slave_idx: TtyIndex },   // Future
    PtySlave { master_idx: TtyIndex },    // Future
    None,
}
```

### 18.4 Use sentinel newtypes for session/pgrp IDs

Replace raw `u64` with `Option<NonZeroU64>` or dedicated newtypes:
```rust
pub struct SessionId(NonZeroU64);
pub struct ProcessGroupId(NonZeroU64);

pub struct TtySession {
    pub session_leader: Option<SessionId>,
    pub session_id: Option<SessionId>,
    pub fg_pgrp: Option<ProcessGroupId>,
}
```

This eliminates the `NO_SESSION = 0` / `NO_FOREGROUND_PGRP = 0` sentinel constants and makes invalid states unrepresentable.

### 18.5 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Extract session policy methods to `session.rs`, use `LdiscKind` instead of bare `LineDisc` |
| `drivers/src/tty/ldisc.rs` | Add `LdiscKind` enum, `Raw` variant with passthrough |
| `drivers/src/tty/driver.rs` | Add `PtyMaster`/`PtySlave` variant stubs to `TtyDriverKind` |
| `drivers/src/tty/session.rs` | Absorb extracted session methods, add `SessionId`/`ProcessGroupId` newtypes |
| `drivers/src/tty_tests.rs` | Update for `LdiscKind`, add `Raw` passthrough tests, newtype tests |

---

## 19. Phase 15: Verify & Test

> **Priority**: Final gate — comprehensive verification after all correctness and structural fixes.

### 19.1 Build verification

```bash
just build          # Must compile cleanly
just test           # Must pass existing test harness + all new phase tests
```

### 19.2 Manual test cases (original)

| Test | Expected Result |
|------|----------------|
| Boot to shell | Shell prompt appears, typing works normally (one char per keypress) |
| Type "hello" in shell | Exactly "hello" appears (no doubling) |
| Run `echo hello` | "hello" printed to serial |
| Run `nc -v 8888` (with host listener) | Connects, typing echoes once, Enter sends line |
| Ctrl+C in shell | "^C" echoed, line cancelled |
| Ctrl+D on empty line | Shell exits (EOF) |
| Backspace in shell | Erases one character |
| Arrow keys in shell | Navigate history / cursor |
| Run long command | Line editing works normally |
| Fork+exec child process | Child inherits TTY, can read/write, parent waits |
| Child exit → parent resume | Parent shell resumes with working TTY |

### 19.3 New test cases (Phases 10–14)

| Test | Expected Result | Phase |
|------|----------------|-------|
| Background process writes with TOSTOP set | Write blocked, SIGTTOU delivered | 10 |
| Background process writes with TOSTOP unset | Write succeeds (Linux default) | 10 |
| Cross-session read attempt | Returns -EIO, not data | 10 |
| Same-session background read | Returns SIGTTIN | 10 |
| VMIN=5, VTIME=1: type 3 chars, wait | Returns 3 chars after inter-byte timeout (not 0) | 11 |
| VMIN=5, VTIME=1: type 5 chars quickly | Returns 5 chars immediately | 11 |
| Default terminal: `printf("hello\n")` | Produces `hello\r\n` (ONLCR active by default) | 12 |
| Default terminal: type Enter | CR mapped to NL (ICRNL active by default) | 12 |
| Tab character output | Expanded to spaces at correct tab stop | 12 |
| Column tracking after mixed output | Column position accurate after CR/NL/tab/printable | 12 |
| Signal constants consistency | No duplicate definitions — all imports from `signal.rs` | 13 |
| PTY master raw passthrough | Input bytes pass through without ldisc processing | 14 |
| `LdiscKind::Raw` vs `NTty` switching | Correct behavior when swapping line discipline | 14 |
| `SessionId`/`ProcessGroupId` newtypes | No sentinel 0 values — `Option::None` for absent | 14 |

### 19.4 Regression checks

- Shell scrollback still works
- Serial output still works for klog
- Mouse/pointer events still work (input_event.rs unchanged for mouse)
- Pipe operations still work
- File I/O still works (non-console FDs unchanged)
- All 1026+ existing tests still pass
- No new compiler warnings introduced

---

## 20. File Inventory

### New files

| File | Purpose |
|------|---------|
| `drivers/src/tty/mod.rs` | TTY public API (replaces `tty.rs`) |
| `drivers/src/tty/driver.rs` | `TtyDriver` trait, `SerialConsoleDriver`, `VConsoleDriver` |
| `drivers/src/tty/table.rs` | `TTY_SLOTS` global array, init, lookup |
| `drivers/src/tty/ldisc.rs` | Enhanced `LineDisc`, future `LdiscKind` abstraction |
| `drivers/src/tty/session.rs` | `TtySession`, foreground checks, session policy |
| `drivers/src/tty/wait_queue.rs` | Per-TTY `WaitQueue` (extracted from current tty.rs) |

### Modified files

| File | Nature of change |
|------|-----------------|
| `drivers/src/lib.rs` | Update `mod tty` to module dir |
| `drivers/src/ps2/keyboard.rs` | Remove `input_route_key_event`, call `tty::push_input` |
| `fs/src/fileio.rs` | Replace `console` bool with `tty_index`, update routing |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Per-TTY ioctl dispatch |
| `core/src/scheduler/task.rs` | Add `controlling_tty`, `pgrp`, `session_id` |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update to per-TTY API |
| `abi/src/syscall.rs` | Add iflag/oflag/lflag constants, new c_cc indices |
| `abi/src/signal.rs` | Canonical source for all signal constants |

### Deleted files

| File | Reason |
|------|--------|
| `drivers/src/tty.rs` | Replaced by `drivers/src/tty/mod.rs` |
| `drivers/src/line_disc.rs` | Replaced by `drivers/src/tty/ldisc.rs` |

---

## 21. Future: PTY Support

Not in scope for this plan, but the architecture (after Phase 14) is designed to accommodate it:

```rust
pub struct PtyMasterDriver {
    slave_tty: TtyIndex,
    // Master side: reads from slave's output, writes to slave's input
}

pub struct PtySlaveDriver {
    master_tty: TtyIndex,
    // Slave side: reads from line discipline, writes through line discipline
}
```

A PTY pair would be:
- TTY N (slave) — has `PtySlaveDriver`, owns a `LineDisc` via `LdiscKind::NTty`
- TTY M (master) — has `PtyMasterDriver`, uses `LdiscKind::Raw` (no ldisc processing)

The master reads what the slave writes (after output processing), and the master writes directly to the slave's line discipline input.

This enables:
- `ssh` / `screen` / `tmux` style multiplexing
- Subshells with proper terminal control
- Remote terminal access

---

## Appendix: Linux N_TTY Reference

For implementation reference, Linux's N_TTY line discipline (`drivers/tty/n_tty.c`) handles:

1. **Input processing** (`n_tty_receive_char_inline`):
   - c_iflag: ISTRIP, IGNCR, ICRNL, INLCR, IUCLC
   - Signal chars: VINTR→SIGINT, VQUIT→SIGQUIT, VSUSP→SIGTSTP
   - Flow control: VSTOP/VSTART with IXON
   - Canonical editing: VERASE, VWERASE, VKILL, VREPRINT, VLNEXT
   - Echo processing with column tracking

2. **Output processing** (`do_output_char`):
   - c_oflag: OPOST, ONLCR, OCRNL, ONOCR, ONLRET
   - Tab expansion (TABDLY)
   - Column tracking for proper backspace echo

3. **Read** (`n_tty_read`):
   - Canonical: return on newline or EOF
   - Non-canonical: VMIN/VTIME timing (inter-byte for VMIN>0/VTIME>0)
   - Job control: SIGTTIN for background reads, SIGTTOU for background writes with TOSTOP

The SlopOS implementation follows this structure but simplified (no IUCLC, no TABDLY baud rate, no UTF-8 for now).
