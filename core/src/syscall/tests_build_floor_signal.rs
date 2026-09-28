//! Coverage for the signal surface a build system needs.

use core::ffi::c_char;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use slopos_abi::quota::{QuotaMode, ResourceKind};
use slopos_abi::signal::{
    MINSIGSTKSZ, SA_NODEFER, SA_ONSTACK, SA_RESETHAND, SA_SIGINFO, SEGV_MAPERR, SI_ADDR_OFFSET,
    SIG_DFL, SIGCONT, SIGKILL, SIGSEGV, SIGSTOP, SIGTERM, SIGTSTP, SIGUSR1, SS_DISABLE, SS_ONSTACK,
    SignalFrame, UserSigAltStack, UserSiginfo, UserUcontext, sig_bit,
};
use slopos_abi::syscall::{
    CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE,
};
use slopos_abi::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_NAME_MAX_LEN,
    TaskExitReason, TaskStatus,
};
use slopos_fs::fileio::FdTable;
use slopos_kernel_services::driver_runtime::{signal_process_group, signal_session};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::oom::{Killed, OomOps, Standing, oom_forget_victim_for_test, oom_swap_ops};
use slopos_mm::page_fault::{FaultOutcome, try_resolve_user_fault};
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{
    create_process_vm, destroy_process_vm, pack_process_vm_handle, process_vm_alloc,
    process_vm_get_stack_top, process_vm_handle, process_vm_mmap,
};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_test_process_id};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::KBox;
use slopos_ostd::process::quota::{
    quota_mode, root as quota_root, set_limit, set_quota_mode, stats,
};
use slopos_ostd::process::{Process, ProcessId};
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
    deliver_pending_signal, deliver_pending_signal_on_irq_exit, syscall_kill,
    syscall_rt_sigpending, syscall_rt_sigprocmask, syscall_rt_sigqueueinfo, syscall_rt_sigreturn,
    syscall_rt_sigtimedwait, syscall_sigaltstack, syscall_tgkill,
};
use crate::tests::helpers::install_static_program;

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
        "kill(pid) must be pending for every thread of the group"
    );
    let taken_by_thread = thread.dequeue_signal(!thread.signal_blocked()).is_some();
    let taken_by_leader = leader.dequeue_signal(!leader.signal_blocked()).is_some();
    assert_test!(
        taken_by_thread && !taken_by_leader,
        "one kill(pid) must be taken by one thread, not by each"
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

/// A member SIGKILLed while its stop join is outstanding dies at its delivery
/// point instead of joining the stop.
pub fn test_a_kill_outranks_an_outstanding_stop_join() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let ids = [thread_id, leader_id];
    let (Some(leader), Some(thread)) = (task_find_by_id(leader_id), task_find_by_id(thread_id))
    else {
        return fail_and_clean(&ids);
    };
    let running = task_set_state(thread_id, TaskStatus::Running) == 0;
    let stopped = task_group_stop(leader_id, SIGTSTP);
    slopos_sched::task::task_sigkill_member(&thread, slopos_abi::signal::SigInfo::KERNEL);
    let ready = task_set_state(thread_id, TaskStatus::Ready) == 0;
    let claimed = crate::syscall::signal::claim_pending_signal_for_test(&thread);
    let kill_taken = thread.signal_pending() & sig_bit(slopos_abi::signal::SIGKILL) == 0;
    let thread_stopped = thread.is_stopped();
    let _ = task_group_continue(leader_id);
    drop((leader, thread));
    terminate_all(&ids);

    assert_test!(running && ready && stopped, "could not stage the group");
    assert_test!(claimed, "the delivery point must act on the kill");
    assert_test!(
        kill_taken,
        "the delivery point joined the stop, not the kill"
    );
    assert_test!(!thread_stopped, "a killed member must not park");
    pass!()
}

/// A poked member that exits instead of parking completes the stop when every
/// other member is already stopped.
pub fn test_a_poked_member_exiting_completes_the_stop() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let ids = [thread_id, leader_id];
    let Some(leader) = task_find_by_id(leader_id) else {
        return fail_and_clean(&ids);
    };
    let running = task_set_state(thread_id, TaskStatus::Running) == 0;
    let stopped = task_group_stop(leader_id, SIGTSTP);
    let early_report = leader.has_stop_report();
    let ready = task_set_state(thread_id, TaskStatus::Ready) == 0;
    task_terminate(thread_id);
    let report = leader.take_stop_report();
    let _ = task_group_continue(leader_id);
    drop(leader);
    terminate_all(&ids);

    assert_test!(running && ready && stopped, "could not stage the group");
    assert_test!(!early_report, "the report must wait for the poked member");
    assert_eq_test!(report, Some(SIGTSTP), "the completed stop must report");
    pass!()
}

/// A group stop reaches a running member that blocks the stop signal, as
/// Linux's does; the report waits for that member to park.
pub fn test_group_stop_reaches_a_member_blocking_the_signal() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let ids = [thread_id, leader_id];
    let (Some(leader), Some(thread)) = (task_find_by_id(leader_id), task_find_by_id(thread_id))
    else {
        return fail_and_clean(&ids);
    };
    thread.set_signal_blocked(sig_bit(SIGTSTP));
    let running = task_set_state(thread_id, TaskStatus::Running) == 0;
    let sent = task_group_signal(leader_id, SIGTSTP) != 0;
    let leader_stopped = leader.is_stopped();
    let early_report = leader.has_stop_report();
    let asked = slopos_sched::task::task_has_deliverable_signal(&thread);

    // The member's own return to user: the join is claimed through its mask,
    // and its park completes the stop.
    let ready = task_set_state(thread_id, TaskStatus::Ready) == 0;
    let claimed = crate::syscall::signal::claim_pending_signal_for_test(&thread);
    let parked = task_group_stop(thread_id, SIGTSTP);
    let thread_stopped = thread.is_stopped();
    let report = leader.take_stop_report();
    let resumed = task_group_continue(leader_id);
    drop((leader, thread));
    terminate_all(&ids);

    assert_test!(running && ready, "could not stage the sibling");
    assert_test!(sent, "SIGTSTP reached no member");
    assert_test!(leader_stopped, "the leader must be Stopped");
    assert_test!(!early_report, "the report must wait for the running member");
    assert_test!(asked, "the blocking member was never asked to stop");
    assert_test!(claimed, "the blocking member's boundary must take the stop");
    assert_test!(parked && thread_stopped, "the blocking member must park");
    assert_eq_test!(report, Some(SIGTSTP), "the stopped group must report");
    assert_test!(resumed, "SIGCONT must resume the group");
    pass!()
}

/// A stop signal every member blocks only pends: nothing stopped, so the
/// group's unconsumed `WCONTINUED` report stands.
pub fn test_a_pending_blocked_stop_keeps_the_continue_report() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let stopped = task_group_stop(task_id, SIGSTOP);
    let continued = task_group_continue(task_id);
    task.set_signal_blocked(sig_bit(SIGTSTP));
    let sent = task_group_signal(task_id, SIGTSTP) != 0;
    let still_running = !task.is_stopped();
    let pending = task.signal_pending() & sig_bit(SIGTSTP) != 0;
    let report = task.take_continue_report();
    drop(task);
    task_terminate(task_id);

    assert_test!(stopped && continued, "could not stage a continue report");
    assert_test!(sent, "SIGTSTP reached no member");
    assert_test!(still_running && pending, "a blocked SIGTSTP must only pend");
    assert_test!(
        report,
        "the continue report must survive a stop that stopped nothing"
    );
    pass!()
}

/// `tgkill` of a stop signal the named thread blocks pends on that thread and
/// wakes it, as every other `tgkill` does: it may have unblocked the signal
/// and gone to sleep since its mask was read.
pub fn test_a_directed_blocked_stop_wakes_its_target() -> TestResult {
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let ids = [thread_id, leader_id];
    let Some(thread) = task_find_by_id(thread_id) else {
        return fail_and_clean(&ids);
    };
    thread.set_signal_blocked(sig_bit(SIGTSTP));
    // Stand in for a published thread asleep in an interruptible wait; a wake
    // refuses a nascent one.
    let _ = slopos_sched::scheduler::clear_nascent_for_test(thread_id);
    let _ = task_set_state(thread_id, TaskStatus::Ready);
    let running = task_set_state(thread_id, TaskStatus::Running);
    let sleeping = task_set_state(thread_id, TaskStatus::Blocked);
    let blocked = running == 0 && sleeping == 0;
    let post = slopos_sched::task::task_thread_signal_info(
        leader_id,
        thread_id,
        SIGTSTP,
        slopos_abi::signal::SigInfo::KERNEL,
    );
    let status = thread.status();
    let pending = thread.signal_pending() & sig_bit(SIGTSTP) != 0;
    drop(thread);
    terminate_all(&ids);

    assert_test!(blocked, "could not put the target to sleep");
    assert_test!(post.is_some(), "tgkill found no target");
    assert_test!(pending, "the stop must pend on the named thread");
    assert_eq_test!(status, TaskStatus::Ready, "the target must be woken");
    pass!()
}

/// An unblocked stop signal set to `SIG_IGN` is discarded at the send.
pub fn test_an_ignored_stop_signal_is_discarded() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let ignored = task.set_signal_action(
        (SIGTSTP - 1) as usize,
        SignalAction {
            handler: slopos_abi::signal::SIG_IGN,
            mask: 0,
            flags: 0,
            restorer: 0,
        },
    );
    let reached = task_group_signal(task_id, SIGTSTP);
    let stopped = task.is_stopped();
    let pending = task.signal_pending() & sig_bit(SIGTSTP) != 0;
    let report = task.has_stop_report();
    drop(task);
    task_terminate(task_id);

    assert_test!(ignored, "installing SIG_IGN failed");
    assert_eq_test!(
        reached,
        1,
        "kill of an ignored signal still reaches the process"
    );
    assert_test!(!stopped, "an ignored SIGTSTP must not stop the process");
    assert_test!(
        !pending && !report,
        "an ignored SIGTSTP must leave no trace"
    );
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

/// A user-authored sigframe mask reaches `signal_blocked` whole — all 64 bits,
/// less the uncatchable pair — and no value of it marks the task killed.
pub fn test_sigreturn_restores_a_full_mask_without_the_kill_flag() -> TestResult {
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
        task.signal_blocked(),
        !slopos_abi::signal::SIG_UNCATCHABLE,
        "a full saved mask must restore every catchable signal, realtime included"
    );
    assert_test!(
        !task.is_killed(),
        "a user-authored sigframe marked the task killed"
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
    name = test_a_kill_outranks_an_outstanding_stop_join,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_poked_member_exiting_completes_the_stop,
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
    name = test_group_stop_reaches_a_member_blocking_the_signal,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_pending_blocked_stop_keeps_the_continue_report,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_directed_blocked_stop_wakes_its_target,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_an_ignored_stop_signal_is_discarded,
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
    name = test_sigreturn_restores_a_full_mask_without_the_kill_flag,
    suite = syscall_signal_build_floor
);

/// Run `handler` as `caller` with `args` (`rdi, rsi, rdx, r10`), returning RAX.
fn call_as(handler: crate::syscall::common::SyscallHandler, caller: u32, args: [u64; 4]) -> u64 {
    let (Some(task), Some(table)) = (task_find_by_id(caller), fdtable_of(caller)) else {
        return u64::MAX;
    };
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("frame alloc");
    {
        let regs = frame.regs_mut();
        regs.rdi = args[0];
        regs.rsi = args[1];
        regs.rdx = args[2];
        regs.r10 = args[3];
    }
    let _ = with_user_process_context(table, || {
        crate::syscall::dispatch::dispatch_handler(handler, &task, &mut frame)
    });
    frame.rax()
}

