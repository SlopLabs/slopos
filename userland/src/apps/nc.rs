//! nc — SlopOS network Swiss army knife (Phase A: UDP)
//!
//! Exercises the full socket lifecycle: socket() → bind() → sendto()/recvfrom() → shutdown().
//! Phase A supports UDP client and listen modes with half-duplex I/O.

use core::ffi::c_void;

use crate::syscall::{
    SockAddrIn,
    core::{exit, exit_with_code, get_time_ms, sleep_ms},
    fs, net, tty,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum NcMode {
    Client,
    Listen,
}

#[derive(Clone, Copy, PartialEq)]
enum NcProtocol {
    Udp,
}

/// Parsed command-line configuration — built once, never mutated.
struct NcConfig {
    mode: NcMode,
    protocol: NcProtocol,
    remote_addr: [u8; 4],
    remote_port: u16,
    local_port: u16,
    verbose: bool,
    timeout_ms: u32,
}

#[derive(Clone, Copy)]
enum NcError {
    MissingHost,
    MissingPort,
    InvalidPort,
    ResolveFailed,
    UdpRequired,
    UnknownFlag,
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn write_out(buf: &[u8]) {
    if fs::write_slice(1, buf).is_err() {
        let _ = tty::write(buf);
    }
}

fn write_u8_dec(mut value: u8, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 3];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10);
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_u16_dec(mut value: u16, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 5];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_u32_dec(mut value: u32, out: &mut [u8], idx: &mut usize) {
    let mut tmp = [0u8; 10];
    let mut n = 0usize;
    loop {
        tmp[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    while n > 0 {
        n -= 1;
        if *idx < out.len() {
            out[*idx] = tmp[n];
            *idx += 1;
        }
    }
}

fn write_ipv4(ip: [u8; 4], out: &mut [u8], idx: &mut usize) {
    write_u8_dec(ip[0], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[1], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[2], out, idx);
    out[*idx] = b'.';
    *idx += 1;
    write_u8_dec(ip[3], out, idx);
}

fn append_bytes(buf: &mut [u8], idx: &mut usize, src: &[u8]) {
    let avail = buf.len() - *idx;
    let len = if src.len() < avail { src.len() } else { avail };
    buf[*idx..*idx + len].copy_from_slice(&src[..len]);
    *idx += len;
}

/// Print a verbose message: `nc: <msg>\n`.  Only emits output when verbose is on.
fn verbose_msg(config: &NcConfig, msg: &[u8]) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: ");
    append_bytes(&mut line, &mut i, msg);
    append_bytes(&mut line, &mut i, b"\n");
    write_out(&line[..i]);
}

/// Print verbose with IP:port: `nc: <prefix> <ip>:<port>\n`
fn verbose_addr(config: &NcConfig, prefix: &[u8], ip: [u8; 4], port: u16) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: ");
    append_bytes(&mut line, &mut i, prefix);
    write_ipv4(ip, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b":");
    write_u16_dec(port, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b"\n");
    write_out(&line[..i]);
}

/// Print verbose with byte count: `nc: <prefix> <count> bytes\n`
fn verbose_bytes(config: &NcConfig, prefix: &[u8], count: usize) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: ");
    append_bytes(&mut line, &mut i, prefix);
    write_u32_dec(count as u32, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b" bytes\n");
    write_out(&line[..i]);
}

/// Print verbose with byte count and source addr:
/// `nc: received <N> bytes from <ip>:<port>\n`
fn verbose_recv(config: &NcConfig, count: usize, ip: [u8; 4], port: u16) {
    if !config.verbose {
        return;
    }
    let mut line = [0u8; 256];
    let mut i = 0usize;
    append_bytes(&mut line, &mut i, b"nc: received ");
    write_u32_dec(count as u32, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b" bytes from ");
    write_ipv4(ip, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b":");
    write_u16_dec(port, &mut line, &mut i);
    append_bytes(&mut line, &mut i, b"\n");
    write_out(&line[..i]);
}

