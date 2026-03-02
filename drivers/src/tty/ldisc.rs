//! Line discipline for TTY subsystem.
//!
//! This is the existing `LineDisc` implementation, moved from `drivers/src/line_disc.rs`
//! into the new `tty/` module directory.  It processes raw input bytes through
//! canonical/non-canonical modes, echo, and signal generation.
//!
//! Phase 2 of the TTY overhaul will enhance this with input/output flag processing,
//! VMIN/VTIME, flow control, and additional echo modes.

use slopos_abi::syscall::{
    ECHO, ECHOE, ECHOK, ECHONL, ICANON, ISIG, NCCS, UserTermios, VEOF, VERASE, VINTR, VKILL,
};

const EDIT_BUF_SIZE: usize = 1024;
const COOKED_BUF_SIZE: usize = 4096;

/// Actions returned by the line discipline after processing an input byte.
pub enum InputAction {
    /// No action needed.
    None,
    /// Echo bytes back to the terminal.  Up to 4 bytes (e.g. BS-SPACE-BS).
    Echo { buf: [u8; 4], len: u8 },
    /// Deliver a signal to the foreground process group.
    Signal(u8),
}

/// The line discipline state machine.
///
/// Each `Tty` owns one `LineDisc` instance.  It maintains an edit buffer
/// (for canonical mode line editing) and a cooked ring buffer (ready for
/// userland `read()`).
pub struct LineDisc {
    termios: UserTermios,
    edit_buf: [u8; EDIT_BUF_SIZE],
    edit_len: usize,
    cooked: [u8; COOKED_BUF_SIZE],
    cooked_head: usize,
    cooked_tail: usize,
    cooked_count: usize,
}

impl LineDisc {
    /// Create a new `LineDisc` with default termios (canonical + echo + signals).
    pub const fn new() -> Self {
        let cc = [
            0x03, 0x1C, 0x7F, 0x15, 0x04, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        Self {
            termios: UserTermios {
                c_iflag: 0,
                c_oflag: 0,
                c_cflag: 0,
                c_lflag: ICANON | ECHO | ISIG | ECHOE,
                c_line: 0,
                c_cc: cc,
                c_ispeed: 0,
                c_ospeed: 0,
            },
            edit_buf: [0; EDIT_BUF_SIZE],
            edit_len: 0,
            cooked: [0; COOKED_BUF_SIZE],
            cooked_head: 0,
            cooked_tail: 0,
            cooked_count: 0,
        }
    }

    /// Immutable reference to the current termios.
    pub fn termios(&self) -> &UserTermios {
        &self.termios
    }

    /// Update termios.  If canonical mode is toggled off, flushes the edit
    /// buffer so that any pending characters become available for raw reads.
    pub fn set_termios(&mut self, t: &UserTermios) {
        let was_canon = (self.termios.c_lflag & ICANON) != 0;
        let is_canon = (t.c_lflag & ICANON) != 0;
        self.termios = *t;
        if was_canon && !is_canon {
            self.flush_edit_to_cooked();
        }
    }

    /// Returns `true` if the cooked ring buffer has bytes available for reading.
    pub fn has_data(&self) -> bool {
        self.cooked_count > 0
    }

    /// Read cooked bytes into `out`, returning the number of bytes copied.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        while copied < out.len() && self.cooked_count > 0 {
            out[copied] = self.cooked[self.cooked_tail];
            self.cooked_tail = (self.cooked_tail + 1) % COOKED_BUF_SIZE;
            self.cooked_count -= 1;
            copied += 1;
        }
        copied
    }

    /// Process a single raw input byte through the line discipline.
    ///
    /// Returns an `InputAction` indicating what the caller should do (echo,
    /// signal, or nothing).
    pub fn input_char(&mut self, c: u8) -> InputAction {
        let lflag = self.termios.c_lflag;

        if (lflag & ISIG) != 0 && c == self.cc(VINTR) {
            return InputAction::Signal(2);
        }

        if (lflag & ICANON) != 0 {
            if c == self.cc(VERASE) || c == 0x08 {
                if self.edit_len > 0 {
                    self.edit_len -= 1;
                    if (lflag & ECHOE) != 0 {
                        return InputAction::Echo {
                            buf: [0x08, 0x20, 0x08, 0],
                            len: 3,
                        };
                    }
                }
                return InputAction::None;
            }

            if c == self.cc(VKILL) {
                self.edit_len = 0;
                if (lflag & ECHOK) != 0 {
                    return InputAction::Echo {
                        buf: [b'\n', 0, 0, 0],
                        len: 1,
                    };
                }
                return InputAction::None;
            }

            if c == self.cc(VEOF) {
                self.flush_edit_to_cooked();
                return InputAction::None;
            }

            if c == b'\n' || c == b'\r' {
                if self.edit_len < EDIT_BUF_SIZE {
                    self.edit_buf[self.edit_len] = b'\n';
                    self.edit_len += 1;
                }
                self.flush_edit_to_cooked();
                if (lflag & (ECHO | ECHONL)) != 0 {
                    return InputAction::Echo {
                        buf: [b'\n', 0, 0, 0],
                        len: 1,
                    };
                }
                return InputAction::None;
            }

            if self.is_printable(c) {
                if self.edit_len < EDIT_BUF_SIZE {
                    self.edit_buf[self.edit_len] = c;
                    self.edit_len += 1;
                }
                if (lflag & ECHO) != 0 {
                    return InputAction::Echo {
                        buf: [c, 0, 0, 0],
                        len: 1,
                    };
                }
            }
            return InputAction::None;
        }

        // Non-canonical mode: push directly to cooked buffer.
        self.push_cooked(c);
        if (lflag & ECHO) != 0 {
            return InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            };
        }
        InputAction::None
    }

    /// Look up a control character from the c_cc array.
    fn cc(&self, idx: usize) -> u8 {
        if idx < NCCS {
            self.termios.c_cc[idx]
        } else {
            0
        }
    }

    /// Returns `true` if `c` is a printable ASCII character or tab.
    fn is_printable(&self, c: u8) -> bool {
        (0x20..=0x7E).contains(&c) || c == b'\t'
    }

    /// Push a single byte into the cooked ring buffer.
    pub(crate) fn push_cooked(&mut self, c: u8) {
        if self.cooked_count >= COOKED_BUF_SIZE {
            return;
        }
        self.cooked[self.cooked_head] = c;
        self.cooked_head = (self.cooked_head + 1) % COOKED_BUF_SIZE;
        self.cooked_count += 1;
    }

    /// Move everything in the edit buffer into the cooked ring buffer.
    fn flush_edit_to_cooked(&mut self) {
        let mut i = 0usize;
        while i < self.edit_len {
            self.push_cooked(self.edit_buf[i]);
            i += 1;
        }
        self.edit_len = 0;
    }
}
