use core::ffi::CStr;

use slopos_abi::task::TaskFaultReason;
use slopos_arch::InterruptFrame;
use slopos_arch::cpu;
use slopos_kernel_services::kernel_vm_space::activate_post_user_fault;
use slopos_ostd::{kdiag_dump_interrupt_frame, klog_info};
use slopos_sched::scheduler::schedule;
use slopos_sched::task::TaskRef;
use slopos_sched::task::task_find_by_cr3;
use slopos_sched::task::{task_group_fatal_signal, task_terminate};

use crate::panic::set_panic_cpu_state;

/// Retire this CPU after a fatal user-mode exception. If no context switch
/// happens, parks in a halt loop with interrupts on so IPIs are still serviced.
///
/// `on_ist` says whether the calling handler runs on a per-CPU IST stack; #PF
/// does not, and unwinds as an ordinary task frame.
///
/// # Safety invariant
/// The caller must be handling a user-mode exception (CS RPL == 3).
/// This function never returns.
fn retire_faulted_cpu(task_ref: TaskRef, reason: TaskFaultReason, on_ist: bool) -> ! {
    let tid = task_ref.record_user_fault_exit(reason);
    let _ = task_group_fatal_signal(tid, task_ref.exit_signal());
    task_terminate(tid);
    // Release the registry upgrade before the diverging switch tail.
    drop(task_ref);
    if on_ist {
        // `schedule()` never returns here, so this handler's exception
        // data-stack depth would leak and accumulate across fatal user faults.
        // Re-prime with a naked store: an instrumented setter would resolve this
        // slot as its own data-SP and undo the write on return.
        let dstack_top = crate::ist_stacks::exc_dstack_top_current_cpu();
        slopos_arch::pcr::reset_ist_unsafe_sp(dstack_top);
        // This handler diverges into `schedule()`, so the exception-entry preempt
        // hold from the IST dispatcher would leak, leaving the next task's preempt
        // count stuck at >=1.
        slopos_ostd::cpu::preempt::release_diverging_exception_hold();
    }
    schedule();
    let _ = activate_post_user_fault();
    cpu::enable_interrupts();
    cpu::halt_loop();
}

pub(crate) fn in_user(frame: &InterruptFrame) -> bool {
    (frame.cs & 0x3) == 0x3
}

/// Turn a user-mode fault into a deliverable signal, or `false` when it must
/// stay fatal.
///
/// A fault taken while the same signal is *blocked* is a fault inside that
/// signal's own handler (delivery blocks it), so it stays fatal rather than
/// re-entering the handler that just failed. The post is forced because a
/// fault signal cannot be deferred: the instruction re-executes on `iretq`.
fn deliver_user_fault_signal(
    task_ref: &TaskRef,
    reason: TaskFaultReason,
    frame: &InterruptFrame,
    fault_addr: u64,
) -> bool {
    // Vectors 0-31 other than #PF run on an IST stack under an
    // `IstPreemptHold`, where no frame may be pushed; those stay fatal.
    if slopos_ostd::irq::vector_uses_ist((frame.vector & 0xFF) as u8) {
        return false;
    }

    let signum = slopos_ostd::task::fault_signal_for(reason);
    let idx = (signum - 1) as usize;
    let deliverable = match task_ref.signal_action(idx) {
        Some(action) => {
            action.handler != slopos_abi::signal::SIG_DFL
                && action.handler != slopos_abi::signal::SIG_IGN
                && action.restorer != 0
        }
        None => false,
    };
    if !deliverable {
        return false;
    }
    if (task_ref.signal_blocked() & slopos_abi::signal::sig_bit(signum)) != 0 {
        return false;
    }

    let si_code = match reason {
        TaskFaultReason::UserOom => slopos_abi::signal::BUS_OBJERR,
        TaskFaultReason::UserUd => slopos_abi::signal::ILL_ILLOPC,
        // Error-code bit 0: the page was present, so the mapping exists and
        // the access itself was refused.
        _ => {
            if (frame.error_code & 1) != 0 {
                slopos_abi::signal::SEGV_ACCERR
            } else {
                slopos_abi::signal::SEGV_MAPERR
            }
        }
    };
    task_ref.set_fault_siginfo(signum, si_code, fault_addr);
    slopos_sched::task::task_signal_force(task_ref, signum)
}

