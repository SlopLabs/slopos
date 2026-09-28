//! Trap state and delivery (POSIX XCU 2.14). A handler only records its signal;
//! the action runs after the command in progress, in the shell's own context.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use slopos_abi::signal::{SIGINT, SIGKILL, SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU, SigSet, sig_bit};
use slopos_shell_core::trap::{self, Action, Condition, MAX_SIGNAL};

use crate::syscall::{OwnedFd, RawFd, fs, process};

use super::{exec, interrupt};

/// Indexed by condition: 0 is `EXIT`, `n` is signal `n`. `None` is the
/// default action; an empty action is an ignored condition.
static TABLE: Mutex<Vec<Option<Vec<u8>>>> = Mutex::new(Vec::new());

static PENDING: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// `$?` when the running action began; negative outside any action.
static STATUS_BEFORE_ACTION: AtomicI32 = AtomicI32::new(-1);

static PROBED: AtomicU64 = AtomicU64::new(0);
static IGNORED_ON_ENTRY: AtomicU64 = AtomicU64::new(0);

/// Write end of the pipe a blocking `wait` polls beside its child, or -1.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn record_trap(signum: i32) {
    PENDING.fetch_or(sig_bit(signum as u8), Ordering::Release);
    wake_waiter();
}

/// Called from a signal handler after its record is stored, so a waiter that
/// checked the record just before this signal still wakes.
pub(super) fn wake_waiter() {
    let fd = WAKE_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let _ = fs::write_slice(fd, &[0]);
    }
}

/// While armed, a trapped signal or an interrupt makes [`WakePipe::fd`]
/// readable, which a `poll` that has not yet blocked cannot miss.
pub struct WakePipe {
    read: OwnedFd,
    _write: OwnedFd,
}

