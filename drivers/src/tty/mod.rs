//! SlopOS TTY subsystem — per-terminal TTY abstraction.
//!
//! This module replaces the old global singleton TTY with a proper per-terminal
//! architecture modeled after Linux's `tty_struct` + `n_tty` line discipline.
//!
//! # Architecture
//!
//! Each `Tty` instance owns:
//! - A `LineDisc` (line discipline) for input processing
//! - A `TtyDriverKind` (hardware backend — serial or virtual console)
//! - A `TtySession` (session/foreground pgrp + focused task)
//! - A `WaitQueue` for tasks blocked on input
//!
//! The `TTY_SLOTS` array (in `table.rs`) holds up to `MAX_TTYS` terminal
//! instances, each with its own `IrqMutex` for fully independent per-TTY
//! locking (Phase 8).
//!
//! # Public API
//!
//! All public functions take an explicit `TtyIndex` — there are no global
//! shims.  The `TtyServices` function pointers (registered in
//! `syscall_services_init.rs`) perform the `u8 → TtyIndex` conversion at the
//! boundary.

pub mod driver;
pub mod ldisc;
pub mod session;
pub mod table;

use core::ffi::c_int;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{SIGCONT, SIGHUP};
use slopos_abi::syscall::{SIGTTIN, UserTermios, UserWinsize};
use slopos_lib::kernel_services::driver_runtime::{
    current_task_id, current_task_pgid, current_task_sid, register_idle_wakeup_callback,
    scheduler_is_enabled, signal_process_group,
};

use self::driver::{TtyDriverKind, write_driver_unlocked};
use self::ldisc::{InputAction, LineDisc, OutputAction};
use self::session::{ForegroundCheck, TtySession};
use self::table::{TTY_INPUT_WAITERS, TTY_SLOTS};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Re-export `TtyIndex` from the ABI crate so that it is the single
/// definition used across the entire kernel.
pub use slopos_abi::syscall::TtyIndex;

/// Maximum number of TTY instances.
pub const MAX_TTYS: usize = 8;

/// The central TTY structure — one per terminal.
pub struct Tty {
    /// Which TTY slot this is (0 = serial console, 1 = virtual console, etc.).
    pub index: TtyIndex,

    /// The line discipline owned by this TTY.
    pub ldisc: LineDisc,

    /// Hardware driver backend.
    pub driver: TtyDriverKind,

    /// Session/foreground state (includes focused_task_id).
    pub session: TtySession,

    /// Window size (for TIOCGWINSZ / TIOCSWINSZ).
    pub winsize: UserWinsize,

    /// Whether this TTY is active/allocated.
    pub active: bool,

    pub open_count: u32,

    pub hung_up: bool,
}

// ---------------------------------------------------------------------------
// Tty helper methods
// ---------------------------------------------------------------------------

impl Tty {
    /// Drain pending hardware input into the line discipline.
    ///
    /// Called while holding the per-TTY lock.  Feeds bytes from the hardware
    /// driver through `ldisc.input_char()`, echoing output via the driver.
    ///
    /// Returns a deferred signal `(pgid, signum)` if signal generation was
    /// triggered (e.g. Ctrl+C on serial).  The caller **must** deliver the
    /// signal **after** dropping the per-TTY lock to avoid deadlock.
    fn drain_hw_input(&mut self) -> Option<(u32, u8)> {
        let mut scratch = [0u8; 64];
        let count = self.driver.drain_input(&mut scratch);
        let mut deferred_signal = None;

        for i in 0..count {
            let mut c = scratch[i];
            // Serial terminals send CR for Enter and DEL (0x7F) for backspace.
            if c == b'\r' {
                c = b'\n';
            } else if c == 0x7F {
                c = 0x08;
            }

            let action = self.ldisc.input_char(c);
            match action {
                InputAction::Echo { buf, len } => {
                    for j in 0..len as usize {
                        self.driver.write_output(&[buf[j]]);
                    }
                }
                InputAction::Signal(sig) => {
                    deferred_signal = Some((self.session.fg_pgrp, sig));
                }
                InputAction::ReprintLine => {
                    self.driver.write_output(b"\n");
                    let content = self.ldisc.edit_content();
                    for &b in content {
                        self.driver.write_output(&[b]);
                    }
                }
                InputAction::None => {}
            }
        }

        deferred_signal
    }
}

