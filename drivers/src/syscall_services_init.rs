use slopos_lib::kernel_services::syscall_services::dns::{DnsServices, register_dns_services};
use slopos_lib::kernel_services::syscall_services::input::{
    InputServices, register_input_services,
};
use slopos_lib::kernel_services::syscall_services::net::{NetServices, register_net_services};
use slopos_lib::kernel_services::syscall_services::socket::{
    SocketServices, register_socket_services,
};
use slopos_lib::kernel_services::syscall_services::tty::{TtyServices, register_tty_services};

use crate::{
    input_event,
    net::{dns, socket},
    tty, virtio_net,
};

// =============================================================================
// Input services
// =============================================================================
//
// Most fields point directly at the driver implementation.  The three adapters
// below exist only because the driver returns a different type than the service
// interface requires.

/// Adapter: driver returns `u32`, service expects `usize`.
fn input_event_count_adapter(task_id: u32) -> usize {
    input_event::input_event_count(task_id) as usize
}

/// Adapter: driver returns `bool`, service expects `i32` (0 = ok, -1 = fail).
fn input_request_close_adapter(task_id: u32, timestamp_ms: u64) -> i32 {
    if input_event::input_request_close(task_id, timestamp_ms) {
        0
    } else {
        -1
    }
}

/// Adapter: driver returns `u8`, service expects `u32`.
fn input_get_button_state_adapter() -> u32 {
    input_event::input_get_button_state() as u32
}

static INPUT_SERVICES: InputServices = InputServices {
    poll: input_event::input_poll,
    drain_batch: input_event::input_drain_batch,
    event_count: input_event_count_adapter,
    set_keyboard_focus: input_event::input_set_keyboard_focus,
    set_pointer_focus: input_event::input_set_pointer_focus,
    set_pointer_focus_with_offset: input_event::input_set_pointer_focus_with_offset,
    request_close: input_request_close_adapter,
    get_pointer_focus: input_event::input_get_pointer_focus,
    get_pointer_position: input_event::input_get_pointer_position,
    get_button_state: input_get_button_state_adapter,
    clipboard_copy: input_event::clipboard_copy,
    clipboard_paste: input_event::clipboard_paste,
};

// =============================================================================
// TTY services — thin adapters converting u8 → TtyIndex for the per-TTY API.
// =============================================================================

fn tty_read_adapter(tty_index: u8, buf: *mut u8, max: usize, nonblock: bool) -> isize {
    tty::read(tty::TtyIndex(tty_index), buf, max, nonblock)
}

fn tty_has_cooked_data_adapter(tty_index: u8) -> bool {
    tty::has_data(tty::TtyIndex(tty_index))
}

fn tty_set_termios_adapter(tty_index: u8, t: *const slopos_abi::syscall::UserTermios) {
    tty::set_termios(tty::TtyIndex(tty_index), t)
}

fn tty_get_termios_adapter(tty_index: u8, t: *mut slopos_abi::syscall::UserTermios) {
    tty::get_termios(tty::TtyIndex(tty_index), t)
}

fn tty_get_winsize_adapter(tty_index: u8, ws: *mut slopos_abi::syscall::UserWinsize) {
    tty::get_winsize(tty::TtyIndex(tty_index), ws)
}

fn tty_set_winsize_adapter(tty_index: u8, ws: *const slopos_abi::syscall::UserWinsize) {
    tty::set_winsize(tty::TtyIndex(tty_index), ws)
}

fn tty_set_focus_adapter(target: u32) -> i32 {
    tty::set_focus(target)
}

fn tty_get_focus_adapter() -> u32 {
    tty::get_focus()
}

fn tty_set_foreground_pgrp_adapter(tty_index: u8, pgid: u32) -> i32 {
    tty::set_foreground_pgrp(tty::TtyIndex(tty_index), pgid);
    0
}

fn tty_get_foreground_pgrp_adapter(tty_index: u8) -> u32 {
    tty::get_foreground_pgrp(tty::TtyIndex(tty_index))
}

fn tty_get_session_id_adapter(tty_index: u8) -> u32 {
    tty::get_session_id(tty::TtyIndex(tty_index))
}

fn tty_set_foreground_pgrp_checked_adapter(tty_index: u8, pgid: u32, caller_sid: u32) -> i32 {
    tty::set_foreground_pgrp_checked(tty::TtyIndex(tty_index), pgid, caller_sid)
}

fn tty_detach_session_by_id_adapter(session_id: u32) {
    tty::detach_session_by_id(session_id)
}

fn tty_write_bytes_adapter(tty_index: u8, buf: *const u8, len: usize) -> usize {
    if buf.is_null() || len == 0 {
        return 0;
    }
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    tty::write(tty::TtyIndex(tty_index), data)
}

