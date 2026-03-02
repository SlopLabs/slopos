//! TTY driver abstraction — backend hardware operations for each terminal.
//!
//! `TtyDriver` is the trait that abstracts over different terminal backends.
//! `TtyDriverKind` is an enum dispatch so we avoid trait objects in `no_std`.
//!
//! Two initial implementations:
//! - `SerialConsoleDriver` — wraps COM1 UART (polling-based)
//! - `VConsoleDriver`      — wraps PS/2 keyboard + framebuffer output (stub)

use slopos_abi::syscall::UserTermios;
use slopos_lib::ports::COM1;

use crate::serial;

/// Backend driver operations for a TTY.
///
/// Implementors provide the hardware-level write and optional input polling.
/// The TTY core calls these methods — the driver never touches the line
/// discipline directly.
pub trait TtyDriver {
    /// Write processed output bytes to the terminal hardware (serial port,
    /// framebuffer, etc.).
    fn write_output(&self, buf: &[u8]);

    /// Poll for pending hardware input, returning bytes read into `out`.
    /// Called by `Tty::drain_hw_input`.  May return 0 if no data is available
    /// (e.g. PS/2 input comes via interrupt, not polling).
    fn drain_input(&self, out: &mut [u8]) -> usize;

    /// Optional: called when termios changes (e.g. baud rate).
    fn set_termios(&self, _termios: &UserTermios) {}
}

// ---------------------------------------------------------------------------
// Enum dispatch — avoids `dyn TtyDriver` in no_std
// ---------------------------------------------------------------------------

/// Concrete driver backend for a `Tty`.
///
/// We use an enum rather than a trait object so that `Tty` can live in a
/// `const`-initialised static array without requiring `alloc`.
pub enum TtyDriverKind {
    /// COM1 serial console.
    SerialConsole(SerialConsoleDriver),
    /// PS/2 keyboard + framebuffer virtual console (stub for Phase 3+).
    VConsole(VConsoleDriver),
    /// Uninitialised / empty slot.
    None,
}

impl TtyDriverKind {
    /// Delegate `write_output` to the inner driver.
    pub fn write_output(&self, buf: &[u8]) {
        match self {
            Self::SerialConsole(d) => d.write_output(buf),
            Self::VConsole(d) => d.write_output(buf),
            Self::None => {}
        }
    }

    /// Delegate `drain_input` to the inner driver.
    pub fn drain_input(&self, out: &mut [u8]) -> usize {
        match self {
            Self::SerialConsole(d) => d.drain_input(out),
            Self::VConsole(d) => d.drain_input(out),
            Self::None => 0,
        }
    }

    /// Delegate `set_termios` to the inner driver.
    pub fn set_termios(&self, termios: &UserTermios) {
        match self {
            Self::SerialConsole(d) => d.set_termios(termios),
            Self::VConsole(d) => d.set_termios(termios),
            Self::None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Serial console driver — wraps COM1 UART polling-based I/O
// ---------------------------------------------------------------------------

/// Driver backend for COM1 serial console (TTY 0).
///
/// Output goes through `serial_putc_com1`.  Input is polled from the serial
/// UART's `INPUT_BUFFER` ring via `serial_poll_receive` + buffer drain.
pub struct SerialConsoleDriver;

impl TtyDriver for SerialConsoleDriver {
    fn write_output(&self, buf: &[u8]) {
        for &b in buf {
            serial::serial_putc_com1(b);
        }
    }

    fn drain_input(&self, out: &mut [u8]) -> usize {
        // Poll the UART first — moves bytes from hardware FIFO into
        // INPUT_BUFFER.
        serial::serial_poll_receive(COM1.address());

        // Drain whatever the UART deposited into our scratch buffer.
        let mut buf = serial::input_buffer_lock();
        let mut n = 0usize;
        while n < out.len() {
            match buf.try_pop() {
                Some(b) => {
                    out[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------
// Virtual console driver — PS/2 keyboard + framebuffer (stub)
// ---------------------------------------------------------------------------

/// Driver backend for a virtual console (PS/2 keyboard + framebuffer).
///
/// Input arrives via interrupt (`tty::push_input`), so `drain_input` returns 0.
/// Output will eventually go to the framebuffer; for now it mirrors to serial.
pub struct VConsoleDriver;

impl TtyDriver for VConsoleDriver {
    fn write_output(&self, buf: &[u8]) {
        // TODO(Phase 3+): Route to framebuffer text renderer.
        // For now, mirror to serial so we don't lose output.
        for &b in buf {
            serial::serial_putc_com1(b);
        }
    }

    fn drain_input(&self, _out: &mut [u8]) -> usize {
        // PS/2 keyboard input comes via interrupt → tty::push_input.
        // No polling needed.
        0
    }
}