// ---------------------------------------------------------------------------
// Active TTY tracking (for keyboard input routing)
// ---------------------------------------------------------------------------

/// The currently active TTY index (receives keyboard input).
/// Defaults to 0 (serial console).
static ACTIVE_TTY: AtomicU8 = AtomicU8::new(0);

/// Returns the TTY index that should receive keyboard input.
pub fn active_tty() -> TtyIndex {
    TtyIndex(ACTIVE_TTY.load(Ordering::Relaxed))
}

/// Set the active TTY (the one receiving keyboard input).
pub fn set_active_tty(idx: TtyIndex) {
    ACTIVE_TTY.store(idx.0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Per-TTY public API
// ---------------------------------------------------------------------------

/// Push a raw input byte to a specific TTY.
///
/// Called from interrupt context (keyboard ISR) or from `drain_hw_input`.
/// Feeds the byte through the line discipline and handles echo/signal actions.
pub fn push_input(idx: TtyIndex, c: u8) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }

    let wake = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };

        if tty.hung_up {
            return;
        }

        let action = tty.ldisc.input_char(c);
        let has_data = tty.ldisc.has_data();

        // Handle echo, reprint, and signal actions while we hold the lock.
        match action {
            InputAction::Echo { buf, len } => {
                for i in 0..len as usize {
                    tty.driver.write_output(&[buf[i]]);
                }
                has_data
            }
            InputAction::ReprintLine => {
                // Redisplay: newline + current edit buffer contents.
                tty.driver.write_output(b"\n");
                let content = tty.ldisc.edit_content();
                for &b in content {
                    tty.driver.write_output(&[b]);
                }
                has_data
            }
            InputAction::Signal(sig) => {
                let pgid = tty.session.fg_pgrp;
                // Release lock before signalling to avoid deadlock.
                drop(guard);
                if pgid != 0 {
                    let _ = signal_process_group(pgid, sig);
                }
                return; // Signal path — wakeup not needed here.
            }
            InputAction::None => has_data,
        }
    };

    if wake {
        notify_input_ready(idx);
    }
}

/// Wake one task blocked on input for a specific TTY.
fn notify_input_ready(idx: TtyIndex) {
    if scheduler_is_enabled() == 0 {
        return;
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    TTY_INPUT_WAITERS[slot].wake_one();
}

/// Lazily attach a session and set focus for a task that is reading from
/// a TTY for the first time.  If no session is attached yet, the calling
/// task becomes the session leader with its own pgid as foreground group.
fn auto_attach_session(idx: TtyIndex, task_id: u32, caller_pgid: u32, caller_sid: u32) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        // Set compositor focus if not already set.
        if tty.session.focused_task_id == 0 {
            tty.session.focused_task_id = task_id;
        }
        // Auto-attach session if none exists (first reader becomes leader).
        if !tty.session.has_session() && caller_sid != 0 {
            tty.session.attach(caller_sid, caller_pgid);
        }
    }
}

