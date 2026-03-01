//! UDP client and listen modes for nc.

use crate::syscall::{
    SockAddrIn,
    core::{exit_with_code, get_time_ms, sleep_ms},
    net,
};

use super::{
    NcConfig, check_interrupt, read_line_from_stdin, verbose_addr, verbose_bytes, verbose_msg,
    verbose_recv, write_out,
};

// ---------------------------------------------------------------------------
// UDP Client
// ---------------------------------------------------------------------------

pub(super) fn udp_client(config: &NcConfig) {
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

pub(super) fn udp_listen(config: &NcConfig) {
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
    if config.verbose {
        let mut line = [0u8; 128];
        let mut i = 0usize;
        super::append_bytes(&mut line, &mut i, b"nc: listening on 0.0.0.0:");
        super::write_u16_dec(config.local_port, &mut line, &mut i);
        super::append_bytes(&mut line, &mut i, b" (udp)\n");
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
