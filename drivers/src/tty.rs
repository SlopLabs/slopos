use core::ffi::c_int;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_lib::{IrqMutex, cpu, ports::COM1};

use crate::ps2::keyboard;
use crate::serial;
use slopos_abi::signal::SIGINT;
use slopos_lib::kernel_services::driver_runtime::{
    DriverTaskHandle, block_current_task, current_task, current_task_id,
    register_idle_wakeup_callback, scheduler_is_enabled, signal_process_group, unblock_task,
};

const TTY_MAX_WAITERS: usize = 32;

#[repr(C)]
struct TtyWaitQueue {
    tasks: [DriverTaskHandle; TTY_MAX_WAITERS],
    head: usize,
    tail: usize,
    count: usize,
}

// SAFETY: The wait queue only stores task pointers managed by the scheduler,
// and access is synchronized through the TTY_WAIT_QUEUE mutex.
unsafe impl Send for TtyWaitQueue {}

static TTY_WAIT_QUEUE: IrqMutex<TtyWaitQueue> = IrqMutex::new(TtyWaitQueue {
    tasks: [ptr::null_mut::<c_void>(); TTY_MAX_WAITERS],
    head: 0,
    tail: 0,
    count: 0,
});
static TTY_FOCUS_QUEUE: IrqMutex<TtyWaitQueue> = IrqMutex::new(TtyWaitQueue {
    tasks: [ptr::null_mut::<c_void>(); TTY_MAX_WAITERS],
    head: 0,
    tail: 0,
    count: 0,
});
static TTY_FOCUSED_TASK_ID: AtomicU32 = AtomicU32::new(0);
static TTY_FOREGROUND_PGRP: AtomicU32 = AtomicU32::new(0);

fn tty_signal_foreground_pgrp(signum: u8) {
    let pgid = tty_get_foreground_pgrp();
    if pgid == 0 {
        return;
    }

    let _ = signal_process_group(pgid, signum);
}

use crate::serial::{serial_buffer_pending, serial_buffer_read, serial_poll_receive};

#[inline]
fn tty_cpu_relax() {
    cpu::pause();
}

#[inline]
fn tty_service_serial_input() {
    serial_poll_receive(COM1.address());
}

fn tty_input_available() -> c_int {
    tty_service_serial_input();
    if keyboard::has_input() != 0 {
        return 1;
    }
    if serial_buffer_pending(COM1.address()) != 0 {
        return 1;
    }
    0
}

fn tty_input_available_cb() -> c_int {
    tty_input_available()
}

fn tty_register_idle_callback() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::AcqRel) {
        return;
    }
    register_idle_wakeup_callback(Some(tty_input_available_cb));
}

fn tty_wait_queue_pop() -> DriverTaskHandle {
    let mut queue = TTY_WAIT_QUEUE.lock();
    if queue.count == 0 {
        return ptr::null_mut();
    }
    let task = queue.tasks[queue.tail];
    queue.tail = (queue.tail + 1) % TTY_MAX_WAITERS;
    queue.count = queue.count.saturating_sub(1);
    task
}

fn tty_wait_queue_push(task: DriverTaskHandle) -> bool {
    if task.is_null() {
        return false;
    }
    let mut queue = TTY_WAIT_QUEUE.lock();
    if queue.count >= TTY_MAX_WAITERS {
        return false;
    }
    for i in 0..queue.count {
        let idx = (queue.tail + i) % TTY_MAX_WAITERS;
        if queue.tasks[idx] == task {
            return true;
        }
    }
    let head = queue.head;
    queue.tasks[head] = task;
    queue.head = (head + 1) % TTY_MAX_WAITERS;
    queue.count = queue.count.saturating_add(1);
    true
}

fn tty_focus_queue_push(task: DriverTaskHandle) -> bool {
    if task.is_null() {
        return false;
    }
    let mut queue = TTY_FOCUS_QUEUE.lock();
    if queue.count >= TTY_MAX_WAITERS {
        return false;
    }
    for i in 0..queue.count {
        let idx = (queue.tail + i) % TTY_MAX_WAITERS;
        if queue.tasks[idx] == task {
            return true;
        }
    }
    let head = queue.head;
    queue.tasks[head] = task;
    queue.head = (head + 1) % TTY_MAX_WAITERS;
    queue.count = queue.count.saturating_add(1);
    true
}

fn tty_focus_queue_pop() -> DriverTaskHandle {
    let mut queue = TTY_FOCUS_QUEUE.lock();
    if queue.count == 0 {
        return ptr::null_mut();
    }
    let task = queue.tasks[queue.tail];
    queue.tail = (queue.tail + 1) % TTY_MAX_WAITERS;
    queue.count = queue.count.saturating_sub(1);
    task
}

fn tty_current_task_id() -> Option<u32> {
    let task_id = current_task_id();
    if task_id == 0 {
        return None;
    }
    Some(task_id)
}

fn tty_task_has_focus(task_id: u32) -> bool {
    let focused = TTY_FOCUSED_TASK_ID.load(Ordering::Relaxed);
    focused != 0 && focused == task_id
}

fn tty_ensure_focus_for_task(task_id: u32) {
    if TTY_FOCUSED_TASK_ID.load(Ordering::Relaxed) == 0 {
        TTY_FOCUSED_TASK_ID.store(task_id, Ordering::Relaxed);
    }
}

