use slopos_core::syscall_services::{
    InputServices, TtyServices, register_input_services, register_tty_services,
};

use crate::{input_event, tty};

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
};

// =============================================================================
// TTY services — all fields point directly at the driver implementation.
// =============================================================================

static TTY_SERVICES: TtyServices = TtyServices {
    read_line: tty::tty_read_line,
    read_char_blocking: tty::tty_read_char_blocking,
    set_focus: tty::tty_set_focus,
    get_focus: tty::tty_get_focus,
    set_foreground_pgrp: tty::tty_set_foreground_pgrp,
    get_foreground_pgrp: tty::tty_get_foreground_pgrp,
};

pub fn init_syscall_services() {
    register_input_services(&INPUT_SERVICES);
    register_tty_services(&TTY_SERVICES);
}
