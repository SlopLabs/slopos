# SlopOS TTY Overhaul Plan

> **Status**: Planned
> **Target**: Replace the global singleton TTY with a proper per-terminal TTY subsystem comparable to Linux N_TTY / RedoxOS
> **Current**: `drivers/src/tty.rs` (373 lines), `drivers/src/line_disc.rs` (183 lines) — single global `LINE_DISC`, ad-hoc focus system
> **Bugs Addressed**: Double-typing on PS/2 keyboard, nc immediate termination, dual input delivery

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
11. [File Inventory](#11-file-inventory)
12. [Future: PTY Support](#12-future-pty-support)

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
| 1 | TTY core structs | `drivers/src/tty.rs` | `drivers/src/tty/mod.rs`, `tty/driver.rs`, `tty/table.rs` |
| 2 | Line discipline | `drivers/src/line_disc.rs` | `drivers/src/tty/ldisc.rs` |
| 3 | Input pipeline | `drivers/src/ps2/keyboard.rs`, `drivers/src/input_event.rs` | — |
| 4 | Sessions/pgrps | `core/src/scheduler/task.rs` | `drivers/src/tty/session.rs` |
| 5 | FD integration | `fs/src/fileio.rs`, `core/src/syscall/fs/poll_ioctl_handlers.rs` | — |
| 6 | Verification | — | — |

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

## 5. Phase 1: TTY Core Abstraction

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

### 5.6 Files modified

| File | Change |
|------|--------|
| `drivers/src/tty.rs` | Replaced by `drivers/src/tty/mod.rs` |
| `drivers/src/line_disc.rs` | Moved to `drivers/src/tty/ldisc.rs` |
| `drivers/src/lib.rs` | Update `mod tty` declaration |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update imports |
| `fs/src/fileio.rs` | Update `use crate::...tty` paths |

---

## 6. Phase 2: Enhanced Line Discipline

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

## 7. Phase 3: Input Pipeline Cleanup

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

## 8. Phase 4: Session & Process Group Management

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

## 9. Phase 5: FD Integration

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

### 9.6 Files modified

| File | Change |
|------|--------|
| `fs/src/fileio.rs` | Replace `console: bool` with `tty_index: Option<TtyIndex>`, update read/write/poll |
| `core/src/syscall/fs/poll_ioctl_handlers.rs` | Route ioctl to per-TTY, add TIOCGWINSZ/TIOCSWINSZ |
| `lib/src/kernel_services/syscall_services/tty.rs` | Update to call `tty::*` with TTY index |

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

---

## 11. File Inventory

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

## 12. Future: PTY Support

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