/// Realtime instances queue FIFO, each with its own record, lowest signal
/// first; a repeated standard signal keeps the first sender's record.
pub fn test_realtime_signals_queue_and_standard_ones_coalesce() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SI_USER, SIGRTMIN, SigInfo};
    use slopos_sched::task::{SignalPost, task_signal_post_info};
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let queued = |pid: u32, value: u64| SigInfo::sent(SI_QUEUE, pid, 0, value);

    let posts = [
        task_signal_post_info(&task, SIGRTMIN + 1, queued(7, 1)),
        task_signal_post_info(&task, SIGRTMIN, queued(7, 2)),
        task_signal_post_info(&task, SIGRTMIN + 1, queued(7, 3)),
        task_signal_post_info(&task, SIGRTMIN, queued(7, 4)),
        task_signal_post_info(&task, SIGUSR1, SigInfo::sent(SI_USER, 10, 0, 0)),
    ];
    let coalesced = task_signal_post_info(&task, SIGUSR1, SigInfo::sent(SI_USER, 11, 0, 0));
    let mut order = [(0u8, 0u32, 0u64); 5];
    for slot in order.iter_mut() {
        if let Some(taken) = task.dequeue_signal(u64::MAX) {
            *slot = (taken.signum, taken.info.pid, taken.info.value);
        }
    }
    let drained = task.dequeue_signal(u64::MAX).is_none() && task.signal_pending() == 0;
    drop(task);
    task_terminate(task_id);

    assert_test!(
        posts.iter().all(|p| *p == SignalPost::Pending),
        "every first instance must pend"
    );
    assert_eq_test!(
        coalesced,
        SignalPost::Dropped,
        "a second SIGUSR1 must coalesce"
    );
    assert_eq_test!(
        order[0],
        (SIGUSR1, 10, 0),
        "the standard signal, first sender's record"
    );
    assert_eq_test!(order[1], (SIGRTMIN, 7, 2), "SIGRTMIN's first instance");
    assert_eq_test!(order[2], (SIGRTMIN, 7, 4), "SIGRTMIN's second instance");
    assert_eq_test!(
        order[3],
        (SIGRTMIN + 1, 7, 1),
        "SIGRTMIN+1's first instance"
    );
    assert_eq_test!(
        order[4],
        (SIGRTMIN + 1, 7, 3),
        "SIGRTMIN+1's second instance"
    );
    assert_test!(drained, "five deliveries must drain the pending set");
    pass!()
}

/// The surplus past `SIGQUEUE_MAX` is refused until a delivery makes room.
pub fn test_realtime_queue_limit_refuses_the_surplus() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGQUEUE_MAX, SIGRTMAX, SigInfo};
    use slopos_sched::task::{SignalPost, task_signal_post_info};
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let info = SigInfo::sent(SI_QUEUE, 1, 0, 0);
    let mut accepted = 0;
    for _ in 0..SIGQUEUE_MAX {
        if task_signal_post_info(&task, SIGRTMAX, info) == SignalPost::Pending {
            accepted += 1;
        }
    }
    let surplus = task_signal_post_info(&task, SIGRTMAX, info);
    let delivered = task.dequeue_signal(u64::MAX).is_some();
    let after = task_signal_post_info(&task, SIGRTMAX, info);
    let held = task.queued_realtime_signals();
    drop(task);
    task_terminate(task_id);

    assert_eq_test!(
        accepted,
        SIGQUEUE_MAX,
        "the queue must take SIGQUEUE_MAX instances"
    );
    assert_eq_test!(
        surplus,
        SignalPost::QueueFull,
        "the surplus instance must be refused"
    );
    assert_test!(delivered, "a queued instance must deliver");
    assert_eq_test!(after, SignalPost::Pending, "a delivery must make room");
    assert_eq_test!(held, SIGQUEUE_MAX, "the queue must be full again");
    pass!()
}

/// `rt_sigqueueinfo` takes `si_code`/`si_value` but never `si_pid`, refusing
/// forged codes aimed elsewhere; `kill`/`tgkill` report `SI_USER`/`SI_TKILL`.
pub fn test_sent_signals_carry_the_real_sender() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SI_TKILL, SI_USER, SIGRTMIN, SigInfo, UserSiginfo};
    let _fixture = SyscallFixture::new();

    let sender = create_test_user_task();
    let target = create_test_user_task();
    if sender == INVALID_TASK_ID || target == INVALID_TASK_ID {
        return fail_and_clean(&[sender, target]);
    }
    let (Some(table), Some(target_task)) = (fdtable_of(sender), task_find_by_id(target)) else {
        return fail_and_clean(&[sender, target]);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        drop(target_task);
        return fail_and_clean(&[sender, target]);
    };
    let write_info = |code: i32| {
        let mut info =
            UserSiginfo::from_info(SIGRTMIN as i32, &SigInfo::sent(code, 999, 999, 0x1234_5678));
        info.si_errno = 0;
        user_copy_out(table, page, &info)
    };
    let queue = |sig: u8| {
        call_as(
            syscall_rt_sigqueueinfo,
            sender,
            [target as u64, sig as u64, page, 0],
        )
    };
    let eperm = slopos_abi::Errno::EPERM.as_u64();

    let forged_user = write_info(SI_USER) && queue(SIGRTMIN) == eperm;
    let forged_tkill = write_info(SI_TKILL) && queue(SIGRTMIN) == eperm;
    let queued = write_info(SI_QUEUE) && queue(SIGRTMIN) == 0;
    let taken = |task: &slopos_sched::task::TaskRef| {
        task.dequeue_signal(u64::MAX)
            .map(|taken| (taken.signum, taken.info))
    };
    let got_queued = taken(&target_task);
    let killed = call_as(syscall_kill, sender, [target as u64, SIGUSR1 as u64, 0, 0]) == 0;
    let got_kill = taken(&target_task);
    let tkilled = call_as(
        syscall_tgkill,
        sender,
        [target as u64, target as u64, SIGUSR1 as u64, 0],
    ) == 0;
    let got_tkill = taken(&target_task);
    let own = write_info(SI_USER)
        && call_as(
            syscall_rt_sigqueueinfo,
            sender,
            [sender as u64, SIGRTMIN as u64, page, 0],
        ) == 0;
    drop(target_task);
    task_terminate(sender);
    task_terminate(target);

    assert_test!(
        forged_user,
        "a forged SI_USER to another process must be EPERM"
    );
    assert_test!(
        forged_tkill,
        "a forged SI_TKILL to another process must be EPERM"
    );
    assert_test!(queued && killed && tkilled, "the legitimate sends failed");
    assert_test!(own, "a process may queue any code to itself");
    let q = SigInfo::sent(SI_QUEUE, sender, 0, 0x1234_5678);
    assert_eq_test!(got_queued, Some((SIGRTMIN, q)), "sigqueue's record");
    let k = SigInfo::sent(SI_USER, sender, 0, 0);
    assert_eq_test!(got_kill, Some((SIGUSR1, k)), "kill's record");
    let t = SigInfo::sent(SI_TKILL, sender, 0, 0);
    assert_eq_test!(got_tkill, Some((SIGUSR1, t)), "tgkill's record");
    pass!()
}

static OOM_VICTIM_PID: AtomicU32 = AtomicU32::new(INVALID_PROCESS_ID);
static OOM_CEILING: AtomicU32 = AtomicU32::new(u32::MAX);
static OOM_KILLS: AtomicU32 = AtomicU32::new(0);

/// The killer's task side: only the scratch victim may be taken, and taking it
/// frees what its exit would.
struct ScratchVictim;

static SCRATCH_VICTIM: ScratchVictim = ScratchVictim;

impl OomOps for ScratchVictim {
    fn standing(&self, process: &Process) -> Standing {
        if process.id() == OOM_VICTIM_PID.load(Ordering::Acquire) {
            Standing::Killable
        } else {
            Standing::Exempt
        }
    }

    fn kill(&self, process: &Process) -> Option<Killed> {
        if process.id() != OOM_VICTIM_PID.load(Ordering::Acquire) {
            return None;
        }
        OOM_KILLS.fetch_add(1, Ordering::AcqRel);
        set_limit(
            quota_root(),
            ResourceKind::CommitPages,
            OOM_CEILING.load(Ordering::Acquire),
        );
        if let Some(id) = ProcessId::resolve(process.id()) {
            destroy_process_vm(id);
        }
        Some(Killed {
            pid: process.id(),
            name: [0; TASK_NAME_MAX_LEN],
        })
    }
}

const USER_WRITE_TO_ABSENT_PAGE: u64 = 0x06;

/// An address space holding one written page, for the killer to choose.
fn scratch_victim() -> Option<ProcessId> {
    let id = ProcessId::resolve(create_process_vm())?;
    let addr = process_vm_mmap(
        id,
        0,
        4096,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE,
        -1,
        0,
    );
    let packed = pack_process_vm_handle(process_vm_handle(id)?);
    let written = addr != 0
        && try_resolve_user_fault(addr, USER_WRITE_TO_ABSENT_PAGE, packed, 1)
            == FaultOutcome::Resolved;
    if !written {
        destroy_process_vm(id);
        return None;
    }
    Some(id)
}

fn is_cow_in(table: FdTable, addr: u64) -> bool {
    table.process().is_some_and(|process| {
        slopos_mm::process_vm::process_vm_with_vm_space(process, |vs| {
            slopos_mm::user_mappings::ostd_get_pte_flags_4kb(
                vs,
                slopos_abi::addr::VirtAddr::new(addr),
            )
        })
        .flatten()
        .is_some_and(|flags| flags.contains(PageFlags::COW))
    })
}

#[derive(Clone, Copy)]
enum Delivery {
    SyscallExit,
    IrqExit,
}

/// Deliver as a trap's way out does, one level into interrupt nesting with
/// interrupts masked, then restore the bootstrap current-task pointer.
#[must_use]
fn deliver_on_irq_exit_as_current(
    task_id: u32,
    table: FdTable,
    frame: &mut slopos_arch::InterruptFrame,
) -> bool {
    use slopos_arch::cpu;
    use slopos_ostd::cpu::x86_64::pcr::{interrupt_nesting_enter, interrupt_nesting_exit};

    if !make_task_current(task_id) {
        return false;
    }
    let frame = ptr::from_mut(frame);
    let delivered = with_user_process_context(table, || {
        let unmasked = cpu::are_interrupts_enabled();
        cpu::disable_interrupts();
        interrupt_nesting_enter();
        deliver_pending_signal_on_irq_exit(frame);
        interrupt_nesting_exit();
        if unmasked {
            cpu::enable_interrupts();
        }
    })
    .is_some();
    park_bootstrap_on_current_cpu();
    delivered
}

/// With the commit ceiling full, a forked child's frame push onto a
/// still-shared stack page waits for the OOM killer instead of ending in
/// `SIGSEGV`.
pub fn test_sigframe_on_a_shared_forked_stack_waits_for_the_killer() -> TestResult {
    sigframe_on_a_shared_forked_stack(Delivery::SyscallExit)
}

