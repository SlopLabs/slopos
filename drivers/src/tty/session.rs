//! TTY session and process-group management.
//!
//! Implements POSIX-like session semantics for per-TTY foreground control.
//!
//! # Model
//!
//! Each TTY may have at most one **controlling session**.  Within that session,
//! exactly one process group is the **foreground group** — only members of this
//! group are allowed to read from (and, if `TOSTOP` is set, write to) the
//! terminal without receiving `SIGTTIN` / `SIGTTOU`.
//!
//! The compositor still drives `focused_task_id` for window-level focus.
//! `set_compositor_focus()` (called by the compositor) sets only
//! `focused_task_id` — it does NOT alter `fg_pgrp`.  The two concepts are
//! independent (Phase 6 split).

/// Sentinel value indicating "no session attached".
pub const NO_SESSION: u32 = 0;

/// Sentinel value indicating "no foreground process group".
pub const NO_FOREGROUND_PGRP: u32 = 0;

/// Per-TTY session and foreground process-group state.
///
/// In the POSIX model, each terminal has at most one controlling session,
/// and within that session exactly one process group is "foreground" (allowed
/// to read from / write to the terminal without signals).
#[derive(Clone, Copy)]
pub struct TtySession {
    /// Session leader's PID (0 = no session attached).
    pub session_leader: u32,
    /// Session ID (typically == session leader's PID).
    pub session_id: u32,
    /// Foreground process group ID (0 = none).
    pub fg_pgrp: u32,
    /// The task ID that currently has input focus on this TTY.
    /// Set by the compositor via `set_focus()`.  0 = no specific task focused.
    pub focused_task_id: u32,
}

/// Result of a foreground access check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundCheck {
    /// Caller is in the foreground group (or no session control) — access allowed.
    Allowed,
    /// No session is attached yet — access allowed (permissive boot path).
    NoSession,
    /// Caller is a background process trying to read — should receive `SIGTTIN`.
    BackgroundRead,
    /// Caller is a background process trying to write with `TOSTOP` — should
    /// receive `SIGTTOU`.
    BackgroundWrite,
}

impl TtySession {
    /// Create a new empty session (no controlling process).
    pub const fn new() -> Self {
        Self {
            session_leader: NO_SESSION,
            session_id: NO_SESSION,
            fg_pgrp: NO_FOREGROUND_PGRP,
            focused_task_id: 0,
        }
    }

    /// Returns `true` if a session is currently attached to this TTY.
    pub fn has_session(&self) -> bool {
        self.session_id != NO_SESSION
    }

    /// Attach a session to this TTY.
    ///
    /// `leader_pid` is the PID of the session leader (from `setsid()`).
    /// The session ID is set equal to the leader's PID, matching POSIX semantics.
    /// The foreground process group is initially set to the leader's process group.
    pub fn attach(&mut self, leader_pid: u32, leader_pgid: u32) {
        self.session_leader = leader_pid;
        self.session_id = leader_pid;
        self.fg_pgrp = leader_pgid;
    }

    /// Detach the current session from this TTY.
    ///
    /// Called when the session leader calls `setsid()` on a different terminal,
    /// or when the session leader exits.
    pub fn detach(&mut self) {
        self.session_leader = NO_SESSION;
        self.session_id = NO_SESSION;
        self.fg_pgrp = NO_FOREGROUND_PGRP;
        // Note: focused_task_id is NOT cleared — compositor focus is independent.
    }

    /// Check whether a process with the given pgid and sid may **read** from
    /// this TTY.
    ///
    /// # Returns
    ///
    /// - `Allowed` if the caller is in the foreground group.
    /// - `NoSession` if no session is attached (permissive).
    /// - `BackgroundRead` if the caller is in a background group.
    pub fn check_read(&self, caller_pgid: u32, caller_sid: u32) -> ForegroundCheck {
        // No session attached — permissive (pre-session-setup path).
        if !self.has_session() {
            return ForegroundCheck::NoSession;
        }

        // No foreground pgrp set — permissive.
        if self.fg_pgrp == NO_FOREGROUND_PGRP {
            return ForegroundCheck::NoSession;
        }

        // Caller not in the same session — should not be reading this TTY at
        // all, but we allow it permissively (controlling-terminal enforcement
        // is Phase 5 FD-layer work).
        if caller_sid != self.session_id && caller_sid != 0 {
            // For now, allow — Phase 5 will enforce per-FD controlling TTY.
        }

        // Foreground check.
        if caller_pgid == self.fg_pgrp {
            return ForegroundCheck::Allowed;
        }

        // Caller's pgid doesn't match, but maybe caller_pgid is 0 (kernel
        // task or unknown) — be permissive.
        if caller_pgid == 0 {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundRead
    }

    /// Check whether a process with the given pgid may **write** to this TTY.
    ///
    /// Write-side foreground enforcement only applies when `TOSTOP` is set in
    /// the TTY's termios.  Without `TOSTOP`, any process may write.
    ///
    /// # Arguments
    ///
    /// * `caller_pgid` — The caller's process group ID.
    /// * `tostop` — Whether the `TOSTOP` flag is set in `c_lflag`.
    pub fn check_write(&self, caller_pgid: u32, tostop: bool) -> ForegroundCheck {
        if !tostop {
            return ForegroundCheck::Allowed;
        }

        if !self.has_session() || self.fg_pgrp == NO_FOREGROUND_PGRP {
            return ForegroundCheck::Allowed;
        }

        if caller_pgid == 0 || caller_pgid == self.fg_pgrp {
            return ForegroundCheck::Allowed;
        }

        ForegroundCheck::BackgroundWrite
    }

    // NOTE: `task_has_access()` has been removed in Phase 6.
    // Use `check_read()` and `check_write()` directly instead.

    /// Set the foreground process group, with session validation.
    ///
    /// In POSIX, only processes in the same session as the TTY's controlling
    /// session may set the foreground pgrp.
    ///
    /// Returns `true` if the operation was allowed.
    pub fn set_fg_pgrp_checked(&mut self, pgid: u32, caller_sid: u32) -> bool {
        // If no session is attached, allow freely (pre-session path).
        if !self.has_session() {
            self.fg_pgrp = pgid;
            return true;
        }

        // Caller must be in the same session.
        if caller_sid != 0 && caller_sid != self.session_id {
            return false;
        }

        self.fg_pgrp = pgid;
        true
    }
}
