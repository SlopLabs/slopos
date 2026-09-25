//! Coverage for the signal surface a build system needs.

use core::ffi::c_char;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{
    MINSIGSTKSZ, SA_NODEFER, SA_ONSTACK, SA_RESETHAND, SA_SIGINFO, SEGV_MAPERR, SI_ADDR_OFFSET,
    SIG_DFL, SIGCONT, SIGNAL_KILLED, SIGSEGV, SIGSTOP, SIGTERM, SIGTSTP, SIGUSR1, SS_DISABLE,
    SS_ONSTACK, SignalFrame, UserSigAltStack, UserSiginfo, UserUcontext, sig_bit,
};
use slopos_abi::syscall::{CLONE_SIGHAND, CLONE_THREAD, CLONE_VM};
use slopos_abi::task::{
    INVALID_TASK_ID, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TaskExitReason, TaskStatus,
};
use slopos_fs::fileio::FdTable;
use slopos_kernel_services::driver_runtime::{signal_process_group, signal_session};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{process_vm_alloc, process_vm_get_stack_top};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_test_process_id};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::KBox;
use slopos_ostd::task::SchedPlacement;
use slopos_ostd::user::context::UserContext;
use slopos_sched::task;
use slopos_sched::task::{
    task_clone, task_consume_zombie, task_create, task_find_by_id, task_fork, task_group_continue,
    task_group_exit, task_group_signal, task_group_stop, task_peek_exit_info, task_set_parent,
    task_set_state, task_terminate,
};
use slopos_sched::task_struct::{Current, SignalAction};
use slopos_testing::{TestResult, assert_eq_test, assert_some, assert_test, pass};

use crate::syscall::signal::{
    deliver_pending_signal, syscall_kill, syscall_rt_sigreturn, syscall_sigaltstack,
};

type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

const TEST_HANDLER: u64 = 0x4100_0000;
const TEST_RESTORER: u64 = 0x4200_0000;

fn create_test_user_task_with(flags: u16) -> u32 {
    let user_entry = slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);
    task_create(
        b"SigTest\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        1,
        flags,
    )
}

fn create_test_user_task() -> u32 {
    create_test_user_task_with(TASK_FLAG_USER_MODE)
}

/// `TestResult::Fail` on its own would leak the tasks into the rest of the
/// suite.
fn fail_and_clean(ids: &[u32]) -> TestResult {
    for id in ids {
        task_terminate(*id);
    }
    TestResult::Fail
}

fn fdtable_of(task_id: u32) -> Option<FdTable> {
    task_find_by_id(task_id)?
        .process()
        .as_deref()
        .and_then(FdTable::of)
}

fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::park_bootstrap_task(
        slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

/// Install `task_id` as this CPU's current task, as the dispatcher would.
///
/// `false` rather than a panic: a panic unwinds out of the test harness as an
/// opaque `Panic` outcome and takes every test sharing the helper with it.
#[must_use]
fn make_task_current(task_id: u32) -> bool {
    let Some(task) = task_find_by_id(task_id) else {
        return false;
    };
    // `dispatch` asserts the task is Ready or Running; a parked one has to be
    // walked back through Ready first. Only these two statuses have that edge.
    if matches!(task.status(), TaskStatus::Blocked | TaskStatus::Stopped)
        && task_set_state(task_id, TaskStatus::Ready) != 0
    {
        return false;
    }
    if !matches!(task.status(), TaskStatus::Ready | TaskStatus::Running) {
        return false;
    }
    task.set_sched_placement(SchedPlacement::OnCpu);
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    slopos_sched::scheduler::dispatch_task_for_test(cpu_id, task_id)
}

fn with_user_process_context<R>(table: FdTable, f: impl FnOnce() -> R) -> Option<R> {
    let process = table.process()?;
    if slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(process) == 0 {
        return None;
    }
    if !slopos_mm::process_vm::process_vm_activate(process) {
        return None;
    }
    set_test_process_id(table.id());
    let out = f();
    set_test_process_id(slopos_abi::task::INVALID_PROCESS_ID);
    slopos_kernel_services::kernel_vm_space::kernel_vm_space()
        .lock()
        .activate_kernel_master();
    Some(out)
}

/// Deliver holding the `Current` witness, as production does, then restore
/// the bootstrap current-task pointer.
#[must_use]
fn deliver_pending_signal_as_current(task_id: u32, table: FdTable, ctx: &UserContext) -> bool {
    if !make_task_current(task_id) {
        return false;
    }
    let delivered = match Current::get() {
        Some(current) => {
            with_user_process_context(table, || deliver_pending_signal(&current, ctx)).is_some()
        }
        None => false,
    };
    park_bootstrap_on_current_cpu();
    delivered
}

/// `rt_sigreturn` refuses a frame without the `Current` witness, so drive it
/// as current like production. `ctx` carries the RSP the handler returned on.
#[must_use]
fn sigreturn_as_current(task_id: u32, table: FdTable, ctx: &mut UserContext) -> bool {
    if !make_task_current(task_id) {
        return false;
    }
    let returned = with_user_process_context(table, || {
        let Some(task) = task_find_by_id(task_id) else {
            return false;
        };
        crate::syscall::dispatch::dispatch_handler(syscall_rt_sigreturn, &task, ctx);
        true
    })
    .unwrap_or(false);
    park_bootstrap_on_current_cpu();
    returned
}

fn user_copy_out<T: Copy>(table: FdTable, addr: u64, value: &T) -> bool {
    with_user_process_context(table, || {
        let Ok(ptr) = UserPtr::<T>::try_new(addr) else {
            return false;
        };
        copy_to_user(ptr, value).is_ok()
    })
    .unwrap_or(false)
}

fn user_copy_in<T: Copy>(table: FdTable, addr: u64) -> Option<T> {
    with_user_process_context(table, || {
        let ptr = UserPtr::<T>::try_new(addr).ok()?;
        copy_from_user(ptr).ok()
    })?
}

fn map_user_rw_region(table: FdTable, pages: usize) -> Option<u64> {
    let process = table.process()?;
    let base = process_vm_alloc(
        process,
        (pages * 4096) as u64,
        PageFlags::USER_RW.bits() as u32,
    );
    if base == 0 {
        return None;
    }
    for page in 0..pages {
        let addr = base + (page * 4096) as u64;
        let mapped = slopos_mm::process_vm::process_vm_with_vm_space(process, |vs| {
            slopos_mm::user_mappings::ostd_map_4kb_user_fresh(
                vs,
                slopos_abi::addr::VirtAddr::new(addr),
                PageFlags::USER_RW.bits(),
            )
            .is_ok()
        });
        if !matches!(mapped, Some(true)) {
            return None;
        }
    }
    Some(base)
}

fn install_action(task_id: u32, signum: u8, flags: u64) -> bool {
    let Some(task) = task_find_by_id(task_id) else {
        return false;
    };
    task.set_signal_action(
        (signum - 1) as usize,
        SignalAction {
            handler: TEST_HANDLER,
            mask: 0,
            flags,
            restorer: TEST_RESTORER,
        },
    )
}

/// Leader plus one `CLONE_SIGHAND` sibling, left where `task_clone` publishes
/// them — `Ready` on a runqueue, off-CPU because `KernelTestScope` parks the
/// APs. They are deliberately not forced to `Blocked`: `Ready` has no edge to
/// `Blocked`, and a stopped-from-`Ready` member is what the group-stop path
/// has to handle.
fn spawn_thread_group() -> Option<(u32, u32)> {
    let leader_id = create_test_user_task();
    if leader_id == INVALID_TASK_ID {
        return None;
    }
    let leader = task_find_by_id(leader_id)?;
    let thread_id = task_clone(
        &leader,
        None,
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        0,
        0,
        0,
        0,
    )
    .ok()?;
    Some((leader_id, thread_id))
}

pub fn test_group_stop_parks_every_thread_and_continue_resumes() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");

    // Named through the *non-leader* thread, so the fan-out is what reaches the
    // leader rather than the id happening to name it.
    assert_test!(
        task_group_stop(thread_id, SIGSTOP),
        "SIGSTOP must report that it stopped the group"
    );
    assert_test!(leader.is_stopped(), "the group leader must be Stopped");
    assert_test!(thread.is_stopped(), "the sibling thread must be Stopped");
    // Queue membership, not the exact placement token: a newly published task
    // lands in a CPU's ready queue or in its remote-wake inbox depending on
    // which CPU the scheduler picked, and only the former is the stop's to
    // strip.
    assert_test!(
        !matches!(
            thread.sched_placement(),
            SchedPlacement::ReadyQueue | SchedPlacement::Migrating
        ),
        "a stopped task must hold no runqueue position"
    );

    assert_test!(
        task_group_stop(thread_id, SIGSTOP),
        "a second stop must be idempotent, not a failure"
    );

    assert_test!(
        task_group_continue(leader_id),
        "SIGCONT must report that it resumed the group"
    );
    assert_eq_test!(
        leader.status(),
        TaskStatus::Ready,
        "the leader must be runnable again"
    );
    assert_eq_test!(
        thread.status(),
        TaskStatus::Ready,
        "the sibling must be runnable again"
    );
    assert_test!(
        !task_group_continue(leader_id),
        "continuing a group that is not stopped owes no report"
    );

    drop(leader);
    drop(thread);
    task_terminate(thread_id);
    task_terminate(leader_id);
    pass!()
}

/// A stop also retires an unconsumed continue report.
pub fn test_stop_report_is_consumed_once() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");

    assert_test!(
        !task.has_stop_report(),
        "a fresh task owes no job-control report"
    );
    assert_test!(task_group_stop(task_id, SIGSTOP), "stop must succeed");
    assert_test!(task.has_stop_report(), "a stop must publish a report");
    assert_eq_test!(
        task.take_stop_report(),
        Some(SIGSTOP),
        "the report must carry the stop signal"
    );
    assert_eq_test!(
        task.take_stop_report(),
        None,
        "a consumed stop report must not be reported twice"
    );

    assert_test!(task_group_continue(task_id), "continue must succeed");
    assert_test!(
        task.take_continue_report(),
        "a resume must publish a continue report"
    );
    assert_test!(
        !task.take_continue_report(),
        "a consumed continue report must not be reported twice"
    );

    assert_test!(
        task_group_stop(task_id, SIGSTOP),
        "second stop must succeed"
    );
    assert_test!(task_group_continue(task_id), "second continue must succeed");
    assert_test!(task_group_stop(task_id, SIGSTOP), "third stop must succeed");
    assert_test!(
        !task.has_continue_report(),
        "a stop must retire an unconsumed continue report"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

pub fn test_catchable_stop_signal_handler_wins_over_stop() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");

    assert_test!(
        install_action(task_id, SIGTSTP, 0),
        "installing a SIGTSTP handler failed"
    );
    assert_test!(
        task_group_signal(task_id, SIGTSTP) != 0,
        "SIGTSTP must reach the group"
    );
    assert_test!(
        !task.is_stopped(),
        "a caught SIGTSTP must not park the task"
    );
    assert_test!(
        (task.signal_pending() & sig_bit(SIGTSTP)) != 0,
        "a caught SIGTSTP must be left pending for its handler"
    );
    assert_test!(
        !task.has_stop_report(),
        "a caught stop signal owes no WUNTRACED report"
    );

    // SIGSTOP is in SIG_UNCATCHABLE, so a handler cannot be installed.
    assert_test!(
        task_group_signal(task_id, SIGSTOP) != 0,
        "SIGSTOP must reach the group"
    );
    assert_test!(task.is_stopped(), "SIGSTOP must park the task");

    drop(task);
    task_terminate(task_id);
    pass!()
}