impl WakePipe {
    pub fn arm() -> Option<Self> {
        let flags = (slopos_abi::syscall::O_NONBLOCK | slopos_abi::syscall::O_CLOEXEC) as u32;
        let (read, write) = fs::pipe2(flags).ok()?;
        WAKE_FD.store(write.raw(), Ordering::Release);
        Some(Self {
            read,
            _write: write,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.read.raw()
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 16];
        while matches!(fs::read_slice(self.read.raw(), &mut buf), Ok(n) if n > 0) {}
    }
}

impl Drop for WakePipe {
    fn drop(&mut self) {
        WAKE_FD.store(-1, Ordering::Release);
    }
}

fn slot(condition: Condition) -> usize {
    match condition {
        Condition::Exit => 0,
        Condition::Signal(signum) => signum as usize,
    }
}

fn store(condition: Condition, action: Option<Vec<u8>>) {
    let mut table = TABLE.lock().unwrap();
    let index = slot(condition);
    if table.len() <= index {
        table.resize(MAX_SIGNAL as usize + 1, None);
    }
    table[index] = action;
}

/// POSIX: a signal ignored on entry to a non-interactive shell can be neither
/// trapped nor reset. Probed at its first `trap`, before the shell changed it;
/// std's `SIGPIPE` reset is off for this binary (`build_userland.sh`).
fn ignored_on_entry(signum: u8) -> bool {
    if super::is_interactive() {
        return false;
    }
    let bit = sig_bit(signum);
    if PROBED.fetch_or(bit, Ordering::Relaxed) & bit == 0 && process::signal_ignored(signum) {
        IGNORED_ON_ENTRY.fetch_or(bit, Ordering::Relaxed);
    }
    IGNORED_ON_ENTRY.load(Ordering::Relaxed) & bit != 0
}

/// An interactive shell's own default is not SIG_DFL: it catches SIGINT and
/// ignores the job-control stop signals.
fn restore_default(signum: u8) {
    if super::is_interactive() && !interrupt::in_forked_child() {
        match signum {
            SIGINT => return interrupt::install(),
            SIGTSTP | SIGTTIN | SIGTTOU => {
                let _ = process::ignore_signal(signum);
                return;
            }
            _ => {}
        }
    }
    let _ = process::default_signal(signum);
}

/// Set `condition`'s action. `Err` for a signal that cannot be caught.
pub fn set(condition: Condition, action: Action<'_>) -> Result<(), ()> {
    if let Condition::Signal(signum) = condition {
        if signum == SIGKILL || signum == SIGSTOP {
            return Err(());
        }
        if ignored_on_entry(signum) {
            return Ok(());
        }
        match action {
            Action::Default => restore_default(signum),
            Action::Ignore => {
                let _ = process::ignore_signal(signum);
            }
            Action::Command(_) => {
                let _ = process::set_signal_handler(signum, record_trap);
            }
        }
        if !matches!(action, Action::Command(_)) {
            PENDING.fetch_and(!sig_bit(signum), Ordering::AcqRel);
        }
    }
    store(
        condition,
        match action {
            Action::Default => None,
            Action::Ignore => Some(Vec::new()),
            Action::Command(text) => Some(text.to_vec()),
        },
    );
    Ok(())
}

/// `trap` with no operands: one re-inputtable line per condition that is not
/// at its default.
pub fn listing() -> Vec<u8> {
    let table = TABLE.lock().unwrap();
    let mut out = Vec::new();
    for (index, action) in table.iter().enumerate() {
        let Some(action) = action else { continue };
        let condition = if index == 0 {
            Condition::Exit
        } else {
            Condition::Signal(index as u8)
        };
        trap::listing_line(&mut out, action, condition);
    }
    out
}

fn ignored_signals(table: &[Option<Vec<u8>>]) -> SigSet {
    let mut mask = 0;
    for (index, action) in table.iter().enumerate().skip(1) {
        if action.as_ref().is_some_and(Vec::is_empty) {
            mask |= sig_bit(index as u8);
        }
    }
    mask
}

/// Enter a subshell (POSIX XCU 2.12): caught traps revert, ignored ones stay,
/// nothing pending runs; `also_default` resets unless a trap ignores it.
pub fn enter_subshell(also_default: SigSet) {
    let mut table = TABLE.lock().unwrap();
    let mut reset = also_default;
    for (index, action) in table.iter_mut().enumerate() {
        if action.as_ref().is_some_and(|text| !text.is_empty()) {
            *action = None;
            if index > 0 {
                reset |= sig_bit(index as u8);
            }
        }
    }
    reset &= !ignored_signals(&table);
    drop(table);
    PENDING.store(0, Ordering::Release);
    if reset != 0 {
        let _ = process::sigdefault(reset);
    }
}

pub fn any_pending() -> bool {
    PENDING.load(Ordering::Acquire) != 0
}

/// The lowest trapped signal received but not yet acted on.
pub fn pending_signal() -> Option<u8> {
    let pending = PENDING.load(Ordering::Acquire);
    (pending != 0).then(|| pending.trailing_zeros() as u8 + 1)
}

/// The status an argument-less `exit` takes inside a trap action: the one
/// before the action began.
pub fn status_before_action() -> Option<i32> {
    let status = STATUS_BEFORE_ACTION.load(Ordering::Relaxed);
    (status >= 0).then_some(status)
}

fn run_action(text: &[u8], status: i32) {
    let outer = STATUS_BEFORE_ACTION.swap(status, Ordering::Relaxed);
    super::set_last_exit_code(status);
    match exec::parse_text(text) {
        Ok(list) => {
            let _ = exec::run_nested_list(&list);
        }
        Err(failure) => exec::report_parse_failure(failure),
    }
    STATUS_BEFORE_ACTION.store(outer, Ordering::Relaxed);
    if super::exit_requested().is_none() {
        super::set_last_exit_code(status);
    }
}

fn command_for(signum: u8) -> Option<Vec<u8>> {
    let table = TABLE.lock().unwrap();
    table
        .get(signum as usize)?
        .as_ref()
        .filter(|text| !text.is_empty())
        .cloned()
}

/// Run the action of each trapped signal received since the last call, in
/// signal order; one arriving during an action is taken when it ends.
pub fn run_pending() {
    if !any_pending() || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    'drain: loop {
        let pending = PENDING.swap(0, Ordering::AcqRel);
        if pending == 0 {
            break;
        }
        for signum in 1..=MAX_SIGNAL {
            if pending & sig_bit(signum) == 0 {
                continue;
            }
            if let Some(text) = command_for(signum) {
                run_action(&text, super::last_exit_code());
                if super::exit_requested().is_some() {
                    break 'drain;
                }
            }
        }
    }
    RUNNING.store(false, Ordering::Release);
}

/// Run the `EXIT` trap, once, as the shell ends with `status`; the result is
/// the status to exit with, which only an `exit n` inside the action changes.
pub fn run_exit_trap(status: i32) -> i32 {
    let action = TABLE
        .lock()
        .unwrap()
        .get_mut(0)
        .and_then(Option::take)
        .filter(|text| !text.is_empty());
    let Some(text) = action else {
        return status;
    };
    super::clear_exit_request();
    run_action(&text, status);
    super::exit_requested().unwrap_or(status)
}