// ---------------------------------------------------------------------------
// Stdin reading
// ---------------------------------------------------------------------------

/// Read one line from stdin (fd 0), blocking byte-by-byte.
/// Returns the number of bytes read (excluding the newline).
/// Returns 0 on EOF.
fn read_line_from_stdin(buf: &mut [u8]) -> usize {
    let mut pos = 0usize;
    while pos < buf.len() {
        let mut byte = [0u8; 1];
        match fs::read_slice(0, &mut byte) {
            Ok(0) => return pos, // EOF
            Ok(_) => {
                if byte[0] == b'\n' {
                    return pos;
                }
                buf[pos] = byte[0];
                pos += 1;
            }
            Err(_) => return pos,
        }
    }
    pos
}

/// Check for Ctrl+C (0x03) via non-blocking TTY read.
/// Returns true if the user pressed Ctrl+C.
fn check_interrupt() -> bool {
    let ch = tty::try_read_char();
    ch == 0x03
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

fn print_usage() {
    write_out(b"usage: nc [-ulv] [-p port] [-w timeout] [host] port\n");
    write_out(b"\n");
    write_out(b"  -u        UDP mode (required in current release)\n");
    write_out(b"  -l        Listen mode (bind and receive)\n");
    write_out(b"  -v        Verbose output\n");
    write_out(b"  -p port   Source port (client mode)\n");
    write_out(b"  -w secs   Timeout in seconds\n");
    write_out(b"  host      Remote hostname or IP (client mode)\n");
    write_out(b"  port      Remote port (client) or listen port (listen mode)\n");
}

fn print_error(err: NcError) {
    let msg = match err {
        NcError::MissingHost => b"nc: missing host\n" as &[u8],
        NcError::MissingPort => b"nc: missing port\n" as &[u8],
        NcError::InvalidPort => b"nc: invalid port number\n" as &[u8],
        NcError::ResolveFailed => b"nc: cannot resolve hostname\n" as &[u8],
        NcError::UdpRequired => b"nc: -u flag required (only UDP supported)\n" as &[u8],
        NcError::UnknownFlag => b"nc: unknown flag\n" as &[u8],
    };
    write_out(msg);
}

/// Parse a port number from a byte slice.  Returns `None` on invalid input.
fn parse_port(s: &[u8]) -> Option<u16> {
    if s.is_empty() {
        return None;
    }
    let mut val: u32 = 0;
    for &b in s {
        if b < b'0' || b > b'9' {
            return None;
        }
        val = val * 10 + (b - b'0') as u32;
        if val > 65535 {
            return None;
        }
    }
    if val == 0 {
        return None;
    }
    Some(val as u16)
}

/// Parse a dotted-quad IPv4 address (e.g. "10.0.2.2").
fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0usize;
    let mut current: u16 = 0;
    let mut has_digit = false;

    for &b in s {
        if b == b'.' {
            if !has_digit || octet_idx >= 3 {
                return None;
            }
            if current > 255 {
                return None;
            }
            octets[octet_idx] = current as u8;
            octet_idx += 1;
            current = 0;
            has_digit = false;
        } else if b >= b'0' && b <= b'9' {
            current = current * 10 + (b - b'0') as u16;
            has_digit = true;
        } else {
            return None;
        }
    }

    if !has_digit || octet_idx != 3 || current > 255 {
        return None;
    }
    octets[3] = current as u8;
    Some(octets)
}

/// Resolve a host argument: try dotted-quad first, then kernel DNS.
fn resolve_host(host: &[u8]) -> Result<[u8; 4], NcError> {
    if let Some(ip) = parse_ipv4(host) {
        return Ok(ip);
    }
    // Try kernel DNS resolution
    match net::resolve(host) {
        Some(ip) => Ok(ip),
        None => Err(NcError::ResolveFailed),
    }
}