/// Try to hand `reason`'s fault to a userland handler, building the signal
/// frame against the very interrupt frame the `iretq` will resume from.
///
/// `true` when the fault was consumed: the caller must return to the trap exit
/// without reporting anything, since the faulting instruction is no longer
/// what resumes.
pub(crate) fn try_deliver_user_fault(
    reason: TaskFaultReason,
    frame: *mut InterruptFrame,
    fault_addr: u64,
) -> bool {
    let frame_anchor = ();
    let Some(frame_ref) = InterruptFrame::from_ptr(&frame_anchor, frame) else {
        return false;
    };
    if !in_user(frame_ref) {
        return false;
    }
    let Some(task_ref) = resolve_user_fault_task() else {
        return false;
    };
    if !deliver_user_fault_signal(&task_ref, reason, frame_ref, fault_addr) {
        return false;
    }
    // Released before delivery: the frame push can fail and force termination,
    // which context-switches, and a held registry upgrade must not straddle it.
    drop(task_ref);
    slopos_core::syscall::signal::deliver_pending_signal_on_irq_exit(frame);
    true
}

pub(crate) fn cstr_from_bytes(bytes: &'static [u8]) -> &'static CStr {
    CStr::from_bytes_until_nul(bytes).expect("cstr_from_bytes input must be NUL-terminated")
}

#[inline]
pub(crate) fn resolve_user_fault_task() -> Option<TaskRef> {
    let hw_cr3 = cpu::read_cr3() & !0xFFF;

    // Recoverable fault, not a panic path, so the registry lookups stay; a
    // bootstrap stub has no valid id and falls through to the CR3 scan.
    if let Some(task_ref) = slopos_sched::task_struct::Current::get()
        .map(|current| current.id())
        .and_then(slopos_sched::task::task_find_by_id)
    {
        let task_cr3 = task_ref.context_cr3() & !0xFFF;
        if task_cr3 == hw_cr3 {
            return Some(task_ref);
        }
    }

    task_find_by_cr3(hw_cr3)
}

pub(crate) fn terminate_user_task(
    reason: TaskFaultReason,
    frame: &InterruptFrame,
    fault_addr: u64,
    detail: &'static CStr,
) {
    let Some(task_ref) = resolve_user_fault_task() else {
        klog_info!(
            "Terminating user fault context without a valid current task: {}",
            detail.to_str().unwrap_or("<invalid utf-8>")
        );
        kdiag_dump_interrupt_frame(frame as *const _);
        panic_with_frame(
            "user fault with invalid current task",
            frame as *const _ as *mut _,
        );
        return;
    };
    let tid = task_ref.task_id;
    let detail_str = detail.to_str().unwrap_or("<invalid utf-8>");
    let cr2 = fault_addr;
    let (rip, rsp, vec, err) = (frame.rip, frame.rsp, frame.vector, frame.error_code);
    let name_raw = task_ref.name_bytes();
    let name_len = name_raw
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_raw.len());
    let name_str = core::str::from_utf8(&name_raw[..name_len]).unwrap_or("<invalid utf-8>");
    let entry_point = task_ref.entry_point;
    let proc_id = task_ref.process_id;
    let flags = task_ref.flags;
    klog_info!(
        "Terminating user task {} ('{}'): {} | vec={} err=0x{:x} cr2=0x{:x} rip=0x{:x} rsp=0x{:x} entry=0x{:x} pid={} flags=0x{:x}",
        tid,
        name_str,
        detail_str,
        vec,
        err,
        cr2,
        rip,
        rsp,
        entry_point,
        proc_id,
        flags
    );
    kdiag_dump_interrupt_frame(frame as *const _);
    let on_ist = slopos_ostd::irq::vector_uses_ist((vec & 0xFF) as u8);
    retire_faulted_cpu(task_ref, reason, on_ist);
}

pub(crate) fn panic_with_frame(message: &str, frame: *mut InterruptFrame) {
    let frame_anchor = ();
    if let Some(frame_ref) = InterruptFrame::from_ptr(&frame_anchor, frame) {
        set_panic_cpu_state(frame_ref.rip, frame_ref.rsp, frame_ref.rbp);
    }
    panic!("{}", message);
}