fn tty_wait_for_focus(task_id: u32) {
    if tty_task_has_focus(task_id) {
        return;
    }
    if scheduler_is_enabled() != 0 {
        let current = current_task();
        if tty_focus_queue_push(current) {
            block_current_task();
            return;
        }
    }
    while !tty_task_has_focus(task_id) {
        tty_cpu_relax();
    }
}
pub fn tty_notify_input_ready() {
    if scheduler_is_enabled() == 0 {
        return;
    }

    let task = tty_wait_queue_pop();

    if !task.is_null() {
        let _ = unblock_task(task);
    }
}

pub fn tty_set_focus(task_id: u32) -> c_int {
    TTY_FOCUSED_TASK_ID.store(task_id, Ordering::Relaxed);
    if scheduler_is_enabled() == 0 {
        return 0;
    }

    loop {
        let candidate = tty_focus_queue_pop();
        if candidate.is_null() {
            break;
        }
        let _ = unblock_task(candidate);
    }
    0
}

pub fn tty_get_focus() -> u32 {
    TTY_FOCUSED_TASK_ID.load(Ordering::Relaxed)
}

pub fn tty_set_foreground_pgrp(pgid: u32) -> c_int {
    TTY_FOREGROUND_PGRP.store(pgid, Ordering::Release);
    0
}

pub fn tty_get_foreground_pgrp() -> u32 {
    TTY_FOREGROUND_PGRP.load(Ordering::Acquire)
}

pub fn tty_handle_input_char(c: u8) {
    if c == 0x03 {
        tty_signal_foreground_pgrp(SIGINT);
    }
}

#[inline]
fn is_printable(c: u8) -> bool {
    (c >= 0x20 && c <= 0x7E) || c == b'\t'
}

#[inline]
fn is_control_char(c: u8) -> bool {
    (c <= 0x1F) || c == 0x7F
}

fn tty_dequeue_input_char(out_char: &mut u8) -> bool {
    tty_service_serial_input();
    if keyboard::has_input() != 0 {
        *out_char = keyboard::getchar();
        return true;
    }

    tty_service_serial_input();
    let mut raw = 0u8;
    if serial_buffer_read(COM1.address(), &mut raw as *mut u8) == 0 {
        if raw == b'\r' {
            raw = b'\n';
        } else if raw == 0x7F {
            raw = b'\x08';
        }
        *out_char = raw;
        return true;
    }
    false
}

fn tty_block_until_input_ready() {
    if tty_input_available() != 0 {
        return;
    }
    if scheduler_is_enabled() != 0 {
        let current = current_task();
        if tty_wait_queue_push(current) {
            block_current_task();
            return;
        }
    }
    loop {
        if tty_input_available() != 0 {
            break;
        }
        tty_service_serial_input();
        tty_cpu_relax();
    }
}

#[inline]
fn serial_putc(c: u8) {
    serial::serial_putc_com1(c);
}
pub fn tty_read_line(buffer: *mut u8, buffer_size: usize) -> usize {
    if buffer.is_null() || buffer_size == 0 {
        return 0;
    }

    tty_register_idle_callback();
    let task_id = match tty_current_task_id() {
        Some(id) => id,
        None => return 0,
    };
    tty_ensure_focus_for_task(task_id);

    if buffer_size < 2 {
        unsafe { *buffer = 0 };
        return 0;
    }

    let mut pos = 0usize;
    let max_pos = buffer_size - 1;

    loop {
        if !tty_task_has_focus(task_id) {
            tty_wait_for_focus(task_id);
            continue;
        }
        let mut c = 0u8;
        if !tty_dequeue_input_char(&mut c) {
            tty_block_until_input_ready();
            continue;
        }

        if c == b'\n' || c == b'\r' {
            unsafe {
                *buffer.add(pos) = 0;
            }
            serial_putc(b'\n');
            return pos;
        }

        if c == b'\x08' {
            if pos > 0 {
                pos -= 1;
                serial_putc(b'\x08');
                serial_putc(b' ');
                serial_putc(b'\x08');
            }
            continue;
        }

        if pos >= max_pos {
            continue;
        }

        if is_printable(c) {
            unsafe {
                *buffer.add(pos) = c;
            }
            pos += 1;
            serial_putc(c);
            continue;
        }

        if is_control_char(c) {
            continue;
        }

        unsafe {
            *buffer.add(pos) = c;
        }
        pos += 1;
        serial_putc(c);
    }
}

pub fn tty_read_char_blocking(out_char: *mut u8) -> c_int {
    if out_char.is_null() {
        return -1;
    }
    tty_register_idle_callback();
    let task_id = match tty_current_task_id() {
        Some(id) => id,
        None => return -1,
    };
    tty_ensure_focus_for_task(task_id);
    loop {
        if !tty_task_has_focus(task_id) {
            tty_wait_for_focus(task_id);
            continue;
        }
        let mut c = 0u8;
        if tty_dequeue_input_char(&mut c) {
            unsafe {
                *out_char = c;
            }
            return 0;
        }
        tty_block_until_input_ready();
    }
}

pub fn tty_read_char_nonblocking(out_char: *mut u8) -> c_int {
    if out_char.is_null() {
        return -1;
    }
    let task_id = match tty_current_task_id() {
        Some(id) => id,
        None => return -1,
    };
    tty_ensure_focus_for_task(task_id);
    if !tty_task_has_focus(task_id) {
        return -1;
    }
    let mut c = 0u8;
    if tty_dequeue_input_char(&mut c) {
        unsafe {
            *out_char = c;
        }
        return 0;
    }
    -1
}