fn sigframe_on_a_shared_forked_stack(delivery: Delivery) -> TestResult {
    const INTERRUPTED_RIP: u64 = 0x5000_7777;
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create the parent");
    let parent = assert_some!(task_find_by_id(parent_id), "parent lookup failed");
    let Some(parent_table) = fdtable_of(parent_id) else {
        drop(parent);
        return fail_and_clean(&[parent_id]);
    };
    let stack_top = process_vm_get_stack_top(parent_table.process().expect("a live process"));
    let rsp = stack_top.wrapping_sub(0x200);
    let frame_page = crate::syscall::signal::sigframe_base_for_stack_top(rsp) & !0xFFF;
    let parent_wrote = (frame_page..rsp)
        .step_by(4096)
        .all(|page| user_copy_out(parent_table, page, &0u64));

    let child_id = task_fork(&parent, None);
    drop(parent);
    if !parent_wrote || child_id == INVALID_TASK_ID {
        return fail_and_clean(&[parent_id]);
    }
    task_set_state(child_id, TaskStatus::Blocked);
    let child = assert_some!(task_find_by_id(child_id), "child lookup failed");
    let Some(child_table) = fdtable_of(child_id) else {
        drop(child);
        return fail_and_clean(&[child_id, parent_id]);
    };
    let shared = is_cow_in(child_table, frame_page);
    let Some(victim) = scratch_victim() else {
        drop(child);
        return fail_and_clean(&[child_id, parent_id]);
    };
    if !shared || !install_action(child_id, SIGUSR1, 0) || !task::task_signal_post(&child, SIGUSR1)
    {
        destroy_process_vm(victim);
        drop(child);
        return fail_and_clean(&[child_id, parent_id]);
    }

    let mut ctx: KBox<UserContext> = KBox::zeroed().expect("alloc");
    ctx.regs_mut().rsp = rsp;
    ctx.regs_mut().rip = INTERRUPTED_RIP;
    let mut trap: KBox<slopos_arch::InterruptFrame> = KBox::zeroed().expect("alloc");
    trap.rip = INTERRUPTED_RIP;
    trap.rsp = rsp;
    trap.cs = 0x23;
    trap.ss = 0x1B;
    trap.rflags = 0x202;

    OOM_VICTIM_PID.store(victim.id(), Ordering::Release);
    OOM_KILLS.store(0, Ordering::Release);
    let restore_mode = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    let commit = stats(quota_root(), ResourceKind::CommitPages);
    let ceiling = commit.map_or(u32::MAX, |s| s.limit);
    OOM_CEILING.store(ceiling, Ordering::Release);
    let restore_ops = oom_swap_ops(Some(&SCRATCH_VICTIM));
    oom_forget_victim_for_test();
    set_limit(
        quota_root(),
        ResourceKind::CommitPages,
        commit.map_or(0, |s| s.used),
    );

    let (delivered, resumed_at) = match delivery {
        Delivery::SyscallExit => (
            deliver_pending_signal_as_current(child_id, child_table, &ctx),
            ctx.rip(),
        ),
        Delivery::IrqExit => (
            deliver_on_irq_exit_as_current(child_id, child_table, &mut trap),
            trap.rip,
        ),
    };

    set_limit(quota_root(), ResourceKind::CommitPages, ceiling);
    oom_forget_victim_for_test();
    oom_swap_ops(restore_ops);
    set_quota_mode(restore_mode);
    OOM_VICTIM_PID.store(INVALID_PROCESS_ID, Ordering::Release);
    destroy_process_vm(victim);
    let kills = OOM_KILLS.load(Ordering::Acquire);
    let child_killed = child.is_killed();
    let broken = !is_cow_in(child_table, frame_page);
    drop(child);
    task_terminate(child_id);
    task_terminate(parent_id);

    assert_test!(delivered, "the delivery did not run");
    assert_eq_test!(
        resumed_at,
        TEST_HANDLER,
        "the frame push was refused instead of waiting for the killer"
    );
    assert_eq_test!(kills, 1, "the killer must take exactly one victim");
    assert_test!(
        broken && !child_killed,
        "after the kill the child's stack page is still shared: {}, the child killed: {}",
        !broken,
        child_killed
    );
    pass!()
}

const FAULTER_PATH: &[u8] = b"/tmp/abandoned_trap_faulter";
/// `ud2`: the task dies inside its own `#UD` handler.
const FAULTER: [u8; 2] = [0x0f, 0x0b];
const SPINNER_PATH: &[u8] = b"/tmp/abandoned_trap_spinner";
/// `setpgid(0, 0)`, then `jmp $`: past the syscall it enters the kernel only
/// through an interrupt.
const SPINNER: [u8; 13] = [
    0x31, 0xff, // xor edi, edi
    0x31, 0xf6, // xor esi, esi
    0xb8, 0x6d, 0x00, 0x00, 0x00, // mov eax, SYS_setpgid
    0x0f, 0x05, // syscall
    0xeb, 0xfe, // jmp $
];
const PROBE_BUDGET_MS: u64 = 5_000;

fn wait_until(done: impl Fn() -> bool) -> bool {
    use slopos_kernel_services::platform::get_time_ms;
    let deadline = get_time_ms().saturating_add(PROBE_BUDGET_MS);
    while !done() && get_time_ms() < deadline {
        slopos_sched::scheduler::sleep_current_task_ms(1);
    }
    done()
}

/// Run `path` until it dies, `kill` ending it if it would not end itself, and
/// reap it. Answers the CPU it died on.
fn run_probe_to_death(
    path: &[u8],
    parent: u32,
    kill: fn(&slopos_sched::task_struct::Task) -> bool,
) -> Option<usize> {
    let pid = crate::exec::spawn_program_with_attrs(
        path,
        None,
        None,
        slopos_abi::task::TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &[],
        0,
        None,
        parent,
    )
    .ok()?;
    let probe = task_find_by_id(pid)?;
    let died = kill(&probe) && wait_until(|| probe.exit_info_is_set());
    let cpu = probe.last_cpu() as usize;
    drop(probe);
    if !died {
        slopos_ostd::klog_info!("ABANDONED_TRAP: the probe never died");
        task_terminate(pid);
    }
    let _ = task_consume_zombie(pid);
    died.then_some(cpu)
}

/// Send `SIGKILL` as the OOM killer does once the spinner is past its syscall,
/// then kick its CPU, so the kill is taken on an interrupt's way out.
fn kill_once_spinning(spinner: &slopos_sched::task_struct::Task) -> bool {
    use slopos_kernel_services::platform::get_time_ms;
    let spinning = wait_until(|| spinner.pgid() == spinner.task_id);
    let settled = get_time_ms().saturating_add(20);
    wait_until(|| get_time_ms() >= settled);
    if !spinning || task::task_group_signal(spinner.task_id, SIGKILL) == 0 {
        return false;
    }
    slopos_sched::scheduler::send_reschedule_ipi(spinner.last_cpu() as usize);
    true
}

/// Sampled, since an interrupt the CPU is taking right now is a level too.
fn leaves_interrupt_nesting(cpu: usize) -> bool {
    let depth = || {
        slopos_ostd::cpu::x86_64::pcr::get_pcr(cpu).map_or(u32::MAX, |pcr| {
            pcr.interrupt_nesting.load(Ordering::Acquire)
        })
    };
    if wait_until(|| depth() == 0) {
        return true;
    }
    slopos_ostd::klog_info!(
        "ABANDONED_TRAP: CPU {} stays {} deep in interrupt nesting",
        cpu,
        depth()
    );
    false
}

/// A task dying inside a trap never leaves its interrupt nesting; its CPU must
/// still leave interrupt context, and a later frame push at trap depth on a
/// shared stack still waits for the OOM killer.
pub fn test_an_abandoned_trap_leaves_its_cpu_out_of_interrupt_nesting() -> TestResult {
    let parent = Current::get().map_or(INVALID_TASK_ID, |current| current.id());
    assert_test!(
        install_static_program(FAULTER_PATH, &FAULTER)
            && install_static_program(SPINNER_PATH, &SPINNER),
        "could not write the probes"
    );
    let killed_on = run_probe_to_death(SPINNER_PATH, parent, kill_once_spinning);
    let killed_left = killed_on.is_some_and(leaves_interrupt_nesting);
    let faulted_on = run_probe_to_death(FAULTER_PATH, parent, |_| true);
    let faulted_left = faulted_on.is_some_and(leaves_interrupt_nesting);
    let _ = slopos_fs::vfs::vfs_unlink(FAULTER_PATH);
    let _ = slopos_fs::vfs::vfs_unlink(SPINNER_PATH);

    assert_test!(killed_on.is_some(), "the spinner was not killed");
    assert_test!(
        killed_left,
        "a kill on an interrupt's way out left CPU {:?} in interrupt nesting",
        killed_on
    );
    assert_test!(faulted_on.is_some(), "the faulter did not die");
    assert_test!(
        faulted_left,
        "a task dead of its own #UD left CPU {:?} in interrupt nesting",
        faulted_on
    );
    sigframe_on_a_shared_forked_stack(Delivery::IrqExit)
}

slopos_testing::stest!(
    name = test_realtime_signals_queue_and_standard_ones_coalesce,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_realtime_queue_limit_refuses_the_surplus,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sent_signals_carry_the_real_sender,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigframe_on_a_shared_forked_stack_waits_for_the_killer,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_an_abandoned_trap_leaves_its_cpu_out_of_interrupt_nesting,
    suite = syscall_signal_build_floor
);

/// A process of `N` threads: the leader, then `N - 1` `CLONE_THREAD` siblings.
fn spawn_threads<const N: usize>() -> Option<[u32; N]> {
    let mut ids = [INVALID_TASK_ID; N];
    ids[0] = create_test_user_task();
    let leader = task_find_by_id(ids[0])?;
    for slot in ids.iter_mut().skip(1) {
        *slot = task_clone(
            &leader,
            None,
            CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
            0,
            0,
            0,
            0,
        )
        .unwrap_or(INVALID_TASK_ID);
    }
    drop(leader);
    if ids.contains(&INVALID_TASK_ID) {
        terminate_all(&ids);
        return None;
    }
    Some(ids)
}

fn terminate_all(ids: &[u32]) {
    for &id in ids.iter().filter(|&&id| id != INVALID_TASK_ID) {
        task_terminate(id);
    }
}

/// `sigqueue(pid, signum, value)` from `sender`, whose scratch word at `page`
/// carries the record. Returns RAX.
fn sigqueue_as(sender: u32, page: u64, pid: u32, signum: u8, value: u64) -> u64 {
    use slopos_abi::signal::{SI_QUEUE, SigInfo};
    let Some(table) = fdtable_of(sender) else {
        return u64::MAX;
    };
    let info = UserSiginfo::from_info(signum as i32, &SigInfo::sent(SI_QUEUE, 0, 0, value));
    if !user_copy_out(table, page, &info) {
        return u64::MAX;
    }
    call_as(
        syscall_rt_sigqueueinfo,
        sender,
        [pid as u64, signum as u64, page, 0],
    )
}

/// What `task` takes next with its own mask, as `(signal, value)`.
fn take_next(task_id: u32) -> Option<(u8, u64)> {
    let task = task_find_by_id(task_id)?;
    task.dequeue_signal(!task.signal_blocked())
        .map(|taken| (taken.signum, taken.info.value))
}