pub fn test_kill_reaches_a_non_leader_thread() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");
    let Some(leader_table) = fdtable_of(leader_id) else {
        return TestResult::Fail;
    };

    assert_eq_test!(
        thread.signal_pending() & sig_bit(SIGTERM),
        0,
        "the sibling must start with no pending SIGTERM"
    );

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = leader_id as u64;
    frame.regs_mut().rsi = SIGTERM as u64;
    let _ = with_user_process_context(leader_table, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &leader, &mut frame)
    });
    assert_eq_test!(frame.rax(), 0, "kill(pid, SIGTERM) must succeed");

    assert_test!(
        (thread.signal_pending() & sig_bit(SIGTERM)) != 0,
        "kill(pid) must reach every thread of the group"
    );
    assert_test!(
        (leader.signal_pending() & sig_bit(SIGTERM)) != 0,
        "kill(pid) must also reach the named thread"
    );

    drop(leader);
    drop(thread);
    task_terminate(thread_id);
    task_terminate(leader_id);
    pass!()
}

/// The exit code alone cannot distinguish a signal death from
/// `exit(128 + signal)`.
pub fn test_exit_info_reports_the_killing_signal() -> TestResult {
    let _fixture = SyscallFixture::new();

    let killed_id = create_test_user_task();
    assert_test!(killed_id != INVALID_TASK_ID, "failed to create user task");
    let killed = assert_some!(task_find_by_id(killed_id), "task lookup failed");
    let Some(killed_table) = fdtable_of(killed_id) else {
        return TestResult::Fail;
    };

    assert_test!(
        task::task_signal_post(&killed, SIGTERM),
        "SIGTERM must pend"
    );
    let frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    assert_test!(
        deliver_pending_signal_as_current(killed_id, killed_table, &frame),
        "delivering the pending SIGTERM failed"
    );

    let info = assert_some!(
        task_peek_exit_info(killed_id),
        "a task killed by a signal must publish exit info"
    );
    assert_eq_test!(
        info.signal,
        SIGTERM,
        "ExitInfo must name the signal that killed the task"
    );
    assert_eq_test!(
        info.exit_reason,
        TaskExitReason::Signalled,
        "a signal death must be reported as Signalled"
    );
    assert_eq_test!(
        info.exit_code,
        128 + SIGTERM as i32,
        "a signal death still reports 128 + signal as its code"
    );

    // The parent link is what retains the exit info: a task nobody stands in
    // a parent relation to is reclaimed the moment it exits. Holding a
    // `TaskRef` is not that, because the rule keys on `parent_alive_for`.
    let reaper_id = create_test_user_task();
    assert_test!(reaper_id != INVALID_TASK_ID, "failed to create the reaper");
    let exited_id = create_test_user_task();
    assert_test!(exited_id != INVALID_TASK_ID, "failed to create user task");
    assert_eq_test!(
        task::task_set_parent(exited_id, reaper_id),
        0,
        "could not parent the exiting task"
    );
    assert_test!(
        task_group_exit(exited_id, 128 + SIGTERM as u32) != 0,
        "exit_group must terminate its group"
    );
    // The member acting on its kill, from its own context.
    task_terminate(exited_id);
    let exited_info = assert_some!(
        task_peek_exit_info(exited_id),
        "an exited task must publish exit info"
    );
    assert_eq_test!(
        exited_info.signal,
        0,
        "exit(143) must not claim a killing signal"
    );
    assert_eq_test!(
        exited_info.exit_reason,
        TaskExitReason::Normal,
        "exit(143) is a normal exit"
    );

    drop(killed);
    task_terminate(exited_id);
    task_terminate(reaper_id);
    task_terminate(killed_id);
    pass!()
}

