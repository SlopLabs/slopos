//! TTY session and process-group management.
//!
//! This is a **Phase 4 stub** — the struct definitions and basic helpers are
//! here so that `Tty` can embed a `TtySession` from the start, but the full
//! POSIX session/pgrp logic will be implemented in Phase 4.

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

impl TtySession {
    /// Create a new empty session (no controlling process).
    pub const fn new() -> Self {
        Self {
            session_leader: 0,
            session_id: 0,
            fg_pgrp: 0,
            focused_task_id: 0,
        }
    }
}
