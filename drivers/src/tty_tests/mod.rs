//! Regression tests for the TTY subsystem: `LineDisc`, `TtyDriverKind`,
//! `TtyIndex`, the TTY table, and the per-TTY public API.

use slopos_abi::syscall::{CcIndex, InputFlags, LocalFlags};
use slopos_ostd::klog_info;
use slopos_ostd::ring_buffer::RingBuffer;
use slopos_testing::TestResult;

use crate::tty;
use crate::tty::TtyError;
use crate::tty::TtyIndex;
use crate::tty::driver::{TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LineDisc};
use crate::tty::table::TTY_SLOTS;
use crate::tty::vconsole::{CursorAttributes, VCONSOLE_MAX_COLS, VCONSOLE_MAX_ROWS, VConsoleState};
use slopos_vt::{SgrAttr, VtAction, VtParser};

pub(crate) fn boxed_vconsole_state() -> slopos_ostd::KBox<VConsoleState> {
    let mut state = slopos_ostd::KBox::try_init(VConsoleState::init_default()).expect("test alloc");
    state.rows = 25;
    state.cols = 80;
    state.cursor_attrs = CursorAttributes {
        fg: 0x00AAAAAA,
        bg: 0x00000000,
        bold: false,
        underline: false,
        inverse: false,
    };
    state.saved_cursor_attrs = state.cursor_attrs;
    state.cells.allocate(VCONSOLE_MAX_ROWS, VCONSOLE_MAX_COLS);
    state
        .alt_cells
        .allocate(VCONSOLE_MAX_ROWS, VCONSOLE_MAX_COLS);
    state
}

/// A canonical discipline hands back only complete lines, so an unterminated
/// tail survives every read and reappears the moment something clears
/// `ICANON` — hence the flush after the reads.
pub(crate) fn drain_tty_nonblock(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    loop {
        match tty::read(idx, &mut scratch, true) {
            Ok(0) | Err(_) => break,
            Ok(_) => continue,
        }
    }
    let _ = tty::tcflush(idx, slopos_abi::syscall::TCIFLUSH);
}

pub mod fixtures;
pub mod test_driver;
pub mod test_echo_defer;
pub mod test_integration;
pub mod test_ioctls_ext;
pub mod test_ldisc_core;
pub mod test_ldisc_flags;
pub mod test_ldisc_flow;
pub mod test_ldisc_noncanon;
pub mod test_ldisc_regression;
pub mod test_ldisc_signals;
pub mod test_ldisc_utf8;
pub mod test_pty;
pub mod test_pty_core;
pub mod test_pty_packet;
pub mod test_ringbuf;
pub mod test_session;
pub mod test_session_fg;
pub mod test_table;
pub mod test_vconsole;
pub mod test_vtparser;