/// `SA_SIGINFO` is what puts the records in RSI and RDX.
pub fn test_fault_signal_delivers_siginfo_with_fault_address() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return TestResult::Fail;
    };

    assert_test!(
        install_action(task_id, SIGSEGV, SA_SIGINFO),
        "installing a SIGSEGV handler failed"
    );

    const FAULT_ADDR: u64 = 0x0000_2000_0000_0008;
    task.set_fault_siginfo(SIGSEGV, SEGV_MAPERR, FAULT_ADDR);
    assert_test!(
        task::task_signal_force(&task, SIGSEGV),
        "a forced fault signal must pend"
    );

    let stack_top = process_vm_get_stack_top(table.process().expect("a live process"));
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = stack_top.wrapping_sub(0x200);
    frame.regs_mut().rip = 0x5000_1234;
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "delivering the forced SIGSEGV failed"
    );

    assert_eq_test!(frame.rip(), TEST_HANDLER, "the SIGSEGV handler must run");
    assert_eq_test!(frame.rdi(), SIGSEGV as u64, "RDI must carry the signal");
    assert_test!(
        frame.rsi() != 0 && frame.rdx() != 0,
        "SA_SIGINFO must pass siginfo and ucontext addresses"
    );

    let info: UserSiginfo = match user_copy_in(table, frame.rsi()) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    // Addressed by offset, not by field: what a fault handler compiled
    // against a real `siginfo_t` indexes is byte 16 of the struct, and that
    // is the thing under test.
    let si_addr: u64 = match user_copy_in(table, frame.rsi() + SI_ADDR_OFFSET as u64) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        si_addr,
        FAULT_ADDR,
        "si_addr must be the faulting address, at Linux's offset 16"
    );
    assert_eq_test!(
        info.si_addr(),
        FAULT_ADDR,
        "si_addr's accessor must read the same word"
    );
    assert_eq_test!(info.si_code, SEGV_MAPERR, "si_code must survive delivery");
    assert_eq_test!(
        info.si_signo,
        SIGSEGV as i32,
        "si_signo must name the signal"
    );

    let uc: UserUcontext = match user_copy_in(table, frame.rdx()) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        uc.uc_mcontext_gregs[slopos_abi::signal::REG_RIP],
        0x5000_1234,
        "uc_mcontext must carry the interrupted RIP"
    );
    assert_eq_test!(uc.uc_sigmask, 0, "uc_sigmask must be the pre-delivery mask");

    // The siginfo is consume-once: a later `kill` of the same signal must not
    // inherit the faulting address.
    assert_test!(
        task.fault_siginfo_for(SIGSEGV).is_none(),
        "delivery must retire the recorded fault siginfo"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// Re-pending instead would return to user mode, re-execute the faulting
/// instruction and loop forever.
pub fn test_fault_signal_with_unpushable_frame_terminates() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return TestResult::Fail;
    };

    assert_test!(
        install_action(task_id, SIGSEGV, 0),
        "installing a SIGSEGV handler failed"
    );
    task.set_fault_siginfo(SIGSEGV, SEGV_MAPERR, 0x1000);
    assert_test!(
        task::task_signal_force(&task, SIGSEGV),
        "a forced fault signal must pend"
    );

    // Nothing is mapped under this stack pointer, so the frame cannot be
    // written.
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = 0x0000_7000_0000_0000;
    frame.regs_mut().rip = 0x5000_1234;
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "delivering the forced SIGSEGV failed"
    );

    let status = task.status();
    assert_test!(
        status == TaskStatus::Zombie || status == TaskStatus::Terminated,
        "an undeliverable fault signal must terminate the task, not loop"
    );
    assert_eq_test!(
        task.exit_signal(),
        SIGSEGV,
        "the forced death must be reported as a SIGSEGV death"
    );
    assert_eq_test!(
        task.signal_pending() & sig_bit(SIGSEGV),
        0,
        "the signal must not have been left pending for another attempt"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// On-stack is a property of the interrupted stack pointer, so every call
/// below states the RSP it is made with.
pub fn test_sigaltstack_bounds_and_onstack_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return TestResult::Fail;
    };

    let pages = MINSIGSTKSZ.div_ceil(4096) + 1;
    let Some(region) = map_user_rw_region(table, pages + 1) else {
        task_terminate(task_id);
        return TestResult::Fail;
    };
    // First page is the argument scratch; the rest is the alternate stack.
    let args = region;
    let alt_base = region + 4096;
    let alt_size = (pages * 4096) as u64;

    let call = |new_addr: u64, old_addr: u64, rsp: u64| -> u64 {
        let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
        frame.regs_mut().rdi = new_addr;
        frame.regs_mut().rsi = old_addr;
        frame.regs_mut().rsp = rsp;
        let _ = with_user_process_context(table, || {
            crate::syscall::dispatch::dispatch_handler(syscall_sigaltstack, &task, &mut frame)
        });
        frame.rax()
    };
    let main_rsp =
        process_vm_get_stack_top(table.process().expect("a live process")).wrapping_sub(0x200);

    let too_small = UserSigAltStack {
        ss_sp: alt_base,
        ss_flags: 0,
        _pad: 0,
        ss_size: MINSIGSTKSZ as u64 - 1,
    };
    assert_test!(
        user_copy_out(table, args, &too_small),
        "failed to stage the sigaltstack argument"
    );
    assert_eq_test!(
        call(args, 0, main_rsp),
        slopos_abi::Errno::ENOMEM.as_u64(),
        "an alternate stack below MINSIGSTKSZ must be ENOMEM"
    );
    assert_eq_test!(
        task.sigaltstack().0,
        0,
        "a refused sigaltstack must install nothing"
    );

    let good = UserSigAltStack {
        ss_sp: alt_base,
        ss_flags: 0,
        _pad: 0,
        ss_size: alt_size,
    };
    assert_test!(
        user_copy_out(table, args, &good),
        "failed to stage the sigaltstack argument"
    );
    assert_eq_test!(
        call(args, 0, main_rsp),
        0,
        "installing an alternate stack failed"
    );
    assert_eq_test!(
        task.sigaltstack(),
        (alt_base, alt_size),
        "the installed alternate stack must be readable back"
    );

    assert_test!(
        install_action(task_id, SIGUSR1, SA_ONSTACK),
        "installing a SIGUSR1 handler failed"
    );
    assert_test!(task::task_signal_post(&task, SIGUSR1), "SIGUSR1 must pend");
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = main_rsp;
    frame.regs_mut().rip = 0x5000_4321;
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "delivering SIGUSR1 on the alternate stack failed"
    );

    assert_eq_test!(frame.rip(), TEST_HANDLER, "the handler must run");
    assert_test!(
        frame.rsp() >= alt_base && frame.rsp() < alt_base + alt_size,
        "an SA_ONSTACK frame must be based on the alternate stack"
    );
    let handler_rsp = frame.rsp();

    let old_addr = args + 128;
    assert_eq_test!(
        call(0, old_addr, handler_rsp),
        0,
        "querying sigaltstack failed"
    );
    let reported: UserSigAltStack = match user_copy_in(table, old_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        reported.ss_flags,
        SS_ONSTACK,
        "a task on its alternate stack must report SS_ONSTACK"
    );
    assert_eq_test!(
        reported.ss_sp,
        alt_base,
        "the reported alternate stack must be the installed one"
    );
    assert_eq_test!(
        call(args, 0, handler_rsp),
        slopos_abi::Errno::EPERM.as_u64(),
        "changing the alternate stack while on it must be EPERM"
    );

    let disable = UserSigAltStack {
        ss_sp: 0,
        ss_flags: SS_DISABLE,
        _pad: 0,
        ss_size: 0,
    };
    assert_test!(
        user_copy_out(table, args, &disable),
        "failed to stage the SS_DISABLE argument"
    );
    assert_eq_test!(call(args, 0, main_rsp), 0, "SS_DISABLE must be accepted");
    assert_eq_test!(
        task.sigaltstack(),
        (0, 0),
        "SS_DISABLE must retire the alternate stack"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// `SA_RESETHAND` reverts to `SIG_DFL` before the handler runs.
pub fn test_sa_resethand_restores_default_after_one_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return TestResult::Fail;
    };

    assert_test!(
        install_action(task_id, SIGUSR1, SA_RESETHAND),
        "installing a SIGUSR1 handler failed"
    );
    assert_test!(task::task_signal_post(&task, SIGUSR1), "SIGUSR1 must pend");

    let stack_top = process_vm_get_stack_top(table.process().expect("a live process"));
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = stack_top.wrapping_sub(0x200);
    frame.regs_mut().rip = 0x5000_9999;
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "delivering SIGUSR1 failed"
    );

    assert_eq_test!(frame.rip(), TEST_HANDLER, "the handler must run once");
    assert_eq_test!(
        task.signal_handler((SIGUSR1 - 1) as usize),
        Some(SIG_DFL),
        "SA_RESETHAND must restore SIG_DFL after one delivery"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// A `fork` child gets a private copy of the shared table.
pub fn test_clone_sighand_shares_the_action_table() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");

    let child_id = task_fork(&leader, None);
    assert_test!(child_id != INVALID_TASK_ID, "fork failed");
    let child = assert_some!(task_find_by_id(child_id), "fork child lookup failed");

    // Installed after both the thread and the fork child exist, so the sharing
    // is observed rather than merely copied at creation.
    assert_test!(
        install_action(leader_id, SIGUSR1, 0),
        "installing a SIGUSR1 handler failed"
    );

    assert_eq_test!(
        thread.signal_handler((SIGUSR1 - 1) as usize),
        Some(TEST_HANDLER),
        "a CLONE_SIGHAND sibling must observe its group's sigaction"
    );
    assert_eq_test!(
        child.signal_handler((SIGUSR1 - 1) as usize),
        Some(SIG_DFL),
        "a fork child must not observe its parent's later sigaction"
    );

    assert_test!(
        install_action(thread_id, SIGTERM, 0),
        "installing a SIGTERM handler on the sibling failed"
    );
    assert_eq_test!(
        leader.signal_handler((SIGTERM - 1) as usize),
        Some(TEST_HANDLER),
        "the shared table must be visible in both directions"
    );

    // exec resets the shared table, which is what makes an exec'd group
    // single-dispositioned again.
    task::task_reset_caught_handlers(&leader);
    assert_eq_test!(
        thread.signal_handler((SIGUSR1 - 1) as usize),
        Some(SIG_DFL),
        "an exec disposition reset must reach the shared table"
    );

    drop(leader);
    drop(thread);
    drop(child);
    task_terminate(child_id);
    task_terminate(thread_id);
    task_terminate(leader_id);
    pass!()
}