pub fn test_one_sigqueue_to_a_process_is_taken_once() -> TestResult {
    use slopos_abi::signal::SIGRTMIN;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<3>() else {
        return TestResult::Fail;
    };
    let Some(page) = fdtable_of(ids[0]).and_then(|table| map_user_rw_region(table, 1)) else {
        return fail_and_clean(&ids);
    };
    // Sent by a sibling, as a watchdog thread does: the leader the pid names
    // is the one picked, not the sender that reaches a boundary first.
    let sent = sigqueue_as(ids[1], page, ids[0], SIGRTMIN, 0x5eed);
    let picked = ids.map(|id| {
        task_find_by_id(id).is_some_and(|t| slopos_sched::task::task_has_deliverable_signal(&t))
    });
    let taken = ids.map(take_next);
    terminate_all(&ids);

    assert_eq_test!(sent, 0, "sigqueue to the process failed");
    assert_eq_test!(
        picked,
        [true, false, false],
        "only the named thread may act on it"
    );
    let takers = taken.iter().flatten().count();
    assert_eq_test!(takers, 1, "each thread took its own copy of one sigqueue");
    assert_eq_test!(
        taken.iter().flatten().next().copied(),
        Some((SIGRTMIN, 0x5eed)),
        "the taker must see the queued value"
    );
    pass!()
}

/// Forty `sigqueue`s (past `SIGQUEUE_MAX`) to a process whose other threads
/// block `SIGRTMIN` all succeed, and the unblocked thread takes each in order.
pub fn test_blocked_threads_leave_process_signals_to_the_unblocked_one() -> TestResult {
    use slopos_abi::signal::SIGRTMIN;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<3>() else {
        return TestResult::Fail;
    };
    let [leader, worker, handler] = ids;
    let Some(page) = fdtable_of(leader).and_then(|table| map_user_rw_region(table, 1)) else {
        return fail_and_clean(&ids);
    };
    for id in [leader, worker] {
        if let Some(task) = task_find_by_id(id) {
            task.set_signal_blocked(sig_bit(SIGRTMIN));
        }
    }

    let (mut sent, mut in_order, mut elsewhere) = (0usize, 0usize, 0usize);
    for value in 0..40u64 {
        if sigqueue_as(leader, page, leader, SIGRTMIN, value) == 0 {
            sent += 1;
        }
        if take_next(handler) == Some((SIGRTMIN, value)) {
            in_order += 1;
        }
        elsewhere += [leader, worker]
            .map(|id| take_next(id).is_some() as usize)
            .iter()
            .sum::<usize>();
    }
    let parked = [leader, worker]
        .map(|id| task_find_by_id(id).map_or(0, |task| task.queued_realtime_signals()));
    terminate_all(&ids);

    assert_eq_test!(sent, 40, "a blocked thread's backlog refused a sigqueue");
    assert_eq_test!(
        in_order,
        40,
        "the unblocked thread must take each, in order"
    );
    assert_eq_test!(elsewhere, 0, "a thread blocking the signal took it");
    assert_eq_test!(
        parked,
        [0, 0],
        "an instance was parked on a blocking thread"
    );
    pass!()
}

pub fn test_a_signal_every_thread_blocks_goes_to_the_first_to_unblock() -> TestResult {
    use slopos_abi::signal::SIGRTMIN;
    use slopos_sched::task::task_has_deliverable_signal;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<3>() else {
        return TestResult::Fail;
    };
    let Some(page) = fdtable_of(ids[0]).and_then(|table| map_user_rw_region(table, 1)) else {
        return fail_and_clean(&ids);
    };
    let threads = ids.map(task_find_by_id);
    let bit = sig_bit(SIGRTMIN);
    for task in threads.iter().flatten() {
        task.set_signal_blocked(bit);
    }
    let sent = sigqueue_as(ids[0], page, ids[0], SIGRTMIN, 77);
    let pending_for_all = threads
        .iter()
        .all(|t| t.as_ref().is_some_and(|t| t.signal_pending() & bit != 0));
    let deliverable_before = threads
        .iter()
        .flatten()
        .filter(|t| task_has_deliverable_signal(t))
        .count();
    let blocked_takes = ids.map(take_next);

    if let Some(unblocker) = &threads[2] {
        unblocker.set_signal_blocked(0);
    }
    let deliverable_after = threads
        .each_ref()
        .map(|t| t.as_ref().is_some_and(|t| task_has_deliverable_signal(t)));
    let taken = take_next(ids[2]);
    let left = threads
        .iter()
        .flatten()
        .any(|t| t.signal_pending() & bit != 0);
    drop(threads);
    terminate_all(&ids);

    assert_eq_test!(sent, 0, "sigqueue to the process failed");
    assert_test!(
        pending_for_all,
        "the signal must be pending for every thread"
    );
    assert_eq_test!(deliverable_before, 0, "a thread blocking it could take it");
    assert_eq_test!(blocked_takes, [None; 3], "a blocking thread took it");
    assert_eq_test!(
        deliverable_after,
        [false, false, true],
        "only the thread that unblocked it may take it"
    );
    assert_eq_test!(taken, Some((SIGRTMIN, 77)), "the unblocker must take it");
    assert_test!(!left, "a taken instance must leave the process");
    pass!()
}

/// Past the limit a `kill` pends without its record (`SI_USER`, pid 0, as on
/// Linux); `sigqueue` and `tgkill` get `EAGAIN`.
pub fn test_only_a_kill_pends_past_the_queue_limit() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SI_USER, SIGQUEUE_MAX, SIGRTMAX, SIGRTMIN, SigInfo};
    use slopos_sched::task::{SignalPost, task_signal_post_info};
    let _fixture = SyscallFixture::new();

    let sender = create_test_user_task();
    let target = create_test_user_task();
    let ids = [sender, target];
    if ids.contains(&INVALID_TASK_ID) {
        return fail_and_clean(&ids);
    }
    let Some(page) = fdtable_of(sender).and_then(|table| map_user_rw_region(table, 1)) else {
        return fail_and_clean(&ids);
    };
    let Some(target_task) = task_find_by_id(target) else {
        return fail_and_clean(&ids);
    };
    let eagain = slopos_abi::Errno::EAGAIN.as_u64();

    let filled = (0..SIGQUEUE_MAX)
        .filter(|&i| sigqueue_as(sender, page, target, SIGRTMAX, i as u64) == 0)
        .count();
    let queued_past = sigqueue_as(sender, page, target, SIGRTMAX, 99);
    let kill = |signum: u8| call_as(syscall_kill, sender, [target as u64, signum as u64, 0, 0]);
    let killed_same = kill(SIGRTMAX);
    let killed_other = kill(SIGRTMAX - 1);
    let kill_pending = target_task.signal_pending() & sig_bit(SIGRTMAX - 1) != 0;
    let lost = target_task
        .dequeue_signal(sig_bit(SIGRTMAX - 1))
        .map(|taken| taken.info);

    let own_filled = (0..SIGQUEUE_MAX)
        .filter(|_| {
            task_signal_post_info(&target_task, SIGRTMIN, SigInfo::sent(SI_QUEUE, 1, 0, 0))
                == SignalPost::Pending
        })
        .count();
    let tkilled_past = call_as(
        syscall_tgkill,
        sender,
        [target as u64, target as u64, SIGRTMIN as u64, 0],
    );
    drop(target_task);
    terminate_all(&ids);

    assert_eq_test!(
        filled,
        SIGQUEUE_MAX,
        "the process queue must take SIGQUEUE_MAX"
    );
    assert_eq_test!(
        queued_past,
        eagain,
        "a sigqueue past the limit must be EAGAIN"
    );
    assert_eq_test!(killed_same, 0, "a kill past the limit must succeed");
    assert_eq_test!(killed_other, 0, "a kill of another RT signal must succeed");
    assert_test!(kill_pending, "the kill past the limit must pend");
    assert_eq_test!(
        lost,
        Some(SigInfo::sent(SI_USER, 0, 0, 0)),
        "an instance past the limit must report a kill from no one, not the kernel"
    );
    assert_eq_test!(
        own_filled,
        SIGQUEUE_MAX,
        "the thread queue must take SIGQUEUE_MAX"
    );
    assert_eq_test!(
        tkilled_past,
        eagain,
        "a tgkill past the limit must be EAGAIN"
    );
    pass!()
}

/// A requeued instance goes back ahead of those queued behind it meanwhile.
pub fn test_a_requeued_instance_survives_a_refilled_queue() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGQUEUE_MAX, SIGRTMIN, SigInfo};
    use slopos_sched::task::{SignalPost, task_signal_post_info};
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    let Some(task) = task_find_by_id(task_id) else {
        return fail_and_clean(&[task_id]);
    };
    let post = |value: u64| {
        task_signal_post_info(&task, SIGRTMIN, SigInfo::sent(SI_QUEUE, 1, 0, value))
            == SignalPost::Pending
    };
    let filled = (0..SIGQUEUE_MAX as u64).filter(|&v| post(v)).count();
    let taken = task.dequeue_signal(u64::MAX);
    let refilled = post(100);
    let put_back = taken
        .as_ref()
        .is_some_and(|taken| task.requeue_signal(taken));
    let mut order = [u64::MAX; SIGQUEUE_MAX + 2];
    let mut drained = 0usize;
    for slot in order.iter_mut() {
        let Some(next) = task.dequeue_signal(u64::MAX) else {
            break;
        };
        *slot = next.info.value;
        drained += 1;
    }
    drop(task);
    task_terminate(task_id);

    assert_eq_test!(filled, SIGQUEUE_MAX, "the queue must fill");
    assert_test!(
        refilled,
        "the slot a delivery freed must take a new instance"
    );
    assert_test!(put_back, "the reserve slot must take the instance back");
    assert_eq_test!(drained, SIGQUEUE_MAX + 1, "an instance was lost");
    let expected = (0..SIGQUEUE_MAX as u64).chain([100]);
    assert_test!(
        order
            .iter()
            .copied()
            .zip(expected)
            .all(|(got, want)| got == want),
        "the requeued instance must come back first"
    );
    pass!()
}

/// Of two threads putting back into a full process queue only one gets its
/// reserve slot, and no instance a sender was told had queued is lost.
pub fn test_a_second_put_back_into_a_full_process_queue_is_refused() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGQUEUE_MAX, SIGRTMIN, SigInfo};
    use slopos_ostd::task::ops::task_group_post_info;
    use slopos_sched::task::SignalPost;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<2>() else {
        return TestResult::Fail;
    };
    let (Some(first), Some(second)) = (task_find_by_id(ids[0]), task_find_by_id(ids[1])) else {
        return fail_and_clean(&ids);
    };
    let post = |value: u64| {
        task_group_post_info(&first, SIGRTMIN, SigInfo::sent(SI_QUEUE, 1, 0, value))
            == SignalPost::Pending
    };
    let filled = (0..SIGQUEUE_MAX as u64).filter(|&v| post(v)).count();
    let taken = [
        first.dequeue_signal(u64::MAX),
        second.dequeue_signal(u64::MAX),
    ];
    let refilled = [post(100), post(101)];
    let put_back = [
        taken[0].as_ref().is_some_and(|t| first.requeue_signal(t)),
        taken[1].as_ref().is_some_and(|t| second.requeue_signal(t)),
    ];
    let mut order = [u64::MAX; SIGQUEUE_MAX + 2];
    let mut drained = 0usize;
    for slot in order.iter_mut() {
        let Some(next) = first.dequeue_signal(u64::MAX) else {
            break;
        };
        *slot = next.info.value;
        drained += 1;
    }
    drop((first, second));
    terminate_all(&ids);

    assert_eq_test!(filled, SIGQUEUE_MAX, "the process queue must fill");
    assert_eq_test!(
        refilled,
        [true, true],
        "the slots two deliveries freed must take new instances"
    );
    assert_eq_test!(
        put_back,
        [true, false],
        "only one put-back fits the reserve slot"
    );
    assert_eq_test!(drained, SIGQUEUE_MAX + 1, "an accepted instance was lost");
    let expected = [0]
        .into_iter()
        .chain(2..SIGQUEUE_MAX as u64)
        .chain([100, 101]);
    assert_test!(
        order
            .iter()
            .copied()
            .zip(expected)
            .all(|(got, want)| got == want),
        "the put-back must come first and every accepted instance follow"
    );
    pass!()
}