/// Read cooked data from a specific TTY.
///
/// Uses `TtySession::check_read()` as the sole read-side gate.  Background
/// processes receive `SIGTTIN` instead of silently blocking.
///
/// Phase 8: drain + foreground check + read are merged into a single per-TTY
/// lock acquisition per loop iteration (previously 5–6 separate locks).
pub fn read(idx: TtyIndex, buffer: *mut u8, max: usize, nonblock: bool) -> isize {
    if buffer.is_null() || max == 0 {
        return 0;
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return -1;
    }

    register_idle_callback();
    let task_id = current_task_id();
    let caller_pgid = current_task_pgid();
    let caller_sid = current_task_sid();
    let enforce_access = task_id != 0;

    if enforce_access {
        auto_attach_session(idx, task_id, caller_pgid, caller_sid);
    }

    loop {
        // --- Single lock acquisition: state check + drain + foreground + read ---
        let deferred_signal;
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return -1,
            };

            // Hung-up check (before drain).
            if tty.hung_up && !tty.ldisc.has_data() {
                return if nonblock { -5 } else { 0 };
            }

            // Foreground check via check_read().
            if enforce_access {
                match tty.session.check_read(caller_pgid, caller_sid) {
                    ForegroundCheck::BackgroundRead => {
                        drop(guard);
                        if caller_pgid != 0 {
                            let _ = signal_process_group(caller_pgid, SIGTTIN);
                        }
                        return -1;
                    }
                    ForegroundCheck::Allowed | ForegroundCheck::NoSession => {}
                    ForegroundCheck::BackgroundWrite => {
                        // Should not happen on read path, treat as allowed.
                    }
                }
            }

            // Drain hardware input (merged — single lock for drain + read).
            deferred_signal = tty.drain_hw_input();

            // Try to read from the cooked buffer.
            let out = unsafe { core::slice::from_raw_parts_mut(buffer, max) };
            let got = tty.ldisc.read(out);

            if got > 0 {
                // Drop guard before delivering deferred signal.
                drop(guard);
                if let Some((pgid, sig)) = deferred_signal {
                    if pgid != 0 {
                        let _ = signal_process_group(pgid, sig);
                    }
                }
                return got as isize;
            }

            // Check hung-up after drain (data may have been flushed by hangup).
            if tty.hung_up {
                return if nonblock { -5 } else { 0 };
            }
        }
        // --- Per-TTY lock dropped ---

        // Deliver deferred signal from drain (e.g. Ctrl+C on serial).
        if let Some((pgid, sig)) = deferred_signal {
            if pgid != 0 {
                let _ = signal_process_group(pgid, sig);
            }
        }

        if nonblock {
            return -11; // EAGAIN
        }

        // Block on per-TTY wait queue.
        let wait_ok = TTY_INPUT_WAITERS[slot].wait_event(|| {
            let (sig, result) = {
                let mut guard = TTY_SLOTS[slot].lock();
                match guard.as_mut() {
                    Some(tty) => {
                        if enforce_access {
                            if matches!(
                                tty.session.check_read(caller_pgid, caller_sid),
                                ForegroundCheck::BackgroundRead
                            ) {
                                return false;
                            }
                        }
                        let sig = tty.drain_hw_input();
                        let result = tty.hung_up || tty.ldisc.has_data();
                        (sig, result)
                    }
                    None => return true,
                }
            };
            // Deliver deferred signal outside lock.
            if let Some((pgid, signum)) = sig {
                if pgid != 0 {
                    let _ = signal_process_group(pgid, signum);
                }
            }
            result
        });
        if !wait_ok {
            return -11;
        }
    }
}

/// Write bytes to a specific TTY.
///
/// Applies output processing (`c_oflag`) — e.g. OPOST + ONLCR converts
/// `\n` to `\r\n` before sending to the driver.
///
/// Phase 8: split-write pattern — output is processed through the line
/// discipline under the per-TTY lock into a local stack buffer, the lock is
/// dropped, and the buffered bytes are written to the hardware without
/// holding any TTY lock.  This prevents slow serial I/O from blocking
/// operations on other TTYs.
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }

    // Maximum output bytes per chunk.  Each input byte can expand to at most
    // 2 output bytes (e.g. NL → CR+NL with ONLCR).  256 bytes leaves room
    // for expansion while keeping the stack buffer small.
    const OUT_BUF_CAP: usize = 256;

    let mut pos = 0;
    while pos < data.len() {
        let mut out_buf = [0u8; OUT_BUF_CAP];
        let mut out_len = 0;
        let driver_id;

        // Phase 1: Process output under per-TTY lock (fast — pure computation).
        {
            let mut guard = TTY_SLOTS[slot].lock();
            let tty = match guard.as_mut() {
                Some(t) => t,
                None => return 0,
            };
            driver_id = tty.driver.id();

            while pos < data.len() {
                match tty.ldisc.process_output_byte(data[pos]) {
                    OutputAction::Emit { buf, len } => {
                        for i in 0..len as usize {
                            if out_len < OUT_BUF_CAP {
                                out_buf[out_len] = buf[i];
                                out_len += 1;
                            }
                        }
                    }
                    OutputAction::Suppress => {}
                }
                pos += 1;
                // If buffer nearly full, break to flush.
                if out_len >= OUT_BUF_CAP - 2 {
                    break;
                }
            }
        }
        // Per-TTY lock dropped.

        // Phase 2: Driver I/O without any TTY lock (slow — hardware).
        write_driver_unlocked(driver_id, &out_buf[..out_len]);
    }

    data.len()
}