pub fn test_stopped_parent_still_parents_its_children() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    let child_id = task_fork(&parent, None);
    assert_test!(child_id != INVALID_TASK_ID, "fork failed");

    assert_test!(task_group_stop(parent_id, SIGSTOP), "stop must succeed");
    assert_test!(parent.is_stopped(), "the parent must be Stopped");

    let child = assert_some!(task_find_by_id(child_id), "child lookup failed");
    child
        .exit_code
        .store(7, core::sync::atomic::Ordering::Release);
    drop(child);
    task_terminate(child_id);

    let info = assert_some!(
        task_peek_exit_info(child_id),
        "a child of a stopped parent must still publish exit info"
    );
    assert_eq_test!(
        info.exit_code,
        7,
        "the child's exit status must survive for its stopped parent"
    );

    drop(parent);
    task_terminate(parent_id);
    pass!()
}

/// `allocate_task` installs a disposition table into every slot, and the
/// bytewise copy `fork`/`clone` run over it must release that handle:
/// nothing else references the overwritten table, so the leak is
/// unreclaimable.
pub fn test_clone_from_releases_the_slots_own_signal_table() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create user task");

    let released = assert_some!(
        slopos_sched::task::task_clone_from_releases_slot_sighand_for_test(parent_id),
        "the clone probe could not build a task"
    );
    assert_test!(
        released,
        "task_clone_from leaked the child slot's own disposition table"
    );

    task_terminate(parent_id);
    pass!()
}