/// A picked thread that exits before the pick lands does not take the signal
/// with it: the pick moves to a sibling that does not block it.
pub fn test_a_pick_landing_after_the_takers_exit_moves_to_a_sibling() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGRTMIN, SigInfo};
    use slopos_ostd::task::ops::task_group_post_info;
    use slopos_sched::task::{
        SignalPost, task_has_deliverable_signal, task_pick_for_shared_signals,
    };
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<3>() else {
        return TestResult::Fail;
    };
    let [leader, chosen, sibling] = ids;
    let (Some(leader_task), Some(chosen_task), Some(sibling_task)) = (
        task_find_by_id(leader),
        task_find_by_id(chosen),
        task_find_by_id(sibling),
    ) else {
        return fail_and_clean(&ids);
    };
    let bit = sig_bit(SIGRTMIN);
    leader_task.set_signal_blocked(bit);
    let posted = task_group_post_info(
        &leader_task,
        SIGRTMIN,
        SigInfo::sent(SI_QUEUE, 1, 0, 0x7a11),
    );
    task_terminate(chosen);
    let exited = chosen_task.is_exited();
    task_pick_for_shared_signals(&chosen_task, bit);
    let deliverable = [&leader_task, &sibling_task].map(|task| task_has_deliverable_signal(task));
    drop((leader_task, chosen_task, sibling_task));
    terminate_all(&[leader, sibling]);

    assert_eq_test!(
        posted,
        SignalPost::Pending,
        "the post to the process failed"
    );
    assert_test!(exited, "the chosen thread must have exited");
    assert_eq_test!(
        deliverable,
        [false, true],
        "the signal must pass to the sibling that does not block it"
    );
    pass!()
}

/// A forked process gets its own pending set, shared by its threads: a signal
/// sent to it is not pending for its parent.
pub fn test_a_forked_process_shares_its_own_signal_set_with_its_threads() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGRTMIN, SigInfo};
    use slopos_ostd::task::ops::task_group_post_info;
    use slopos_sched::task::SignalPost;
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    let Some(parent) = task_find_by_id(parent_id) else {
        return fail_and_clean(&[parent_id]);
    };
    let child_id = task_fork(&parent, None);
    drop(parent);
    let thread_id = task_find_by_id(child_id)
        .and_then(|child| {
            task_clone(
                &child,
                None,
                CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
                0,
                0,
                0,
                0,
            )
            .ok()
        })
        .unwrap_or(INVALID_TASK_ID);
    let ids = [thread_id, child_id, parent_id];
    if ids.contains(&INVALID_TASK_ID) {
        return fail_and_clean(&ids);
    }
    let posted = task_find_by_id(child_id)
        .map(|child| task_group_post_info(&child, SIGRTMIN, SigInfo::sent(SI_QUEUE, 1, 0, 0)));
    let bit = sig_bit(SIGRTMIN);
    let pending =
        ids.map(|id| task_find_by_id(id).is_some_and(|task| task.signal_pending() & bit != 0));
    terminate_all(&ids);

    assert_eq_test!(
        posted,
        Some(SignalPost::Pending),
        "the post to the forked process failed"
    );
    assert_eq_test!(
        pending,
        [true, true, false],
        "the forked process and its thread must share one set of their own"
    );
    pass!()
}

/// As a `kill` to itself would: `SI_USER` and the writer's own pid.
pub fn test_sigpipe_names_the_writer_as_sender() -> TestResult {
    use slopos_abi::signal::{SI_USER, SIGPIPE, SigInfo};
    use slopos_fs::fileio::{file_close_fd, file_pipe_create};
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    let (Some(task), Some(table)) = (task_find_by_id(task_id), fdtable_of(task_id)) else {
        return fail_and_clean(&[task_id]);
    };
    let (mut read_fd, mut write_fd) = (-1, -1);
    let piped = file_pipe_create(table, 0, &mut read_fd, &mut write_fd) == 0
        && file_close_fd(table, read_fd) == 0;
    let page = map_user_rw_region(table, 1).unwrap_or(0);
    let wrote = call_as(
        crate::syscall::fs::path_handlers::syscall_write,
        task_id,
        [write_fd as u64, page, 8, 0],
    );
    let pid = call_as(
        crate::syscall::process_handlers::syscall_getpid,
        task_id,
        [0; 4],
    );
    let taken = task
        .dequeue_signal(sig_bit(SIGPIPE))
        .map(|taken| (taken.signum, taken.info));
    let _ = file_close_fd(table, write_fd);
    drop(task);
    task_terminate(task_id);

    assert_test!(piped && page != 0, "could not build the broken pipe");
    assert_eq_test!(
        wrote,
        slopos_abi::Errno::EPIPE.as_u64(),
        "the write must fail EPIPE"
    );
    assert_eq_test!(
        taken,
        Some((SIGPIPE, SigInfo::sent(SI_USER, pid as u32, 0, 0))),
        "SIGPIPE must be SI_USER from the writer's own pid"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_one_sigqueue_to_a_process_is_taken_once,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_blocked_threads_leave_process_signals_to_the_unblocked_one,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_signal_every_thread_blocks_goes_to_the_first_to_unblock,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_only_a_kill_pends_past_the_queue_limit,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_requeued_instance_survives_a_refilled_queue,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_second_put_back_into_a_full_process_queue_is_refused,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_pick_landing_after_the_takers_exit_moves_to_a_sibling,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_forked_process_shares_its_own_signal_set_with_its_threads,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigpipe_names_the_writer_as_sender,
    suite = syscall_signal_build_floor
);

/// What `task` takes at a delivery point with its own mask, as
/// `(signal, value)`: a process signal only once it was picked for it.
fn take_deliverable(task_id: u32) -> Option<(u8, u64)> {
    let task = task_find_by_id(task_id)?;
    task.take_deliverable_signal(!task.signal_blocked())
        .map(|taken| (taken.signum, taken.info.value))
}

/// Blocking it through `rt_sigprocmask` before taking it moves the pick.
pub fn test_blocking_a_picked_signal_hands_it_to_a_sibling() -> TestResult {
    use slopos_abi::signal::{SIG_BLOCK, SIGRTMIN};
    use slopos_sched::task::task_has_deliverable_signal;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<2>() else {
        return TestResult::Fail;
    };
    let [named, sibling] = ids;
    let Some(table) = fdtable_of(named) else {
        return fail_and_clean(&ids);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&ids);
    };
    let deliverable =
        |id: u32| task_find_by_id(id).is_some_and(|task| task_has_deliverable_signal(&task));
    let sent = sigqueue_as(sibling, page, named, SIGRTMIN, 0x7a11);
    let picked_before = ids.map(deliverable);
    let mask_addr = page + 256;
    let staged = user_copy_out(table, mask_addr, &sig_bit(SIGRTMIN));
    let blocked = call_as(
        syscall_rt_sigprocmask,
        named,
        [SIG_BLOCK as u64, mask_addr, 0, 8],
    );
    let picked_after = ids.map(deliverable);
    let taken = ids.map(take_deliverable);
    terminate_all(&ids);

    assert_eq_test!(sent, 0, "sigqueue to the process failed");
    assert_eq_test!(
        picked_before,
        [true, false],
        "the send must hand the signal to the thread it named"
    );
    assert_test!(staged, "could not stage the mask");
    assert_eq_test!(blocked, 0, "rt_sigprocmask(SIG_BLOCK) failed");
    assert_eq_test!(
        picked_after,
        [false, true],
        "blocking a picked signal must hand it to the sibling"
    );
    assert_eq_test!(
        taken,
        [None, Some((SIGRTMIN, 0x7a11))],
        "the sibling's delivery must take the instance"
    );
    pass!()
}

/// `rt_sigpending` reports the caller's pending signals, its own and its
/// process's, that it blocks — nothing it would deliver, and no sibling's own.
pub fn test_sigpending_reports_the_blocked_pending_signals() -> TestResult {
    use slopos_abi::signal::{SIGRTMIN, SIGUSR2, SigInfo};
    use slopos_sched::task::task_signal_post_info;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<2>() else {
        return TestResult::Fail;
    };
    let [leader, sibling] = ids;
    let Some(table) = fdtable_of(leader) else {
        return fail_and_clean(&ids);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&ids);
    };
    let (own, shared, unblocked) = (sig_bit(SIGUSR1), sig_bit(SIGRTMIN), sig_bit(SIGUSR2));
    let posted = match (task_find_by_id(leader), task_find_by_id(sibling)) {
        (Some(leader_task), Some(sibling_task)) => {
            leader_task.set_signal_blocked(own | shared);
            sibling_task.set_signal_blocked(shared);
            task_signal_post_info(&leader_task, SIGUSR1, SigInfo::KERNEL).is_pending()
                && task_signal_post_info(&leader_task, SIGUSR2, SigInfo::KERNEL).is_pending()
        }
        _ => false,
    };
    let sent = sigqueue_as(sibling, page, leader, SIGRTMIN, 0);
    let set_addr = page + 256;
    let pending_of = |id: u32| {
        let rax = call_as(syscall_rt_sigpending, id, [set_addr, 8, 0, 0]);
        (rax, user_copy_in::<u64>(table, set_addr))
    };
    let leaders = pending_of(leader);
    let siblings = pending_of(sibling);
    let short = call_as(syscall_rt_sigpending, leader, [set_addr, 4, 0, 0]);
    terminate_all(&ids);

    assert_test!(posted, "could not leave the thread's own signals pending");
    assert_eq_test!(sent, 0, "sigqueue to the process failed");
    assert_eq_test!(
        leaders,
        (0, Some(own | shared)),
        "the leader's blocked pending set"
    );
    assert_test!(
        leaders.1.unwrap_or(0) & unblocked == 0,
        "an unblocked signal was reported"
    );
    assert_eq_test!(
        siblings,
        (0, Some(shared)),
        "the sibling's blocked pending set"
    );
    assert_eq_test!(
        short,
        slopos_abi::Errno::EINVAL.as_u64(),
        "a sigsetsize other than 8 must be EINVAL"
    );
    pass!()
}

