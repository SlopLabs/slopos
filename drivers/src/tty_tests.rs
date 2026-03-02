//! Regression tests for the TTY subsystem.
//!
//! Tests the `LineDisc`, `TtyDriverKind`, `TtyIndex`, TTY table, and
//! the per-TTY public API (focus, foreground pgrp, active TTY).

use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::tty;
use crate::tty::TtyIndex;
use crate::tty::driver::{TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LineDisc};
use crate::tty::session::TtySession;
use crate::tty::table::TTY_TABLE;

fn drain_tty_nonblock(idx: TtyIndex) {
    let mut scratch = [0u8; 64];
    loop {
        let n = tty::read(idx, scratch.as_mut_ptr(), scratch.len(), true);
        if n <= 0 {
            break;
        }
    }
}

// ===========================================================================
// LineDisc tests
// ===========================================================================

/// A fresh LineDisc has no data.
pub fn test_ldisc_new_has_no_data() -> TestResult {
    let ld = LineDisc::new();
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - new LineDisc reports has_data()=true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Reading from an empty LineDisc returns 0 bytes.
pub fn test_ldisc_read_empty() -> TestResult {
    let mut ld = LineDisc::new();
    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - read from empty LineDisc returned {}", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode: characters accumulate in edit buffer, flush on newline.
pub fn test_ldisc_canonical_newline() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abc" — should not produce cooked data yet.
    for &c in b"abc" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical mode has data before newline");
        return TestResult::Fail;
    }

    // Press Enter — should flush "abc\n" to cooked.
    ld.input_char(b'\n');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical mode has no data after newline");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 {
        klog_info!("TTY_TEST: BUG - expected 4 bytes, got {}", n);
        return TestResult::Fail;
    }
    if &buf[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - cooked data mismatch");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// Canonical mode: VERASE (backspace) removes the last character.
pub fn test_ldisc_canonical_backspace() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abcd", then backspace, then newline.
    for &c in b"abcd" {
        ld.input_char(c);
    }
    ld.input_char(0x08); // BS
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 4 {
        klog_info!("TTY_TEST: BUG - expected 4 bytes (abc\\n), got {}", n);
        return TestResult::Fail;
    }
    if &buf[..4] != b"abc\n" {
        klog_info!("TTY_TEST: BUG - expected \"abc\\n\", got {:?}", &buf[..n]);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode: VKILL clears the entire edit buffer.
pub fn test_ldisc_canonical_kill() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"hello" {
        ld.input_char(c);
    }
    // Kill line (default VKILL = 0x15 = Ctrl+U).
    ld.input_char(0x15);
    // Type "world" and newline.
    for &c in b"world" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 6 {
        klog_info!("TTY_TEST: BUG - expected 6 bytes (world\\n), got {}", n);
        return TestResult::Fail;
    }
    if &buf[..6] != b"world\n" {
        klog_info!("TTY_TEST: BUG - data mismatch after kill");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Canonical mode: VEOF (Ctrl+D) flushes without adding a newline.
pub fn test_ldisc_canonical_eof() -> TestResult {
    let mut ld = LineDisc::new();

    for &c in b"abc" {
        ld.input_char(c);
    }
    // EOF = 0x04
    ld.input_char(0x04);

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 3 {
        klog_info!("TTY_TEST: BUG - expected 3 bytes after EOF, got {}", n);
        return TestResult::Fail;
    }
    if &buf[..3] != b"abc" {
        klog_info!("TTY_TEST: BUG - data mismatch after EOF");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ISIG: Ctrl+C (VINTR) generates a signal action.
pub fn test_ldisc_signal_ctrl_c() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(0x03); // Ctrl+C
    match action {
        InputAction::Signal(2) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!("TTY_TEST: BUG - expected signal 2 (SIGINT), got {}", s);
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+C did not produce Signal action");
            TestResult::Fail
        }
    }
}

/// Non-canonical mode: characters go directly to cooked buffer.
pub fn test_ldisc_raw_mode() -> TestResult {
    let mut ld = LineDisc::new();

    // Switch to raw mode.
    let mut termios = *ld.termios();
    termios.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&termios);

    // Each character should be immediately available.
    ld.input_char(b'a');
    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - raw mode char not immediately available");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 1];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'a' {
        klog_info!("TTY_TEST: BUG - raw mode read mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_termios: switching from canonical to raw flushes the edit buffer.
pub fn test_ldisc_set_termios_flush() -> TestResult {
    let mut ld = LineDisc::new();

    // Type some chars in canonical mode (not yet flushed).
    for &c in b"partial" {
        ld.input_char(c);
    }
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - canonical should not have data before newline");
        return TestResult::Fail;
    }

    // Switch to raw mode — edit buffer should flush.
    let mut termios = *ld.termios();
    termios.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&termios);

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - set_termios to raw did not flush edit buffer");
        return TestResult::Fail;
    }

    let mut buf = [0u8; 64];
    let n = ld.read(&mut buf);
    if n != 7 || &buf[..7] != b"partial" {
        klog_info!("TTY_TEST: BUG - flushed data mismatch (got {} bytes)", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ECHO mode: printable characters return Echo action.
pub fn test_ldisc_echo_printable() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(b'x');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'x' {
                klog_info!("TTY_TEST: BUG - echo mismatch for 'x'");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo action for printable char");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// ECHO mode: newline returns Echo action with '\n'.
pub fn test_ldisc_echo_newline() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(b'\n');
    match action {
        InputAction::Echo { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - echo mismatch for newline");
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - expected Echo action for newline");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

// ===========================================================================
// TtySession tests
// ===========================================================================

/// New TtySession has zero values.
pub fn test_session_new_empty() -> TestResult {
    let s = TtySession::new();
    if s.session_leader != 0 || s.session_id != 0 || s.fg_pgrp != 0 || s.focused_task_id != 0 {
        klog_info!("TTY_TEST: BUG - new TtySession has non-zero fields");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// TtyIndex tests
// ===========================================================================

/// TtyIndex equality.
pub fn test_tty_index_eq() -> TestResult {
    let a = TtyIndex(0);
    let b = TtyIndex(0);
    let c = TtyIndex(1);
    if a != b {
        klog_info!("TTY_TEST: BUG - TtyIndex(0) != TtyIndex(0)");
        return TestResult::Fail;
    }
    if a == c {
        klog_info!("TTY_TEST: BUG - TtyIndex(0) == TtyIndex(1)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// TtyDriverKind tests
// ===========================================================================

/// TtyDriverKind::None does not panic on write/drain.
pub fn test_driver_none_no_panic() -> TestResult {
    let driver = TtyDriverKind::None;
    driver.write_output(b"test");
    let mut buf = [0u8; 16];
    let n = driver.drain_input(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - None driver returned {} from drain", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// VConsoleDriver drain_input returns 0 (input is interrupt-driven).
pub fn test_vconsole_drain_returns_zero() -> TestResult {
    let driver = TtyDriverKind::VConsole(VConsoleDriver);
    let mut buf = [0u8; 16];
    let n = driver.drain_input(&mut buf);
    if n != 0 {
        klog_info!("TTY_TEST: BUG - VConsole drain returned {}", n);
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// TTY Table tests
// ===========================================================================

/// After tty_table_init, TTY 0 and TTY 1 are allocated.
pub fn test_table_init_allocates_tty0_and_tty1() -> TestResult {
    // Ensure init has been called (it's idempotent — re-calling overwrites).
    tty::table::tty_table_init();

    let table = TTY_TABLE.lock();
    if table[0].is_none() {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated after init");
        return TestResult::Fail;
    }
    if table[1].is_none() {
        klog_info!("TTY_TEST: BUG - TTY 1 not allocated after init");
        return TestResult::Fail;
    }
    // Slots 2..MAX_TTYS should be None.
    for i in 2..tty::MAX_TTYS {
        if table[i].is_some() {
            klog_info!("TTY_TEST: BUG - TTY {} unexpectedly allocated", i);
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// TTY 0 has the correct index.
pub fn test_table_tty0_has_index_zero() -> TestResult {
    tty::table::tty_table_init();

    let table = TTY_TABLE.lock();
    if let Some(tty) = &table[0] {
        if tty.index != TtyIndex(0) {
            klog_info!("TTY_TEST: BUG - TTY 0 has wrong index {:?}", tty.index);
            return TestResult::Fail;
        }
    } else {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TTY 0 is active by default.
pub fn test_table_tty0_active() -> TestResult {
    tty::table::tty_table_init();

    let table = TTY_TABLE.lock();
    if let Some(tty) = &table[0] {
        if !tty.active {
            klog_info!("TTY_TEST: BUG - TTY 0 is not active");
            return TestResult::Fail;
        }
    } else {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// with_tty helper works for existing TTY.
pub fn test_table_with_tty_exists() -> TestResult {
    tty::table::tty_table_init();

    let result = tty::table::with_tty(TtyIndex(0), |tty| tty.index);
    match result {
        Some(idx) => {
            if idx != TtyIndex(0) {
                klog_info!("TTY_TEST: BUG - with_tty returned wrong index");
                return TestResult::Fail;
            }
        }
        None => {
            klog_info!("TTY_TEST: BUG - with_tty returned None for TTY 0");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// with_tty helper returns None for empty slot.
pub fn test_table_with_tty_empty() -> TestResult {
    tty::table::tty_table_init();

    let result = tty::table::with_tty(TtyIndex(5), |_tty| ());
    if result.is_some() {
        klog_info!("TTY_TEST: BUG - with_tty returned Some for empty slot 5");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Per-TTY API tests (replaced compat shim tests)
// ===========================================================================
/// active_tty defaults to 0.
pub fn test_active_tty_default() -> TestResult {
    let idx = tty::active_tty();
    if idx != TtyIndex(0) {
        klog_info!(
            "TTY_TEST: BUG - active_tty default is {:?}, expected 0",
            idx
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_active_tty + active_tty round-trip.
pub fn test_set_active_tty() -> TestResult {
    tty::set_active_tty(TtyIndex(1));
    let idx = tty::active_tty();
    // Reset to default.
    tty::set_active_tty(TtyIndex(0));

    if idx != TtyIndex(1) {
        klog_info!("TTY_TEST: BUG - set_active_tty(1) did not stick");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_foreground_pgrp / get_foreground_pgrp round-trip via per-TTY API.
pub fn test_foreground_pgrp() -> TestResult {
    tty::table::tty_table_init();
    tty::set_foreground_pgrp(TtyIndex(0), 42);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0));
    tty::set_foreground_pgrp(TtyIndex(0), 0); // Reset.

    if pgid != 42 {
        klog_info!(
            "TTY_TEST: BUG - foreground pgrp round-trip failed (got {})",
            pgid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_focus / get_focus round-trip via per-TTY API.
pub fn test_focus() -> TestResult {
    tty::table::tty_table_init();
    tty::set_focus(99);
    let focus = tty::get_focus();
    tty::set_focus(0); // Reset.

    if focus != 99 {
        klog_info!("TTY_TEST: BUG - focus round-trip failed (got {})", focus);
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_keyboard_enter_scancode_reaches_active_tty() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    crate::ps2::keyboard::handle_scancode(0x1C);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), out.as_mut_ptr(), out.len(), true);
    if n != 1 || out[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - enter scancode did not reach active tty (n={}, b0={})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_keyboard_scancode_routes_to_active_tty_index() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(1));
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    crate::ps2::keyboard::handle_scancode(0x1C);

    let mut out0 = [0u8; 8];
    let n0 = tty::read(TtyIndex(0), out0.as_mut_ptr(), out0.len(), true);
    let mut out1 = [0u8; 8];
    let n1 = tty::read(TtyIndex(1), out1.as_mut_ptr(), out1.len(), true);

    tty::set_active_tty(TtyIndex(0));

    if n0 != -11 || n1 != 1 || out1[0] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - active tty routing mismatch (n0={}, n1={}, b1={})",
            n0,
            n1,
            out1[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

pub fn test_keyboard_extended_up_arrow_reaches_tty() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw as *const _);

    crate::ps2::keyboard::handle_scancode(0xE0);
    crate::ps2::keyboard::handle_scancode(0x48);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), out.as_mut_ptr(), out.len(), true);
    tty::set_termios(TtyIndex(0), &saved as *const _);
    if n != 1 || out[0] != 0x82 {
        klog_info!(
            "TTY_TEST: BUG - extended up arrow not delivered (n={}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }

    TestResult::Pass
}

// ===========================================================================
// Cooked ring buffer boundary tests
// ===========================================================================

/// Multiple reads drain the cooked buffer correctly.
pub fn test_ldisc_multiple_reads() -> TestResult {
    let mut ld = LineDisc::new();

    // Type "abcdef\n" — 7 bytes in cooked.
    for &c in b"abcdef" {
        ld.input_char(c);
    }
    ld.input_char(b'\n');

    // Read 3 bytes.
    let mut buf1 = [0u8; 3];
    let n1 = ld.read(&mut buf1);
    if n1 != 3 || &buf1 != b"abc" {
        klog_info!("TTY_TEST: BUG - first read mismatch");
        return TestResult::Fail;
    }

    // Read remaining 4 bytes.
    let mut buf2 = [0u8; 10];
    let n2 = ld.read(&mut buf2);
    if n2 != 4 || &buf2[..4] != b"def\n" {
        klog_info!("TTY_TEST: BUG - second read mismatch (got {} bytes)", n2);
        return TestResult::Fail;
    }

    // Buffer should now be empty.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - buffer not empty after full drain");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Backspace on empty edit buffer is a no-op.
pub fn test_ldisc_backspace_empty() -> TestResult {
    let mut ld = LineDisc::new();

    let action = ld.input_char(0x08); // BS on empty buffer.
    match action {
        InputAction::None => TestResult::Pass,
        _ => {
            klog_info!("TTY_TEST: BUG - backspace on empty produced non-None action");
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Test suite registration
// ===========================================================================

slopos_lib::define_test_suite!(
    tty,
    [
        test_ldisc_new_has_no_data,
        test_ldisc_read_empty,
        test_ldisc_canonical_newline,
        test_ldisc_canonical_backspace,
        test_ldisc_canonical_kill,
        test_ldisc_canonical_eof,
        test_ldisc_signal_ctrl_c,
        test_ldisc_raw_mode,
        test_ldisc_set_termios_flush,
        test_ldisc_echo_printable,
        test_ldisc_echo_newline,
        test_ldisc_multiple_reads,
        test_ldisc_backspace_empty,
        test_session_new_empty,
        test_tty_index_eq,
        test_driver_none_no_panic,
        test_vconsole_drain_returns_zero,
        test_table_init_allocates_tty0_and_tty1,
        test_table_tty0_has_index_zero,
        test_table_tty0_active,
        test_table_with_tty_exists,
        test_table_with_tty_empty,
        test_active_tty_default,
        test_set_active_tty,
        test_foreground_pgrp,
        test_focus,
        test_keyboard_enter_scancode_reaches_active_tty,
        test_keyboard_scancode_routes_to_active_tty_index,
        test_keyboard_extended_up_arrow_reaches_tty,
    ]
);
