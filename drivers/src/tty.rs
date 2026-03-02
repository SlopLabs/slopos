use core::ffi::c_int;
use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::UserTermios;
use slopos_lib::{IrqMutex, cpu, ports::COM1};

use crate::line_disc::{InputAction, LineDisc};
use crate::ps2::keyboard;
use crate::serial;
use crate::serial::serial_poll_receive;
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
static LINE_DISC: IrqMutex<LineDisc> = IrqMutex::new(LineDisc::new());

fn tty_signal_foreground_pgrp(signum: u8) {
    let pgid = tty_get_foreground_pgrp();
    if pgid == 0 {
        return;
    }
    let _ = signal_process_group(pgid, signum);
}

#[inline]
fn tty_cpu_relax() {
    cpu::pause();
}

/// Drain ALL pending hardware input (serial UART + PS/2 keyboard) into the
/// line discipline.  This is the canonical "push" path — every check for
/// available cooked data and every blocking read MUST call this first so that
/// `LINE_DISC` is the single source of truth for input readiness.
///
/// Modelled after Linux's `flush_to_ldisc` — hardware buffers are drained
/// eagerly rather than lazily inside `read()`.
fn tty_drain_hw_input() {
    // 1. Poll the UART once — moves bytes from the hardware FIFO into
    //    INPUT_BUFFER.  Do this exactly once to avoid redundant port I/O.
    serial_poll_receive(COM1.address());

    // 2. Drain the keyboard char_buffer into LINE_DISC.
    while keyboard::has_input() != 0 {
        let c = keyboard::getchar();
        process_raw_char(c);
    }

    // 3. Drain the serial INPUT_BUFFER into a local scratch array first,
    //    avoiding the per-byte serial_poll_receive() that serial_buffer_read()
    //    would otherwise perform.  Then feed through the line discipline.
    let mut scratch = [0u8; 64];
    let count = {
        let mut buf = crate::serial::input_buffer_lock();
        let mut n = 0usize;
        while n < scratch.len() {
            match buf.try_pop() {
                Some(b) => {
                    scratch[n] = b;
                    n += 1;
                }
                None => break,
            }
        }
        n
    };
    for i in 0..count {
        let mut c = scratch[i];
        if c == b'\r' {
            c = b'\n';
        } else if c == 0x7F {
            c = 0x08;
        }
        process_raw_char(c);
    }
}

fn tty_input_available() -> c_int {
    tty_drain_hw_input();
    LINE_DISC.lock().has_data() as c_int
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
    if focused != 0 && focused == task_id {
        return true;
    }
    let fg_pgrp = TTY_FOREGROUND_PGRP.load(Ordering::Acquire);
    fg_pgrp != 0 && fg_pgrp == task_id
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

#[inline]
fn serial_putc(c: u8) {
    serial::serial_putc_com1(c);
}

fn process_raw_char(c: u8) {
    let mut ld = LINE_DISC.lock();
    let action = ld.input_char(c);
    let has_data = ld.has_data();
    drop(ld);

    match action {
        InputAction::Echo { buf, len } => {
            for i in 0..len as usize {
                serial_putc(buf[i]);
            }
        }
        InputAction::Signal(sig) => {
            tty_signal_foreground_pgrp(sig);
        }
        InputAction::None => {}
    }
    if has_data {
        tty_notify_input_ready();
    }
}

pub fn tty_handle_input_char(c: u8) {
    process_raw_char(c);
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
        tty_drain_hw_input();
        tty_cpu_relax();
    }
}

pub fn tty_read_cooked(buffer: *mut u8, max: usize, nonblock: bool) -> isize {
    if buffer.is_null() || max == 0 {
        return 0;
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

        tty_drain_hw_input();

        let out = unsafe { core::slice::from_raw_parts_mut(buffer, max) };
        let got = LINE_DISC.lock().read(out);
        if got > 0 {
            return got as isize;
        }

        if nonblock {
            return -11;
        }

        tty_block_until_input_ready();
    }
}

pub fn tty_has_cooked_data() -> bool {
    tty_drain_hw_input();
    LINE_DISC.lock().has_data()
}

pub fn tty_set_termios(t: *const UserTermios) {
    if t.is_null() {
        return;
    }
    let val = unsafe { *t };
    LINE_DISC.lock().set_termios(&val);
}

pub fn tty_get_termios(t: *mut UserTermios) {
    if t.is_null() {
        return;
    }
    let val = *LINE_DISC.lock().termios();
    unsafe {
        *t = val;
    }
}