/// Lowest signal first, each instance in send order with its own record; a
/// zero timeout with nothing left is `EAGAIN`.
pub fn test_sigtimedwait_takes_realtime_instances_in_order() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGRTMIN};
    use slopos_abi::syscall::Timespec;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<2>() else {
        return TestResult::Fail;
    };
    let [leader, sibling] = ids;
    let Some(table) = fdtable_of(leader) else {
        return fail_and_clean(&ids);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&ids);
    };
    let set = sig_bit(SIGRTMIN) | sig_bit(SIGRTMIN + 1);
    for id in ids {
        if let Some(task) = task_find_by_id(id) {
            task.set_signal_blocked(set);
        }
    }
    let sends = [
        sigqueue_as(sibling, page, leader, SIGRTMIN + 1, 3),
        sigqueue_as(sibling, page, leader, SIGRTMIN, 1),
        sigqueue_as(sibling, page, leader, SIGRTMIN, 2),
    ];
    let (set_addr, info_addr, zero_addr) = (page + 256, page + 512, page + 1024);
    let staged = user_copy_out(table, set_addr, &set)
        && user_copy_out(
            table,
            zero_addr,
            &Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        );
    let mut taken = [(0u64, 0i32, 0i32, 0u64, 0u32); 3];
    for slot in taken.iter_mut() {
        let rax = call_as(syscall_rt_sigtimedwait, leader, [set_addr, info_addr, 0, 8]);
        if let Some(info) = user_copy_in::<UserSiginfo>(table, info_addr) {
            *slot = (
                rax,
                info.si_signo,
                info.si_code,
                info.si_value(),
                info.si_pid(),
            );
        }
    }
    let drained = call_as(
        syscall_rt_sigtimedwait,
        leader,
        [set_addr, info_addr, zero_addr, 8],
    );
    terminate_all(&ids);

    assert_eq_test!(sends, [0, 0, 0], "a sigqueue to the process failed");
    assert_test!(staged, "could not stage the set and timeout");
    let rt = SIGRTMIN as u64;
    let pid = leader;
    assert_eq_test!(
        taken[0],
        (rt, rt as i32, SI_QUEUE, 1, pid),
        "SIGRTMIN's first instance"
    );
    assert_eq_test!(
        taken[1],
        (rt, rt as i32, SI_QUEUE, 2, pid),
        "SIGRTMIN's second instance"
    );
    assert_eq_test!(
        taken[2],
        (rt + 1, rt as i32 + 1, SI_QUEUE, 3, pid),
        "SIGRTMIN+1's instance"
    );
    assert_eq_test!(
        drained,
        slopos_abi::Errno::EAGAIN.as_u64(),
        "a zero timeout with nothing pending must be EAGAIN"
    );
    pass!()
}

/// One instance sent to a process is taken by one waiter: any thread's
/// `rt_sigtimedwait`, picked or not, and no other thread's after it.
pub fn test_sigtimedwait_takes_a_process_instance_once() -> TestResult {
    use slopos_abi::signal::SIGRTMIN;
    use slopos_abi::syscall::Timespec;
    let _fixture = SyscallFixture::new();

    let Some(ids) = spawn_threads::<3>() else {
        return TestResult::Fail;
    };
    let Some(table) = fdtable_of(ids[0]) else {
        return fail_and_clean(&ids);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&ids);
    };
    let bit = sig_bit(SIGRTMIN);
    for id in ids {
        if let Some(task) = task_find_by_id(id) {
            task.set_signal_blocked(bit);
        }
    }
    let sent = sigqueue_as(ids[0], page, ids[0], SIGRTMIN, 0x0dd);
    let (set_addr, zero_addr) = (page + 256, page + 512);
    let staged = user_copy_out(table, set_addr, &bit)
        && user_copy_out(
            table,
            zero_addr,
            &Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        );
    let takes = [ids[2], ids[0], ids[1]]
        .map(|id| call_as(syscall_rt_sigtimedwait, id, [set_addr, 0, zero_addr, 8]));
    let left = ids
        .map(task_find_by_id)
        .iter()
        .flatten()
        .any(|task| task.signal_pending() & bit != 0);
    terminate_all(&ids);

    assert_eq_test!(sent, 0, "sigqueue to the process failed");
    assert_test!(staged, "could not stage the set and timeout");
    let eagain = slopos_abi::Errno::EAGAIN.as_u64();
    assert_eq_test!(
        takes,
        [SIGRTMIN as u64, eagain, eagain],
        "exactly the first waiter must take the instance"
    );
    assert_test!(!left, "a taken instance must leave the process");
    pass!()
}

const WAITER_BUDGET_MS: u64 = 5_000;

static WAITER_TASK: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);
static WAITER_CALLER: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);
static WAITER_ARGS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static WAITER_RESULT: AtomicU64 = AtomicU64::new(0);
static WAITER_SIGNO: AtomicU32 = AtomicU32::new(0);
static WAITER_ELAPSED_MS: AtomicU64 = AtomicU64::new(0);
static WAITER_DONE: AtomicBool = AtomicBool::new(false);

fn spin_until(done: impl Fn() -> bool, budget_ms: u64) -> bool {
    use slopos_kernel_services::clock::uptime_ms;
    let deadline = uptime_ms().saturating_add(budget_ms);
    while !done() {
        if uptime_ms() > deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn waiter_body(wait: impl FnOnce() -> u64) {
    use slopos_kernel_services::clock::uptime_ms;
    WAITER_TASK.store(slopos_arch::pcr::current_task_id(), Ordering::Release);
    let started = uptime_ms();
    let result = wait();
    WAITER_ELAPSED_MS.store(uptime_ms() - started, Ordering::Release);
    WAITER_RESULT.store(result, Ordering::Release);
    WAITER_DONE.store(true, Ordering::Release);
}

fn sigtimedwait_waiter() {
    waiter_body(|| {
        let args = WAITER_ARGS
            .each_ref()
            .map(|arg| arg.load(Ordering::Acquire));
        call_as(
            syscall_rt_sigtimedwait,
            WAITER_CALLER.load(Ordering::Acquire),
            args,
        )
    });
}

/// Reads the signalfd `WAITER_ARGS[0]` of `caller`'s table as itself, the task
/// a signalfd serves, blocking the watched `WAITER_ARGS[1]` so they wait.
fn signalfd_waiter() {
    waiter_body(|| {
        let caller = WAITER_CALLER.load(Ordering::Acquire);
        let (Some(table), Some(current)) = (fdtable_of(caller), Current::get()) else {
            return u64::MAX;
        };
        current
            .task()
            .set_signal_blocked(WAITER_ARGS[1].load(Ordering::Acquire));
        let mut record = [0u8; slopos_abi::signal::SignalfdSiginfo::SERIALIZED_LEN];
        let fd = WAITER_ARGS[0].load(Ordering::Acquire) as i32;
        let read = slopos_fs::fileio::file_read_fd(
            table,
            fd,
            &mut slopos_abi::io::KernelIoBuf::new(&mut record),
        );
        WAITER_SIGNO.store(
            u32::from_ne_bytes([record[0], record[1], record[2], record[3]]),
            Ordering::Release,
        );
        read as u64
    });
}

/// Run `waiter` as `caller` on its own kernel thread, since only a task can
/// sleep; `false` if it neither parked on a signal event nor finished.
fn start_waiter(waiter: fn(), caller: u32, args: [u64; 4]) -> bool {
    use slopos_ostd::sync::BUS;
    use slopos_ostd::task::ops::signal_pending_event;
    WAITER_DONE.store(false, Ordering::Release);
    WAITER_TASK.store(INVALID_TASK_ID, Ordering::Release);
    WAITER_SIGNO.store(0, Ordering::Release);
    WAITER_CALLER.store(caller, Ordering::Release);
    for (slot, value) in WAITER_ARGS.iter().zip(args) {
        slot.store(value, Ordering::Release);
    }
    let priority = slopos_abi::task::TaskPriority::Normal;
    if slopos_ostd::task::spawn("sigwait-waiter", waiter, priority).is_err() {
        return false;
    }
    spin_until(
        || {
            let waiter = WAITER_TASK.load(Ordering::Acquire);
            WAITER_DONE.load(Ordering::Acquire)
                || BUS.has_waiters(signal_pending_event(caller))
                || (waiter != INVALID_TASK_ID && BUS.has_waiters(signal_pending_event(waiter)))
        },
        WAITER_BUDGET_MS,
    )
}

/// The waiter's result and how long it waited, or `None` when it never
/// finished; it is then killed, so it cannot outlive the test.
fn finish_waiter() -> Option<(u64, u64)> {
    let done = || WAITER_DONE.load(Ordering::Acquire);
    if !spin_until(done, WAITER_BUDGET_MS) {
        if let Some(waiter) = task_find_by_id(WAITER_TASK.load(Ordering::Acquire)) {
            slopos_ostd::task::ops::task_kill_and_wake(&*waiter);
        }
        let _ = spin_until(done, WAITER_BUDGET_MS);
        return None;
    }
    Some((
        WAITER_RESULT.load(Ordering::Acquire),
        WAITER_ELAPSED_MS.load(Ordering::Acquire),
    ))
}

/// A user task, never run, blocking `blocked`, with one scratch page.
fn idle_caller(blocked: u64) -> Option<(u32, FdTable, u64)> {
    let id = create_test_user_task();
    if id == INVALID_TASK_ID {
        return None;
    }
    let ready = task_find_by_id(id).and_then(|task| {
        task.set_signal_blocked(blocked);
        let table = fdtable_of(id)?;
        Some((id, table, map_user_rw_region(table, 1)?))
    });
    if ready.is_none() {
        task_terminate(id);
    }
    ready
}

pub fn test_sigtimedwait_sleeps_until_a_signal_is_sent() -> TestResult {
    use slopos_abi::signal::{SI_QUEUE, SIGRTMIN, SigInfo};
    let bit = sig_bit(SIGRTMIN);
    let Some((caller, table, page)) = idle_caller(bit) else {
        return TestResult::Fail;
    };
    let staged = user_copy_out(table, page, &bit);
    let parked = staged && start_waiter(sigtimedwait_waiter, caller, [page, 0, 0, 8]);
    let early = WAITER_DONE.load(Ordering::Acquire);
    let reached =
        task::task_group_signal_info(caller, SIGRTMIN, SigInfo::sent(SI_QUEUE, 7, 0, 5)).reached;
    let finished = finish_waiter();
    let left = task_find_by_id(caller).map_or(0, |task| task.signal_pending() & bit);
    task_terminate(caller);

    assert_test!(parked, "the waiter never began to wait");
    assert_test!(!early, "rt_sigtimedwait returned with nothing sent");
    assert_eq_test!(reached, 1, "the send reached no thread");
    assert_eq_test!(
        finished.map(|(rax, _)| rax),
        Some(SIGRTMIN as u64),
        "the waiter must wake with the signal sent"
    );
    assert_eq_test!(left, 0, "the waited signal must be taken");
    pass!()
}

/// `EAGAIN` comes once the timeout passes, and not before.
pub fn test_sigtimedwait_times_out_with_eagain() -> TestResult {
    use slopos_abi::signal::SIGRTMIN;
    use slopos_abi::syscall::Timespec;
    const TIMEOUT_MS: u64 = 30;
    let bit = sig_bit(SIGRTMIN);
    let Some((caller, table, page)) = idle_caller(bit) else {
        return TestResult::Fail;
    };
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: (TIMEOUT_MS * 1_000_000) as i64,
    };
    let staged = user_copy_out(table, page, &bit) && user_copy_out(table, page + 64, &timeout);
    let started = staged && start_waiter(sigtimedwait_waiter, caller, [page, 0, page + 64, 8]);
    let finished = finish_waiter();
    task_terminate(caller);

    assert_test!(started, "the waiter never began to wait");
    let Some((rax, elapsed)) = finished else {
        return slopos_testing::fail!("the timed wait never ended");
    };
    assert_eq_test!(
        rax,
        slopos_abi::Errno::EAGAIN.as_u64(),
        "a timed-out wait must be EAGAIN"
    );
    assert_test!(elapsed >= TIMEOUT_MS, "the wait ended before its timeout");
    pass!()
}

pub fn test_a_blocking_signalfd_read_sleeps_until_a_signal_is_sent() -> TestResult {
    use slopos_abi::signal::{SI_USER, SigInfo, SignalfdSiginfo};
    use slopos_sched::task::task_signal_post_info;
    let bit = sig_bit(SIGUSR1);
    let Some((caller, table, _page)) = idle_caller(bit) else {
        return TestResult::Fail;
    };
    let fd = slopos_signalfd::signalfd_create(table, bit, false, false);
    let parked = fd >= 0 && start_waiter(signalfd_waiter, caller, [fd as u64, bit, 0, 0]);
    let early = WAITER_DONE.load(Ordering::Acquire);
    let posted = task_find_by_id(WAITER_TASK.load(Ordering::Acquire)).is_some_and(|reader| {
        task_signal_post_info(&reader, SIGUSR1, SigInfo::sent(SI_USER, 9, 0, 0)).is_pending()
    });
    let finished = finish_waiter();
    let signo = WAITER_SIGNO.load(Ordering::Acquire);
    if fd >= 0 {
        let _ = slopos_fs::fileio::file_close_fd(table, fd);
    }
    task_terminate(caller);

    assert_test!(parked, "the reader never began to wait");
    assert_test!(!early, "the read returned with nothing sent");
    assert_test!(posted, "the signal did not pend on the reader");
    assert_eq_test!(
        finished.map(|(read, _)| read),
        Some(SignalfdSiginfo::SERIALIZED_LEN as u64),
        "the reader must wake with one record"
    );
    assert_eq_test!(signo, SIGUSR1 as u32, "the record names another signal");
    pass!()
}

/// `(read result, signal read)` of `fd` in `table`, read as `reader`.
fn read_signalfd_as(reader: u32, table: FdTable, fd: i32) -> Option<(isize, u32)> {
    if !make_task_current(reader) {
        return None;
    }
    let mut record = [0u8; slopos_abi::signal::SignalfdSiginfo::SERIALIZED_LEN];
    let read = slopos_fs::fileio::file_read_fd(
        table,
        fd,
        &mut slopos_abi::io::KernelIoBuf::new(&mut record),
    );
    park_bootstrap_on_current_cpu();
    Some((
        read,
        u32::from_ne_bytes([record[0], record[1], record[2], record[3]]),
    ))
}

/// As on Linux, a forked child holding the descriptor polls and reads its own
/// signals, never the creator's.
pub fn test_a_signalfd_serves_the_task_using_it() -> TestResult {
    use slopos_abi::signal::{SIGUSR2, SigInfo};
    use slopos_abi::syscall::{POLLIN, SFD_NONBLOCK};
    use slopos_sched::task::task_signal_post_info;
    let _fixture = SyscallFixture::new();

    let creator = create_test_user_task();
    let holder = create_test_user_task();
    let ids = [creator, holder];
    if ids.contains(&INVALID_TASK_ID) {
        return fail_and_clean(&ids);
    }
    let Some(table) = fdtable_of(creator) else {
        return fail_and_clean(&ids);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&ids);
    };
    let mask = sig_bit(SIGUSR1) | sig_bit(SIGUSR2);
    let staged = user_copy_out(table, page, &mask);
    let fd = call_as(
        crate::syscall::signalfd_handlers::syscall_signalfd4,
        creator,
        [-1i64 as u64, page, 8, SFD_NONBLOCK as u64],
    ) as i64 as i32;
    let posted = [(creator, SIGUSR1), (holder, SIGUSR2)]
        .iter()
        .all(|&(id, signum)| {
            task_find_by_id(id).is_some_and(|task| {
                task.set_signal_blocked(mask);
                task_signal_post_info(&task, signum, SigInfo::KERNEL).is_pending()
            })
        });
    let polled_as_holder =
        make_task_current(holder) && slopos_fs::fileio::file_poll_fd(table, fd, POLLIN) == POLLIN;
    park_bootstrap_on_current_cpu();
    let holders = read_signalfd_as(holder, table, fd);
    let holders_again = read_signalfd_as(holder, table, fd);
    let polled_empty =
        make_task_current(holder) && slopos_fs::fileio::file_poll_fd(table, fd, POLLIN) == 0;
    park_bootstrap_on_current_cpu();
    let creators = read_signalfd_as(creator, table, fd);
    terminate_all(&ids);

    let record = slopos_abi::signal::SignalfdSiginfo::SERIALIZED_LEN as isize;
    assert_test!(staged, "could not stage the mask");
    assert_test!(fd >= 0, "signalfd4(SFD_NONBLOCK) failed");
    assert_test!(posted, "could not leave the signals pending");
    assert_test!(polled_as_holder, "the holder's own signal must poll ready");
    assert_eq_test!(
        holders,
        Some((record, SIGUSR2 as u32)),
        "the holder must read its own signal"
    );
    assert_eq_test!(
        holders_again.map(|(read, _)| read),
        Some(slopos_abi::Errno::EAGAIN.raw() as isize),
        "the holder must not drain the creator's signal"
    );
    assert_test!(
        polled_empty,
        "the creator's signal must not poll ready for the holder"
    );
    assert_eq_test!(
        creators,
        Some((record, SIGUSR1 as u32)),
        "the creator's signal must wait for the creator"
    );
    pass!()
}

