# SlopOS TTY Overhaul Plan

> **Status**: Phase 1 Complete + Shim Removal Complete + Read/Wakeup Hotfix Complete + Phase 2 Complete + Phase 3 Complete + Phase 4 Complete + **Phase 5 Complete** (Phases 6–10 Planned)
> **Target**: Replace the global singleton TTY with a proper per-terminal TTY subsystem comparable to Linux N_TTY / RedoxOS
> **Current**: `drivers/src/tty/` module directory — clean per-TTY API, no backward-compatible shims, `TtyServices` takes `tty_index: u8` for per-TTY operations
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
10. [Phase 6: Verify & Test](#10-phase-6-verify--test)
11. [Phase 7: Control-Plane Correctness](#11-phase-7-control-plane-correctness)
12. [Phase 8: Lifecycle & Hangup Semantics](#12-phase-8-lifecycle--hangup-semantics)
13. [Phase 9: Per-TTY Locking & Performance](#13-phase-9-per-tty-locking--performance)
14. [Phase 10: Rust Idioms & Termios Completion](#14-phase-10-rust-idioms--termios-completion)
15. [File Inventory](#15-file-inventory)
16. [Future: PTY Support](#16-future-pty-support)

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
| 6 | Verification | — | — |
| 7 | Control-plane correctness | `drivers/src/tty/mod.rs`, `drivers/src/tty/session.rs`, `drivers/src/tty/ldisc.rs`, `fs/src/fileio.rs`, `lib/src/kernel_services/syscall_services/tty.rs`, `drivers/src/syscall_services_init.rs`, `core/src/syscall/ui_handlers.rs`, `drivers/src/tty_tests.rs` | — |
| 8 | Lifecycle & hangup | `drivers/src/tty/mod.rs`, `drivers/src/tty/table.rs`, `drivers/src/tty/session.rs`, `core/src/scheduler/task.rs`, `fs/src/fileio.rs`, `drivers/src/tty_tests.rs` | — |
| 9 | Per-TTY locking & perf | `drivers/src/tty/table.rs`, `drivers/src/tty/mod.rs`, `drivers/src/tty_tests.rs` | — |
| 10 | Rust idioms & termios | `drivers/src/tty/mod.rs`, `drivers/src/tty/ldisc.rs`, `drivers/src/tty/session.rs`, `drivers/src/tty_tests.rs` | — |

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
| VMIN/VTIME | Full non-canonical timing | Yes | ❌ Parsed but not enforced |
| Controlling terminal | Per-process `/dev/tty` | Per-process scheme | ❌ Ad-hoc `TTY_FOCUSED_TASK_ID` |
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

## 10. Phase 6: Verify & Test

### 10.1 Build verification

```bash
just build          # Must compile cleanly
just test           # Must pass existing test harness
```

### 10.2 Manual test cases

| Test | Expected Result |
|------|----------------|
| Boot to shell | Shell prompt appears, typing works normally (one char per keypress) |
| Type "hello" in shell | Exactly "hello" appears (no doubling) |
| Run `echo hello` | "hello" printed to serial |
| Run `nc -v 8888` (with host listener) | Connects, typing echoes once, Enter sends line (TCP bug separate) |
| Ctrl+C in shell | "^C" echoed, line cancelled |
| Ctrl+D on empty line | Shell exits (EOF) |
| Backspace in shell | Erases one character |
| Arrow keys in shell | Navigate history / cursor |
| Run long command | Line editing works normally |
| Fork+exec child process | Child inherits TTY, can read/write, parent waits |
| Child exit → parent resume | Parent shell resumes with working TTY |

### 10.3 Regression checks

- Shell scrollback still works
- Serial output still works for klog
- Mouse/pointer events still work (input_event.rs unchanged for mouse)
- Pipe operations still work
- File I/O still works (non-console FDs unchanged)


## 11. Phase 7: Control-Plane Correctness

> **Priority**: P0 — Must fix before any new feature work.
> **Rationale**: The deep architectural review (comparing against Linux `tty_struct` + `n_tty` and RedoxOS) identified that the biggest risks in SlopOS's TTY are **not** parsing/processing logic (which is solid) but **control-plane correctness**: compositor focus is conflated with POSIX session foreground, the transitional `task_has_access()` permanently bypasses session control, and the `TtyIndex` type leaks to raw `u8` at the FD boundary.

**Goal**: Establish correct POSIX-like control semantics by separating compositor focus from session foreground, making `check_read()` the authoritative gate, and enforcing type safety across crate boundaries.

### 11.1 Split compositor focus from POSIX foreground

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

### 11.2 Replace `task_has_access()` with proper `check_read()` gating

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

### 11.3 Type safety: `Option<TtyIndex>` end-to-end

**Problem**: `fs/src/fileio.rs` uses `tty_index: Option<u8>` and `TtyServices` bridge functions accept raw `u8`.  Any integer can be silently passed as a TTY index without compile-time checking.

**Fix**:
- Change `FileDescriptor.tty_index` from `Option<u8>` to `Option<TtyIndex>` (re-export `TtyIndex` in `abi/` or `lib/`)
- Update `TtyServices` function signatures to accept `TtyIndex` (or a newtype `TtyHandle(u8)` that lives in `abi/`)
- Update `file_get_tty_index()` to return `Option<TtyIndex>`
- Update all ioctl dispatch in `poll_ioctl_handlers.rs` to pass `TtyIndex`
- Update `drivers/src/syscall_services_init.rs` adapters

### 11.4 Replace hardcoded signal numbers with ABI constants

**Problem**: `ldisc.rs` uses magic numbers `2` (SIGINT), `3` (SIGQUIT), `20` (SIGTSTP) in `InputAction::Signal()`.

**Fix**:
- Add `SIGINT`, `SIGQUIT`, `SIGTSTP` constants to `abi/src/syscall.rs` (or a new `abi/src/signal.rs`)
- Replace all hardcoded signal numbers in `ldisc.rs` with named constants
- Update test assertions to use the same constants

### 11.5 Files modified

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

## 12. Phase 8: Lifecycle & Hangup Semantics

> **Priority**: P1 — Must fix before PTY implementation or proper shell exit.
> **Rationale**: Linux's biggest TTY complexity is lifecycle management, and it exists for a reason.  Without open counts and hangup signaling, dead processes can hold TTY resources, PTY pairs can't clean up, and shell exit doesn't notify children.  This is the single biggest "pain later" item if deferred.

**Goal**: Add TTY lifecycle management (open/close tracking, hangup signaling, controlling terminal acquisition) modeled after Linux's `tty_port` + `kref` pattern, adapted for SlopOS's static table.

### 12.1 Add open count to `Tty`

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

### 12.2 Implement `tty_hangup()`

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

### 12.3 Wire hangup into process/session leader exit

- In `core/src/scheduler/task.rs` (or process exit path): when a session leader exits, find its controlling TTY and call `tty::hangup()`
- In `setsid()`: already calls `detach_session_by_id()` — verify it also handles hangup if old session had a controlling terminal

### 12.4 Controlling terminal acquisition (`TIOCSCTTY`)

**What**: Implement the `TIOCSCTTY` ioctl so a session leader can explicitly acquire a controlling terminal.

- Only session leaders with no existing controlling TTY may call this
- Set `tty.session.attach(caller_sid, caller_pgid)`
- Set `controlling_tty` in the process's task struct
- Add `TIOCSCTTY` constant to `abi/src/syscall.rs` and dispatch in `poll_ioctl_handlers.rs`

### 12.5 Add `flush_all()` to `LineDisc`

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

### 12.6 Blocked reader behavior on hangup

- Readers blocked in `TTY_INPUT_WAITERS[idx].wait_event(...)` are woken by `hangup()`
- On wakeup, `read()` re-checks: if session is detached and no data, return 0 (EOF)
- Non-blocking reads return `-EIO` if TTY is hung up
- Add a `hung_up: bool` flag to `Tty` to track post-hangup state

### 12.7 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/mod.rs` | Add `hangup()`, update `read()` for EOF-on-hangup, add `open_count` management |
| `drivers/src/tty/table.rs` | Add `open_count` to `Tty::new()` |
| `drivers/src/tty/ldisc.rs` | Add `flush_all()` |
| `drivers/src/tty/session.rs` | No changes (detach/attach already exist) |
| `core/src/scheduler/task.rs` | Wire session-leader exit → `tty::hangup()` |
| `fs/src/fileio.rs` | Increment/decrement `open_count` on FD open/close/dup/fork |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Add `TIOCSCTTY` dispatch |
| `abi/src/syscall.rs` | Add `TIOCSCTTY`, `SIGHUP`, `SIGCONT` constants |
| `drivers/src/tty_tests.rs` | Tests: hangup wakes readers, flush_all, open_count lifecycle, TIOCSCTTY |

---

## 13. Phase 9: Per-TTY Locking & Performance

> **Priority**: P1 — Must fix before multiple active TTYs or PTY support.
> **Rationale**: The single `TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>` lock protects **all** 8 TTY slots.  Any operation on TTY 0 blocks all operations on TTY 1–7.  `write()` holds the lock for the entire byte-by-byte serial output loop (~86μs/byte at 115200 baud).  A 1 KB write holds the global lock for ~86 ms.  Linux uses per-tty mutexes with a global lock only for lookup/registration.

**Goal**: Move to per-TTY locking so that operations on different TTYs are fully independent, and release the lock before slow driver I/O.

### 13.1 Per-TTY lock architecture

**Current**:
```rust
pub static TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]>;  // One lock for everything
```

**New**:
```rust
/// Thin registry lock — held only during lookup/allocation, never during I/O.
pub static TTY_REGISTRY: IrqMutex<[Option<TtySlot>; MAX_TTYS]>;

/// Per-TTY inner state, each with its own lock.
pub struct TtySlot {
    pub inner: IrqMutex<TtyInner>,
}

pub struct TtyInner {
    pub index: TtyIndex,
    pub ldisc: LineDisc,
    pub driver: TtyDriverKind,
    pub session: TtySession,
    pub winsize: UserWinsize,
    pub active: bool,
    pub open_count: u16,
    pub hung_up: bool,
}
```

- `TTY_REGISTRY` lock is held **only** to get a reference to a `TtySlot`
- `TtySlot.inner` lock is held for per-TTY operations
- Different TTYs never contend

### 13.2 Release lock before driver I/O in `write()`

**Problem**: `write()` currently holds `TTY_TABLE` lock while calling `driver.write_output()` for each byte.  Serial I/O is slow.

**Fix**: Process output through ldisc into a local buffer, drop the lock, then write to the driver:

```rust
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    let mut out_buf = [0u8; 256];
    let mut out_len = 0;

    // Phase 1: Process under lock (fast — pure computation).
    {
        let mut inner = get_tty_inner(idx)?;
        for &c in data {
            match inner.ldisc.process_output_byte(c) {
                OutputAction::Emit { buf, len } => {
                    for i in 0..len as usize {
                        if out_len < out_buf.len() {
                            out_buf[out_len] = buf[i];
                            out_len += 1;
                        }
                    }
                }
                OutputAction::Suppress => {}
            }
        }
    } // Lock dropped here.

    // Phase 2: Driver I/O without lock (slow — hardware).
    write_to_driver(idx, &out_buf[..out_len]);
    data.len()
}
```

### 13.3 Combine `drain_hw_input` + `read` into single lock acquisition

**Problem**: `read()` calls `drain_hw_input(idx)` which locks the table once, then separately locks it again for `ldisc.read()`.  This is 2–3 lock/unlock cycles per read attempt.

**Fix**: Merge drain + read into a single `with_tty_inner()` call.

### 13.4 Fix idle callback (not TTY 0 only)

**Problem**: `input_available_cb()` only checks TTY 0.  Future serial-on-TTY-1 or PTY polling breaks.

**Fix**: Iterate all active TTYs in the idle callback, or register per-TTY callbacks.

### 13.5 Lock ordering rules (MUST document)

Strict lock hierarchy to prevent deadlock:

1. **`TTY_REGISTRY`** (global) — held only for slot lookup, never during I/O
2. **`TtySlot.inner`** (per-TTY) — held for ldisc/session/termios operations
3. **`TTY_INPUT_WAITERS[idx]`** — never hold while holding any of the above

Rule: Never acquire `TTY_REGISTRY` while holding `TtySlot.inner`.

### 13.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty/table.rs` | Replace `TTY_TABLE` with `TTY_REGISTRY` + `TtySlot` per-TTY locks |
| `drivers/src/tty/mod.rs` | Update all functions to use per-TTY locking; split write into process + I/O; merge drain+read |
| `drivers/src/tty_tests.rs` | Add concurrency/multi-TTY tests; verify lock ordering |

---

## 14. Phase 10: Rust Idioms & Termios Completion

> **Priority**: P2 — Improves code quality and enables realistic userspace.
> **Rationale**: The current codebase uses C-style error patterns (`isize`, `-1`, raw pointers) internally where Rust `Result` types and slices would catch bugs at compile time.  Additionally, `VMIN/VTIME` is parsed but not enforced — terminal applications like readline and curses depend on this for responsive input.

**Goal**: Adopt Rust-idiomatic error handling internally, enforce VMIN/VTIME in non-canonical mode, and add `/dev/tty` support for POSIX compliance.

### 14.1 Internal `Result<usize, Errno>` error handling

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

### 14.2 Accept slices internally (not raw pointers)

**What**: Change internal functions from `read(idx, buffer: *mut u8, max: usize)` to `read(idx, buf: &mut [u8])`.  Keep raw pointers only at the syscall adapter boundary.

### 14.3 VMIN/VTIME enforcement

**What**: Implement non-canonical read timing:

- **VMIN > 0, VTIME == 0**: Block until VMIN bytes available (current behavior, but enforce count)
- **VMIN == 0, VTIME > 0**: Return immediately if data; else wait up to VTIME deciseconds
- **VMIN > 0, VTIME > 0**: Block until VMIN bytes OR VTIME inter-byte timeout
- **VMIN == 0, VTIME == 0**: Return immediately with available data (pure non-blocking)

Requires timer integration (scheduler or PIT-based timeout) for VTIME.

### 14.4 `/dev/tty` support

**What**: Allow a process to refer to "my controlling terminal" generically, needed for `isatty()`, `ttyname()`, and POSIX utilities.

- When opening `/dev/tty`, resolve to the process's `controlling_tty` from its task struct
- Return `-ENXIO` if no controlling terminal
- Wire into VFS/devfs if present, or handle as a special path in `open()`

### 14.5 Files modified

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

## 15. File Inventory

### New files

| File | Purpose |
|------|---------|
| `drivers/src/tty/mod.rs` | TTY public API (replaces `tty.rs`) |
| `drivers/src/tty/driver.rs` | `TtyDriver` trait, `SerialConsoleDriver`, `VConsoleDriver` |
| `drivers/src/tty/table.rs` | `TTY_TABLE` global, init, lookup |
| `drivers/src/tty/ldisc.rs` | Enhanced `LineDisc` (replaces `line_disc.rs`) |
| `drivers/src/tty/session.rs` | `TtySession`, foreground checks |
| `drivers/src/tty/wait_queue.rs` | Per-TTY `WaitQueue` |

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

### Deleted files

| File | Reason |
|------|--------|
| `drivers/src/tty.rs` | Replaced by `drivers/src/tty/mod.rs` |
| `drivers/src/line_disc.rs` | Replaced by `drivers/src/tty/ldisc.rs` |

---

## 16. Future: PTY Support

Not in scope for this plan, but the architecture is designed to accommodate it:

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
- TTY N (slave) — has `PtySlaveDriver`, owns a `LineDisc`
- TTY M (master) — has `PtyMasterDriver`, no line discipline

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
   - Non-canonical: VMIN/VTIME timing
   - Job control: SIGTTIN for background reads

The SlopOS implementation will follow this structure but simplified (no IUCLC, no TABDLY, no baud rate handling, no UTF-8 for now).
