//! Regression tests for the TTY subsystem.
//!
//! Tests the `LineDisc`, `TtyDriverKind`, `TtyIndex`, TTY table, and
//! the per-TTY public API (compositor focus, foreground pgrp, active TTY).
//!
//! Phase 2 additions: input flag processing, output processing, signal
//! generation, flow control, VLNEXT, VWERASE, ECHOCTL.
//!
//! Phase 6 additions: compositor focus / fg_pgrp split, check_read() as sole
//! read gate, TtyIndex type safety, signal constant verification.

use slopos_abi::syscall::{SIGINT, SIGQUIT, SIGTSTP};
use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::tty;
use crate::tty::TtyIndex;
use crate::tty::driver::{TtyDriverKind, VConsoleDriver};
use crate::tty::ldisc::{InputAction, LineDisc, OutputAction};
use crate::tty::session::TtySession;
use crate::tty::session::{ForegroundCheck, NO_FOREGROUND_PGRP, NO_SESSION};
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
        InputAction::Signal(SIGINT) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!("TTY_TEST: BUG - expected SIGINT({}), got {}", SIGINT, s);
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
    if s.session_leader != NO_SESSION
        || s.session_id != NO_SESSION
        || s.fg_pgrp != NO_FOREGROUND_PGRP
        || s.focused_task_id != 0
    {
        klog_info!("TTY_TEST: BUG - new TtySession has non-zero fields");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Attaching a session sets leader, session_id, and fg_pgrp.
pub fn test_session_attach() -> TestResult {
    let mut s = TtySession::new();
    s.attach(100, 100);
    if s.session_leader != 100 || s.session_id != 100 || s.fg_pgrp != 100 {
        klog_info!("TTY_TEST: BUG - session attach did not set fields correctly");
        return TestResult::Fail;
    }
    if !s.has_session() {
        klog_info!("TTY_TEST: BUG - has_session() false after attach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Detaching a session resets leader, session_id, and fg_pgrp.
pub fn test_session_detach() -> TestResult {
    let mut s = TtySession::new();
    s.attach(200, 200);
    s.detach();
    if s.session_leader != NO_SESSION
        || s.session_id != NO_SESSION
        || s.fg_pgrp != NO_FOREGROUND_PGRP
    {
        klog_info!("TTY_TEST: BUG - session detach did not reset fields");
        return TestResult::Fail;
    }
    if s.has_session() {
        klog_info!("TTY_TEST: BUG - has_session() true after detach");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Foreground reader gets Allowed.
pub fn test_session_check_read_foreground() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(10, 10) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - foreground read expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Background reader gets BackgroundRead.
pub fn test_session_check_read_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - background read expected BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// No session attached — check_read returns NoSession (permissive).
pub fn test_session_check_read_no_session() -> TestResult {
    let s = TtySession::new();
    match s.check_read(42, 42) {
        ForegroundCheck::NoSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no-session read expected NoSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Kernel task (pgid=0) gets Allowed even if not in foreground group.
pub fn test_session_check_read_kernel_task() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(0, 0) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - kernel task read expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_write without TOSTOP always returns Allowed.
pub fn test_session_check_write_no_tostop() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    // Background process, but TOSTOP is false.
    match s.check_write(99, false) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - write without TOSTOP expected Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// check_write with TOSTOP and background caller returns BackgroundWrite.
pub fn test_session_check_write_tostop_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_write(99, true) {
        ForegroundCheck::BackgroundWrite => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - TOSTOP background write expected BackgroundWrite, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 6: check_read replaces task_has_access — foreground task allowed.
pub fn test_session_check_read_replaces_task_has_access_foreground() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    match s.check_read(10, 10) {
        ForegroundCheck::Allowed => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - fg pgrp member should be Allowed, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 6: check_read replaces task_has_access — background task gets BackgroundRead.
pub fn test_session_check_read_replaces_task_has_access_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    s.focused_task_id = 0; // No compositor focus.
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - background task should be BackgroundRead, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// Phase 6: check_read replaces task_has_access — permissive when no session.
pub fn test_session_check_read_replaces_task_has_access_permissive() -> TestResult {
    let s = TtySession::new();
    match s.check_read(999, 0) {
        ForegroundCheck::NoSession => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - no session should be NoSession, got {:?}",
                other
            );
            TestResult::Fail
        }
    }
}

/// set_fg_pgrp_checked: allowed when caller is in the same session.
pub fn test_session_set_fg_pgrp_checked_allowed() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    if !s.set_fg_pgrp_checked(20, 10) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should allow same-session caller");
        return TestResult::Fail;
    }
    if s.fg_pgrp != 20 {
        klog_info!("TTY_TEST: BUG - fg_pgrp not updated to 20");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_fg_pgrp_checked: denied when caller is in a different session.
pub fn test_session_set_fg_pgrp_checked_denied() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10);
    if s.set_fg_pgrp_checked(20, 99) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should deny different-session caller");
        return TestResult::Fail;
    }
    if s.fg_pgrp != 10 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should remain 10 after denied set");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// set_fg_pgrp_checked: allowed when no session is attached (permissive).
pub fn test_session_set_fg_pgrp_checked_no_session() -> TestResult {
    let mut s = TtySession::new();
    if !s.set_fg_pgrp_checked(50, 99) {
        klog_info!("TTY_TEST: BUG - set_fg_pgrp_checked should allow when no session");
        return TestResult::Fail;
    }
    if s.fg_pgrp != 50 {
        klog_info!("TTY_TEST: BUG - fg_pgrp not updated to 50");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: get_session_id returns 0 when no session is attached.
pub fn test_tty_get_session_id_default() -> TestResult {
    tty::table::tty_table_init();
    let sid = tty::get_session_id(TtyIndex(0));
    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - default session_id should be 0, got {}",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: attach_session + get_session_id round-trip.
pub fn test_tty_attach_session() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 300, 300);
    let sid = tty::get_session_id(TtyIndex(0));
    // Clean up.
    tty::detach_session(TtyIndex(0));
    if sid != 300 {
        klog_info!(
            "TTY_TEST: BUG - attach_session/get_session_id round-trip failed (got {})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: detach_session resets session_id to 0.
pub fn test_tty_detach_session() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 400, 400);
    tty::detach_session(TtyIndex(0));
    let sid = tty::get_session_id(TtyIndex(0));
    if sid != 0 {
        klog_info!(
            "TTY_TEST: BUG - detach_session did not reset session_id (got {})",
            sid
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: detach_session_by_id only detaches matching session.
pub fn test_tty_detach_session_by_id() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 500, 500);
    // Detach with wrong ID — should be a no-op.
    tty::detach_session_by_id(999);
    let sid_after_wrong = tty::get_session_id(TtyIndex(0));
    // Detach with correct ID.
    tty::detach_session_by_id(500);
    let sid_after_correct = tty::get_session_id(TtyIndex(0));
    if sid_after_wrong != 500 {
        klog_info!(
            "TTY_TEST: BUG - detach_session_by_id with wrong ID should be no-op (got {})",
            sid_after_wrong
        );
        return TestResult::Fail;
    }
    if sid_after_correct != 0 {
        klog_info!(
            "TTY_TEST: BUG - detach_session_by_id with correct ID should reset (got {})",
            sid_after_correct
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Per-TTY API: set_foreground_pgrp_checked with session validation.
pub fn test_tty_set_fg_pgrp_checked() -> TestResult {
    tty::table::tty_table_init();
    tty::attach_session(TtyIndex(0), 600, 600);
    // Same session — should succeed.
    let ok = tty::set_foreground_pgrp_checked(TtyIndex(0), 700, 600);
    let pgid = tty::get_foreground_pgrp(TtyIndex(0));
    // Different session — should fail.
    let denied = tty::set_foreground_pgrp_checked(TtyIndex(0), 800, 999);
    let pgid_after = tty::get_foreground_pgrp(TtyIndex(0));
    // Clean up.
    tty::detach_session(TtyIndex(0));
    tty::set_foreground_pgrp(TtyIndex(0), 0);
    if ok != 0 {
        klog_info!(
            "TTY_TEST: BUG - set_fg_pgrp_checked same-session returned {}",
            ok
        );
        return TestResult::Fail;
    }
    if pgid != 700 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should be 700 after checked set (got {})",
            pgid
        );
        return TestResult::Fail;
    }
    if denied != -1 {
        klog_info!(
            "TTY_TEST: BUG - set_fg_pgrp_checked different-session should return -1 (got {})",
            denied
        );
        return TestResult::Fail;
    }
    if pgid_after != 700 {
        klog_info!(
            "TTY_TEST: BUG - fg_pgrp should remain 700 after denied set (got {})",
            pgid_after
        );
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

/// Phase 6: set_compositor_focus / get_compositor_focus round-trip.
///
/// Verifies that compositor focus only sets `focused_task_id`, NOT `fg_pgrp`.
pub fn test_compositor_focus() -> TestResult {
    tty::table::tty_table_init();
    tty::set_compositor_focus(99);
    let focus = tty::get_compositor_focus();
    tty::set_compositor_focus(0); // Reset.

    if focus != 99 {
        klog_info!(
            "TTY_TEST: BUG - compositor focus round-trip failed (got {})",
            focus
        );
        return TestResult::Fail;
    }

    // Verify that fg_pgrp was NOT modified by set_compositor_focus.
    tty::table::tty_table_init();
    let fg_before = tty::get_foreground_pgrp(TtyIndex(0));
    tty::set_compositor_focus(42);
    let fg_after = tty::get_foreground_pgrp(TtyIndex(0));
    tty::set_compositor_focus(0); // Reset.

    if fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - set_compositor_focus changed fg_pgrp ({} -> {})",
            fg_before,
            fg_after
        );
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
// Phase 2: Input flag processing tests
// ===========================================================================

/// ICRNL: CR (0x0D) is mapped to NL (0x0A) when ICRNL is set.
pub fn test_ldisc_icrnl() -> TestResult {
    let mut ld = LineDisc::new();
    // Enable ICRNL in c_iflag.
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::ICRNL;
    ld.set_termios(&t);

    // Feed CR — should be treated as NL and flush edit buffer.
    ld.input_char(b'a');
    ld.input_char(b'b');
    ld.input_char(0x0D); // CR

    if !ld.has_data() {
        klog_info!("TTY_TEST: BUG - ICRNL did not flush on CR");
        return TestResult::Fail;
    }
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    // Should get "ab\n" (3 bytes) — CR was converted to NL.
    if n != 3 || buf[2] != b'\n' {
        klog_info!(
            "TTY_TEST: BUG - ICRNL mismatch (n={}, b2=0x{:02x})",
            n,
            buf[2]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// IGNCR: CR is discarded entirely when IGNCR is set.
pub fn test_ldisc_igncr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::IGNCR;
    ld.set_termios(&t);

    // Feed CR — should be silently discarded.
    for &c in b"abc" {
        ld.input_char(c);
    }
    ld.input_char(0x0D); // CR — should be ignored

    // No newline was delivered, so canonical mode should NOT have flushed.
    if ld.has_data() {
        klog_info!("TTY_TEST: BUG - IGNCR did not discard CR");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// INLCR: NL (0x0A) is mapped to CR (0x0D) when INLCR is set.
pub fn test_ldisc_inlcr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::INLCR;
    // Disable ICANON so we can inspect raw bytes.
    t.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&t);

    ld.input_char(b'\n'); // NL — should become CR
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != b'\r' {
        klog_info!(
            "TTY_TEST: BUG - INLCR did not map NL to CR (got 0x{:02x})",
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// ISTRIP: bit 7 is stripped from input bytes.
pub fn test_ldisc_istrip() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::ISTRIP;
    t.c_lflag &= !slopos_abi::syscall::ICANON;
    ld.set_termios(&t);

    ld.input_char(0xC1); // 0xC1 with bit 7 set -> 0x41 = 'A'
    let mut buf = [0u8; 4];
    let n = ld.read(&mut buf);
    if n != 1 || buf[0] != 0x41 {
        klog_info!(
            "TTY_TEST: BUG - ISTRIP did not strip bit 7 (got 0x{:02x})",
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: Output processing tests
// ===========================================================================

/// OPOST+ONLCR: NL is converted to CR+NL on output.
pub fn test_ldisc_opost_onlcr() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 2 || buf[0] != b'\r' || buf[1] != b'\n' {
                klog_info!("TTY_TEST: BUG - ONLCR expected CR+NL, got len={}", len);
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - ONLCR suppressed NL");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// OPOST+OCRNL: CR is converted to NL on output.
pub fn test_ldisc_opost_ocrnl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::OCRNL;
    ld.set_termios(&t);

    match ld.process_output_byte(b'\r') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - OCRNL expected NL, got 0x{:02x}", buf[0]);
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - OCRNL suppressed CR");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// No OPOST: bytes pass through unmodified.
pub fn test_ldisc_output_raw() -> TestResult {
    let ld = LineDisc::new();
    // c_oflag defaults to 0 (no OPOST).
    match ld.process_output_byte(b'\n') {
        OutputAction::Emit { buf, len } => {
            if len != 1 || buf[0] != b'\n' {
                klog_info!("TTY_TEST: BUG - raw output modified NL");
                return TestResult::Fail;
            }
        }
        OutputAction::Suppress => {
            klog_info!("TTY_TEST: BUG - raw output suppressed NL");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: Signal generation tests
// ===========================================================================

/// SIGQUIT: Ctrl+\ generates SIGQUIT (signal 3).
pub fn test_ldisc_signal_ctrl_backslash() -> TestResult {
    let mut ld = LineDisc::new();
    let action = ld.input_char(0x1C); // Ctrl+\ = VQUIT default
    match action {
        InputAction::Signal(SIGQUIT) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!(
                "TTY_TEST: BUG - expected SIGQUIT({}), got signal {}",
                SIGQUIT,
                s
            );
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+\\ did not produce Signal action");
            TestResult::Fail
        }
    }
}

/// SIGTSTP: Ctrl+Z generates SIGTSTP (signal 20).
pub fn test_ldisc_signal_ctrl_z() -> TestResult {
    let mut ld = LineDisc::new();
    // VSUSP default = 0x1A = Ctrl+Z.
    let action = ld.input_char(0x1A);
    match action {
        InputAction::Signal(SIGTSTP) => TestResult::Pass,
        InputAction::Signal(s) => {
            klog_info!(
                "TTY_TEST: BUG - expected SIGTSTP({}), got signal {}",
                SIGTSTP,
                s
            );
            TestResult::Fail
        }
        _ => {
            klog_info!("TTY_TEST: BUG - Ctrl+Z did not produce Signal action");
            TestResult::Fail
        }
    }
}

// ===========================================================================
// Phase 2: Flow control tests
// ===========================================================================

/// IXON: Ctrl+S stops output, Ctrl+Q resumes.
pub fn test_ldisc_flow_control_ixon() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_iflag |= slopos_abi::syscall::IXON;
    ld.set_termios(&t);

    // Ctrl+S (VSTOP = 0x13) should stop output.
    ld.input_char(0x13);
    if !ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - IXON Ctrl+S did not stop output");
        return TestResult::Fail;
    }

    // Ctrl+Q (VSTART = 0x11) should resume.
    ld.input_char(0x11);
    if ld.is_stopped() {
        klog_info!("TTY_TEST: BUG - IXON Ctrl+Q did not resume output");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: ECHOCTL tests
// ===========================================================================

/// ECHOCTL: control characters are echoed as ^X.
pub fn test_ldisc_echoctl() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::ECHOCTL;
    // Disable ISIG so Ctrl+C is not caught as signal.
    t.c_lflag &= !slopos_abi::syscall::ISIG;
    ld.set_termios(&t);

    // Feed Ctrl+C (0x03) — should echo ^C (2 bytes).
    let action = ld.input_char(0x03);
    match action {
        InputAction::Echo { buf, len } => {
            if len != 2 || buf[0] != b'^' || buf[1] != b'C' {
                klog_info!(
                    "TTY_TEST: BUG - ECHOCTL expected ^C, got [{}, {}] len={}",
                    buf[0] as char,
                    buf[1] as char,
                    len
                );
                return TestResult::Fail;
            }
        }
        _ => {
            klog_info!("TTY_TEST: BUG - ECHOCTL did not produce Echo for Ctrl+C");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: VLNEXT (literal next) tests
// ===========================================================================

/// VLNEXT: Ctrl+V makes the next character literal.
pub fn test_ldisc_vlnext() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Press Ctrl+V (VLNEXT = 0x16).
    ld.input_char(0x16);

    // Now press Ctrl+C (0x03) — should be inserted literally, not generate signal.
    let action = ld.input_char(0x03);
    match action {
        InputAction::Signal(_) => {
            klog_info!("TTY_TEST: BUG - VLNEXT did not prevent signal");
            return TestResult::Fail;
        }
        _ => {} // Any non-signal action is correct.
    }

    // Flush and read — should contain 0x03 as a literal byte.
    ld.input_char(b'\n');
    let mut buf = [0u8; 16];
    let n = ld.read(&mut buf);
    // Expect: 0x03 + '\n' = 2 bytes.
    if n < 2 || buf[0] != 0x03 {
        klog_info!(
            "TTY_TEST: BUG - VLNEXT literal byte missing (n={}, b0=0x{:02x})",
            n,
            buf[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: VWERASE (word erase) tests
// ===========================================================================

/// VWERASE: Ctrl+W erases the previous word.
pub fn test_ldisc_vwerase() -> TestResult {
    let mut ld = LineDisc::new();
    let mut t = *ld.termios();
    t.c_lflag |= slopos_abi::syscall::IEXTEN;
    ld.set_termios(&t);

    // Type "hello world".
    for &c in b"hello world" {
        ld.input_char(c);
    }

    // Ctrl+W (VWERASE = 0x17) should erase "world".
    ld.input_char(0x17);

    // Now press Enter — should get "hello \n" (the trailing space stays
    // because word erase only removes the word, not trailing spaces before it).
    ld.input_char(b'\n');
    let mut buf = [0u8; 32];
    let n = ld.read(&mut buf);
    // "hello " + NL = 7 bytes.
    if n != 7 || &buf[..6] != b"hello " {
        klog_info!(
            "TTY_TEST: BUG - VWERASE mismatch (n={}, data={:?})",
            n,
            &buf[..n]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: edit_content() for ReprintLine
// ===========================================================================

/// edit_content returns current edit buffer contents.
pub fn test_ldisc_edit_content() -> TestResult {
    let mut ld = LineDisc::new();
    for &c in b"hello" {
        ld.input_char(c);
    }
    let content = ld.edit_content();
    if content != b"hello" {
        klog_info!("TTY_TEST: BUG - edit_content mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 2: Output processing via TTY write
// ===========================================================================

/// TTY write with OPOST+ONLCR: verify data.len() is returned (bytes consumed).
pub fn test_tty_write_returns_input_len() -> TestResult {
    tty::table::tty_table_init();
    // Enable OPOST+ONLCR on TTY 0.
    let mut t = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut t as *mut _);
    let saved = t;
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t as *const _);

    let data = b"hello\n";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved as *const _);
    if n != data.len() {
        klog_info!(
            "TTY_TEST: BUG - write returned {} instead of {}",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 3: Input pipeline cleanup tests
// ===========================================================================

/// Phase 3: Keyboard events no longer routed to the input_event compositor queue.
/// After pressing a key, the compositor event queue should remain empty.
pub fn test_keyboard_no_input_event_delivery() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Set keyboard focus in the compositor to a dummy task.
    let dummy_task: u32 = 9999;
    crate::input_event::input_set_keyboard_focus(dummy_task);

    // Press 'a' (scancode 0x1E).
    crate::ps2::keyboard::handle_scancode(0x1E);

    // The compositor queue for the dummy task should be empty.
    let has_events = crate::input_event::input_has_events(dummy_task);

    // Clean up keyboard focus.
    crate::input_event::input_set_keyboard_focus(0);
    drain_tty_nonblock(TtyIndex(0));

    if has_events {
        klog_info!("TTY_TEST: BUG - keyboard event leaked into input_event queue");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Break codes (key release) do not produce TTY input.
pub fn test_keyboard_break_code_no_input() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode so any delivered byte is immediately readable.
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw as *const _);

    // Send break code for 'a' (0x1E | 0x80 = 0x9E).
    crate::ps2::keyboard::handle_scancode(0x9E);

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), out.as_mut_ptr(), out.len(), true);
    tty::set_termios(TtyIndex(0), &saved as *const _);

    if n > 0 {
        klog_info!(
            "TTY_TEST: BUG - break code produced input (n={}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Modifier key presses (shift, ctrl, alt, caps lock) do not produce
/// TTY input.
pub fn test_keyboard_modifier_no_input() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode.
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw as *const _);

    // Press Left Shift (make code 0x2A), Left Ctrl (0x1D), Left Alt (0x38).
    crate::ps2::keyboard::handle_scancode(0x2A); // shift press
    crate::ps2::keyboard::handle_scancode(0x1D); // ctrl press
    crate::ps2::keyboard::handle_scancode(0x38); // alt press

    // Release them.
    crate::ps2::keyboard::handle_scancode(0xAA); // shift release
    crate::ps2::keyboard::handle_scancode(0x9D); // ctrl release
    crate::ps2::keyboard::handle_scancode(0xB8); // alt release

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), out.as_mut_ptr(), out.len(), true);
    tty::set_termios(TtyIndex(0), &saved as *const _);

    if n > 0 {
        klog_info!(
            "TTY_TEST: BUG - modifier key produced input (n={}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Press + release produces exactly one character (no duplication).
pub fn test_keyboard_press_release_single_char() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode.
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw as *const _);

    // Press 'a' (0x1E) then release 'a' (0x9E).
    crate::ps2::keyboard::handle_scancode(0x1E); // press
    crate::ps2::keyboard::handle_scancode(0x9E); // release

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), out.as_mut_ptr(), out.len(), true);
    tty::set_termios(TtyIndex(0), &saved as *const _);

    if n != 1 || out[0] != b'a' {
        klog_info!(
            "TTY_TEST: BUG - press+release should yield 1 char 'a' (n={}, b0=0x{:02x})",
            n,
            out[0]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: VConsole driver drain_input returns 0 via drain_hw_input (interrupt-driven).
pub fn test_vconsole_drain_via_drain_hw_input() -> TestResult {
    tty::table::tty_table_init();

    // TTY 1 is VConsole — drain_hw_input should be a no-op (input is
    // interrupt-driven via push_input), so no data should appear.
    drain_tty_nonblock(TtyIndex(1));

    // Switch to raw mode on TTY 1.
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(1), &mut saved as *mut _);
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(1), &raw as *const _);

    // has_data should be false — no hardware polling for VConsole.
    let has = tty::has_data(TtyIndex(1));
    tty::set_termios(TtyIndex(1), &saved as *const _);

    if has {
        klog_info!("TTY_TEST: BUG - VConsole drain_hw_input produced phantom data");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 3: Multiple key presses produce correct sequence in active TTY.
pub fn test_keyboard_multi_key_sequence() -> TestResult {
    tty::table::tty_table_init();
    tty::set_active_tty(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(0));

    // Switch to raw mode.
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    let mut raw = saved;
    raw.c_lflag &= !slopos_abi::syscall::ICANON;
    tty::set_termios(TtyIndex(0), &raw as *const _);

    // Press 'h' (0x23), 'i' (0x17).
    crate::ps2::keyboard::handle_scancode(0x23); // 'h'
    crate::ps2::keyboard::handle_scancode(0x17); // 'i'

    let mut out = [0u8; 8];
    let n = tty::read(TtyIndex(0), out.as_mut_ptr(), out.len(), true);
    tty::set_termios(TtyIndex(0), &saved as *const _);

    if n != 2 || out[0] != b'h' || out[1] != b'i' {
        klog_info!(
            "TTY_TEST: BUG - multi-key sequence mismatch (n={}, b0=0x{:02x}, b1=0x{:02x})",
            n,
            out[0],
            out[1]
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 5: FD integration tests
// ===========================================================================

/// Phase 5: tty::write routes bytes through output processing.
/// With OPOST+ONLCR enabled, writing "\n" should produce 2 bytes on the wire
/// (CR+LF), but write() must return the *input* byte count.
pub fn test_tty_write_output_processing() -> TestResult {
    tty::table::tty_table_init();
    // Save current termios.
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    // Enable OPOST + ONLCR.
    let mut t = saved;
    t.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t as *const _);

    let data = b"hello\nworld\n";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved as *const _);

    // write() returns input length regardless of output expansion.
    if n != data.len() {
        klog_info!(
            "TTY_TEST: BUG - write with OPOST+ONLCR returned {} instead of {}",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: tty::write with output processing disabled passes bytes through.
pub fn test_tty_write_raw_passthrough() -> TestResult {
    tty::table::tty_table_init();
    // Ensure c_oflag is 0 (no output processing — default).
    let mut saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut saved as *mut _);
    let mut t = saved;
    t.c_oflag = 0;
    tty::set_termios(TtyIndex(0), &t as *const _);

    let data = b"raw\ndata";
    let n = tty::write(TtyIndex(0), data);
    tty::set_termios(TtyIndex(0), &saved as *const _);

    if n != data.len() {
        klog_info!(
            "TTY_TEST: BUG - raw write returned {} instead of {}",
            n,
            data.len()
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: tty::write to invalid TTY index returns 0.
pub fn test_tty_write_invalid_index() -> TestResult {
    tty::table::tty_table_init();
    let data = b"nothing";
    let n = tty::write(TtyIndex(7), data); // Slot 7 is not allocated.
    if n != 0 {
        klog_info!(
            "TTY_TEST: BUG - write to invalid TTY returned {} instead of 0",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY termios isolation — changing TTY 0's termios does not
/// affect TTY 1.
pub fn test_tty_per_tty_termios_isolation() -> TestResult {
    tty::table::tty_table_init();

    // Save TTY 0 and TTY 1 termios.
    let mut t0_saved = slopos_abi::syscall::UserTermios::default();
    let mut t1_saved = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(0), &mut t0_saved as *mut _);
    tty::get_termios(TtyIndex(1), &mut t1_saved as *mut _);

    // Set OPOST on TTY 0 only.
    let mut t0_new = t0_saved;
    t0_new.c_oflag = slopos_abi::syscall::OPOST | slopos_abi::syscall::ONLCR;
    tty::set_termios(TtyIndex(0), &t0_new as *const _);

    // Read back TTY 1 — it should still have its original c_oflag.
    let mut t1_check = slopos_abi::syscall::UserTermios::default();
    tty::get_termios(TtyIndex(1), &mut t1_check as *mut _);

    // Restore TTY 0.
    tty::set_termios(TtyIndex(0), &t0_saved as *const _);

    if t1_check.c_oflag != t1_saved.c_oflag {
        klog_info!(
            "TTY_TEST: BUG - TTY 1 c_oflag changed when TTY 0 was modified ({} vs {})",
            t1_check.c_oflag,
            t1_saved.c_oflag
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY winsize isolation — setting winsize on TTY 0 does not
/// affect TTY 1.
pub fn test_tty_per_tty_winsize_isolation() -> TestResult {
    tty::table::tty_table_init();

    let mut ws0_saved = slopos_abi::syscall::UserWinsize::default();
    let mut ws1_saved = slopos_abi::syscall::UserWinsize::default();
    tty::get_winsize(TtyIndex(0), &mut ws0_saved as *mut _);
    tty::get_winsize(TtyIndex(1), &mut ws1_saved as *mut _);

    // Set a distinct winsize on TTY 0.
    let custom = slopos_abi::syscall::UserWinsize {
        ws_row: 42,
        ws_col: 120,
        ws_xpixel: 1920,
        ws_ypixel: 1080,
    };
    tty::set_winsize(TtyIndex(0), &custom as *const _);

    // Read back TTY 1 — should be unchanged.
    let mut ws1_check = slopos_abi::syscall::UserWinsize::default();
    tty::get_winsize(TtyIndex(1), &mut ws1_check as *mut _);

    // Restore TTY 0.
    tty::set_winsize(TtyIndex(0), &ws0_saved as *const _);

    if ws1_check.ws_row != ws1_saved.ws_row || ws1_check.ws_col != ws1_saved.ws_col {
        klog_info!(
            "TTY_TEST: BUG - TTY 1 winsize changed when TTY 0 was modified ({}x{} vs {}x{})",
            ws1_check.ws_row,
            ws1_check.ws_col,
            ws1_saved.ws_row,
            ws1_saved.ws_col
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY foreground pgrp isolation.
pub fn test_tty_per_tty_fg_pgrp_isolation() -> TestResult {
    tty::table::tty_table_init();

    // Set different foreground pgrps on TTY 0 and TTY 1.
    tty::set_foreground_pgrp(TtyIndex(0), 100);
    tty::set_foreground_pgrp(TtyIndex(1), 200);

    let pgid0 = tty::get_foreground_pgrp(TtyIndex(0));
    let pgid1 = tty::get_foreground_pgrp(TtyIndex(1));

    // Clean up.
    tty::set_foreground_pgrp(TtyIndex(0), 0);
    tty::set_foreground_pgrp(TtyIndex(1), 0);

    if pgid0 != 100 {
        klog_info!("TTY_TEST: BUG - TTY 0 fg_pgrp should be 100, got {}", pgid0);
        return TestResult::Fail;
    }
    if pgid1 != 200 {
        klog_info!("TTY_TEST: BUG - TTY 1 fg_pgrp should be 200, got {}", pgid1);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY has_data isolation — data pushed to TTY 0 does not
/// appear on TTY 1.
pub fn test_tty_per_tty_has_data_isolation() -> TestResult {
    tty::table::tty_table_init();
    drain_tty_nonblock(TtyIndex(0));
    drain_tty_nonblock(TtyIndex(1));

    // Push a character + newline to TTY 0 only.
    tty::push_input(TtyIndex(0), b'x');
    tty::push_input(TtyIndex(0), b'\n');

    let has0 = tty::has_data(TtyIndex(0));
    let has1 = tty::has_data(TtyIndex(1));

    // Clean up.
    drain_tty_nonblock(TtyIndex(0));

    if !has0 {
        klog_info!("TTY_TEST: BUG - TTY 0 should have data after push_input");
        return TestResult::Fail;
    }
    if has1 {
        klog_info!("TTY_TEST: BUG - TTY 1 should NOT have data (isolation failure)");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: Per-TTY session isolation — attaching session to TTY 0 does not
/// affect TTY 1's session.
pub fn test_tty_per_tty_session_isolation() -> TestResult {
    tty::table::tty_table_init();

    tty::attach_session(TtyIndex(0), 500, 500);
    let sid0 = tty::get_session_id(TtyIndex(0));
    let sid1 = tty::get_session_id(TtyIndex(1));

    // Clean up.
    tty::detach_session(TtyIndex(0));

    if sid0 != 500 {
        klog_info!(
            "TTY_TEST: BUG - TTY 0 session_id should be 500, got {}",
            sid0
        );
        return TestResult::Fail;
    }
    if sid1 != 0 {
        klog_info!("TTY_TEST: BUG - TTY 1 session_id should be 0, got {}", sid1);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 5: tty::read on non-existent TTY returns -1.
pub fn test_tty_read_invalid_tty_returns_error() -> TestResult {
    tty::table::tty_table_init();
    let mut buf = [0u8; 8];
    let n = tty::read(TtyIndex(7), buf.as_mut_ptr(), buf.len(), true);
    if n != -1 {
        klog_info!(
            "TTY_TEST: BUG - read from invalid TTY returned {} instead of -1",
            n
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ===========================================================================
// Phase 6: Control-Plane Correctness regression tests
// ===========================================================================

/// Phase 6: TtyIndex from ABI crate is the same type used in drivers.
pub fn test_tty_index_abi_type() -> TestResult {
    let idx: slopos_abi::syscall::TtyIndex = slopos_abi::syscall::TtyIndex(3);
    let idx2: TtyIndex = TtyIndex(3);
    if idx != idx2 {
        klog_info!("TTY_TEST: BUG - ABI TtyIndex != drivers TtyIndex");
        return TestResult::Fail;
    }
    if idx.0 != 3 {
        klog_info!("TTY_TEST: BUG - TtyIndex inner value mismatch");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: Signal constants from ABI match expected POSIX values.
pub fn test_signal_constants() -> TestResult {
    if SIGINT != 2 {
        klog_info!("TTY_TEST: BUG - SIGINT should be 2, got {}", SIGINT);
        return TestResult::Fail;
    }
    if SIGQUIT != 3 {
        klog_info!("TTY_TEST: BUG - SIGQUIT should be 3, got {}", SIGQUIT);
        return TestResult::Fail;
    }
    if SIGTSTP != 20 {
        klog_info!("TTY_TEST: BUG - SIGTSTP should be 20, got {}", SIGTSTP);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: set_compositor_focus does NOT modify fg_pgrp.
pub fn test_set_compositor_focus_does_not_set_fg_pgrp() -> TestResult {
    tty::table::tty_table_init();
    // Set a known fg_pgrp first.
    tty::set_foreground_pgrp(TtyIndex(0), 42);
    let fg_before = tty::get_foreground_pgrp(TtyIndex(0));

    // Change compositor focus.
    tty::set_compositor_focus(99);
    let fg_after = tty::get_foreground_pgrp(TtyIndex(0));
    tty::set_compositor_focus(0); // Reset.

    if fg_before != fg_after {
        klog_info!(
            "TTY_TEST: BUG - set_compositor_focus changed fg_pgrp: {} -> {}",
            fg_before,
            fg_after
        );
        return TestResult::Fail;
    }
    if fg_before != 42 {
        klog_info!("TTY_TEST: BUG - fg_pgrp should be 42, got {}", fg_before);
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Phase 6: check_read is the sole read gate — BackgroundRead for non-fg pgrp.
pub fn test_check_read_sole_gate_background() -> TestResult {
    let mut s = TtySession::new();
    s.attach(10, 10); // session=10, fg_pgrp=10
    s.focused_task_id = 42; // compositor says task 42 is focused

    // Even though task 42 has compositor focus, if its pgid (99) is NOT
    // in the foreground pgrp (10), check_read must return BackgroundRead.
    // This is the key Phase 6 semantic: compositor focus != POSIX foreground.
    match s.check_read(99, 10) {
        ForegroundCheck::BackgroundRead => TestResult::Pass,
        other => {
            klog_info!(
                "TTY_TEST: BUG - compositor-focused but bg pgrp should be BackgroundRead, got {:?}",
                other
            );
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
        test_session_attach,
        test_session_detach,
        test_session_check_read_foreground,
        test_session_check_read_background,
        test_session_check_read_no_session,
        test_session_check_read_kernel_task,
        test_session_check_write_no_tostop,
        test_session_check_write_tostop_background,
        // Phase 6: check_read replaces task_has_access
        test_session_check_read_replaces_task_has_access_foreground,
        test_session_check_read_replaces_task_has_access_background,
        test_session_check_read_replaces_task_has_access_permissive,
        test_session_set_fg_pgrp_checked_allowed,
        test_session_set_fg_pgrp_checked_denied,
        test_session_set_fg_pgrp_checked_no_session,
        test_tty_get_session_id_default,
        test_tty_attach_session,
        test_tty_detach_session,
        test_tty_detach_session_by_id,
        test_tty_set_fg_pgrp_checked,
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
        test_compositor_focus,
        test_keyboard_enter_scancode_reaches_active_tty,
        test_keyboard_scancode_routes_to_active_tty_index,
        test_keyboard_extended_up_arrow_reaches_tty,
        // Phase 2: Input flag processing
        test_ldisc_icrnl,
        test_ldisc_igncr,
        test_ldisc_inlcr,
        test_ldisc_istrip,
        // Phase 2: Output processing
        test_ldisc_opost_onlcr,
        test_ldisc_opost_ocrnl,
        test_ldisc_output_raw,
        // Phase 2: Signal generation
        test_ldisc_signal_ctrl_backslash,
        test_ldisc_signal_ctrl_z,
        // Phase 2: Flow control
        test_ldisc_flow_control_ixon,
        // Phase 2: ECHOCTL
        test_ldisc_echoctl,
        // Phase 2: VLNEXT
        test_ldisc_vlnext,
        // Phase 2: VWERASE
        test_ldisc_vwerase,
        // Phase 2: edit_content / reprint
        test_ldisc_edit_content,
        // Phase 2: Output processing via TTY write
        test_tty_write_returns_input_len,
        // Phase 3: Input pipeline cleanup
        test_keyboard_no_input_event_delivery,
        test_keyboard_break_code_no_input,
        test_keyboard_modifier_no_input,
        test_keyboard_press_release_single_char,
        test_vconsole_drain_via_drain_hw_input,
        test_keyboard_multi_key_sequence,
        // Phase 5: FD integration
        test_tty_write_output_processing,
        test_tty_write_raw_passthrough,
        test_tty_write_invalid_index,
        test_tty_per_tty_termios_isolation,
        test_tty_per_tty_winsize_isolation,
        test_tty_per_tty_fg_pgrp_isolation,
        test_tty_per_tty_has_data_isolation,
        test_tty_per_tty_session_isolation,
        test_tty_read_invalid_tty_returns_error,
        // Phase 6: Control-Plane Correctness
        test_tty_index_abi_type,
        test_signal_constants,
        test_set_compositor_focus_does_not_set_fg_pgrp,
        test_check_read_sole_gate_background,
    ]
);
