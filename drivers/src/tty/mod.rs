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
//! The `TTY_TABLE` (in `table.rs`) holds up to `MAX_TTYS` terminal instances.
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

use slopos_abi::syscall::{UserTermios, UserWinsize};
use slopos_lib::kernel_services::driver_runtime::{
    current_task_id, register_idle_wakeup_callback, scheduler_is_enabled, signal_process_group,
};

use self::driver::TtyDriverKind;
use self::ldisc::{InputAction, LineDisc, OutputAction};
use self::session::TtySession;
use self::table::{TTY_INPUT_WAITERS, TTY_TABLE};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Index into the global TTY table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TtyIndex(pub u8);

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
    let wake = {
        let mut table = TTY_TABLE.lock();
        let slot = match table.get_mut(idx.0 as usize) {
            Some(s) => s,
            None => return,
        };
        let tty = match slot.as_mut() {
            Some(t) => t,
            None => return,
        };

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
                drop(table);
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

fn task_has_focus(idx: TtyIndex, task_id: u32) -> bool {
    let table = TTY_TABLE.lock();
    let tty = match table.get(idx.0 as usize) {
        Some(Some(tty)) => tty,
        _ => return false,
    };

    let focused = tty.session.focused_task_id;
    if focused != 0 && focused == task_id {
        return true;
    }

    let fg_pgrp = tty.session.fg_pgrp;
    fg_pgrp != 0 && fg_pgrp == task_id
}

fn ensure_focus(idx: TtyIndex, task_id: u32) {
    let mut table = TTY_TABLE.lock();
    if let Some(Some(tty)) = table.get_mut(idx.0 as usize) {
        if tty.session.focused_task_id == 0 {
            tty.session.focused_task_id = task_id;
        }
    }
}

/// Read cooked data from a specific TTY.
pub fn read(idx: TtyIndex, buffer: *mut u8, max: usize, nonblock: bool) -> isize {
    if buffer.is_null() || max == 0 {
        return 0;
    }
    if (idx.0 as usize) >= MAX_TTYS {
        return -1;
    }

    register_idle_callback();
    let task_id = current_task_id();
    let enforce_focus = task_id != 0;

    if enforce_focus {
        ensure_focus(idx, task_id);
    }

    loop {
        if enforce_focus && !task_has_focus(idx, task_id) {
            if nonblock {
                return -11;
            }
            let wait_ok =
                TTY_INPUT_WAITERS[idx.0 as usize].wait_event(|| task_has_focus(idx, task_id));
            if !wait_ok {
                return -11;
            }
            continue;
        }

        // Drain hardware input into the line discipline.
        drain_hw_input(idx);

        // Try to read from the cooked buffer.
        let out = unsafe { core::slice::from_raw_parts_mut(buffer, max) };
        let got = {
            let mut table = TTY_TABLE.lock();
            match table.get_mut(idx.0 as usize) {
                Some(Some(tty)) => tty.ldisc.read(out),
                _ => return -1,
            }
        };
        if got > 0 {
            return got as isize;
        }

        if nonblock {
            return -11; // EAGAIN
        }

        let wait_ok = TTY_INPUT_WAITERS[idx.0 as usize].wait_event(|| {
            if enforce_focus && !task_has_focus(idx, task_id) {
                return false;
            }
            drain_hw_input(idx);
            let table = TTY_TABLE.lock();
            match table.get(idx.0 as usize) {
                Some(Some(tty)) => tty.ldisc.has_data(),
                _ => true,
            }
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
pub fn write(idx: TtyIndex, data: &[u8]) -> usize {
    let mut table = TTY_TABLE.lock();
    let tty = match table.get_mut(idx.0 as usize) {
        Some(Some(t)) => t,
        _ => return 0,
    };
    for &c in data {
        match tty.ldisc.process_output_byte(c) {
            OutputAction::Emit { buf, len } => {
                tty.driver.write_output(&buf[..len as usize]);
            }
            OutputAction::Suppress => {}
        }
    }
    data.len()
}

/// Check if a TTY has cooked data available for reading.
pub fn has_data(idx: TtyIndex) -> bool {
    drain_hw_input(idx);
    let table = TTY_TABLE.lock();
    match table.get(idx.0 as usize) {
        Some(Some(tty)) => tty.ldisc.has_data(),
        _ => false,
    }
}

/// Get termios for a specific TTY.
pub fn get_termios(idx: TtyIndex, t: *mut UserTermios) {
    if t.is_null() {
        return;
    }
    let table = TTY_TABLE.lock();
    if let Some(Some(tty)) = table.get(idx.0 as usize) {
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
    let mut table = TTY_TABLE.lock();
    if let Some(Some(tty)) = table.get_mut(idx.0 as usize) {
        tty.ldisc.set_termios(&val);
        tty.driver.set_termios(&val);
    }
}

/// Get the foreground process group for a specific TTY.
pub fn get_foreground_pgrp(idx: TtyIndex) -> u32 {
    let table = TTY_TABLE.lock();
    match table.get(idx.0 as usize) {
        Some(Some(tty)) => tty.session.fg_pgrp,
        _ => 0,
    }
}

/// Set the foreground process group for a specific TTY.
pub fn set_foreground_pgrp(idx: TtyIndex, pgid: u32) {
    let mut table = TTY_TABLE.lock();
    if let Some(Some(tty)) = table.get_mut(idx.0 as usize) {
        tty.session.fg_pgrp = pgid;
    }
}

/// Get window size for a specific TTY.
pub fn get_winsize(idx: TtyIndex, ws: *mut UserWinsize) {
    if ws.is_null() {
        return;
    }
    let table = TTY_TABLE.lock();
    if let Some(Some(tty)) = table.get(idx.0 as usize) {
        unsafe { *ws = tty.winsize };
    }
}

/// Set window size for a specific TTY.
pub fn set_winsize(idx: TtyIndex, ws: *const UserWinsize) {
    if ws.is_null() {
        return;
    }
    let val = unsafe { *ws };
    let mut table = TTY_TABLE.lock();
    if let Some(Some(tty)) = table.get_mut(idx.0 as usize) {
        tty.winsize = val;
    }
}

/// Set the focused task on the active TTY.
///
/// Called by the compositor when window focus changes.  Sets both the
/// per-TTY `focused_task_id` and `fg_pgrp` so that `read()` allows the
/// newly focused task to receive input.
pub fn set_focus(task_id: u32) -> i32 {
    let idx = active_tty();
    {
        let mut table = TTY_TABLE.lock();
        if let Some(Some(tty)) = table.get_mut(idx.0 as usize) {
            tty.session.focused_task_id = task_id;
            tty.session.fg_pgrp = task_id;
        }
    }
    if scheduler_is_enabled() != 0 {
        let slot = idx.0 as usize;
        if slot < MAX_TTYS {
            TTY_INPUT_WAITERS[slot].wake_all();
        }
    }
    0
}

/// Get the focused task ID from the active TTY.
pub fn get_focus() -> u32 {
    let idx = active_tty();
    let table = TTY_TABLE.lock();
    match table.get(idx.0 as usize) {
        Some(Some(tty)) => tty.session.focused_task_id,
        _ => 0,
    }
}

/// Initialise the TTY subsystem.  Call during early boot after serial is ready.
pub fn init() {
    table::tty_table_init();
}

// ---------------------------------------------------------------------------
// Hardware input drain
// ---------------------------------------------------------------------------

/// Drain pending hardware input into the line discipline for a specific TTY.
///
/// Uses the driver's `drain_input` method to poll for hardware bytes (serial
/// UART for TTY 0, no-op for virtual console TTY 1+), then feeds each byte
/// through `process_raw_char_for`.
fn drain_hw_input(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    let count = {
        let table = TTY_TABLE.lock();
        match table.get(idx.0 as usize) {
            Some(Some(tty)) => tty.driver.drain_input(&mut scratch),
            _ => return,
        }
    };

    for i in 0..count {
        let mut c = scratch[i];
        // Serial terminals send CR for Enter and DEL (0x7F) for backspace.
        if c == b'\r' {
            c = b'\n';
        } else if c == 0x7F {
            c = 0x08;
        }
        process_raw_char_for(idx, c);
    }
}

/// Feed a raw character through the line discipline for a specific TTY.
///
/// This is the internal "process" path used by `drain_hw_input`.  For
/// interrupt-driven input, use `push_input` instead.
fn process_raw_char_for(idx: TtyIndex, c: u8) {
    let (action, has_data, fg_pgrp) = {
        let mut table = TTY_TABLE.lock();
        let tty = match table.get_mut(idx.0 as usize) {
            Some(Some(t)) => t,
            _ => return,
        };
        let action = tty.ldisc.input_char(c);
        let has_data = tty.ldisc.has_data();
        let fg_pgrp = tty.session.fg_pgrp;
        (action, has_data, fg_pgrp)
    };

    match action {
        InputAction::Echo { buf, len } => {
            let table = TTY_TABLE.lock();
            if let Some(Some(tty)) = table.get(idx.0 as usize) {
                for i in 0..len as usize {
                    tty.driver.write_output(&[buf[i]]);
                }
            }
        }
        InputAction::ReprintLine => {
            let table = TTY_TABLE.lock();
            if let Some(Some(tty)) = table.get(idx.0 as usize) {
                tty.driver.write_output(b"\n");
                let content = tty.ldisc.edit_content();
                for &b in content {
                    tty.driver.write_output(&[b]);
                }
            }
        }
        InputAction::Signal(sig) => {
            if fg_pgrp != 0 {
                let _ = signal_process_group(fg_pgrp, sig);
            }
        }
        InputAction::None => {}
    }

    if has_data {
        notify_input_ready(idx);
    }
}

// ---------------------------------------------------------------------------
// Idle callback
// ---------------------------------------------------------------------------

fn input_available_cb() -> c_int {
    // Check TTY 0 for data availability (used by scheduler idle loop).
    let idx = TtyIndex(0);
    drain_hw_input(idx);
    let has_data = {
        let table = TTY_TABLE.lock();
        match table.get(idx.0 as usize) {
            Some(Some(tty)) => tty.ldisc.has_data(),
            _ => false,
        }
    };
    if has_data {
        notify_input_ready(idx);
    }
    has_data as c_int
}

fn register_idle_callback() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    register_idle_wakeup_callback(Some(input_available_cb));
}