static TTY_SERVICES: TtyServices = TtyServices {
    read_cooked: tty_read_adapter,
    has_cooked_data: tty_has_cooked_data_adapter,
    set_termios: tty_set_termios_adapter,
    get_termios: tty_get_termios_adapter,
    get_winsize: tty_get_winsize_adapter,
    set_winsize: tty_set_winsize_adapter,
    set_focus: tty_set_focus_adapter,
    get_focus: tty_get_focus_adapter,
    set_foreground_pgrp: tty_set_foreground_pgrp_adapter,
    get_foreground_pgrp: tty_get_foreground_pgrp_adapter,
    get_session_id: tty_get_session_id_adapter,
    set_foreground_pgrp_checked: tty_set_foreground_pgrp_checked_adapter,
    detach_session_by_id: tty_detach_session_by_id_adapter,
    write_bytes: tty_write_bytes_adapter,
};

fn net_scan_members_adapter(
    out: *mut slopos_abi::net::UserNetMember,
    max: usize,
    active: u32,
) -> usize {
    virtio_net::virtio_net_scan_members(out, max, active != 0)
}

fn net_is_ready_adapter() -> u32 {
    if virtio_net::virtio_net_is_ready() {
        1
    } else {
        0
    }
}

fn net_get_info_adapter(out: *mut slopos_abi::net::UserNetInfo) -> u32 {
    if out.is_null() {
        return 0;
    }
    unsafe {
        // SAFETY: null is checked above and caller provides writable UserNetInfo storage.
        virtio_net::virtio_net_get_info(&mut *out);
    }
    1
}

static NET_SERVICES: NetServices = NetServices {
    scan_members: net_scan_members_adapter,
    is_ready: net_is_ready_adapter,
    get_info: net_get_info_adapter,
};

fn socket_send_adapter(sock_idx: u32, data: *const u8, len: usize) -> i64 {
    socket::socket_send(sock_idx, data, len)
}

fn socket_recv_adapter(sock_idx: u32, buf: *mut u8, len: usize) -> i64 {
    socket::socket_recv(sock_idx, buf, len)
}

fn socket_sendto_adapter(
    sock_idx: u32,
    data: *const u8,
    len: usize,
    dst_ip: [u8; 4],
    dst_port: u16,
) -> i64 {
    socket::socket_sendto(sock_idx, data, len, dst_ip, dst_port)
}

fn socket_recvfrom_adapter(
    sock_idx: u32,
    buf: *mut u8,
    len: usize,
    src_ip: *mut [u8; 4],
    src_port: *mut u16,
) -> i64 {
    socket::socket_recvfrom(sock_idx, buf, len, src_ip, src_port)
}

fn socket_setsockopt_adapter(
    sock_idx: u32,
    level: i32,
    optname: i32,
    val: *const u8,
    len: usize,
) -> i32 {
    if val.is_null() && len > 0 {
        return -14;
    }
    let slice = if len > 0 {
        unsafe { core::slice::from_raw_parts(val, len) }
    } else {
        &[]
    };
    socket::socket_setsockopt(sock_idx, level, optname, slice)
}

fn socket_getsockopt_adapter(
    sock_idx: u32,
    level: i32,
    optname: i32,
    out: *mut u8,
    len: usize,
) -> i32 {
    if out.is_null() && len > 0 {
        return -14;
    }
    let slice = if len > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, len) }
    } else {
        &mut []
    };
    socket::socket_getsockopt(sock_idx, level, optname, slice)
}

static SOCKET_SERVICES: SocketServices = SocketServices {
    create: socket::socket_create,
    bind: socket::socket_bind,
    listen: socket::socket_listen,
    accept: socket::socket_accept,
    connect: socket::socket_connect,
    send: socket_send_adapter,
    recv: socket_recv_adapter,
    sendto: socket_sendto_adapter,
    recvfrom: socket_recvfrom_adapter,
    close: socket::socket_close,
    poll_readable: socket::socket_poll_readable,
    poll_writable: socket::socket_poll_writable,
    set_nonblocking: socket::socket_set_nonblocking,
    setsockopt: socket_setsockopt_adapter,
    getsockopt: socket_getsockopt_adapter,
    shutdown: socket::socket_shutdown,
};

// =============================================================================
// DNS services
// =============================================================================

fn dns_resolve_adapter(hostname: *const u8, hostname_len: usize, result: *mut [u8; 4]) -> i32 {
    if hostname.is_null() || result.is_null() || hostname_len == 0 || hostname_len > 253 {
        return -22; // EINVAL
    }
    let hostname_slice = unsafe { core::slice::from_raw_parts(hostname, hostname_len) };
    match dns::dns_resolve(hostname_slice) {
        Some(addr) => {
            unsafe {
                *result = addr;
            }
            0
        }
        None => -113, // EHOSTUNREACH
    }
}

static DNS_SERVICES: DnsServices = DnsServices {
    resolve: dns_resolve_adapter,
};

pub fn init_syscall_services() {
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
    register_net_services(&NET_SERVICES);
    register_socket_services(&SOCKET_SERVICES);
    register_dns_services(&DNS_SERVICES);
}