/// Onto the open file and the descriptor; any other flag is refused.
pub fn test_signalfd4_takes_nonblock_and_cloexec() -> TestResult {
    use slopos_abi::syscall::{
        F_GETFD, F_GETFL, FD_CLOEXEC, O_NONBLOCK, SFD_CLOEXEC, SFD_NONBLOCK,
    };
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(table) = fdtable_of(task_id) else {
        return fail_and_clean(&[task_id]);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&[task_id]);
    };
    let staged = user_copy_out(table, page, &sig_bit(SIGUSR1));
    let open = |flags: u32| {
        call_as(
            crate::syscall::signalfd_handlers::syscall_signalfd4,
            task_id,
            [-1i64 as u64, page, 8, flags as u64],
        )
    };
    let fcntl =
        |fd: u64, cmd: u64| call_as(crate::syscall::fs::syscall_fcntl, task_id, [fd, cmd, 0, 0]);
    let both = open(SFD_NONBLOCK | SFD_CLOEXEC);
    let plain = open(0);
    let refused = open(0x1);
    let both_fl = fcntl(both, F_GETFL);
    let both_fd = fcntl(both, F_GETFD);
    let plain_fl = fcntl(plain, F_GETFL);
    let plain_fd = fcntl(plain, F_GETFD);
    task_terminate(task_id);

    assert_test!(staged, "could not stage the mask");
    assert_test!(
        (both as i64) >= 0 && (plain as i64) >= 0,
        "signalfd4 failed"
    );
    assert_eq_test!(
        refused,
        slopos_abi::Errno::EINVAL.as_u64(),
        "an unknown flag must be EINVAL"
    );
    assert_test!(
        both_fl & O_NONBLOCK != 0,
        "SFD_NONBLOCK must set O_NONBLOCK"
    );
    assert_test!(both_fd & FD_CLOEXEC != 0, "SFD_CLOEXEC must set FD_CLOEXEC");
    assert_test!(
        plain_fl & O_NONBLOCK == 0,
        "no flag must leave the file blocking"
    );
    assert_test!(
        plain_fd & FD_CLOEXEC == 0,
        "no flag must leave the fd inherited"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_blocking_a_picked_signal_hands_it_to_a_sibling,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigpending_reports_the_blocked_pending_signals,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigtimedwait_takes_realtime_instances_in_order,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigtimedwait_takes_a_process_instance_once,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigtimedwait_sleeps_until_a_signal_is_sent,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigtimedwait_times_out_with_eagain,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_blocking_signalfd_read_sleeps_until_a_signal_is_sent,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_signalfd_serves_the_task_using_it,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_signalfd4_takes_nonblock_and_cloexec,
    suite = syscall_signal_build_floor
);

/// Run the way back to userland of a `sysno` that returned `ERESTARTSYS` as
/// `task_id`, as `syscall_handle` ends.
#[must_use]
fn return_from_syscall_as_current(
    task_id: u32,
    table: FdTable,
    ctx: &UserContext,
    sysno: u64,
) -> bool {
    if !make_task_current(task_id) {
        return false;
    }
    let returned = match Current::get() {
        Some(current) => with_user_process_context(table, || {
            crate::syscall::dispatch::return_from_syscall(&current, ctx, sysno, true)
        })
        .is_some(),
        None => false,
    };
    park_bootstrap_on_current_cpu();
    returned
}

