use slopos_mm::memory_layout_defs::{EXCEPTION_STACK_REGION_BASE, EXCEPTION_STACK_REGION_STRIDE};
use slopos_ostd::cpu::x86_64::pcr::MAX_CPUS;
use slopos_ostd::cpu::x86_64::stack::read_rsp;
use slopos_ostd::irq::interrupt_frame::InterruptFrame;
use slopos_ostd::sync::PreemptGuard;

use super::scheduler::{is_scheduling_active, schedule_from_trap_exit, scheduler_timer_tick};
use super::task::TaskStatus;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RescheduleReason {
    TimerTick,
    InterruptWake,
    RescheduleIpi,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrapExitSource {
    Irq,
    RescheduleIpi,
}

#[inline]
pub fn trap_running_on_exception_stack() -> bool {
    let rsp = read_rsp();
    let ist_region_end =
        EXCEPTION_STACK_REGION_BASE + (MAX_CPUS as u64) * 7 * EXCEPTION_STACK_REGION_STRIDE;
    rsp >= EXCEPTION_STACK_REGION_BASE && rsp < ist_region_end
}

pub fn scheduler_request_reschedule(_reason: RescheduleReason) {
    if is_scheduling_active() {
        PreemptGuard::set_reschedule_pending();
    }
}

pub fn scheduler_request_reschedule_from_interrupt() {
    scheduler_request_reschedule(RescheduleReason::InterruptWake);
}

pub fn scheduler_handle_timer_interrupt(frame: *mut InterruptFrame) {
    // Ahead of `scheduler_timer_tick` so the heartbeat is recorded before any
    // lock is taken, and outside it so synthetic test calls to that function do
    // not drive the detector with no wall time elapsed.
    slopos_ostd::watchdog::tick();
    slopos_mm::mmu::quiesce::tick();
    slopos_ostd::kconsole::poll_from_timer();
    if crate::profile::enabled() {
        let anchor = ();
        if let Some(frame_ref) = InterruptFrame::from_ptr(&anchor, frame) {
            crate::profile::note_tick(frame_ref.rip, frame_ref.cs);
        }
    }
    scheduler_timer_tick();
}

pub fn scheduler_handoff_on_trap_exit(source: TrapExitSource) {
    if matches!(source, TrapExitSource::Irq) && trap_running_on_exception_stack() {
        return;
    }

    if PreemptGuard::is_active() {
        return;
    }

    if !PreemptGuard::is_reschedule_pending() {
        return;
    }

    // A task inside its blocking protocol — committed `Running → Blocked`, not
    // yet yielded — must not be descheduled from the trap exit. `Blocked` would
    // park with no wake armed; `Ready` is a wake that already landed and
    // enqueued the task still running here, which `schedule` would dequeue as
    // its own successor and wait on forever. See
    // `scheduler::deferred_reschedule_callback`.
    if crate::task_struct::Current::get().is_some_and(|current| {
        matches!(
            current.task().status(),
            TaskStatus::Blocked | TaskStatus::Ready
        )
    }) {
        return;
    }

    if is_scheduling_active() {
        PreemptGuard::clear_reschedule_pending();
        schedule_from_trap_exit();
    }
}

pub fn scheduler_handle_post_irq() {
    scheduler_handoff_on_trap_exit(TrapExitSource::Irq);
}
