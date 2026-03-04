//! Global TTY table — the central registry of all terminal instances.
//!
//! `TTY_TABLE` is a fixed-size array of `Option<Tty>` protected by an
//! `IrqMutex`.  At boot, `tty_table_init()` allocates TTY 0 (serial console)
//! and optionally TTY 1 (virtual console).
//!
//! `TTY_INPUT_WAITERS` is a **separate** static array of `WaitQueue`s — one
//! per TTY slot.  They live outside `TTY_TABLE` so that `read()` can call
//! `wait_event(|| ...)` without holding the table lock (the condition closure
//! locks the table internally to check for data).

use slopos_lib::IrqMutex;
use slopos_lib::WaitQueue;

use super::driver::{SerialConsoleDriver, TtyDriverKind, VConsoleDriver};
use super::ldisc::LineDisc;
use super::session::TtySession;
use super::{MAX_TTYS, Tty, TtyIndex};
use slopos_abi::syscall::UserWinsize;
// ---------------------------------------------------------------------------
// Global table
// ---------------------------------------------------------------------------

/// The global TTY table.  Each slot holds an `Option<Tty>`.
///
/// Slots 0 and 1 are pre-allocated at init time:
/// - 0 → serial console (COM1)
/// - 1 → virtual console (PS/2 keyboard + framebuffer)
///
/// The remaining slots are reserved for future PTY support.
pub static TTY_TABLE: IrqMutex<[Option<Tty>; MAX_TTYS]> = IrqMutex::new([NONE_TTY; MAX_TTYS]);

const NONE_TTY: Option<Tty> = None;

/// Per-TTY input wait queues — separate from TTY_TABLE to avoid lock ordering
/// issues (read() needs to block on the wait queue while the condition closure
/// independently locks TTY_TABLE to check for data).
pub static TTY_INPUT_WAITERS: [WaitQueue; MAX_TTYS] = [const { WaitQueue::new() }; MAX_TTYS];

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialise the TTY table.  Must be called once during early boot, after
/// the serial port is ready.
///
/// Allocates:
/// - TTY 0  → SerialConsoleDriver (COM1)
/// - TTY 1  → VConsoleDriver (PS/2 + framebuffer, stub)
pub fn tty_table_init() {
    let mut table = TTY_TABLE.lock();
    table[0] = Some(Tty::new(
        TtyIndex(0),
        TtyDriverKind::SerialConsole(SerialConsoleDriver),
    ));
    table[1] = Some(Tty::new(
        TtyIndex(1),
        TtyDriverKind::VConsole(VConsoleDriver),
    ));
}

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Execute a closure with a mutable reference to the `Tty` at `idx`, if it
/// exists.  Returns `None` if the slot is empty.
///
/// The TTY table lock is held for the duration of the closure.
pub fn with_tty<F, R>(idx: TtyIndex, f: F) -> Option<R>
where
    F: FnOnce(&mut Tty) -> R,
{
    let mut table = TTY_TABLE.lock();
    let slot = table.get_mut(idx.0 as usize)?;
    slot.as_mut().map(f)
}

/// Execute a closure with an immutable reference to the `Tty` at `idx`.
pub fn with_tty_ref<F, R>(idx: TtyIndex, f: F) -> Option<R>
where
    F: FnOnce(&Tty) -> R,
{
    let table = TTY_TABLE.lock();
    let slot = table.get(idx.0 as usize)?;
    slot.as_ref().map(f)
}

impl Tty {
    /// Create a new TTY with the given index and driver backend.
    pub fn new(index: TtyIndex, driver: TtyDriverKind) -> Self {
        Self {
            index,
            ldisc: LineDisc::new(),
            driver,
            session: TtySession::new(),
            winsize: UserWinsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
            active: true,
            open_count: 0,
            hung_up: false,
        }
    }
}