/// A user `rax` of -512 that `rt_sigreturn` restores is the interrupted
/// code's value, not a restart request: the delivery on its own way out must
/// neither rewind into another `rt_sigreturn` nor rewrite it to `EINTR`.
pub fn test_sigreturn_restoring_erestartsys_does_not_restart() -> TestResult {
    use slopos_abi::signal::SA_RESTART;
    use slopos_abi::syscall::{ERRNO_ERESTARTSYS, SYSCALL_RT_SIGRETURN};
    const INTERRUPTED_RIP: u64 = 0x5000_6006;
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(table) = fdtable_of(task_id) else {
        return fail_and_clean(&[task_id]);
    };
    assert_test!(
        install_action(task_id, SIGUSR1, SA_RESTART),
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
    let sigframe_addr = frame.rsp().wrapping_add(8);
    let Some(mut sigframe) = user_copy_in::<SignalFrame>(table, sigframe_addr) else {
        drop(task);
        return fail_and_clean(&[task_id]);
    };
    sigframe.rax = ERRNO_ERESTARTSYS;
    assert_test!(
        user_copy_out(table, sigframe_addr, &sigframe),
        "rewriting the sigframe's rax failed"
    );
    // Blocked by the running handler; the restored mask makes it deliverable
    // on rt_sigreturn's own way out.
    assert_test!(
        task::task_signal_post(&task, SIGUSR1),
        "SIGUSR1 must pend again"
    );

    frame.regs_mut().rsp = sigframe_addr;
    frame.regs_mut().rax = SYSCALL_RT_SIGRETURN;
    let handled = make_task_current(task_id)
        && with_user_process_context(table, || crate::syscall::dispatch::syscall_handle(&frame))
            .is_some();
    park_bootstrap_on_current_cpu();
    let saved = user_copy_in::<SignalFrame>(table, frame.rsp().wrapping_add(8))
        .map(|saved| (saved.rip, saved.rax));
    drop(task);
    task_terminate(task_id);

    assert_test!(handled, "rt_sigreturn did not run");
    assert_eq_test!(
        frame.rip(),
        TEST_HANDLER,
        "the second SIGUSR1 must be delivered"
    );
    assert_eq_test!(
        saved,
        Some((INTERRUPTED_RIP, ERRNO_ERESTARTSYS)),
        "the restored registers must reach the next frame untouched"
    );
    pass!()
}

/// `ERESTARTSYS` is settled on the signal the way out takes, as Linux's
/// `get_signal` does: none left (a sibling's `sigtimedwait` took it) restarts,
/// a caught one fails `EINTR` unless `SA_RESTART`.
pub fn test_erestartsys_is_settled_on_the_signal_taken() -> TestResult {
    use slopos_abi::signal::SA_RESTART;
    use slopos_abi::syscall::{ERRNO_ERESTARTSYS, SYSCALL_WRITE};
    const SYSCALL_RIP: u64 = 0x5000_3002;
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let ids = [thread_id, leader_id];
    let (Some(leader), Some(thread), Some(table)) = (
        task_find_by_id(leader_id),
        task_find_by_id(thread_id),
        fdtable_of(leader_id),
    ) else {
        return fail_and_clean(&ids);
    };
    let bit = sig_bit(SIGUSR1);
    // The sibling sits in `sigtimedwait({SIGUSR1})`, which blocks it.
    thread.set_signal_blocked(bit);
    let stack_top = process_vm_get_stack_top(table.process().expect("a live process"));
    let interrupted = || -> KBox<UserContext> {
        let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
        frame.regs_mut().rax = ERRNO_ERESTARTSYS;
        frame.regs_mut().rip = SYSCALL_RIP;
        frame.regs_mut().rsp = stack_top.wrapping_sub(0x200);
        frame
    };
    let saved_frame = |frame: &UserContext| {
        user_copy_in::<SignalFrame>(table, frame.rsp().wrapping_add(8))
            .map(|saved| (saved.rip, saved.rax))
    };

    let installed = install_action(leader_id, SIGUSR1, 0);
    let sent = task_group_signal(leader_id, SIGUSR1) != 0;
    let picked = leader.has_deliverable_signal();
    let stolen = thread.dequeue_signal(bit).map(|taken| taken.signum);
    let robbed = interrupted();
    let robbed_returned = return_from_syscall_as_current(leader_id, table, &robbed, SYSCALL_WRITE);

    let resent = task_group_signal(leader_id, SIGUSR1) != 0;
    let caught = interrupted();
    let caught_returned = return_from_syscall_as_current(leader_id, table, &caught, SYSCALL_WRITE);
    let caught_saved = saved_frame(&caught);

    leader.set_signal_blocked(0);
    let restarting = install_action(leader_id, SIGUSR1, SA_RESTART);
    let sent_again = task_group_signal(leader_id, SIGUSR1) != 0;
    let restarted = interrupted();
    let restarted_returned =
        return_from_syscall_as_current(leader_id, table, &restarted, SYSCALL_WRITE);
    let restarted_saved = saved_frame(&restarted);
    drop((leader, thread));
    terminate_all(&ids);

    assert_test!(installed && restarting, "installing a handler failed");
    assert_test!(sent && resent && sent_again, "the sends reached no thread");
    assert_test!(picked, "the send must pick the leader");
    assert_eq_test!(stolen, Some(SIGUSR1), "the sibling must take the instance");
    assert_test!(
        robbed_returned && caught_returned && restarted_returned,
        "the way out did not run"
    );
    assert_eq_test!(
        (robbed.rip(), robbed.rax()),
        (SYSCALL_RIP - 2, SYSCALL_WRITE),
        "with the instance taken by a sibling the syscall must restart"
    );
    assert_eq_test!(caught.rip(), TEST_HANDLER, "the handler must run");
    assert_eq_test!(
        caught_saved,
        Some((SYSCALL_RIP, slopos_abi::Errno::EINTR.as_u64())),
        "a handler without SA_RESTART must return EINTR"
    );
    assert_eq_test!(
        restarted.rip(),
        TEST_HANDLER,
        "the SA_RESTART handler must run"
    );
    assert_eq_test!(
        restarted_saved,
        Some((SYSCALL_RIP - 2, SYSCALL_WRITE)),
        "an SA_RESTART handler must return into the restarted syscall"
    );
    pass!()
}

pub fn test_signalfd4_never_watches_kill_or_stop() -> TestResult {
    use slopos_abi::syscall::SFD_NONBLOCK;
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let (Some(task), Some(table)) = (task_find_by_id(task_id), fdtable_of(task_id)) else {
        return fail_and_clean(&[task_id]);
    };
    let Some(page) = map_user_rw_region(table, 1) else {
        return fail_and_clean(&[task_id]);
    };
    let uncatchable = sig_bit(SIGKILL) | sig_bit(SIGSTOP);
    let staged = user_copy_out(table, page, &(uncatchable | sig_bit(SIGUSR1)));
    let fd = call_as(
        crate::syscall::signalfd_handlers::syscall_signalfd4,
        task_id,
        [-1i64 as u64, page, 8, SFD_NONBLOCK as u64],
    ) as i64 as i32;
    let _ = task.raise_signal_pending(uncatchable);
    let read = read_signalfd_as(task_id, table, fd);
    let left = task.signal_pending() & uncatchable;
    let _ = task.clear_signal_pending(uncatchable);
    let posted = task::task_signal_post(&task, SIGUSR1);
    let watched = read_signalfd_as(task_id, table, fd);
    drop(task);
    task_terminate(task_id);

    assert_test!(staged, "could not stage the mask");
    assert_test!(fd >= 0, "signalfd4 failed");
    assert_eq_test!(
        read.map(|(read, _)| read),
        Some(slopos_abi::Errno::EAGAIN.raw() as isize),
        "a signalfd must not read SIGKILL or SIGSTOP"
    );
    assert_eq_test!(left, uncatchable, "both must stay pending");
    assert_test!(posted, "SIGUSR1 must pend");
    assert_eq_test!(
        watched,
        Some((
            slopos_abi::signal::SignalfdSiginfo::SERIALIZED_LEN as isize,
            SIGUSR1 as u32
        )),
        "the rest of the mask must still be watched"
    );
    pass!()
}

/// A blocked stop signal stops nothing at the send: it pends for `sigwait` or
/// a signalfd, or stops the group (or the `tgkill`ed thread) once unblocked.
pub fn test_a_blocked_stop_signal_pends_instead_of_stopping() -> TestResult {
    use slopos_abi::signal::{SI_TKILL, SIGTTIN, SigInfo};
    let _fixture = SyscallFixture::new();

    let Some((leader_id, thread_id)) = spawn_thread_group() else {
        return TestResult::Fail;
    };
    let ids = [thread_id, leader_id];
    let (Some(leader), Some(thread)) = (task_find_by_id(leader_id), task_find_by_id(thread_id))
    else {
        return fail_and_clean(&ids);
    };
    let tstp = sig_bit(SIGTSTP);
    leader.set_signal_blocked(tstp);
    thread.set_signal_blocked(tstp);

    let reached = task_group_signal(leader_id, SIGTSTP);
    let stopped_by_send = leader.is_stopped() || thread.is_stopped();
    let pending = leader.signal_pending() & tstp;
    let waited = thread.dequeue_signal(tstp).map(|taken| taken.signum);
    let stopped_by_wait = leader.is_stopped() || thread.is_stopped();

    let resent = task_group_signal(leader_id, SIGTSTP) != 0;
    task::task_set_signal_blocked(&leader, 0);
    let deliverable = leader.has_deliverable_signal();
    let _ = leader.clear_signal_pending(tstp);

    let ttin = sig_bit(SIGTTIN);
    thread.set_signal_blocked(ttin);
    let directed = task::task_thread_signal_info(
        leader_id,
        thread_id,
        SIGTTIN,
        SigInfo::sent(SI_TKILL, leader_id, 0, 0),
    );
    let stopped_by_tgkill = leader.is_stopped() || thread.is_stopped();
    let own = thread.dequeue_signal(ttin).map(|taken| taken.signum);
    let leaders = leader.signal_pending() & ttin;

    let reposted = task_group_signal(leader_id, SIGTSTP) != 0;
    let stopped_unblocked = leader.is_stopped() && thread.is_stopped();
    let _ = task_group_continue(leader_id);
    drop((leader, thread));
    terminate_all(&ids);

    assert_eq_test!(reached, 2, "the send must reach both threads");
    assert_test!(
        !stopped_by_send,
        "a blocked SIGTSTP must not stop the group"
    );
    assert_eq_test!(pending, tstp, "a blocked SIGTSTP must pend");
    assert_eq_test!(waited, Some(SIGTSTP), "sigwait must take a blocked SIGTSTP");
    assert_test!(!stopped_by_wait, "a SIGTSTP taken by sigwait stops nothing");
    assert_test!(resent, "the second send reached no thread");
    assert_test!(
        deliverable,
        "unblocking must leave the pending SIGTSTP for delivery to act on"
    );
    assert_eq_test!(
        directed,
        Some(slopos_sched::task::SignalPost::Pending),
        "tgkill of a blocked SIGTTIN must pend"
    );
    assert_test!(
        !stopped_by_tgkill,
        "tgkill of a SIGTTIN its thread blocks must not stop the group"
    );
    assert_eq_test!(
        own,
        Some(SIGTTIN),
        "the SIGTTIN must pend on the thread named"
    );
    assert_eq_test!(leaders, 0, "the SIGTTIN must not pend for the process");
    assert_test!(
        reposted && stopped_unblocked,
        "a SIGTSTP a thread does not block must still stop the group at the send"
    );
    pass!()
}

/// A process-sent standard signal whose record could not be allocated reads
/// as a `kill` from pid 0, as on Linux; a kernel one keeps `SI_KERNEL`.
pub fn test_a_lost_standard_record_reads_as_a_kill_from_no_one() -> TestResult {
    use slopos_abi::signal::{CLD_EXITED, SI_KERNEL, SI_QUEUE, SI_USER, SIGCHLD, SigInfo};
    use slopos_ostd::task::ops::inject_sigqueue_store_alloc_failures;
    use slopos_sched::task::task_signal_post_info;
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    if task_id == INVALID_TASK_ID {
        return TestResult::Fail;
    }
    let Some(task) = task_find_by_id(task_id) else {
        return fail_and_clean(&[task_id]);
    };
    task.set_signal_blocked(sig_bit(SIGCHLD));
    inject_sigqueue_store_alloc_failures(2);
    let posts = [
        task_signal_post_info(&task, SIGUSR1, SigInfo::sent(SI_QUEUE, 42, 7, 9)),
        task_signal_post_info(&task, SIGCHLD, SigInfo::sent(CLD_EXITED, 43, 0, 0)),
    ];
    inject_sigqueue_store_alloc_failures(0);
    let user = task.dequeue_signal(u64::MAX).map(|t| (t.signum, t.info));
    let child = task.dequeue_signal(u64::MAX).map(|t| (t.signum, t.info));
    drop(task);
    task_terminate(task_id);

    assert_test!(
        posts.iter().all(|post| post.is_pending()),
        "both signals must pend without a record"
    );
    assert_eq_test!(
        user,
        Some((SIGUSR1, SigInfo::sent(SI_USER, 0, 0, 0))),
        "a lost sender's record must read as SI_USER from pid 0"
    );
    assert_eq_test!(
        child.map(|(signum, info)| (signum, info.code)),
        Some((SIGCHLD, SI_KERNEL)),
        "a lost kernel record must keep SI_KERNEL"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_erestartsys_is_settled_on_the_signal_taken,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_sigreturn_restoring_erestartsys_does_not_restart,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_signalfd4_never_watches_kill_or_stop,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_blocked_stop_signal_pends_instead_of_stopping,
    suite = syscall_signal_build_floor
);
slopos_testing::stest!(
    name = test_a_lost_standard_record_reads_as_a_kill_from_no_one,
    suite = syscall_signal_build_floor
);

#[allow(dead_code)]
const _UNUSED: Ordering = Ordering::Relaxed;