/// Check if a TTY has cooked data available for reading.
pub fn has_data(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        let _ = tty.drain_hw_input();
        tty.ldisc.has_data()
    } else {
        false
    }
}

/// Get termios for a specific TTY.
pub fn get_termios(idx: TtyIndex, t: *mut UserTermios) {
    if t.is_null() {
        return;
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_ref() {
        let val = *tty.ldisc.termios();
        unsafe { *t = val };
    }
}

/// Set termios for a specific TTY.
pub fn set_termios(idx: TtyIndex, t: *const UserTermios) {
    if t.is_null() {
        return;
    }
    let val = unsafe { *t };
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.ldisc.set_termios(&val);
        tty.driver.set_termios(&val);
    }
}

/// Get the foreground process group for a specific TTY.
pub fn get_foreground_pgrp(idx: TtyIndex) -> u32 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.session.fg_pgrp,
        None => 0,
    }
}

/// Set the foreground process group for a specific TTY.
pub fn set_foreground_pgrp(idx: TtyIndex, pgid: u32) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.fg_pgrp = pgid;
    }
}

/// Set foreground pgrp with session validation (POSIX TIOCSPGRP semantics).
///
/// Only processes in the same session as the TTY's controlling session may
/// change the foreground pgrp.  Returns 0 on success, -1 on permission error.
pub fn set_foreground_pgrp_checked(idx: TtyIndex, pgid: u32, caller_sid: u32) -> i32 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return -1;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        if tty.session.set_fg_pgrp_checked(pgid, caller_sid) {
            return 0;
        }
    }
    -1
}

/// Get window size for a specific TTY.
pub fn get_winsize(idx: TtyIndex, ws: *mut UserWinsize) {
    if ws.is_null() {
        return;
    }
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_ref() {
        unsafe { *ws = tty.winsize };
    }
}

/// Set window size for a specific TTY.
pub fn set_winsize(idx: TtyIndex, ws: *const UserWinsize) {
    if ws.is_null() {
        return;
    }
    let val = unsafe { *ws };
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.winsize = val;
    }
}

/// Set the compositor-level focus on the active TTY.
///
/// Called by the compositor when window focus changes.  Sets ONLY the
/// `focused_task_id` — it does NOT alter the POSIX foreground process
/// group (`fg_pgrp`).  The two concepts are independent:
///
/// - `focused_task_id` — which task the compositor considers "active"
/// - `fg_pgrp` — which process group may read/write the terminal (POSIX)
///
/// Compositor focus is used for input routing; foreground pgrp is used
/// for job control signals and read/write access gating.
pub fn set_compositor_focus(task_id: u32) -> i32 {
    let idx = active_tty();
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return -1;
    }
    {
        let mut guard = TTY_SLOTS[slot].lock();
        if let Some(tty) = guard.as_mut() {
            tty.session.focused_task_id = task_id;
        }
    }
    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
    0
}

/// Get the compositor-focused task ID from the active TTY.
pub fn get_compositor_focus() -> u32 {
    let idx = active_tty();
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.session.focused_task_id,
        None => 0,
    }
}

/// Initialise the TTY subsystem.  Call during early boot after serial is ready.
pub fn init() {
    table::tty_table_init();
}

// ---------------------------------------------------------------------------
// Session management API
// ---------------------------------------------------------------------------

