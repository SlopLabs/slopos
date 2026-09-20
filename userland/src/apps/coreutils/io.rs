//! Output sinks and the per-invocation context every utility runs against.
//!
//! A utility writes into a [`Sink`] the caller aims, which is what lets one
//! implementation serve `/bin/ls` and the shell's redirected builtin alike.

use crate::syscall::fs;

/// Bytes buffered before a `write(2)`. One page: a listing of a large
/// directory costs one syscall per page rather than one per name.
const SINK_CAPACITY: usize = 4096;

pub struct Sink {
    fd: i32,
    buf: Vec<u8>,
    tty: bool,
    broken: bool,
}

impl Sink {
    /// A sink on `fd`, probing whether it is a terminal so colour and column
    /// layout follow the destination rather than a flag.
    pub fn new(fd: i32) -> Self {
        let tty = fs::isatty(fd);
        Self::raw(fd, tty)
    }

    pub fn raw(fd: i32, tty: bool) -> Self {
        Self {
            fd,
            buf: Vec::with_capacity(SINK_CAPACITY),
            tty,
            broken: false,
        }
    }

    pub fn fd(&self) -> i32 {
        self.fd
    }

    pub fn is_tty(&self) -> bool {
        self.tty
    }

    /// The destination stopped accepting bytes — a closed pipe, most often.
    /// A tool producing unbounded output polls this and stops.
    pub fn broken(&self) -> bool {
        self.broken
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.broken {
            return;
        }
        if self.buf.len() + bytes.len() > SINK_CAPACITY {
            self.flush();
            if bytes.len() >= SINK_CAPACITY {
                self.write_through(bytes);
                return;
            }
        }
        self.buf.extend_from_slice(bytes);
    }

    pub fn s(&mut self, text: &str) {
        self.write(text.as_bytes());
    }

    pub fn b(&mut self, byte: u8) {
        self.write(&[byte]);
    }

    pub fn nl(&mut self) {
        self.b(b'\n');
    }

    pub fn u(&mut self, value: u64) {
        let mut digits = [0u8; 20];
        let mut pos = digits.len();
        let mut rest = value;
        loop {
            pos -= 1;
            digits[pos] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.write(&digits[pos..]);
    }

    pub fn i(&mut self, value: i64) {
        if value < 0 {
            self.b(b'-');
            self.u(value.unsigned_abs());
        } else {
            self.u(value as u64);
        }
    }

    /// `value` right-aligned in `width` columns, as `wc` and `ls -l` need.
    pub fn u_right(&mut self, value: u64, width: usize) {
        let digits = decimal_width(value);
        for _ in digits..width {
            self.b(b' ');
        }
        self.u(value);
    }

    /// An SGR run, emitted only when the destination is a terminal, so a
    /// redirected or piped listing stays plain bytes.
    pub fn sgr(&mut self, params: &str) {
        if !self.tty {
            return;
        }
        self.s("\x1b[");
        self.s(params);
        self.b(b'm');
    }

    pub fn flush(&mut self) -> bool {
        if self.buf.is_empty() {
            return !self.broken;
        }
        let taken = core::mem::take(&mut self.buf);
        self.write_through(&taken);
        self.buf = taken;
        self.buf.clear();
        !self.broken
    }

    /// A single `write(2)` may transfer less than it was given; treating that
    /// count as success drops the tail.
    fn write_through(&mut self, bytes: &[u8]) {
        let mut sent = 0usize;
        while sent < bytes.len() {
            match fs::write_slice(self.fd, &bytes[sent..]) {
                Ok(0) => {
                    self.broken = true;
                    return;
                }
                Ok(n) => sent += n,
                Err(_) => {
                    self.broken = true;
                    return;
                }
            }
        }
    }
}

pub fn decimal_width(value: u64) -> usize {
    let mut width = 1;
    let mut rest = value / 10;
    while rest > 0 {
        width += 1;
        rest /= 10;
    }
    width
}

/// One invocation: where output goes, where diagnostics go, and which name the
/// caller typed — a diagnostic says `grep:` because argv[0] said `grep`, not
/// because the binary is called that.
pub struct Ctx {
    pub out: Sink,
    pub err: Sink,
    tool: &'static str,
}

impl Ctx {
    /// The shape a spawned `/bin/<tool>` runs with.
    pub fn stdio() -> Self {
        Self {
            out: Sink::new(1),
            err: Sink::new(2),
            tool: "",
        }
    }

    /// The shape an in-process caller runs with: output goes where it says,
    /// diagnostics still go to fd 2 — a builtin's `>` redirects its output,
    /// not its complaints.
    pub fn with_out(out_fd: i32, out_tty: bool) -> Self {
        Self {
            out: Sink::raw(out_fd, out_tty),
            err: Sink::new(2),
            tool: "",
        }
    }

    pub fn tool(&self) -> &'static str {
        self.tool
    }

    pub fn set_tool(&mut self, name: &'static str) {
        self.tool = name;
    }

    /// `tool: message`, with the newline.
    pub fn warn(&mut self, message: &[u8]) {
        self.err.s(self.tool);
        self.err.s(": ");
        self.err.write(message);
        self.err.nl();
        self.err.flush();
    }

    /// `tool: operand: message` — the POSIX diagnostic shape.
    pub fn warn_at(&mut self, operand: &[u8], message: &[u8]) {
        self.err.s(self.tool);
        self.err.s(": ");
        self.err.write(operand);
        self.err.s(": ");
        self.err.write(message);
        self.err.nl();
        self.err.flush();
    }

    pub fn warn_io(&mut self, operand: &[u8], error: &std::io::Error) {
        self.warn_at(operand, io_message(error).as_bytes());
    }

    /// `usage: ...` on fd 2 and status 2, which POSIX reserves for a usage
    /// error so a caller can tell it from the tool's own failure status.
    pub fn usage(&mut self, usage: &str) -> i32 {
        self.err.s("usage: ");
        self.err.s(usage);
        self.err.nl();
        self.err.flush();
        2
    }
}

/// The POSIX diagnostic text for an `io::Error`, as a `&'static str`.
///
/// `io::Error`'s `Display` reaches `strerror_r` but appends
/// `" (os error {code})"`, and only into a `Formatter`.
pub fn io_message(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound => "No such file or directory",
        PermissionDenied => "Permission denied",
        AlreadyExists => "File exists",
        InvalidInput | InvalidData => "Invalid argument",
        IsADirectory => "Is a directory",
        NotADirectory => "Not a directory",
        DirectoryNotEmpty => "Directory not empty",
        ReadOnlyFilesystem => "Read-only file system",
        BrokenPipe => "Broken pipe",
        StorageFull => "No space left on device",
        FileTooLarge => "File too large",
        WouldBlock => "Resource temporarily unavailable",
        CrossesDevices => "Invalid cross-device link",
        Unsupported => "Operation not supported",
        _ => "I/O error",
    }
}