/// The kernel's own TTY layer sends `SIGHUP` then `SIGCONT`, and that
/// `SIGCONT` has to resume a job stopped with Ctrl-Z. Posting the bit cannot:
/// `unblock_task` refuses anything that is not `Blocked`, and a `Stopped`
/// task is not.
pub fn test_tty_hangup_continue_resumes_a_stopped_group() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    // Its own group and session: ids are monotonic, so no live task shares
    // them and the walk below reaches exactly this task.
    task.set_pgid(task_id);
    task.set_sid(task_id);

    assert_test!(
        task_group_stop(task_id, SIGTSTP),
        "Ctrl-Z must stop the job"
    );
    assert_test!(task.is_stopped(), "the job must be Stopped");

    assert_test!(
        signal_process_group(task_id, SIGCONT),
        "a process-group SIGCONT must reach the group"
    );
    assert_eq_test!(
        task.status(),
        TaskStatus::Ready,
        "a SIGCONT from the TTY layer must resume a stopped group"
    );

    assert_test!(
        task_group_stop(task_id, SIGTSTP),
        "second stop must succeed"
    );
    assert_test!(task.is_stopped(), "the job must be Stopped again");
    assert_test!(
        signal_session(task_id, SIGCONT),
        "a session SIGCONT must reach the session"
    );
    assert_eq_test!(
        task.status(),
        TaskStatus::Ready,
        "a hangup's SIGCONT must resume every member of the session"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// Only the dispatcher checks a capability, so this goes through the table
/// entry rather than the handler. A capability no task flag confers would
/// leave the syscall dead for every caller.
pub fn test_clock_settime_requires_the_clock_capability() -> TestResult {
    let _fixture = SyscallFixture::new();

    let plain_id = create_test_user_task();
    let granted_id = create_test_user_task_with(TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM);
    assert_test!(
        plain_id != INVALID_TASK_ID && granted_id != INVALID_TASK_ID,
        "failed to create the two user tasks"
    );
    let plain = assert_some!(task_find_by_id(plain_id), "task lookup failed");
    let granted = assert_some!(task_find_by_id(granted_id), "task lookup failed");
    let Some(plain_table) = fdtable_of(plain_id) else {
        return fail_and_clean(&[plain_id, granted_id]);
    };
    let Some(granted_table) = fdtable_of(granted_id) else {
        return fail_and_clean(&[plain_id, granted_id]);
    };

    let entry = assert_some!(
        crate::syscall::handlers::syscall_lookup(slopos_abi::syscall::SYSCALL_CLOCK_SETTIME),
        "clock_settime is not registered"
    );

    // `CLOCK_MONOTONIC` is refused by the handler itself, so a caller that
    // holds the capability gets a *different* refusal. The wall clock is
    // deliberately not moved: every filesystem timestamp hangs off it.
    let call = |table: FdTable, task: &slopos_sched::task::TaskRef, ts: u64| -> u64 {
        let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
        frame.regs_mut().rdi = slopos_abi::syscall::CLOCK_MONOTONIC;
        frame.regs_mut().rsi = ts;
        let _ = with_user_process_context(table, || {
            crate::syscall::dispatch::dispatch_entry(entry, task, &mut frame)
        });
        frame.rax()
    };

    let Some(plain_ts) = map_user_rw_region(plain_table, 1) else {
        return fail_and_clean(&[plain_id, granted_id]);
    };
    let Some(granted_ts) = map_user_rw_region(granted_table, 1) else {
        return fail_and_clean(&[plain_id, granted_id]);
    };

    assert_eq_test!(
        call(plain_table, &plain, plain_ts),
        slopos_abi::Errno::EPERM.as_u64(),
        "an ordinary task must not set the wall clock"
    );
    assert_eq_test!(
        call(granted_table, &granted, granted_ts),
        slopos_abi::Errno::EINVAL.as_u64(),
        "a system task must reach clock_settime's own argument check"
    );

    drop(plain);
    drop(granted);
    task_terminate(plain_id);
    task_terminate(granted_id);
    pass!()
}

pub fn test_a_reaped_leader_does_not_strand_its_threads() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    let leader_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID && leader_id != INVALID_TASK_ID,
        "failed to create the parent and the leader"
    );
    assert_eq_test!(
        task_set_parent(leader_id, parent_id),
        0,
        "parenting the leader failed"
    );

    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let thread_id = match task_clone(
        &leader,
        None,
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        0,
        0,
        0,
        0,
    ) {
        Ok(id) => id,
        Err(_) => return fail_and_clean(&[leader_id, parent_id]),
    };
    drop(leader);

    task_terminate(leader_id);
    assert_some!(
        task_consume_zombie(leader_id),
        "the parent must be able to reap the leader"
    );
    assert_test!(
        task_find_by_id(leader_id).is_none(),
        "the reap must retire the leader's registration"
    );

    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");
    assert_eq_test!(
        thread.tgid,
        leader_id,
        "the surviving thread must still name the reaped leader's group"
    );

    assert_test!(
        task_group_stop(thread_id, SIGSTOP),
        "a leaderless group must still stop"
    );
    assert_test!(thread.is_stopped(), "the surviving thread must be Stopped");
    assert_eq_test!(
        thread.take_stop_report(),
        Some(SIGSTOP),
        "a leaderless group's stop report must land on a surviving member"
    );
    assert_test!(
        task_group_continue(thread_id),
        "a leaderless group must still resume"
    );
    drop(thread);

    assert_test!(
        task_group_exit(leader_id, 3) != 0,
        "exit_group must terminate a group whose leader was reaped"
    );
    assert_test!(
        task_find_by_id(thread_id).is_some_and(|thread| thread.is_killed()),
        "exit_group must kill its caller's group, not return having done nothing"
    );

    task_terminate(thread_id);
    task_terminate(parent_id);
    pass!()
}

/// A fatal signal one thread takes ends its whole group with that signal, so
/// the leader a parent waits on reports it rather than living on.
pub fn test_a_fatal_signal_ends_the_whole_group() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    assert_test!(
        task::task_group_fatal_signal(thread_id, SIGSEGV) != 0,
        "the fatal signal ended nobody"
    );
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    assert_test!(
        leader.is_killed(),
        "the leader outlived its thread's fatal signal"
    );
    assert_eq_test!(
        leader.exit_signal(),
        SIGSEGV,
        "the leader must report the signal its thread died of"
    );
    assert_eq_test!(
        TaskExitReason::from_u16(leader.exit_reason.load(Ordering::Acquire)),
        TaskExitReason::Signalled,
        "the leader's death must read as a signal"
    );

    drop(leader);
    task_terminate(thread_id);
    task_terminate(leader_id);
    pass!()
}

/// A sibling acts on a group exit's kill at its next delivery point, where a
/// signal it had pending must neither restamp the group's code nor be handled.
pub fn test_a_group_exit_outranks_a_signal_the_sibling_had_pending() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");
    let _ = task::task_signal_post(&thread, SIGTERM);

    assert_test!(
        task_group_exit(leader_id, 3) != 0,
        "exit_group must end the group"
    );
    assert_test!(thread.is_killed(), "the sibling must be killed");
    assert_test!(
        !crate::syscall::signal::claim_pending_signal_for_test(&thread),
        "delivery acted on a signal pending past the group exit"
    );
    assert_eq_test!(
        thread.exit_code.load(Ordering::Acquire),
        3,
        "the pending signal restamped the group's exit code"
    );

    drop(thread);
    task_terminate(thread_id);
    task_terminate(leader_id);
    pass!()
}