/// Get the session ID for a specific TTY.
pub fn get_session_id(idx: TtyIndex) -> u32 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return 0;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.session.session_id,
        None => 0,
    }
}

/// Attach a session to a TTY.
///
/// The session leader (`leader_pid`) becomes the controlling process.
/// `leader_pgid` is set as the initial foreground process group.
pub fn attach_session(idx: TtyIndex, leader_pid: u32, leader_pgid: u32) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.attach(leader_pid, leader_pgid);
    }
}

/// Detach the controlling session from a TTY.
///
/// Clears session leader, session ID, and foreground pgrp.
/// Compositor focus (`focused_task_id`) is NOT cleared.
pub fn detach_session(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.session.detach();
    }
}

/// Detach any TTY whose session matches `session_id`.
///
/// Called from `setsid()` when the session leader creates a new session —
/// the old controlling terminal must be released.
///
/// Each per-TTY lock is acquired and released individually — no two locks
/// are held simultaneously.
pub fn detach_session_by_id(session_id: u32) {
    if session_id == 0 {
        return;
    }
    for i in 0..MAX_TTYS {
        let mut guard = TTY_SLOTS[i].lock();
        if let Some(tty) = guard.as_mut() {
            if tty.session.session_id == session_id {
                tty.session.detach();
            }
        }
    }
}

pub fn open_ref(idx: TtyIndex) -> i32 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return -1;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        tty.open_count = tty
            .open_count
            .checked_add(1)
            .unwrap_or_else(|| panic!("tty open_count overflow for idx {}", idx.0));
        tty.hung_up = false;
        return tty.open_count as i32;
    }
    -1
}

pub fn close_ref(idx: TtyIndex) -> i32 {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return -1;
    }
    let mut guard = TTY_SLOTS[slot].lock();
    if let Some(tty) = guard.as_mut() {
        if tty.open_count == 0 {
            return 0;
        }
        tty.open_count -= 1;
        if tty.open_count == 0 {
            tty.ldisc.flush_all();
            tty.session.detach();
            tty.hung_up = false;
        }
        return tty.open_count as i32;
    }
    -1
}

pub fn hangup(idx: TtyIndex) {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return;
    }
    let fg_pgrp = {
        let mut guard = TTY_SLOTS[slot].lock();
        let tty = match guard.as_mut() {
            Some(t) => t,
            None => return,
        };
        let fg = tty.session.fg_pgrp;
        tty.ldisc.flush_all();
        tty.session.detach();
        tty.hung_up = true;
        fg
    };

    if fg_pgrp != 0 {
        let _ = signal_process_group(fg_pgrp, SIGHUP);
        let _ = signal_process_group(fg_pgrp, SIGCONT);
    }

    if scheduler_is_enabled() != 0 {
        TTY_INPUT_WAITERS[slot].wake_all();
    }
}

pub fn is_hung_up(idx: TtyIndex) -> bool {
    let slot = idx.0 as usize;
    if slot >= MAX_TTYS {
        return false;
    }
    let guard = TTY_SLOTS[slot].lock();
    match guard.as_ref() {
        Some(tty) => tty.hung_up,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Idle callback (Phase 8: iterates ALL active TTYs)
// ---------------------------------------------------------------------------

/// Idle-loop callback: drain hardware input and wake blocked readers.
///
/// Phase 8: now iterates all active TTYs instead of only TTY 0.  Each
/// per-TTY lock is acquired and released individually.
fn input_available_cb() -> c_int {
    let mut any_data = false;
    for i in 0..MAX_TTYS {
        let has_data = {
            let mut guard = TTY_SLOTS[i].lock();
            if let Some(tty) = guard.as_mut() {
                if tty.active {
                    let _ = tty.drain_hw_input();
                    tty.ldisc.has_data()
                } else {
                    false
                }
            } else {
                false
            }
        };
        if has_data {
            notify_input_ready(TtyIndex(i as u8));
            any_data = true;
        }
    }
    any_data as c_int
}

fn register_idle_callback() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    register_idle_wakeup_callback(Some(input_available_cb));
}
