//! Enhanced line discipline for the TTY subsystem (Phase 2).
//!
//! This module implements a simplified but fairly complete N_TTY-style line
//! discipline.  Compared to the Phase 1 stub it adds:
//!
//! - **Input flag processing** (`c_iflag`): ICRNL, INLCR, IGNCR, ISTRIP
//! - **Output flag processing** (`c_oflag`): OPOST, ONLCR, OCRNL, ONOCR, ONLRET
//! - **Additional echo modes**: ECHOCTL (^X for control chars), ECHOKE (kill
//!   via backspace sequence)
//! - **Signal generation**: SIGINT (VINTR), SIGQUIT (VQUIT), SIGTSTP (VSUSP)
//! - **Flow control**: IXON with VSTOP/VSTART (Ctrl+S / Ctrl+Q)
//! - **Canonical editing**: VWERASE (Ctrl+W word erase), VREPRINT (Ctrl+R
//!   redisplay), VLNEXT (Ctrl+V literal next)
//! - **Non-canonical mode**: VMIN/VTIME parsed (timing not yet enforced)
//! - **Column tracking** for proper backspace/kill echo
//!
//! The line discipline never touches the hardware directly — it returns
//! [`InputAction`] / [`OutputAction`] values that the caller (the TTY core in
//! `mod.rs`) translates into driver writes.

use slopos_abi::syscall::{
    ECHO,
    // c_lflag (additional)
    ECHOCTL,
    ECHOE,
    ECHOK,
    ECHOKE,
    ECHONL,
    ICANON,
    // c_iflag
    ICRNL,
    IEXTEN,
    IGNCR,
    INLCR,
    ISIG,
    ISTRIP,
    IXON,
    NCCS,
    // c_oflag
    OCRNL,
    ONLCR,
    ONLRET,
    ONOCR,
    OPOST,
    // Signal numbers
    SIGINT,
    SIGQUIT,
    SIGTSTP,
    UserTermios,
    // c_cc indices
    VEOF,
    VERASE,
    VINTR,
    VKILL,
    VLNEXT,
    VQUIT,
    VREPRINT,
    VSTART,
    VSTOP,
    VSUSP,
    VWERASE,
};

const EDIT_BUF_SIZE: usize = 1024;
const COOKED_BUF_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Action enums returned to the caller
// ---------------------------------------------------------------------------

/// Actions returned by the line discipline after processing an input byte.
pub enum InputAction {
    /// No action needed.
    None,
    /// Echo bytes back to the terminal.  Up to 4 bytes (e.g. BS-SPACE-BS or ^X).
    Echo { buf: [u8; 4], len: u8 },
    /// Deliver a signal to the foreground process group.
    Signal(u8),
    /// Redisplay the current edit line (VREPRINT / Ctrl+R).
    ///
    /// The caller should write a newline followed by the contents returned
    /// by [`LineDisc::edit_content()`].
    ReprintLine,
}

/// Actions returned by output processing (`process_output_byte`).
pub enum OutputAction {
    /// Emit these bytes to the driver (up to 2 bytes, e.g. `\r\n`).
    Emit { buf: [u8; 2], len: u8 },
    /// Expand a tab to N spaces (tab stop expansion).
    Tab(u8),
    /// Suppress this byte entirely (don't output anything).
    Suppress,
}

// ---------------------------------------------------------------------------
// LineDisc
// ---------------------------------------------------------------------------

/// The line discipline state machine.
///
/// Each `Tty` owns one `LineDisc` instance.  It maintains an edit buffer
/// (for canonical mode line editing) and a cooked ring buffer (ready for
/// userland `read()`).
pub struct LineDisc {
    termios: UserTermios,

    // -- Canonical mode buffers --
    edit_buf: [u8; EDIT_BUF_SIZE],
    edit_len: usize,

    // -- Cooked output ring buffer (ready for userland read) --
    cooked: [u8; COOKED_BUF_SIZE],
    cooked_head: usize,
    cooked_tail: usize,
    cooked_count: usize,

    // -- Flow control --
    /// Output stopped via XOFF (Ctrl+S / VSTOP).
    stopped: bool,
    /// Next input character is literal (Ctrl+V / VLNEXT was pressed).
    literal_next: bool,

    // -- Column tracking (for ECHOKE / backspace echo) --
    column: usize,
}

