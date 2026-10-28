use slopos_abi::syscall::TtyIndex;
use slopos_kernel_services::syscall_services::input::{InputServices, register_input_services};
use slopos_kernel_services::syscall_services::keymap::{KeymapServices, register_keymap_services};
use slopos_kernel_services::syscall_services::tty::{TtyServices, register_tty_services};
use slopos_ostd::KArc;
use slopos_ostd::process::quota::FileBacking;

use crate::{input_event, keyboard, tty};

static INPUT_SERVICES: InputServices = InputServices {
    drain_batch: input_event::input_drain_batch,
    clipboard_copy: input_event::clipboard_copy,
    clipboard_paste: input_event::clipboard_paste,
    register_compositor: input_event::input_register_compositor,
    switch_compositor: input_event::input_switch_compositor,
    cleanup_task: input_event::input_cleanup_task,
    route_key: route_key_adapter,
};

fn route_key_adapter(keycode: u16, codepoint: u32, pressed: bool) {
    input_event::input_route_key(
        keycode,
        codepoint,
        0,
        0,
        pressed,
        input_event::get_timestamp_ms(),
    );
}

fn tty_read_adapter(
    tty_index: TtyIndex,
    buf: *mut u8,
    max: usize,
    nonblock: bool,
) -> Result<usize, tty::TtyError> {
    if buf.is_null() || max == 0 {
        return Ok(0);
    }
    slopos_ostd::util::ptr_buf::with_buf_mut(buf, max, |slice| {
        tty::read(tty_index, slice, nonblock)
    })
}

fn tty_release_controlling_terminal_adapter(
    tty_index: TtyIndex,
    session_id: u32,
) -> Result<bool, tty::TtyError> {
    tty::release_controlling_terminal(tty_index, session_id)
}

fn tty_grantpt_adapter(tty_index: TtyIndex) -> Result<(), tty::TtyError> {
    tty::set_pty_lock(tty_index, false)
}

// Erases the concrete master `KArc<TtyBacking>` to `KArc<dyn FileBacking>` so the
// file layer can own it without naming the driver type.

fn tty_alloc_pty_adapter(
    account: slopos_ostd::process::AccountId,
    owner: (u32, u32),
) -> Result<(TtyIndex, KArc<dyn FileBacking>), tty::TtyError> {
    tty::pty_alloc(account, owner).map(|(idx, b)| -> (TtyIndex, KArc<dyn FileBacking>) { (idx, b) })
}

fn tty_write_bytes_adapter(
    tty_index: TtyIndex,
    buf: *const u8,
    len: usize,
    nonblock: bool,
) -> Result<usize, tty::TtyError> {
    if buf.is_null() || len == 0 {
        return Ok(0);
    }
    slopos_ostd::util::ptr_buf::with_buf(buf, len, |data| tty::write(tty_index, data, nonblock))
}

static TTY_SERVICES: TtyServices = TtyServices {
    read_cooked: tty_read_adapter,
    set_termios: tty::set_termios,
    set_termios_wait: tty::set_termios_wait,
    set_termios_flush: tty::set_termios_flush,
    get_termios: tty::get_termios,
    set_ldisc: tty::set_ldisc,
    get_ldisc: tty::get_ldisc,
    get_winsize: tty::get_winsize,
    set_winsize: tty::set_winsize,
    switch_active_tty: tty::switch_active_tty,
    set_foreground_pgrp: tty::set_foreground_pgrp,
    get_foreground_pgrp: tty::get_foreground_pgrp,
    get_session_id: tty::get_session_id,
    set_foreground_pgrp_checked: tty::set_foreground_pgrp_checked,
    write_bytes: tty_write_bytes_adapter,
    attach_session: tty::attach_session,
    acquire_controlling_terminal: tty::acquire_controlling_terminal,
    release_controlling_terminal: tty_release_controlling_terminal_adapter,
    default_console_tty: tty::default_console_tty,
    open_tty: tty::open_tty,
    hangup: tty::hangup,
    is_hung_up: tty::is_hung_up,
    alloc_pty: tty_alloc_pty_adapter,
    grantpt: tty_grantpt_adapter,
    get_pty_number: tty::get_pty_number,
    is_pty_slave: tty::is_pty_slave,
    pty_slave_owner: tty::pty_slave_owner,
    open_pty_slave: tty::pty_open_slave_as,
    open_pty_peer: tty::pty_open_peer,
    detach_session_by_id: tty::detach_session_by_id,
    poll_events: tty::poll_events,
    poll_enqueue: tty::poll_enqueue,
    poll_dequeue: tty::poll_dequeue,
    detach_controlling_terminal: tty::detach_controlling_terminal,
    bytes_available: tty::bytes_available,
    set_pty_lock: tty::set_pty_lock,
    get_pty_lock: tty::get_pty_lock,
    set_packet_mode: tty::set_packet_mode,
    tcflush: tty::tcflush,
    tcsbrk: tty::tcsbrk,
    tcxonc: tty::tcxonc,
    output_queued_bytes: tty::output_queued_bytes,
    set_exclusive: tty::set_exclusive,
    get_exclusive: tty::get_exclusive,
};

static KEYMAP_SERVICES: KeymapServices = KeymapServices {
    load: keyboard::load_layout_from_user,
    current_name: keyboard::layout_name,
};

pub fn init_syscall_services() {
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
    register_keymap_services(&KEYMAP_SERVICES);
}
