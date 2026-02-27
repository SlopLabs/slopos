crate::define_service! {
    socket => SocketServices {
        create(domain: u16, sock_type: u16, protocol: u16) -> i32;
        bind(sock_idx: u32, addr: [u8; 4], port: u16) -> i32;
        listen(sock_idx: u32, backlog: u32) -> i32;
        accept(sock_idx: u32, peer_addr: *mut [u8; 4], peer_port: *mut u16) -> i32;
        connect(sock_idx: u32, addr: [u8; 4], port: u16) -> i32;
        send(sock_idx: u32, data: *const u8, len: usize) -> i64;
        recv(sock_idx: u32, buf: *mut u8, len: usize) -> i64;
        close(sock_idx: u32) -> i32;
        poll_readable(sock_idx: u32) -> u32;
        poll_writable(sock_idx: u32) -> u32;
    }
}
