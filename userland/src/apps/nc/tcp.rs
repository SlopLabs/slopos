//! TCP client and listen modes for nc.

use crate::syscall::{
    SockAddrIn,
    core::{exit_with_code, get_time_ms, sleep_ms},
    net,
};

use super::{
    NcConfig, check_interrupt, read_line_from_stdin, verbose_addr, verbose_bytes, verbose_msg,
    write_out,
};

// ---------------------------------------------------------------------------
// TCP Client
// ---------------------------------------------------------------------------

pub(super) fn tcp_client(config: &NcConfig) {
    // Create TCP socket
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
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

    // Connect to remote host
    let dest = SockAddrIn {
        family: slopos_abi::net::AF_INET,
        port: config.remote_port.to_be(),
        addr: config.remote_addr,
        _pad: [0; 8],
    };

    verbose_addr(
        config,
        b"connecting to ",
        config.remote_addr,
        config.remote_port,
    );

    if let Err(_) = net::connect(fd, &dest) {
        write_out(b"nc: connect failed\n");
        exit_with_code(1);
    }

    verbose_addr(
        config,
        b"connected to ",
        config.remote_addr,
        config.remote_port,
    );
    verbose_msg(config, b"protocol: tcp");

    // Set non-blocking for receive polling
    if let Err(_) = net::set_nonblocking(fd) {
        write_out(b"nc: failed to set non-blocking\n");
        exit_with_code(1);
    }

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

        // Send via TCP stream
        match net::send(fd, &line_buf[..n], 0) {
            Ok(sent) => {
                verbose_bytes(config, b"sent ", sent);
            }
            Err(_) => {
                write_out(b"nc: send failed (broken pipe)\n");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                exit_with_code(1);
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

            match net::recv(fd, &mut recv_buf, 0) {
                Ok(0) => {
                    // Connection closed by remote
                    verbose_msg(config, b"connection closed by remote");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                    exit_with_code(0);
                }
                Ok(received) => {
                    // Write received data to stdout
                    write_out(&recv_buf[..received]);
                    // Add newline if data doesn't end with one
                    if recv_buf[received - 1] != b'\n' {
                        write_out(b"\n");
                    }
                    verbose_bytes(config, b"received ", received);
                    break;
                }
                Err(_) => {
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
// TCP Listen
// ---------------------------------------------------------------------------

pub(super) fn tcp_listen(config: &NcConfig) {
    // Create TCP socket
    let fd = match net::socket(slopos_abi::net::AF_INET, slopos_abi::net::SOCK_STREAM, 0) {
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

    // Start listening with a backlog of 1
    if let Err(_) = net::listen(fd, 1) {
        write_out(b"nc: listen failed\n");
        exit_with_code(1);
    }

    // Set listening socket non-blocking so we can poll for connections
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
        super::append_bytes(&mut line, &mut i, b" (tcp)\n");
        write_out(&line[..i]);
    }

    let accept_start = get_time_ms();

    // Outer loop: accept connections (runs once unless -k is set)
    loop {
        // Poll for incoming connection (non-blocking)
        let mut peer = SockAddrIn::default();
        let client_fd = loop {
            if check_interrupt() {
                verbose_msg(config, b"interrupted");
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                exit_with_code(0);
            }

            // Timeout check while waiting for connection
            if config.timeout_ms > 0 {
                let elapsed = get_time_ms() - accept_start;
                if elapsed >= config.timeout_ms as u64 {
                    write_out(b"nc: timeout waiting for connection\n");
                    let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                    exit_with_code(1);
                }
            }

            match net::accept(fd, Some(&mut peer)) {
                Ok(cfd) => break cfd,
                Err(_) => {
                    // No connection pending — sleep briefly
                    sleep_ms(10);
                }
            }
        };

        verbose_addr(
            config,
            b"connection from ",
            peer.addr,
            u16::from_be(peer.port),
        );

        // Set accepted socket non-blocking for I/O polling
        if let Err(_) = net::set_nonblocking(client_fd) {
            write_out(b"nc: failed to set non-blocking on client socket\n");
            let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
            if !config.keep_listen {
                exit_with_code(1);
            }
            continue;
        }

        // I/O loop on the accepted connection
        let mut recv_buf = [0u8; 2048];
        let mut line_buf = [0u8; 1024];
        let mut last_activity_ms = get_time_ms();

        loop {
            // Check for Ctrl+C
            if check_interrupt() {
                verbose_msg(config, b"interrupted");
                let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
                exit_with_code(0);
            }

            // Timeout check during I/O
            if config.timeout_ms > 0 {
                let elapsed = get_time_ms() - last_activity_ms;
                if elapsed >= config.timeout_ms as u64 {
                    write_out(b"nc: timeout\n");
                    let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                    break;
                }
            }

            match net::recv(client_fd, &mut recv_buf, 0) {
                Ok(0) => {
                    // Connection closed by remote peer
                    verbose_msg(config, b"connection closed by remote");
                    let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                    break;
                }
                Ok(received) => {
                    last_activity_ms = get_time_ms();

                    // Write received data to stdout
                    write_out(&recv_buf[..received]);
                    // Add newline if data doesn't end with one
                    if recv_buf[received - 1] != b'\n' {
                        write_out(b"\n");
                    }

                    verbose_bytes(config, b"received ", received);

                    // Reply: read one line from stdin and send back
                    let reply_n = read_line_from_stdin(&mut line_buf);
                    if reply_n > 0 {
                        match net::send(client_fd, &line_buf[..reply_n], 0) {
                            Ok(sent) => {
                                verbose_bytes(config, b"sent ", sent);
                            }
                            Err(_) => {
                                write_out(b"nc: send failed (broken pipe)\n");
                                let _ = net::shutdown(client_fd, slopos_abi::syscall::SHUT_RDWR);
                                break;
                            }
                        }
                    } else {
                        // stdin EOF — receive-only mode
                        verbose_msg(config, b"stdin EOF, receive-only mode");
                    }
                }
                Err(_) => {
                    // WouldBlock — sleep briefly to avoid busy-wait
                    sleep_ms(10);
                }
            }
        }

        // After client disconnects: exit unless -k (keep-listening) is set
        if !config.keep_listen {
            verbose_msg(config, b"exiting (single connection mode)");
            let _ = net::shutdown(fd, slopos_abi::syscall::SHUT_RDWR);
            exit_with_code(0);
        }

        verbose_msg(config, b"waiting for next connection");
    }
}
