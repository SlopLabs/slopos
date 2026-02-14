use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::signal::sig_bit;
use slopos_lib::InterruptFrame;
use slopos_lib::kernel_services::driver_runtime::{
    DriverRuntimeServices, DriverTaskHandle, register_driver_runtime_services,
};

use crate::irq;
use crate::scheduler::scheduler;
use crate::scheduler::task::{self, Task};

#[inline]
fn handle_to_task(handle: DriverTaskHandle) -> *mut Task {
    handle.cast::<Task>()
}

#[inline]
fn runtime_save_preempt_context(frame: *mut InterruptFrame) {
    scheduler::save_preempt_context(frame);
}

#[inline]
fn runtime_scheduler_timer_tick() {
    scheduler::scheduler_timer_tick();
}

#[inline]
fn runtime_scheduler_handle_timer_interrupt(frame: *mut InterruptFrame) {
    scheduler::scheduler_handle_timer_interrupt(frame);
}

#[inline]
fn runtime_request_reschedule_from_interrupt() {
    scheduler::scheduler_request_reschedule_from_interrupt();
}

#[inline]
fn runtime_scheduler_is_enabled() -> i32 {
    scheduler::scheduler_is_enabled()
}

#[inline]
fn runtime_current_task() -> DriverTaskHandle {
    scheduler::scheduler_get_current_task().cast()
}

#[inline]
fn runtime_current_task_id() -> u32 {
    let task = scheduler::scheduler_get_current_task();
    if task.is_null() {
        return 0;
    }
    unsafe { (*task).task_id }
}

#[inline]
fn runtime_block_current_task() {
    scheduler::block_current_task();
}

#[inline]
fn runtime_unblock_task(task: DriverTaskHandle) -> i32 {
    scheduler::unblock_task(handle_to_task(task))
}

#[inline]
fn runtime_register_idle_wakeup_callback(callback: Option<fn() -> i32>) {
    scheduler::scheduler_register_idle_wakeup_callback(callback);
}

struct SignalGroupContext {
    pgid: u32,
    signum: u8,
    matched: bool,
}

fn signal_group_task(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *context.cast::<SignalGroupContext>() };
    if unsafe { (*task).pgid } != ctx.pgid {
        return;
    }

    unsafe {
        (*task)
            .signal_pending
            .fetch_or(sig_bit(ctx.signum), Ordering::AcqRel);
    }
    let _ = scheduler::unblock_task(task);
    ctx.matched = true;
}

fn runtime_signal_process_group(pgid: u32, signum: u8) -> bool {
    if pgid == 0 {
        return false;
    }

    let mut ctx = SignalGroupContext {
        pgid,
        signum,
        matched: false,
    };

    task::task_iterate_active(
        Some(signal_group_task),
        (&mut ctx as *mut SignalGroupContext).cast(),
    );

    ctx.matched
}

#[inline]
fn runtime_irq_init() {
    irq::init();
}

#[inline]
fn runtime_irq_set_route(irq_line: u8, gsi: u32) {
    irq::set_irq_route(irq_line, gsi);
}

#[inline]
fn runtime_irq_is_masked(irq_line: u8) -> bool {
    irq::is_masked(irq_line)
}

#[inline]
fn runtime_irq_register_handler(
    irq_line: u8,
    handler: Option<slopos_lib::kernel_services::driver_runtime::DriverIrqHandler>,
    context: *mut c_void,
    name: *const core::ffi::c_char,
) -> i32 {
    irq::register_handler(irq_line, handler, context, name)
}

#[inline]
fn runtime_irq_enable_line(irq_line: u8) {
    irq::enable_line(irq_line);
}

#[inline]
fn runtime_irq_disable_line(irq_line: u8) {
    irq::disable_line(irq_line);
}

#[inline]
fn runtime_irq_get_timer_ticks() -> u64 {
    irq::get_timer_ticks()
}

#[inline]
fn runtime_irq_increment_timer_ticks() {
    irq::increment_timer_ticks();
}

#[inline]
fn runtime_irq_increment_keyboard_events() {
    irq::increment_keyboard_events();
}

static DRIVER_RUNTIME_SERVICES: DriverRuntimeServices = DriverRuntimeServices {
    save_preempt_context: runtime_save_preempt_context,
    scheduler_timer_tick: runtime_scheduler_timer_tick,
    scheduler_handle_timer_interrupt: runtime_scheduler_handle_timer_interrupt,
    request_reschedule_from_interrupt: runtime_request_reschedule_from_interrupt,
    scheduler_is_enabled: runtime_scheduler_is_enabled,
    current_task: runtime_current_task,
    current_task_id: runtime_current_task_id,
    block_current_task: runtime_block_current_task,
    unblock_task: runtime_unblock_task,
    register_idle_wakeup_callback: runtime_register_idle_wakeup_callback,
    signal_process_group: runtime_signal_process_group,
    irq_init: runtime_irq_init,
    irq_set_route: runtime_irq_set_route,
    irq_is_masked: runtime_irq_is_masked,
    irq_register_handler: runtime_irq_register_handler,
    irq_enable_line: runtime_irq_enable_line,
    irq_disable_line: runtime_irq_disable_line,
    irq_get_timer_ticks: runtime_irq_get_timer_ticks,
    irq_increment_timer_ticks: runtime_irq_increment_timer_ticks,
    irq_increment_keyboard_events: runtime_irq_increment_keyboard_events,
};

pub fn register_driver_services() {
    register_driver_runtime_services(&DRIVER_RUNTIME_SERVICES);
}
