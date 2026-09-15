//! Non-interactive input: reading commands from a descriptor.
//!
//! The script descriptor is the same one handed to every command run, so
//! `{ read x; cat; } < file` depends on the shell consuming nothing past the
//! line it executes; the framing lives in [`slopos_shell_core::ScriptReader`],
//! which reads a byte at a time.
//!
//! A command may span lines, so the reader appends them until the parser stops
//! answering [`ParseFailure::Incomplete`] — never past the last line of the
//! command it is about to run.

use slopos_shell_core::{ByteSource, Line, ScriptReader, SourceError};

use slopos_abi::fs::O_RDONLY;

use crate::syscall::{SyscallError, fs};

use super::buffers::SHELL_LINE_MAX;
use super::display::{shell_error, shell_error_named};
use super::exec::{self, ParseFailure};

/// A line longer than this is diagnosed and skipped, not truncated and run.
/// The same limit the interactive editor uses.
pub const SCRIPT_LINE_MAX: usize = SHELL_LINE_MAX;

/// A [`ByteSource`] over a file descriptor.
pub struct FdSource {
    fd: i32,
}

impl FdSource {
    pub const fn new(fd: i32) -> Self {
        Self { fd }
    }
}

impl ByteSource for FdSource {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError> {
        match fs::read_slice(self.fd, buf) {
            Ok(n) => Ok(n),
            Err(SyscallError::EINTR) => Err(SourceError::Interrupted),
            Err(_) => Err(SourceError::Fatal),
        }
    }
}

/// A [`ByteSource`] over bytes already in memory, for `sh -c STRING`.
pub struct SliceSource<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceSource<'a> {
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl ByteSource for SliceSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, SourceError> {
        if self.pos >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.data[self.pos];
        self.pos += 1;
        Ok(1)
    }
}

/// `sh -c STRING`.
pub fn run_command_string(text: &[u8]) -> i32 {
    run_script(&mut SliceSource::new(text))
}

/// `sh FILE` — run a script file.
pub fn run_script_file(path: &[u8]) -> i32 {
    match open_script(path) {
        Some(file) => {
            let status = run_script(&mut FdSource::new(file.raw()));
            drop(file);
            status
        }
        None => {
            shell_error_named(path, b"cannot open");
            exec::STATUS_CANNOT_EXECUTE
        }
    }
}

/// `. FILE` — run a file's commands in this shell, where `return` ends the
/// file rather than the shell.
pub fn source_file(path: &[u8]) -> i32 {
    match open_script(path) {
        Some(file) => {
            let status = run_script(&mut FdSource::new(file.raw()));
            drop(file);
            status
        }
        None => {
            shell_error_named(path, b"cannot open");
            1
        }
    }
}

fn open_script(path: &[u8]) -> Option<crate::syscall::OwnedFd> {
    let mut path_z = Vec::with_capacity(path.len() + 1);
    path_z.extend_from_slice(path);
    path_z.push(0);
    fs::open_path(path_z.as_ptr() as *const core::ffi::c_char, O_RDONLY).ok()
}

/// Read commands from `src` until end of input; the last command's status is
/// the script's own exit status.
pub fn run_script<S: ByteSource>(src: &mut S) -> i32 {
    let mut reader = ScriptReader::new();
    let mut line = vec![0u8; SCRIPT_LINE_MAX];
    let mut status = 0i32;
    let mut lineno = 0u32;

    let mut pending: Vec<u8> = Vec::new();
    let mut pending_start = 0u32;

    loop {
        lineno += 1;
        match reader.next_line(src, &mut line) {
            Line::Line(text) => {
                if pending.is_empty() {
                    pending_start = lineno;
                } else {
                    pending.push(b'\n');
                }
                pending.extend_from_slice(text);

                match exec::parse_text(&pending) {
                    // Unfinished, not wrong: keep reading.
                    Err(ParseFailure::Incomplete) => continue,
                    Err(ParseFailure::Syntax(msg)) => {
                        report_line_error(pending_start, msg.as_bytes());
                        pending.clear();
                        status = exec::STATUS_SYNTAX_ERROR;
                        super::set_last_exit_code(status);
                    }
                    Ok(list) => {
                        pending.clear();
                        if list.is_empty() {
                            continue;
                        }
                        let outcome = exec::execute_list_flow(&list);
                        status = outcome.status;
                        super::set_last_exit_code(status);
                        if let Some(requested) = super::exit_requested() {
                            return requested;
                        }
                        // `return` ends a sourced file, not just this line.
                        if outcome.flow == exec::Flow::Return {
                            return status;
                        }
                    }
                }
            }
            Line::Eof => {
                if !pending.is_empty() {
                    report_line_error(pending_start, b"unexpected end of input");
                    return exec::STATUS_SYNTAX_ERROR;
                }
                return status;
            }
            Line::TooLong => {
                report_line_error(lineno, b"line too long");
                pending.clear();
                status = 2;
                super::set_last_exit_code(status);
            }
            Line::Err => {
                shell_error(b"sh: error reading input\n");
                return status;
            }
        }
    }
}

/// `sh: line N: MSG` — the POSIX shape for a defect in the script itself
/// rather than in a command it ran.
fn report_line_error(lineno: u32, msg: &[u8]) {
    let mut name = [0u8; 24];
    let mut len = 0usize;
    for b in b"line " {
        name[len] = *b;
        len += 1;
    }
    len += write_u32_decimal(&mut name[len..], lineno);
    shell_error_named(&name[..len], msg);
}

fn write_u32_decimal(buf: &mut [u8], mut value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut n = 0usize;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        n += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let n = n.min(buf.len());
    for i in 0..n {
        buf[i] = digits[n - 1 - i];
    }
    n
}