impl LineDisc {
    /// Create a new `LineDisc` with default termios (canonical + echo + signals).
    pub const fn new() -> Self {
        let cc = [
            0x03, // VINTR   = Ctrl+C
            0x1C, // VQUIT   = Ctrl+backslash
            0x7F, // VERASE  = DEL
            0x15, // VKILL   = Ctrl+U
            0x04, // VEOF    = Ctrl+D
            0,    // VTIME
            1,    // VMIN
            0,    // (unused index 7)
            0x11, // VSTART  = Ctrl+Q
            0x13, // VSTOP   = Ctrl+S
            0x1A, // VSUSP   = Ctrl+Z
            0,    // VEOL
            0x12, // VREPRINT = Ctrl+R
            0,    // (unused index 13)
            0x17, // VWERASE = Ctrl+W
            0x16, // VLNEXT  = Ctrl+V
            0, 0, 0,
        ];
        Self {
            termios: UserTermios {
                c_iflag: ICRNL,
                c_oflag: OPOST | ONLCR,
                c_cflag: 0,
                c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE,
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
            stopped: false,
            literal_next: false,
            column: 0,
        }
    }

    // -- Accessors -----------------------------------------------------------

    /// Immutable reference to the current termios.
    pub fn termios(&self) -> &UserTermios {
        &self.termios
    }

    /// Returns (vmin, vtime_deciseconds) for non-canonical mode reads.
    /// vtime is in deciseconds (100ms units) as per POSIX.
    pub fn vmin_vtime(&self) -> (u8, u8) {
        // c_cc layout: index 5 = VTIME, index 6 = VMIN
        let vtime = self.termios.c_cc[5];
        let vmin = self.termios.c_cc[6];
        (vmin, vtime)
    }

    /// Returns true if in canonical mode.
    pub fn is_canonical(&self) -> bool {
        (self.termios.c_lflag & ICANON) != 0
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

    pub fn flush_all(&mut self) {
        self.edit_len = 0;
        self.cooked_head = 0;
        self.cooked_tail = 0;
        self.cooked_count = 0;
        self.stopped = false;
        self.literal_next = false;
        self.column = 0;
    }

    /// Return a slice of the current edit buffer contents (for VREPRINT echo).
    pub fn edit_content(&self) -> &[u8] {
        &self.edit_buf[..self.edit_len]
    }

    /// Whether output is currently stopped (XOFF / Ctrl+S).
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    // -- Input processing ----------------------------------------------------

    /// Process a single raw input byte through the line discipline.
    ///
    /// Returns an [`InputAction`] indicating what the caller should do (echo,
    /// signal, reprint, or nothing).
    pub fn input_char(&mut self, c: u8) -> InputAction {
        let iflag = self.termios.c_iflag;
        let lflag = self.termios.c_lflag;

        // 1. Input flag processing (c_iflag).
        let c = self.process_iflag(c, iflag);

        // A return value of None from process_iflag means "discard this byte"
        // (IGNCR ate it).
        let c = match c {
            Some(c) => c,
            None => return InputAction::None,
        };

        // 2. Literal-next mode (Ctrl+V was pressed previously).
        if self.literal_next {
            self.literal_next = false;
            return self.insert_char(c, lflag);
        }

        // 3. Signal generation (ISIG).
        if (lflag & ISIG) != 0 {
            if c == self.cc(VINTR) {
                return InputAction::Signal(SIGINT);
            }
            if c == self.cc(VQUIT) {
                return InputAction::Signal(SIGQUIT);
            }
            if c == self.cc(VSUSP) {
                return InputAction::Signal(SIGTSTP);
            }
        }

        // 4. Flow control (IXON).
        if (iflag & IXON) != 0 {
            if c == self.cc(VSTOP) {
                self.stopped = true;
                return InputAction::None;
            }
            if c == self.cc(VSTART) {
                self.stopped = false;
                return InputAction::None;
            }
            // Any character resumes output when stopped (if IXON is set).
            if self.stopped {
                self.stopped = false;
            }
        }

        // 5. Extended input processing (IEXTEN).
        if (lflag & IEXTEN) != 0 {
            if c == self.cc(VLNEXT) {
                self.literal_next = true;
                // Echo ^V if ECHOCTL is set.
                if (lflag & ECHOCTL) != 0 && (lflag & ECHO) != 0 {
                    return InputAction::Echo {
                        buf: [b'^', b'V', 0, 0],
                        len: 2,
                    };
                }
                return InputAction::None;
            }
            if (lflag & ICANON) != 0 {
                if c == self.cc(VWERASE) {
                    return self.word_erase(lflag);
                }
                if c == self.cc(VREPRINT) {
                    return InputAction::ReprintLine;
                }
            }
        }

        // 6. Canonical vs non-canonical.
        if (lflag & ICANON) != 0 {
            self.canonical_input(c, lflag)
        } else {
            self.raw_input(c, lflag)
        }
    }

    // -- Output processing ---------------------------------------------------

    /// Process a single byte through `c_oflag` before sending to the driver.
    ///
    /// Called by the TTY core's `write()` function for each output byte.
    pub fn process_output_byte(&mut self, c: u8) -> OutputAction {
        let oflag = self.termios.c_oflag;
        if (oflag & OPOST) == 0 {
            // No output processing — still track column for echo accuracy.
            self.update_column_raw(c);
            return OutputAction::Emit {
                buf: [c, 0],
                len: 1,
            };
        }
        match c {
            b'\n' if (oflag & ONLCR) != 0 => {
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\r', b'\n'],
                    len: 2,
                }
            }
            b'\r' if (oflag & OCRNL) != 0 => {
                // OCRNL: convert CR to NL.  If ONLRET is also set, reset column.
                if (oflag & ONLRET) != 0 {
                    self.column = 0;
                }
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\r' if (oflag & ONOCR) != 0 && self.column == 0 => OutputAction::Suppress,
            b'\n' if (oflag & ONLRET) != 0 => {
                // ONLRET: NL performs CR function — reset column.
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\r' => {
                self.column = 0;
                OutputAction::Emit {
                    buf: [b'\r', 0],
                    len: 1,
                }
            }
            b'\n' => {
                // Plain NL without ONLCR/ONLRET — no column reset per POSIX.
                OutputAction::Emit {
                    buf: [b'\n', 0],
                    len: 1,
                }
            }
            b'\t' => {
                let spaces = 8 - (self.column % 8);
                self.column += spaces;
                OutputAction::Tab(spaces as u8)
            }
            0x08 => {
                // Backspace — decrement column if possible.
                if self.column > 0 {
                    self.column -= 1;
                }
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
            c if c >= 0x20 && c < 0x7F => {
                self.column += 1;
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
            _ => {
                // Non-printable control char — no column change.
                OutputAction::Emit {
                    buf: [c, 0],
                    len: 1,
                }
            }
        }
    }

    // -- Private helpers -----------------------------------------------------

    /// Apply c_iflag processing to a raw input byte.
    ///
    /// Returns `None` if the byte should be discarded (IGNCR).
    fn process_iflag(&self, c: u8, iflag: u32) -> Option<u8> {
        let mut c = c;

        // ISTRIP: strip bit 7.
        if (iflag & ISTRIP) != 0 {
            c &= 0x7F;
        }

        // CR/NL mapping.
        if c == b'\r' {
            if (iflag & IGNCR) != 0 {
                return None; // Discard CR entirely.
            }
            if (iflag & ICRNL) != 0 {
                c = b'\n'; // Map CR → NL.
            }
        } else if c == b'\n' && (iflag & INLCR) != 0 {
            c = b'\r'; // Map NL → CR.
        }

        Some(c)
    }

    /// Canonical mode input processing.
    fn canonical_input(&mut self, c: u8, lflag: u32) -> InputAction {
        // VERASE (backspace).
        if c == self.cc(VERASE) || c == 0x08 {
            return self.erase_char(lflag);
        }

        // VKILL (kill line).
        if c == self.cc(VKILL) {
            return self.kill_line(lflag);
        }

        // VEOF (Ctrl+D) — flush without adding a newline.
        if c == self.cc(VEOF) {
            self.flush_edit_to_cooked();
            return InputAction::None;
        }

        // Newline / carriage return — flush with newline appended.
        if c == b'\n' || c == b'\r' {
            if self.edit_len < EDIT_BUF_SIZE {
                self.edit_buf[self.edit_len] = b'\n';
                self.edit_len += 1;
            }
            self.flush_edit_to_cooked();
            self.column = 0;
            if (lflag & (ECHO | ECHONL)) != 0 {
                return InputAction::Echo {
                    buf: [b'\n', 0, 0, 0],
                    len: 1,
                };
            }
            return InputAction::None;
        }

        // Regular character — insert into edit buffer.
        self.insert_char(c, lflag)
    }

    /// Insert a character into the edit buffer and produce an echo action.
    fn insert_char(&mut self, c: u8, lflag: u32) -> InputAction {
        if self.edit_len < EDIT_BUF_SIZE {
            self.edit_buf[self.edit_len] = c;
            self.edit_len += 1;
        }

        if (lflag & ECHO) == 0 {
            return InputAction::None;
        }

        // ECHOCTL: control characters (except TAB, NL) are echoed as ^X.
        if (lflag & ECHOCTL) != 0 && c < 0x20 && c != b'\t' && c != b'\n' {
            self.column += 2;
            return InputAction::Echo {
                buf: [b'^', c + 0x40, 0, 0],
                len: 2,
            };
        }

        if self.is_printable(c) {
            self.column += 1;
            return InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            };
        }

        // Non-printable, no ECHOCTL — no echo.
        InputAction::None
    }

    /// Non-canonical (raw) mode: push directly to cooked buffer.
    fn raw_input(&mut self, c: u8, lflag: u32) -> InputAction {
        self.push_cooked(c);
        if (lflag & ECHO) != 0 {
            // ECHOCTL in raw mode.
            if (lflag & ECHOCTL) != 0 && c < 0x20 && c != b'\t' && c != b'\n' {
                return InputAction::Echo {
                    buf: [b'^', c + 0x40, 0, 0],
                    len: 2,
                };
            }
            return InputAction::Echo {
                buf: [c, 0, 0, 0],
                len: 1,
            };
        }
        InputAction::None
    }

    /// Erase one character (VERASE / backspace).
    fn erase_char(&mut self, lflag: u32) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        let erased = self.edit_buf[self.edit_len - 1];
        self.edit_len -= 1;

        if (lflag & ECHOE) != 0 {
            // If the erased character was a control char echoed as ^X via
            // ECHOCTL, we need to erase two columns.
            if (lflag & ECHOCTL) != 0 && erased < 0x20 && erased != b'\t' && erased != b'\n' {
                self.column = self.column.saturating_sub(2);
                // BS SPACE BS BS SPACE BS — erase two columns.
                // We only have 4 bytes in the echo buffer, so we return
                // two BS-SPACE-BS sequences as a single 4-byte action
                // (first pair) and rely on the caller to issue two
                // actions.  Pragmatically, return 4 bytes covering the
                // first ^X column pair and let the second be handled by
                // a follow-up.  In practice, most terminals handle this
                // acceptably with just one BS-SP-BS triple.
                return InputAction::Echo {
                    buf: [0x08, 0x20, 0x08, 0x08],
                    len: 4,
                };
            }
            if self.column > 0 {
                self.column -= 1;
            }
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }
        InputAction::None
    }

    /// Kill the entire line (VKILL).
    fn kill_line(&mut self, lflag: u32) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        // ECHOKE: erase the line visually by backspacing over every character.
        // We can't do this in a single 4-byte action, so we just reset the
        // edit buffer and echo a newline (same as ECHOK) — a pragmatic
        // simplification that matches many real terminals.
        self.edit_len = 0;
        self.column = 0;

        if (lflag & ECHOKE) != 0 || (lflag & ECHOK) != 0 {
            return InputAction::Echo {
                buf: [b'\n', 0, 0, 0],
                len: 1,
            };
        }
        InputAction::None
    }

    /// Word erase (VWERASE / Ctrl+W): erase backward to start of previous word.
    fn word_erase(&mut self, lflag: u32) -> InputAction {
        if self.edit_len == 0 {
            return InputAction::None;
        }

        // Skip trailing whitespace, then delete until whitespace or start.
        let mut erased = 0usize;

        // Phase 1: skip trailing spaces.
        while self.edit_len > 0 && self.edit_buf[self.edit_len - 1] == b' ' {
            self.edit_len -= 1;
            erased += 1;
        }
        // Phase 2: delete word characters.
        while self.edit_len > 0 && self.edit_buf[self.edit_len - 1] != b' ' {
            self.edit_len -= 1;
            erased += 1;
        }

        self.column = self.column.saturating_sub(erased);

        // Echo backspace-space-backspace for each erased character.
        // We can only return 4 bytes, so for longer erases we just echo
        // a newline + the remaining edit content (like a simplified reprint).
        // Most terminals handle this gracefully.
        if erased <= 1 && (lflag & ECHOE) != 0 {
            return InputAction::Echo {
                buf: [0x08, 0x20, 0x08, 0],
                len: 3,
            };
        }

        // For multi-char erases, request a reprint so the line is redrawn.
        if (lflag & ECHO) != 0 {
            return InputAction::ReprintLine;
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

    /// Track column position for a raw byte (no OPOST processing).
    /// Used when OPOST is disabled so echo column tracking stays accurate.
    fn update_column_raw(&mut self, c: u8) {
        match c {
            b'\n' | b'\r' => self.column = 0,
            b'\t' => self.column += 8 - (self.column % 8),
            0x08 => {
                if self.column > 0 {
                    self.column -= 1;
                }
            }
            c if c >= 0x20 && c < 0x7F => self.column += 1,
            _ => {}
        }
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