/// The kill flag is only acted on at a delivery point a *running* task
/// reaches, so a task left Stopped with it pending never dies.
pub fn test_a_completed_kill_is_not_parked_by_a_group_stop() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");

    // The interleaving the race produces: the kill has already posted its flag
    // and found the target not yet stopped, so its resume was a no-op.
    let _ = task::task_signal_post(&task, slopos_abi::signal::SIGKILL);
    task::task_kill_and_wake(&task);
    assert_test!(task.is_killed(), "the kill flag must be set");

    let _ = task_group_stop(task_id, SIGTSTP);
    assert_test!(
        !task.is_stopped(),
        "a killed task must not be parked by a concurrent stop"
    );
    assert_test!(
        task.is_killed(),
        "the kill must survive the stop that raced it"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// A member that is still executing is poked and parks at its own boundary,
/// which re-enters the stop — and a group already stopped changes nothing —
/// so neither may publish.
pub fn test_one_stop_publishes_one_report() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");

    // A member the stop can only poke: executing, and not the caller.
    assert_eq_test!(
        task_set_state(thread_id, TaskStatus::Running),
        0,
        "could not make the sibling executing"
    );

    assert_test!(
        task_group_stop(leader_id, SIGTSTP),
        "the stop must reach the group"
    );
    assert_test!(
        leader.is_stopped(),
        "the member that could be parked must be Stopped"
    );
    assert_test!(
        !leader.has_stop_report(),
        "a group with a member still running owes no report yet"
    );

    // The poked member reaching its own boundary and parking, which is where
    // the second report came from.
    assert_eq_test!(
        task_set_state(thread_id, TaskStatus::Ready),
        0,
        "could not return the sibling to Ready"
    );
    assert_test!(
        task_group_stop(thread_id, SIGTSTP),
        "the park must reach the group"
    );
    assert_test!(thread.is_stopped(), "the sibling must now be Stopped");
    assert_eq_test!(
        leader.take_stop_report(),
        Some(SIGTSTP),
        "the completed stop must publish exactly one report"
    );

    // A fan-out names one task per thread, so calls 2..N of a `kill(-pgid)`
    // land on a group that is already stopped.
    assert_test!(
        task_group_stop(leader_id, SIGTSTP),
        "a stop of an already-stopped group is idempotent, not a failure"
    );
    assert_test!(
        !leader.has_stop_report(),
        "an already-stopped group must not re-publish a consumed report"
    );

    drop(leader);
    drop(thread);
    task_terminate(thread_id);
    task_terminate(leader_id);
    pass!()
}

/// On-stack is a property of the interrupted stack pointer: a stored flag the
/// inner return retires would base the next frame at the top of the stack the
/// outer handler is still running on, overwriting it.
pub fn test_a_nested_sigreturn_keeps_the_outer_frame() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return TestResult::Fail;
    };

    let pages = MINSIGSTKSZ.div_ceil(4096) + 2;
    let Some(alt_base) = map_user_rw_region(table, pages) else {
        task_terminate(task_id);
        return TestResult::Fail;
    };
    let alt_size = (pages * 4096) as u64;
    task.set_sigaltstack(alt_base, alt_size);

    // `SA_NODEFER` on the outer signal so the third delivery below is not
    // masked by the second one's saved mask.
    assert_test!(
        install_action(task_id, SIGUSR1, SA_ONSTACK | SA_NODEFER)
            && install_action(task_id, SIGTERM, SA_ONSTACK),
        "installing the two handlers failed"
    );

    let main_rsp =
        process_vm_get_stack_top(table.process().expect("a live process")).wrapping_sub(0x200);
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = main_rsp;
    frame.regs_mut().rip = 0x5000_1111;

    assert_test!(task::task_signal_post(&task, SIGUSR1), "SIGUSR1 must pend");
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "the outer delivery failed"
    );
    let outer_frame = frame.rsp();
    assert_test!(
        outer_frame >= alt_base && outer_frame < alt_base + alt_size,
        "the outer frame must be based on the alternate stack"
    );

    assert_test!(task::task_signal_post(&task, SIGTERM), "SIGTERM must pend");
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "the nested delivery failed"
    );
    let nested_frame = frame.rsp();
    assert_test!(
        nested_frame < outer_frame && nested_frame >= alt_base,
        "a nested frame must be pushed below the frame in use, not over it"
    );

    // The inner handler returns: its `ret` pops the restorer word, so RSP is
    // the frame address plus eight.
    frame.regs_mut().rsp = nested_frame.wrapping_add(8);
    assert_test!(
        sigreturn_as_current(task_id, table, &mut frame),
        "the nested rt_sigreturn failed"
    );
    assert_eq_test!(
        frame.rsp(),
        outer_frame,
        "rt_sigreturn must restore the outer handler's stack pointer"
    );

    assert_test!(
        task::task_signal_post(&task, SIGUSR1),
        "SIGUSR1 must re-pend"
    );
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "the delivery after the nested return failed"
    );
    assert_eq_test!(
        frame.rsp(),
        nested_frame,
        "a frame pushed after a nested return must not land on the outer frame"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// Re-pending over `SIG_DFL` — `SA_RESETHAND` has already reverted the
/// disposition — would convert a caught signal into its default action, and a
/// push that keeps failing must terminate rather than be retried forever.
pub fn test_a_refused_frame_push_keeps_the_handler_then_terminates() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return TestResult::Fail;
    };

    assert_test!(
        install_action(task_id, SIGTERM, SA_RESETHAND),
        "installing a SIGTERM handler failed"
    );
    assert_test!(task::task_signal_post(&task, SIGTERM), "SIGTERM must pend");

    // Nothing is mapped under this stack pointer, so the frame cannot be
    // written. SIGTERM is not a fault signal, so the first failure defers.
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = 0x0000_7000_0000_0000;
    frame.regs_mut().rip = 0x5000_2222;
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "the first delivery attempt failed to run"
    );

    assert_eq_test!(
        frame.rip(),
        0x5000_2222,
        "a refused delivery must not redirect the task"
    );
    assert_test!(
        task.signal_pending() & sig_bit(SIGTERM) != 0,
        "a refused non-fault signal must be re-pended"
    );
    assert_eq_test!(
        task.signal_handler((SIGTERM - 1) as usize),
        Some(TEST_HANDLER),
        "a refused delivery must not leave SA_RESETHAND's reset in place"
    );

    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "the second delivery attempt failed to run"
    );
    let status = task.status();
    assert_test!(
        status == TaskStatus::Zombie || status == TaskStatus::Terminated,
        "a push that keeps failing must terminate the task, not retry forever"
    );
    assert_eq_test!(
        task.exit_signal(),
        SIGSEGV,
        "the forced death must be reported as a SIGSEGV death"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

/// A `CLONE_THREAD` child inherits the group leader's parent, is never a
/// `waitpid` candidate, and retires itself on exit rather than parking as a
/// zombie nothing would claim.
pub fn test_a_thread_is_not_its_creators_child() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    let leader_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID && leader_id != INVALID_TASK_ID,
        "failed to create the parent and the leader"
    );
    assert_eq_test!(
        task_set_parent(leader_id, parent_id),
        0,
        "parenting the leader failed"
    );

    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let Some(leader_table) = fdtable_of(leader_id) else {
        return fail_and_clean(&[leader_id, parent_id]);
    };
    let thread_id = match task_clone(
        &leader,
        None,
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        0,
        0,
        0,
        0,
    ) {
        Ok(id) => id,
        Err(_) => return fail_and_clean(&[leader_id, parent_id]),
    };

    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");
    assert_eq_test!(
        thread.parent_task_id(),
        parent_id,
        "a thread's real parent is its group leader's, not its creator"
    );
    assert_test!(
        !slopos_sched::task::task_has_children(leader_id),
        "a thread must not be its creator's child"
    );
    drop(thread);

    // On its own exit path, not deferred to a parent that will never call: a
    // thread zombie would pin a registry slot, a Task and two stacks forever.
    task_terminate(thread_id);
    assert_test!(
        task_find_by_id(thread_id).is_none(),
        "an exited thread must retire its own registration"
    );

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = -1i64 as u64;
    frame.regs_mut().rsi = 0;
    frame.regs_mut().rdx = slopos_abi::signal::WNOHANG as u64;
    let _ = with_user_process_context(leader_table, || {
        crate::syscall::dispatch::dispatch_handler(
            crate::syscall::process_handlers::syscall_wait4,
            &leader,
            &mut frame,
        )
    });
    assert_eq_test!(
        frame.rax(),
        slopos_abi::Errno::ECHILD.as_u64(),
        "waitpid in a thread's creator must not report the thread"
    );

    drop(leader);
    task_terminate(leader_id);
    task_terminate(parent_id);
    pass!()
}