/// Compare two byte slices for equality.
fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Parse argv into an NcConfig.
///
/// Walks raw C-style argv pointers from the kernel.  The first element (argv[0])
/// is the program name and is skipped.
fn parse_args(argc: usize, argv: *const *const u8) -> Result<NcConfig, NcError> {
    let mut udp = false;
    let mut listen = false;
    let mut verbose = false;
    let mut local_port: u16 = 0;
    let mut timeout_secs: u32 = 0;
    let mut positional: [&[u8]; 2] = [&[], &[]];
    let mut pos_count = 0usize;

    let mut i = 1usize; // skip argv[0]
    while i < argc {
        let arg = unsafe {
            let ptr = *argv.add(i);
            if ptr.is_null() {
                i += 1;
                continue;
            }
            let len = crate::runtime::u_strlen(ptr);
            core::slice::from_raw_parts(ptr, len)
        };

        if arg.is_empty() {
            i += 1;
            continue;
        }

        if arg[0] == b'-' {
            // Flag processing — may contain bundled flags like -ulv
            if bytes_eq(arg, b"-h") || bytes_eq(arg, b"--help") {
                print_usage();
                exit();
            }

            if bytes_eq(arg, b"-p") {
                // Next arg is port number
                i += 1;
                if i >= argc {
                    return Err(NcError::MissingPort);
                }
                let next = unsafe {
                    let ptr = *argv.add(i);
                    let len = crate::runtime::u_strlen(ptr);
                    core::slice::from_raw_parts(ptr, len)
                };
                local_port = parse_port(next).ok_or(NcError::InvalidPort)?;
                i += 1;
                continue;
            }

            if bytes_eq(arg, b"-w") {
                // Next arg is timeout in seconds
                i += 1;
                if i >= argc {
                    return Err(NcError::InvalidPort); // reuse error for missing value
                }
                let next = unsafe {
                    let ptr = *argv.add(i);
                    let len = crate::runtime::u_strlen(ptr);
                    core::slice::from_raw_parts(ptr, len)
                };
                // Parse timeout as u32
                let mut t: u32 = 0;
                for &b in next {
                    if b < b'0' || b > b'9' {
                        return Err(NcError::InvalidPort);
                    }
                    t = t * 10 + (b - b'0') as u32;
                }
                timeout_secs = t;
                i += 1;
                continue;
            }

            // Process bundled flags: -ulv
            let mut j = 1usize;
            while j < arg.len() {
                match arg[j] {
                    b'u' => udp = true,
                    b'l' => listen = true,
                    b'v' => verbose = true,
                    _ => return Err(NcError::UnknownFlag),
                }
                j += 1;
            }
        } else {
            // Positional argument
            if pos_count < 2 {
                positional[pos_count] = arg;
                pos_count += 1;
            }
        }

        i += 1;
    }

    // Phase A: UDP is required
    if !udp {
        return Err(NcError::UdpRequired);
    }

    let mode = if listen {
        NcMode::Listen
    } else {
        NcMode::Client
    };

    match mode {
        NcMode::Listen => {
            // Listen mode: expect exactly one positional arg (port)
            if pos_count == 0 {
                return Err(NcError::MissingPort);
            }
            let port = parse_port(positional[0]).ok_or(NcError::InvalidPort)?;
            Ok(NcConfig {
                mode,
                protocol: NcProtocol::Udp,
                remote_addr: [0; 4],
                remote_port: 0,
                local_port: port,
                verbose,
                timeout_ms: timeout_secs * 1000,
            })
        }
        NcMode::Client => {
            // Client mode: expect host + port
            if pos_count < 1 {
                return Err(NcError::MissingHost);
            }
            if pos_count < 2 {
                return Err(NcError::MissingPort);
            }
            let addr = resolve_host(positional[0])?;
            let port = parse_port(positional[1]).ok_or(NcError::InvalidPort)?;
            Ok(NcConfig {
                mode,
                protocol: NcProtocol::Udp,
                remote_addr: addr,
                remote_port: port,
                local_port,
                verbose,
                timeout_ms: timeout_secs * 1000,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// UDP Client
// ---------------------------------------------------------------------------

fn udp_client(config: &NcConfig) {
    // Create UDP socket
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_DGRAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            write_out(b"nc: socket creation failed\n");
            exit_with_code(1);
        }
    };

    // Bind to local port if specified
    if config.local_port != 0 {
        if let Err(_) = net::bind_any(fd, config.local_port) {
            write_out(b"nc: bind failed (port in use?)\n");
            exit_with_code(1);
        }
    }

    // Set non-blocking for receive polling
    if let Err(_) = net::set_nonblocking(fd) {
        write_out(b"nc: failed to set non-blocking\n");
        exit_with_code(1);
    }

    verbose_addr(
        config,
        b"connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, b"protocol: udp");

    // Build destination address
    let dest = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: config.remote_port.to_be(),
        addr: config.remote_addr,
        _pad: [0; 8],
    };

    let mut line_buf = [0u8; 1024];
    let mut recv_buf = [0u8; 2048];

    // Main I/O loop (half-duplex: send → poll receive → repeat)
    loop {
        // Check for Ctrl+C
        if check_interrupt() {
            verbose_msg(config, b"interrupted");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            exit_with_code(0);
        }

        // Send phase: read one line from stdin
        let n = read_line_from_stdin(&mut line_buf);
        if n == 0 {
            // EOF on stdin — done
            verbose_msg(config, b"EOF on stdin");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            exit_with_code(0);
        }

        // Send the line as a UDP datagram
        match net::sendto(fd, &line_buf[..n], 0, &dest) {
            Ok(sent) => {
                verbose_bytes(config, b"sent ", sent);
            }
            Err(_) => {
                write_out(b"nc: send failed\n");
                // Don't exit on transient send failure — continue
            }
        }

        // Receive phase: poll for a response
        let timeout = if config.timeout_ms > 0 {
            config.timeout_ms
        } else {
            500 // default 500ms receive window
        };
        let start = get_time_ms();
        loop {
            if check_interrupt() {
                verbose_msg(config, b"interrupted");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                exit_with_code(0);
            }

            let mut src_addr = SockAddrIn::default();
            match net::recvfrom(fd, &mut recv_buf, 0, Some(&mut src_addr)) {
                Ok(received) if received > 0 => {
                    // Write received data to stdout
                    write_out(&recv_buf[..received]);
                    // Add newline if data doesn't end with one
                    if recv_buf[received - 1] != b'\n' {
                        write_out(b"\n");
                    }
                    verbose_recv(config, received, src_addr.addr, u16::from_be(src_addr.port));
                    break;
                }
                _ => {
                    // WouldBlock or error — keep polling
                    let elapsed = get_time_ms() - start;
                    if elapsed >= timeout as u64 {
                        if config.timeout_ms > 0 {
                            verbose_msg(config, b"receive timeout");
                        }
                        break;
                    }
                    sleep_ms(10);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// UDP Listen
// ---------------------------------------------------------------------------

fn udp_listen(config: &NcConfig) {
    // Create UDP socket
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_DGRAM, 0) {
        Ok(fd) => fd,
        Err(_) => {
            write_out(b"nc: socket creation failed\n");
            exit_with_code(1);
        }
    };

    // Set reuse addr for quick rebind during development
    let _ = net::set_reuse_addr(fd);

    // Bind to the listen port on all interfaces
    if let Err(_) = net::bind_any(fd, config.local_port) {
        write_out(b"nc: bind failed (port in use?)\n");
        exit_with_code(1);
    }

    // Set non-blocking for polling
    if let Err(_) = net::set_nonblocking(fd) {
        write_out(b"nc: failed to set non-blocking\n");
        exit_with_code(1);
    }

    // Print listen message
    // Print a cleaner listen message
    if config.verbose {
        let mut line = [0u8; 128];
        let mut i = 0usize;
        append_bytes(&mut line, &mut i, b"nc: listening on 0.0.0.0:");
        write_u16_dec(config.local_port, &mut line, &mut i);
        append_bytes(&mut line, &mut i, b" (udp)\n");
        write_out(&line[..i]);
    }

    let mut recv_buf = [0u8; 2048];
    let mut line_buf = [0u8; 1024];
    let last_activity = get_time_ms();
    let mut last_activity_ms = last_activity;

    loop {
        // Check for Ctrl+C
        if check_interrupt() {
            verbose_msg(config, b"interrupted");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            exit_with_code(0);
        }

        // Timeout check
        if config.timeout_ms > 0 {
            let elapsed = get_time_ms() - last_activity_ms;
            if elapsed >= config.timeout_ms as u64 {
                write_out(b"nc: timeout\n");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                exit_with_code(1);
            }
        }

        let mut src_addr = SockAddrIn::default();
        match net::recvfrom(fd, &mut recv_buf, 0, Some(&mut src_addr)) {
            Ok(received) if received > 0 => {
                last_activity_ms = get_time_ms();

                // Write received data to stdout
                write_out(&recv_buf[..received]);
                // Add newline if data doesn't end with one
                if recv_buf[received - 1] != b'\n' {
                    write_out(b"\n");
                }

                verbose_recv(config, received, src_addr.addr, u16::from_be(src_addr.port));

                // Reply: read one line from stdin and send back to sender
                // Use non-blocking check: if stdin has data, send reply
                // For now, attempt a non-blocking-ish approach: try reading
                // with a very small effort. In half-duplex mode this blocks
                // until user types a line — that's acceptable for Phase A.
                let reply_n = read_line_from_stdin(&mut line_buf);
                if reply_n > 0 {
                    let reply_addr = SockAddrIn {
                        family: slopos_abi::net::AF_INET,
                        port: src_addr.port, // already in network byte order
                        addr: src_addr.addr,
                        _pad: [0; 8],
                    };
                    match net::sendto(fd, &line_buf[..reply_n], 0, &reply_addr) {
                        Ok(sent) => {
                            verbose_bytes(config, b"sent ", sent);
                        }
                        Err(_) => {
                            write_out(b"nc: send failed\n");
                        }
                    }
                } else {
                    // stdin EOF — receive-only mode from now on
                    verbose_msg(config, b"stdin EOF, receive-only mode");
                }
            }
            _ => {
                // WouldBlock — sleep briefly to avoid busy-wait
                sleep_ms(10);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Entry point when launched with argc/argv extracted from the user stack.
/// This is the primary entry for fork+execve launches from the shell.
pub fn nc_main_args(argc: usize, argv: *const *const u8) -> ! {
    if argc <= 1 || argv.is_null() {
        print_usage();
        exit_with_code(1);
    }

    let config = match parse_args(argc, argv) {
        Ok(c) => c,
        Err(e) => {
            print_error(e);
            print_usage();
            exit_with_code(1);
        }
    };

    match (config.protocol, config.mode) {
        (NcProtocol::Udp, NcMode::Client) => udp_client(&config),
        (NcProtocol::Udp, NcMode::Listen) => udp_listen(&config),
    }

    exit();
}

/// Legacy entry point for the standard entry! macro (no args).
/// Prints usage and exits since nc requires arguments.
pub fn nc_main(_arg: *mut c_void) -> ! {
    print_usage();
    exit_with_code(1);
}

// ---------------------------------------------------------------------------
// Tests (argument parsing & helpers — no kernel needed)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_port_valid() {
        assert_eq!(parse_port(b"80"), Some(80));
        assert_eq!(parse_port(b"443"), Some(443));
        assert_eq!(parse_port(b"65535"), Some(65535));
        assert_eq!(parse_port(b"1"), Some(1));
        assert_eq!(parse_port(b"12345"), Some(12345));
    }

    #[test]
    fn test_parse_port_invalid() {
        assert_eq!(parse_port(b""), None);
        assert_eq!(parse_port(b"0"), None);
        assert_eq!(parse_port(b"65536"), None);
        assert_eq!(parse_port(b"abc"), None);
        assert_eq!(parse_port(b"12a"), None);
        assert_eq!(parse_port(b"99999"), None);
    }

    #[test]
    fn test_parse_ipv4_valid() {
        assert_eq!(parse_ipv4(b"10.0.2.2"), Some([10, 0, 2, 2]));
        assert_eq!(parse_ipv4(b"192.168.1.1"), Some([192, 168, 1, 1]));
        assert_eq!(parse_ipv4(b"0.0.0.0"), Some([0, 0, 0, 0]));
        assert_eq!(parse_ipv4(b"255.255.255.255"), Some([255, 255, 255, 255]));
        assert_eq!(parse_ipv4(b"127.0.0.1"), Some([127, 0, 0, 1]));
    }

    #[test]
    fn test_parse_ipv4_invalid() {
        assert_eq!(parse_ipv4(b""), None);
        assert_eq!(parse_ipv4(b"10.0.2"), None);
        assert_eq!(parse_ipv4(b"10.0.2.2.1"), None);
        assert_eq!(parse_ipv4(b"256.0.0.1"), None);
        assert_eq!(parse_ipv4(b"10.0.2.abc"), None);
        assert_eq!(parse_ipv4(b"..."), None);
        assert_eq!(parse_ipv4(b"1.2.3."), None);
        assert_eq!(parse_ipv4(b".1.2.3"), None);
    }

    #[test]
    fn test_write_u8_dec() {
        let mut buf = [0u8; 8];
        let mut idx = 0;
        write_u8_dec(0, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"0");

        idx = 0;
        write_u8_dec(255, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"255");

        idx = 0;
        write_u8_dec(42, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"42");
    }

    #[test]
    fn test_write_u16_dec() {
        let mut buf = [0u8; 8];
        let mut idx = 0;
        write_u16_dec(0, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"0");

        idx = 0;
        write_u16_dec(65535, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"65535");

        idx = 0;
        write_u16_dec(8080, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"8080");
    }

    #[test]
    fn test_write_u32_dec() {
        let mut buf = [0u8; 16];
        let mut idx = 0;
        write_u32_dec(0, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"0");

        idx = 0;
        write_u32_dec(1000, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"1000");

        idx = 0;
        write_u32_dec(4294967295, &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"4294967295");
    }

    #[test]
    fn test_write_ipv4() {
        let mut buf = [0u8; 20];
        let mut idx = 0;
        write_ipv4([10, 0, 2, 2], &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"10.0.2.2");

        idx = 0;
        write_ipv4([192, 168, 1, 1], &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"192.168.1.1");

        idx = 0;
        write_ipv4([255, 255, 255, 255], &mut buf, &mut idx);
        assert_eq!(&buf[..idx], b"255.255.255.255");
    }

    #[test]
    fn test_bytes_eq() {
        assert!(bytes_eq(b"hello", b"hello"));
        assert!(!bytes_eq(b"hello", b"world"));
        assert!(!bytes_eq(b"hi", b"hello"));
        assert!(bytes_eq(b"", b""));
    }

    #[test]
    fn test_append_bytes() {
        let mut buf = [0u8; 16];
        let mut idx = 0;
        append_bytes(&mut buf, &mut idx, b"hello");
        assert_eq!(&buf[..idx], b"hello");
        append_bytes(&mut buf, &mut idx, b" world");
        assert_eq!(&buf[..idx], b"hello world");
    }

    #[test]
    fn test_append_bytes_overflow() {
        let mut buf = [0u8; 5];
        let mut idx = 0;
        append_bytes(&mut buf, &mut idx, b"hello world");
        assert_eq!(idx, 5);
        assert_eq!(&buf[..], b"hello");
    }
}