/// `rt_sigreturn` restores the blocked mask from a sigframe the caller wrote,
/// so a user-authored word reaches `signal_blocked` directly. The kill flag
/// lives above `NSIG`, outside `SIGNAL_MASK`, and every reader masks before
/// looking — which is exactly why the writer has to: an unmasked store leaves
/// a kernel-private bit set in a field userland chose.
pub fn test_sigreturn_cannot_set_a_kernel_private_mask_bit() -> TestResult {
    let _fixture = SyscallFixture::new();

    const INTERRUPTED_RIP: u64 = 0x5000_7777;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return fail_and_clean(&[task_id]);
    };

    assert_test!(
        install_action(task_id, SIGUSR1, 0),
        "installing a SIGUSR1 handler failed"
    );
    assert_test!(task::task_signal_post(&task, SIGUSR1), "SIGUSR1 must pend");

    let stack_top = process_vm_get_stack_top(table.process().expect("a live process"));
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rsp = stack_top.wrapping_sub(0x200);
    frame.regs_mut().rip = INTERRUPTED_RIP;
    assert_test!(
        deliver_pending_signal_as_current(task_id, table, &frame),
        "delivering SIGUSR1 failed"
    );

    // The handler's `ret` pops the restorer word, so the frame the restorer
    // enters `rt_sigreturn` on sits eight bytes above the delivered RSP.
    let sigframe_addr = frame.rsp().wrapping_add(8);
    let Some(mut sigframe) = user_copy_in::<SignalFrame>(table, sigframe_addr) else {
        drop(task);
        return fail_and_clean(&[task_id]);
    };
    sigframe.saved_mask = u64::MAX;
    assert_test!(
        user_copy_out(table, sigframe_addr, &sigframe),
        "rewriting the sigframe's saved mask failed"
    );

    frame.regs_mut().rsp = sigframe_addr;
    assert_test!(
        sigreturn_as_current(task_id, table, &mut frame),
        "rt_sigreturn failed to run"
    );
    // A refused frame leaves the delivery's own mask in place, in which the
    // private bit is clear anyway — so the check below would pass for the
    // wrong reason without proving the restore committed.
    assert_eq_test!(
        frame.rip(),
        INTERRUPTED_RIP,
        "rt_sigreturn did not commit the frame"
    );

    assert_eq_test!(
        task.signal_blocked() & SIGNAL_KILLED,
        0,
        "a user-authored sigframe set the kernel-private kill flag in the blocked mask"
    );

    drop(task);
    task_terminate(task_id);
    pass!()
}

slopos_testing::stest!(
    name = test_group_stop_parks_every_thread_and_continue_resumes,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_stop_report_is_consumed_once,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_catchable_stop_signal_handler_wins_over_stop,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_kill_reaches_a_non_leader_thread,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_exit_info_reports_the_killing_signal,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_fault_signal_delivers_siginfo_with_fault_address,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_fault_signal_with_unpushable_frame_terminates,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigaltstack_bounds_and_onstack_delivery,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sa_resethand_restores_default_after_one_delivery,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_clone_sighand_shares_the_action_table,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_stopped_parent_still_parents_its_children,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_clone_from_releases_the_slots_own_signal_table,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_tty_hangup_continue_resumes_a_stopped_group,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_clock_settime_requires_the_clock_capability,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_reaped_leader_does_not_strand_its_threads,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_fatal_signal_ends_the_whole_group,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_group_exit_outranks_a_signal_the_sibling_had_pending,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_completed_kill_is_not_parked_by_a_group_stop,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_one_stop_publishes_one_report,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_nested_sigreturn_keeps_the_outer_frame,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_refused_frame_push_keeps_the_handler_then_terminates,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_thread_is_not_its_creators_child,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigreturn_cannot_set_a_kernel_private_mask_bit,
    suite = syscall_signal_build_floor
);

#[allow(dead_code)]
const _UNUSED: Ordering = Ordering::Relaxed;
