//! Coverage for the Phase 1 process-result and futex surface.

use core::ffi::c_char;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_abi::Errno;
use slopos_abi::signal::{
    SIGKILL, SIGSTOP, WAIT_STATUS_CONTINUED, WCONTINUED, WNOHANG, WUNTRACED, wait_status_exited,
    wait_status_stopped,
};
use slopos_abi::syscall::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID, CLONE_SIGHAND, CLONE_THREAD,
    CLONE_VM, FUTEX_BITSET_MATCH_ANY, FUTEX_CLOCK_REALTIME, FUTEX_PRIVATE_FLAG, FUTEX_WAIT,
    FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, Rusage, Timespec, UserUtsname,
};
use slopos_abi::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE, TaskExitReason, TaskPriority,
    TaskStatus,
};
use slopos_fs::fileio::FdTable;
use slopos_fs::vfs::ops::{vfs_mkdir, vfs_symlink};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::process_vm_alloc;
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_test_process_id};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::{KBox, klog_info};
use slopos_sched::scheduler::{clear_nascent_for_test, dispatch_task_for_test};
use slopos_sched::task::{
    TaskRef, task_clone, task_create, task_find_by_id, task_group_exit, task_group_stop,
    task_set_parent, task_set_state, task_terminate,
};
use slopos_sched::task_struct::Current;
use slopos_testing::{TestResult, assert_eq_test, assert_some, assert_test, fail, pass};

use crate::syscall::common::SyscallHandler;
use crate::syscall::core_handlers::{
    exit_group_terminate, syscall_clock_gettime, syscall_clock_settime, syscall_uname,
};
use crate::syscall::dispatch::dispatch_handler;
use crate::syscall::process_handlers::{
    syscall_chdir, syscall_futex, syscall_getpid, syscall_getppid, syscall_gettid, syscall_wait4,
};
use crate::tests::helpers::dummy_task_entry;

type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::park_bootstrap_task(
        slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

/// `task_create` leaves a fresh task `Blocked` and `Nascent`; `dispatch`
/// asserts `Ready | Running`, so both have to be cleared first.
fn make_task_current(task_id: u32) -> bool {
    if !clear_nascent_for_test(task_id) || task_set_state(task_id, TaskStatus::Ready) != 0 {
        return false;
    }
    dispatch_task_for_test(slopos_arch::pcr::get_current_cpu(), task_id)
}

fn create_user_task() -> u32 {
    let user_entry = slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);
    task_create(
        b"Phase1Proc\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_USER_MODE,
    )
}

fn create_kernel_task() -> u32 {
    task_create(
        b"Phase1Kern\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        TaskPriority::Normal.as_u8(),
        TASK_FLAG_KERNEL_MODE,
    )
}

fn table_of(task: &TaskRef) -> Option<FdTable> {
    task.process().as_deref().and_then(FdTable::of)
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

fn user_copy_out<T: Copy>(table: FdTable, addr: u64, value: &T) -> bool {
    with_user_process_context(table, || match UserPtr::<T>::try_new(addr) {
        Ok(ptr) => copy_to_user(ptr, value).is_ok(),
        Err(_) => false,
    })
    .unwrap_or(false)
}

fn user_copy_in<T: Copy>(table: FdTable, addr: u64) -> Option<T> {
    with_user_process_context(table, || {
        let ptr = UserPtr::<T>::try_new(addr).ok()?;
        copy_from_user(ptr).ok()
    })?
}

fn map_user_rw_page(table: FdTable) -> Option<u64> {
    let process = table.process()?;
    let base = process_vm_alloc(process, 4096, PageFlags::USER_RW.bits() as u32);
    if base == 0 {
        return None;
    }
    let mapped = slopos_mm::process_vm::process_vm_with_vm_space(process, |vs| {
        slopos_mm::user_mappings::ostd_map_4kb_user_fresh(
            vs,
            slopos_abi::addr::VirtAddr::new(base),
            PageFlags::USER_RW.bits(),
        )
        .is_ok()
    });
    if !matches!(mapped, Some(true)) {
        return None;
    }
    Some(base)
}

/// `args` are `rdi, rsi, rdx, r10, r8, r9` — the System V order the syscall
/// entry snapshots.
fn invoke(handler: SyscallHandler, task: &TaskRef, table: FdTable, args: [u64; 6]) -> u64 {
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("frame alloc");
    {
        let regs = frame.regs_mut();
        regs.rdi = args[0];
        regs.rsi = args[1];
        regs.rdx = args[2];
        regs.r10 = args[3];
        regs.r8 = args[4];
        regs.r9 = args[5];
    }
    let _ = with_user_process_context(table, || dispatch_handler(handler, task, &mut frame));
    frame.rax()
}

fn exit_child_normally(child_id: u32, code: u32) {
    if let Some(child) = task_find_by_id(child_id) {
        child
            .exit_reason
            .store(TaskExitReason::Normal.as_u16(), Ordering::Release);
        child.exit_code.store(code, Ordering::Release);
    }
    task_terminate(child_id);
}

fn exit_child_by_signal(child_id: u32, signum: u8) {
    if let Some(child) = task_find_by_id(child_id) {
        child
            .exit_reason
            .store(TaskExitReason::Signalled.as_u16(), Ordering::Release);
        child
            .exit_code
            .store(128 + signum as u32, Ordering::Release);
        child.set_exit_signal(signum);
    }
    task_terminate(child_id);
}

struct WaitFixture {
    parent_id: u32,
    parent: TaskRef,
    table: FdTable,
    status_addr: u64,
    child_id: u32,
}

fn build_wait_fixture() -> Option<WaitFixture> {
    let parent_id = create_user_task();
    if parent_id == INVALID_TASK_ID {
        return None;
    }
    let parent = task_find_by_id(parent_id)?;
    let table = table_of(&parent)?;
    let status_addr = map_user_rw_page(table)?;
    // User-mode: `task_group_stop`'s group walk skips a kernel task, so a
    // kernel child could never carry a job-control report.
    let child_id = create_user_task();
    if child_id == INVALID_TASK_ID || task_set_parent(child_id, parent_id) != 0 {
        return None;
    }
    Some(WaitFixture {
        parent_id,
        parent,
        table,
        status_addr,
        child_id,
    })
}

impl WaitFixture {
    fn wait(&self, pid: i64, status: u64, options: u64) -> u64 {
        invoke(
            syscall_wait4,
            &self.parent,
            self.table,
            [pid as u64, status, options, 0, 0, 0],
        )
    }

    fn status_word(&self) -> Option<i32> {
        user_copy_in::<i32>(self.table, self.status_addr)
    }

    fn child_state(&self) -> Option<TaskStatus> {
        task_find_by_id(self.child_id).map(|t| t.status())
    }

    fn teardown(self) {
        let WaitFixture {
            parent_id,
            parent,
            child_id,
            ..
        } = self;
        drop(parent);
        task_terminate(child_id);
        task_terminate(parent_id);
    }
}

pub fn test_waitpid_returns_the_pid_and_writes_an_exited_status() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    exit_child_normally(fx.child_id, 7);
    let rc = fx.wait(fx.child_id as i64, fx.status_addr, 0);
    let child_id = fx.child_id;
    let status = fx.status_word();
    fx.teardown();

    assert_eq_test!(rc, child_id as u64, "waitpid must return the reaped pid");
    assert_eq_test!(
        status,
        Some(wait_status_exited(7) as i32),
        "the child's exit code did not reach the status word"
    );
    pass!()
}

/// The CPU time a reaped child ran, and its peak resident set, reach the
/// caller's `rusage`.
pub fn test_wait4_reports_the_reaped_childs_usage() -> TestResult {
    const RAN: u64 = 1 << 34;
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };
    let Some(usage_addr) = map_user_rw_page(fx.table) else {
        fx.teardown();
        return fail!("could not map the rusage page");
    };
    let child = task_find_by_id(fx.child_id);
    let resident = child.as_ref().and_then(table_of).and_then(map_user_rw_page);
    let peak = child
        .as_ref()
        .and_then(|child| child.process())
        .and_then(|process| slopos_ostd::process::ProcessId::of(&process))
        .map(slopos_mm::process_vm::process_vm_peak_resident_pages);
    if let Some(child) = child {
        child.add_total_runtime(RAN);
    }

    exit_child_normally(fx.child_id, 0);
    let rc = invoke(
        syscall_wait4,
        &fx.parent,
        fx.table,
        [fx.child_id as u64, fx.status_addr, 0, usage_addr, 0, 0],
    );
    let child_id = fx.child_id;
    let usage = user_copy_in::<Rusage>(fx.table, usage_addr);
    fx.teardown();

    assert_eq_test!(rc, child_id as u64, "wait4 with a usage pointer must reap");
    assert_test!(
        resident.is_some() && peak.is_some_and(|pages| pages > 0),
        "the child held no resident page to report"
    );
    let usage = assert_some!(usage, "the usage page could not be read back");
    let micros = usage.ru_utime.tv_sec as u64 * 1_000_000 + usage.ru_utime.tv_usec as u64;
    assert_test!(
        micros >= slopos_kernel_services::clock::ticks_to_microseconds(RAN),
        "ru_utime {} us is less than the child ran",
        micros
    );
    assert_eq_test!(
        Some(usage.ru_maxrss),
        peak.map(|pages| i64::from(pages) * 4),
        "ru_maxrss is not the child's peak resident set in KiB"
    );
    pass!()
}

/// A signal death must be distinguishable from an exit with the same number.
pub fn test_waitpid_reports_a_signal_death_in_the_low_seven_bits() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    exit_child_by_signal(fx.child_id, SIGKILL);
    let rc = fx.wait(fx.child_id as i64, fx.status_addr, 0);
    let child_id = fx.child_id;
    let status = fx.status_word();
    fx.teardown();

    assert_eq_test!(rc, child_id as u64, "waitpid must return the reaped pid");
    let Some(status) = status else {
        return fail!("no status word was written");
    };
    assert_eq_test!(
        status & 0x7f,
        SIGKILL as i32,
        "the killing signal is not in the low seven bits"
    );
    assert_eq_test!(
        (status >> 8) & 0xff,
        0,
        "a signal death must not also report an exit code"
    );
    pass!()
}

/// The old bug: `waitpid` took the status pointer as its flags word, so
/// `WNOHANG` never arrived.
pub fn test_waitpid_wnohang_on_a_live_child_returns_zero() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    let named = fx.wait(fx.child_id as i64, fx.status_addr, WNOHANG as u64);
    let any = fx.wait(-1, fx.status_addr, WNOHANG as u64);
    fx.teardown();

    assert_eq_test!(named, 0, "WNOHANG on a live named child must report 0");
    assert_eq_test!(any, 0, "WNOHANG wait-any with a live child must report 0");
    pass!()
}

/// `WNOHANG` must not outrank `ECHILD`: a caller with no children is told so,
/// not told to try again.
pub fn test_waitpid_without_children_is_echild() -> TestResult {
    let _fixture = SyscallFixture::new();
    let parent_id = create_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "no parent task");
    let parent = assert_some!(task_find_by_id(parent_id), "parent lookup failed");
    let Some(table) = table_of(&parent) else {
        drop(parent);
        task_terminate(parent_id);
        return fail!("parent has no fd table");
    };

    let any = invoke(
        syscall_wait4,
        &parent,
        table,
        [u64::MAX, 0, WNOHANG as u64, 0, 0, 0],
    );
    // A process-group wait is not implemented and says so rather than
    // answering a different question.
    let group = invoke(
        syscall_wait4,
        &parent,
        table,
        [0, 0, WNOHANG as u64, 0, 0, 0],
    );
    drop(parent);
    task_terminate(parent_id);

    assert_eq_test!(
        any,
        Errno::ECHILD.as_u64(),
        "wait-any with no children must be ECHILD"
    );
    assert_eq_test!(
        group,
        Errno::ESRCH.as_u64(),
        "a process-group wait must be refused, not folded into wait-any"
    );
    pass!()
}

pub fn test_waitpid_wuntraced_reports_a_stop_exactly_once() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    if !task_group_stop(fx.child_id, SIGSTOP) {
        fx.teardown();
        return fail!("could not stop the child");
    }

    let options = (WUNTRACED | WNOHANG) as u64;
    let first = fx.wait(fx.child_id as i64, fx.status_addr, options);
    let first_status = fx.status_word();
    let after_first = fx.child_state();
    let second = fx.wait(fx.child_id as i64, fx.status_addr, options);
    let child_id = fx.child_id;
    fx.teardown();

    assert_eq_test!(
        first,
        child_id as u64,
        "a stopped child was not reported under WUNTRACED"
    );
    assert_eq_test!(
        first_status,
        Some(wait_status_stopped(SIGSTOP) as i32),
        "the stop status word is wrong"
    );
    assert_eq_test!(
        after_first,
        Some(TaskStatus::Stopped),
        "reporting a stop must not reap the child"
    );
    assert_eq_test!(second, 0, "the same stop was reported twice");
    pass!()
}

/// A stop report is consume-once, so the status write comes first: an
/// `EFAULT` that consumed the report would lose it forever.
pub fn test_waitpid_stop_report_survives_a_faulting_status_pointer() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    if !task_group_stop(fx.child_id, SIGSTOP) {
        fx.teardown();
        return fail!("could not stop the child");
    }

    let options = (WUNTRACED | WNOHANG) as u64;
    // Page 1: a well-formed user address that no process maps, so the pointer
    // decodes and the copy is what fails.
    let faulted = fx.wait(fx.child_id as i64, 0x1000, options);
    let retry = fx.wait(fx.child_id as i64, fx.status_addr, options);
    let retry_status = fx.status_word();
    let child_id = fx.child_id;
    fx.teardown();

    assert_eq_test!(
        faulted,
        Errno::EFAULT.as_u64(),
        "an unmapped status pointer must be EFAULT"
    );
    assert_eq_test!(
        retry,
        child_id as u64,
        "the stop report was consumed by the faulting wait"
    );
    assert_eq_test!(
        retry_status,
        Some(wait_status_stopped(SIGSTOP) as i32),
        "the retried wait reported the wrong status"
    );
    pass!()
}

pub fn test_waitpid_wcontinued_reports_a_resume() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    // `post_continue_report` is what a real `SIGCONT` publishes, and it
    // retires the stop report the same way. `task_group_continue` would also
    // make the child dispatchable, and its entry point is unmapped.
    let Some(child) = task_find_by_id(fx.child_id) else {
        fx.teardown();
        return fail!("the child vanished");
    };
    let staged = task_group_stop(fx.child_id, SIGSTOP);
    child.post_continue_report();
    drop(child);
    if !staged {
        fx.teardown();
        return fail!("could not stop the child");
    }

    let options = (WCONTINUED | WNOHANG) as u64;
    let first = fx.wait(fx.child_id as i64, fx.status_addr, options);
    let status = fx.status_word();
    let second = fx.wait(fx.child_id as i64, fx.status_addr, options);
    let child_id = fx.child_id;
    fx.teardown();

    assert_eq_test!(
        first,
        child_id as u64,
        "a continued child was not reported under WCONTINUED"
    );
    assert_eq_test!(
        status,
        Some(WAIT_STATUS_CONTINUED as i32),
        "the continue status word is wrong"
    );
    assert_eq_test!(second, 0, "the same continue was reported twice");
    pass!()
}

/// A null status pointer is POSIX's "discard the status", not an error.
pub fn test_waitpid_accepts_a_null_status_pointer() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    exit_child_normally(fx.child_id, 3);
    let rc = fx.wait(fx.child_id as i64, 0, 0);
    let child_id = fx.child_id;
    fx.teardown();

    assert_eq_test!(
        rc,
        child_id as u64,
        "a null status pointer must still reap and report the pid"
    );
    pass!()
}

/// An exit status lost to a bad pointer is lost forever.
pub fn test_waitpid_bad_status_pointer_faults_without_reaping() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    exit_child_normally(fx.child_id, 9);

    // Kernel-space is refused by the pointer decode; page 1 is a valid user
    // address below `PROCESS_CODE_START_VA` that no process maps, so the copy
    // itself is what refuses it.
    let kernel_addr = fx.wait(fx.child_id as i64, 0xFFFF_8000_0000_0000, 0);
    let after_kernel_addr = fx.child_state();
    let unmapped = fx.wait(fx.child_id as i64, 0x1000, 0);
    let after_unmapped = fx.child_state();
    let rc = fx.wait(fx.child_id as i64, fx.status_addr, 0);
    let child_id = fx.child_id;
    let status = fx.status_word();
    fx.teardown();

    assert_eq_test!(
        kernel_addr,
        Errno::EFAULT.as_u64(),
        "a kernel-space status pointer must be EFAULT"
    );
    assert_eq_test!(
        after_kernel_addr,
        Some(TaskStatus::Zombie),
        "an EFAULT must not reap the child"
    );
    assert_eq_test!(
        unmapped,
        Errno::EFAULT.as_u64(),
        "an unmapped status pointer must be EFAULT"
    );
    assert_eq_test!(
        after_unmapped,
        Some(TaskStatus::Zombie),
        "an EFAULT must not reap the child"
    );
    assert_eq_test!(
        rc,
        child_id as u64,
        "the exit status was no longer claimable after two faults"
    );
    assert_eq_test!(
        status,
        Some(wait_status_exited(9) as i32),
        "the retained exit code came back wrong"
    );
    pass!()
}

/// Not silently ignored: a caller asking for `WNOWAIT` must not be answered
/// with a reap.
pub fn test_waitpid_rejects_unknown_option_bits() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_wait_fixture() else {
        return fail!("could not build the wait fixture");
    };

    exit_child_normally(fx.child_id, 1);
    // `WNOWAIT` (0x0100_0000) plus a reserved low bit.
    let rc = fx.wait(fx.child_id as i64, fx.status_addr, 0x0100_0004);
    let still_zombie = fx.child_state();
    fx.teardown();

    assert_eq_test!(
        rc,
        Errno::EINVAL.as_u64(),
        "unknown waitpid options must be EINVAL"
    );
    assert_eq_test!(
        still_zombie,
        Some(TaskStatus::Zombie),
        "a rejected waitpid must not reap"
    );
    pass!()
}

/// The handler adds only the caller's own `schedule()` on top of the fan-out.
pub fn test_exit_group_terminates_every_thread_of_the_group() -> TestResult {
    let _fixture = SyscallFixture::new();

    let leader_id = create_user_task();
    assert_test!(leader_id != INVALID_TASK_ID, "no group leader task");
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(&leader, None, thread_flags, 0, 0, 0, 0) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            drop(leader);
            task_terminate(leader_id);
            return fail!("could not clone a sibling thread");
        }
    };
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");
    let joined_group = thread.tgid == leader.tgid;
    let tgid = leader.tgid;
    drop(leader);
    drop(thread);

    let terminated = task_group_exit(tgid, 4);
    let thread_state = task_find_by_id(thread_id).map(|t| t.status());
    let leader_state = task_find_by_id(leader_id).map(|t| t.status());

    task_terminate(thread_id);
    task_terminate(leader_id);

    assert_test!(joined_group, "the clone did not join the leader's group");
    assert_test!(
        terminated >= 2,
        "exit_group reported fewer than both group members"
    );
    assert_test!(
        !matches!(
            thread_state,
            Some(TaskStatus::Ready) | Some(TaskStatus::Blocked) | Some(TaskStatus::Running)
        ),
        "the sibling thread survived exit_group"
    );
    assert_test!(
        !matches!(
            leader_state,
            Some(TaskStatus::Ready) | Some(TaskStatus::Blocked) | Some(TaskStatus::Running)
        ),
        "the group leader survived exit_group"
    );
    pass!()
}

pub fn test_getpid_is_the_tgid_and_gettid_is_the_task_id() -> TestResult {
    let _fixture = SyscallFixture::new();

    let leader_id = create_user_task();
    assert_test!(leader_id != INVALID_TASK_ID, "no group leader task");
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let Some(table) = table_of(&leader) else {
        drop(leader);
        task_terminate(leader_id);
        return fail!("leader has no fd table");
    };

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(&leader, None, thread_flags, 0, 0, 0, 0) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            drop(leader);
            task_terminate(leader_id);
            return fail!("could not clone a sibling thread");
        }
    };
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");

    let thread_pid = invoke(syscall_getpid, &thread, table, [0; 6]);
    let thread_tid = invoke(syscall_gettid, &thread, table, [0; 6]);
    let leader_pid = invoke(syscall_getpid, &leader, table, [0; 6]);

    drop(leader);
    drop(thread);
    task_terminate(thread_id);
    task_terminate(leader_id);

    assert_eq_test!(
        thread_pid,
        leader_id as u64,
        "a thread's getpid must be the thread-group id"
    );
    assert_eq_test!(thread_tid, thread_id as u64, "gettid must be the task id");
    assert_test!(
        thread_pid != thread_tid,
        "getpid and gettid must differ on a non-leader thread"
    );
    assert_eq_test!(
        leader_pid,
        leader_id as u64,
        "the leader's getpid must be its own id"
    );
    pass!()
}

/// A `CLONE_THREAD` sibling's own parent link names the thread that created
/// it — another thread of the same process — so answering from it makes two
/// threads disagree about who their parent is.
pub fn test_getppid_is_the_group_parent_for_every_thread() -> TestResult {
    let _fixture = SyscallFixture::new();

    let grandparent_id = create_user_task();
    let leader_id = create_user_task();
    assert_test!(
        grandparent_id != INVALID_TASK_ID && leader_id != INVALID_TASK_ID,
        "could not create the process tasks"
    );
    if task_set_parent(leader_id, grandparent_id) != 0 {
        task_terminate(leader_id);
        task_terminate(grandparent_id);
        return fail!("could not parent the leader");
    }
    let leader = assert_some!(task_find_by_id(leader_id), "leader lookup failed");
    let Some(table) = table_of(&leader) else {
        drop(leader);
        task_terminate(leader_id);
        task_terminate(grandparent_id);
        return fail!("leader has no fd table");
    };

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(&leader, None, thread_flags, 0, 0, 0, 0) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            drop(leader);
            task_terminate(leader_id);
            task_terminate(grandparent_id);
            return fail!("could not clone a sibling thread");
        }
    };
    let thread = assert_some!(task_find_by_id(thread_id), "thread lookup failed");

    let thread_ppid = invoke(syscall_getppid, &thread, table, [0; 6]);
    let leader_ppid = invoke(syscall_getppid, &leader, table, [0; 6]);

    drop(leader);
    drop(thread);
    task_terminate(thread_id);
    task_terminate(leader_id);
    task_terminate(grandparent_id);

    assert_eq_test!(
        thread_ppid,
        leader_ppid,
        "two threads of one process disagree about their parent"
    );
    assert_eq_test!(
        leader_ppid,
        grandparent_id as u64,
        "the leader's getppid must be its own parent"
    );
    assert_eq_test!(
        thread_ppid,
        grandparent_id as u64,
        "a thread's getppid must be the process's parent, not its creator"
    );
    pass!()
}

/// `exit_group` reports `NoReturn`, so the caller must be terminated on every
/// path — including one the group fan-out never reached.
pub fn test_exit_group_terminates_the_caller_when_the_fanout_misses() -> TestResult {
    let _fixture = SyscallFixture::new();

    let caller_id = create_user_task();
    let foreign_id = create_user_task();
    assert_test!(
        caller_id != INVALID_TASK_ID && foreign_id != INVALID_TASK_ID,
        "could not create the exit_group tasks"
    );

    exit_group_terminate(caller_id, foreign_id, 3);
    let caller_state = task_find_by_id(caller_id).map(|t| t.status());

    task_terminate(foreign_id);
    task_terminate(caller_id);

    assert_test!(
        !matches!(
            caller_state,
            Some(TaskStatus::Ready) | Some(TaskStatus::Blocked) | Some(TaskStatus::Running)
        ),
        "exit_group left its caller runnable after the group fan-out missed it"
    );
    pass!()
}

struct PageFixture {
    task_id: u32,
    task: TaskRef,
    table: FdTable,
    page: u64,
}

fn build_page_fixture() -> Option<PageFixture> {
    let task_id = create_user_task();
    if task_id == INVALID_TASK_ID {
        return None;
    }
    let task = task_find_by_id(task_id)?;
    let table = table_of(&task)?;
    let page = map_user_rw_page(table)?;
    Some(PageFixture {
        task_id,
        task,
        table,
        page,
    })
}

impl PageFixture {
    fn call(&self, handler: SyscallHandler, args: [u64; 6]) -> u64 {
        invoke(handler, &self.task, self.table, args)
    }

    fn teardown(self) {
        let PageFixture { task_id, task, .. } = self;
        drop(task);
        task_terminate(task_id);
    }
}

/// `FUTEX_PRIVATE_FLAG` is set by every glibc- and std-shaped caller, and
/// matching `op` exactly made all of them `ENOSYS`.
pub fn test_futex_private_flag_is_not_enosys() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };
    if !user_copy_out(fx.table, fx.page, &1u32) {
        fx.teardown();
        return fail!("could not initialise the futex word");
    }

    let wake = fx.call(
        syscall_futex,
        [fx.page, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0],
    );
    let wake_bitset = fx.call(
        syscall_futex,
        [
            fx.page,
            FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG,
            1,
            0,
            0,
            FUTEX_BITSET_MATCH_ANY as u64,
        ],
    );
    // A mismatched word never blocks, so this reaches the command decode
    // without needing a dispatchable waiter.
    let wait = fx.call(
        syscall_futex,
        [fx.page, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 2, 0, 0, 0],
    );
    fx.teardown();

    assert_eq_test!(wake, 0, "private FUTEX_WAKE with no waiters must report 0");
    assert_eq_test!(
        wake_bitset,
        0,
        "private FUTEX_WAKE_BITSET with no waiters must report 0"
    );
    assert_eq_test!(
        wait,
        Errno::EAGAIN.as_u64(),
        "private FUTEX_WAIT on a mismatched word must be EAGAIN"
    );
    pass!()
}

/// `FUTEX_CLOCK_REALTIME` only means anything on an absolute timeout;
/// ignoring it elsewhere would silently reinterpret a relative one.
pub fn test_futex_clock_realtime_needs_an_absolute_timeout() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };
    if !user_copy_out(fx.table, fx.page, &1u32) {
        fx.teardown();
        return fail!("could not initialise the futex word");
    }

    let plain_wait = fx.call(
        syscall_futex,
        [
            fx.page,
            FUTEX_WAIT | FUTEX_CLOCK_REALTIME | FUTEX_PRIVATE_FLAG,
            1,
            0,
            0,
            0,
        ],
    );
    let wake = fx.call(
        syscall_futex,
        [fx.page, FUTEX_WAKE | FUTEX_CLOCK_REALTIME, 1, 0, 0, 0],
    );
    fx.teardown();

    assert_eq_test!(
        plain_wait,
        Errno::ENOSYS.as_u64(),
        "FUTEX_CLOCK_REALTIME with plain FUTEX_WAIT must be ENOSYS"
    );
    assert_eq_test!(
        wake,
        Errno::ENOSYS.as_u64(),
        "FUTEX_CLOCK_REALTIME with FUTEX_WAKE must be ENOSYS"
    );
    pass!()
}

/// A zero bitset is a waiter no wake could ever match.
pub fn test_futex_zero_bitset_is_einval() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };
    if !user_copy_out(fx.table, fx.page, &1u32) {
        fx.teardown();
        return fail!("could not initialise the futex word");
    }

    let wait = fx.call(
        syscall_futex,
        [fx.page, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0],
    );
    let wake = fx.call(
        syscall_futex,
        [fx.page, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0],
    );
    fx.teardown();

    assert_eq_test!(
        wait,
        Errno::EINVAL.as_u64(),
        "a zero FUTEX_WAIT_BITSET mask must be EINVAL"
    );
    assert_eq_test!(
        wake,
        Errno::EINVAL.as_u64(),
        "a zero FUTEX_WAKE_BITSET mask must be EINVAL"
    );
    pass!()
}

/// `FUTEX_WAIT_BITSET`'s timeout is absolute: read as a relative duration, a
/// deadline in the past would be a ~55-year sleep.
pub fn test_futex_wait_bitset_past_absolute_deadline_times_out() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };
    let ts_addr = fx.page + 16;
    let staged = user_copy_out(fx.table, fx.page, &1u32)
        && user_copy_out(
            fx.table,
            ts_addr,
            &Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
        );
    if !staged {
        fx.teardown();
        return fail!("could not stage the futex word and deadline");
    }

    // The Running->Blocked CAS needs a real current task; the past-deadline
    // branch takes `set_current_runnable` and never deschedules.
    if !make_task_current(fx.task_id) {
        park_bootstrap_on_current_cpu();
        fx.teardown();
        return fail!("could not dispatch the waiter as current");
    }
    let rc = fx.call(
        syscall_futex,
        [
            fx.page,
            FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
            1,
            ts_addr,
            0,
            FUTEX_BITSET_MATCH_ANY as u64,
        ],
    );
    // Read the bucket while the fixture task is still current: the hooks key
    // on the running task's address space.
    let leaked = slopos_sched::futex::futex_waiters_for_test(fx.page);
    park_bootstrap_on_current_cpu();
    fx.teardown();

    assert_eq_test!(
        rc,
        Errno::ETIMEDOUT.as_u64(),
        "an absolute deadline in the past must be ETIMEDOUT immediately"
    );
    assert_eq_test!(leaked, 0, "the timed-out waiter was left in its bucket");
    pass!()
}

pub fn test_futex_wake_bitset_wakes_only_the_intersecting_waiter() -> TestResult {
    let _fixture = SyscallFixture::new();

    let addr = 0x5000_0000u64;
    let first = create_kernel_task();
    let second = create_kernel_task();
    if first == INVALID_TASK_ID || second == INVALID_TASK_ID {
        task_terminate(first);
        task_terminate(second);
        return fail!("could not create the waiter tasks");
    }

    let mut parked = 0usize;
    for (id, mask) in [(first, 0b01u32), (second, 0b10u32)] {
        if make_task_current(id) && slopos_sched::futex::futex_park_bitset_for_test(addr, mask) {
            parked += 1;
        }
    }
    park_bootstrap_on_current_cpu();

    let queued = slopos_sched::futex::futex_waiters_for_test(addr);
    let woken = slopos_sched::futex::futex_wake_bitset(addr, u32::MAX, 0b10);
    let remaining = slopos_sched::futex::futex_waiters_for_test(addr);
    let remaining_mask = slopos_sched::futex::futex_waiter_bitset_for_test(addr);
    let drained = slopos_sched::futex::futex_wake(addr, u32::MAX);

    task_terminate(first);
    task_terminate(second);

    assert_eq_test!(parked, 2, "both waiters must park");
    assert_eq_test!(queued, 2, "the bucket must hold both waiters");
    assert_eq_test!(woken, 1, "only the intersecting waiter may be woken");
    assert_eq_test!(remaining, 1, "the non-matching waiter must stay queued");
    assert_eq_test!(remaining_mask, Some(0b01u32), "the wrong waiter was woken");
    assert_eq_test!(drained, 1, "a match-any wake must take the survivor");
    pass!()
}

/// Two buckets, so this is also the nested-acquire path: the bucket hash
/// shifts by 21 slots per 4-byte step, so `addr` and `addr + 4` never
/// collide.
pub fn test_futex_requeue_moves_the_surplus_to_the_second_word() -> TestResult {
    let _fixture = SyscallFixture::new();

    let src = 0x5100_0000u64;
    let dst = src + 4;
    let mut ids = [INVALID_TASK_ID; 3];
    let mut parked = 0usize;
    for slot in ids.iter_mut() {
        let id = create_kernel_task();
        if id == INVALID_TASK_ID {
            break;
        }
        *slot = id;
        if make_task_current(id) && slopos_sched::futex::futex_park_for_test(src) {
            parked += 1;
        }
    }
    park_bootstrap_on_current_cpu();

    let moved = slopos_sched::futex::futex_requeue(src, dst, 1, 2, None);
    let left_on_src = slopos_sched::futex::futex_waiters_for_test(src);
    let on_dst = slopos_sched::futex::futex_waiters_for_test(dst);
    let drained = slopos_sched::futex::futex_wake(dst, u32::MAX);

    for &id in ids.iter() {
        if id != INVALID_TASK_ID {
            task_terminate(id);
        }
    }

    assert_eq_test!(parked, 3, "all three waiters must park");
    assert_eq_test!(moved, 3, "requeue must report one woken plus two requeued");
    assert_eq_test!(left_on_src, 0, "the source bucket must be empty");
    assert_eq_test!(
        on_dst,
        2,
        "both surplus waiters must land on the second word"
    );
    assert_eq_test!(drained, 2, "the requeued waiters must be wakeable on dst");
    pass!()
}

/// SLOPOS-2026-0056. A futex is keyed on (address space, address): keyed on
/// the address alone, one process's `FUTEX_REQUEUE` restamps a foreign
/// waiter's `futex_addr`, after which the victim's own `FUTEX_WAKE` can never
/// match it and a `std` park with no timeout blocks forever.
pub fn test_futex_key_carries_the_address_space() -> TestResult {
    let _fixture = SyscallFixture::new();

    let addr = 0x5200_0000u64;
    let dst = addr + 4;
    let victim_id = create_user_task();
    let attacker_id = create_user_task();
    if victim_id == INVALID_TASK_ID || attacker_id == INVALID_TASK_ID {
        task_terminate(victim_id);
        task_terminate(attacker_id);
        return fail!("could not create two user tasks");
    }

    let victim_parked =
        make_task_current(victim_id) && slopos_sched::futex::futex_park_for_test(addr);
    let victim_queued = slopos_sched::futex::futex_waiters_for_test(addr);

    let attacker_parked =
        make_task_current(attacker_id) && slopos_sched::futex::futex_park_for_test(addr);
    // Still the attacker: every call below is one address space's view.
    let attacker_queued = slopos_sched::futex::futex_waiters_for_test(addr);
    let requeued = slopos_sched::futex::futex_requeue(addr, dst, 0, u32::MAX, None);
    let attacker_left = slopos_sched::futex::futex_waiters_for_test(addr);
    let attacker_drained = slopos_sched::futex::futex_wake(dst, u32::MAX);

    // Not `make_task_current`: its nascent CAS only fires on a task's first
    // dispatch, and the victim has already had one.
    let switched = dispatch_task_for_test(slopos_arch::pcr::get_current_cpu(), victim_id);
    let victim_left = slopos_sched::futex::futex_waiters_for_test(addr);
    let victim_moved = slopos_sched::futex::futex_waiters_for_test(dst);
    let victim_woken = slopos_sched::futex::futex_wake(addr, u32::MAX);

    park_bootstrap_on_current_cpu();
    task_terminate(attacker_id);
    task_terminate(victim_id);

    assert_test!(
        victim_parked && attacker_parked && switched,
        "could not park both waiters"
    );
    assert_eq_test!(victim_queued, 1, "the victim did not park on its own word");
    assert_eq_test!(
        attacker_queued,
        1,
        "the attacker sees a waiter it does not own"
    );
    assert_eq_test!(
        requeued,
        1,
        "requeue moved a waiter belonging to another address space"
    );
    assert_eq_test!(attacker_left, 0, "the attacker's own waiter did not move");
    assert_eq_test!(attacker_drained, 1, "the requeued waiter is not on dst");
    assert_eq_test!(
        victim_left,
        1,
        "a foreign requeue took the victim's waiter off its own word"
    );
    assert_eq_test!(victim_moved, 0, "the victim's waiter was moved to dst");
    assert_eq_test!(
        victim_woken,
        1,
        "the victim can no longer wake its own waiter"
    );
    pass!()
}

/// The clock counts the settled total plus the slice in progress.
pub fn test_thread_cputime_is_nondecreasing() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };

    // A live slice, so the handler's `now - last_run_timestamp` term is the
    // part that moves between the two reads.
    fx.task.add_total_runtime(1_000_000);
    fx.task
        .set_last_run_timestamp(slopos_ostd::kdiag_timestamp());

    let read = || -> Option<Timespec> {
        let rc = fx.call(
            syscall_clock_gettime,
            [CLOCK_THREAD_CPUTIME_ID, fx.page, 0, 0, 0, 0],
        );
        if rc != 0 {
            return None;
        }
        user_copy_in::<Timespec>(fx.table, fx.page)
    };

    let first = read();
    for _ in 0..200_000 {
        core::hint::spin_loop();
    }
    let second = read();
    fx.teardown();

    let Some(first) = first else {
        return fail!("the first CLOCK_THREAD_CPUTIME_ID read failed");
    };
    let Some(second) = second else {
        return fail!("the second CLOCK_THREAD_CPUTIME_ID read failed");
    };
    let first_ns = first.tv_sec as i128 * 1_000_000_000 + first.tv_nsec as i128;
    let second_ns = second.tv_sec as i128 * 1_000_000_000 + second.tv_nsec as i128;

    assert_test!(
        (0..1_000_000_000).contains(&first.tv_nsec),
        "tv_nsec outside its normalised range"
    );
    assert_test!(
        first_ns > 0,
        "a task with recorded runtime must report non-zero CPU time"
    );
    assert_test!(second_ns >= first_ns, "the thread CPU clock went backwards");
    pass!()
}

/// Only the wall clock is settable, and only with a legal `tv_nsec`.
pub fn test_clock_settime_refuses_unsettable_clocks() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };
    if !user_copy_out(
        fx.table,
        fx.page,
        &Timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 0,
        },
    ) {
        fx.teardown();
        return fail!("could not stage the timespec");
    }

    let monotonic = fx.call(
        syscall_clock_settime,
        [CLOCK_MONOTONIC, fx.page, 0, 0, 0, 0],
    );
    let cputime = fx.call(
        syscall_clock_settime,
        [CLOCK_THREAD_CPUTIME_ID, fx.page, 0, 0, 0, 0],
    );

    let staged = user_copy_out(
        fx.table,
        fx.page,
        &Timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 1_000_000_000,
        },
    );
    let bad_nanos = fx.call(syscall_clock_settime, [CLOCK_REALTIME, fx.page, 0, 0, 0, 0]);
    fx.teardown();

    assert_test!(staged, "could not stage the out-of-range timespec");
    assert_eq_test!(
        monotonic,
        Errno::EINVAL.as_u64(),
        "CLOCK_MONOTONIC must not be settable"
    );
    assert_eq_test!(
        cputime,
        Errno::EINVAL.as_u64(),
        "a CPU-time clock must not be settable"
    );
    assert_eq_test!(
        bad_nanos,
        Errno::EINVAL.as_u64(),
        "tv_nsec outside 0..1e9 must be EINVAL"
    );
    pass!()
}

fn uts_field_is(field: &[u8; 65], want: &[u8]) -> bool {
    let Some(nul) = field.iter().position(|&b| b == 0) else {
        return false;
    };
    &field[..nul] == want && field[nul..].iter().all(|&b| b == 0)
}

pub fn test_uname_reports_a_nul_terminated_identity() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };

    let rc = fx.call(syscall_uname, [fx.page, 0, 0, 0, 0, 0]);
    let uts = user_copy_in::<UserUtsname>(fx.table, fx.page);
    let fault = fx.call(syscall_uname, [0, 0, 0, 0, 0, 0]);
    fx.teardown();

    assert_eq_test!(rc, 0, "uname failed");
    assert_eq_test!(
        fault,
        Errno::EFAULT.as_u64(),
        "a null out-pointer must be EFAULT"
    );
    let Some(uts) = uts else {
        return fail!("could not read the utsname back");
    };
    assert_test!(
        uts_field_is(&uts.sysname, b"SlopOS"),
        "sysname must be a zero-padded \"SlopOS\""
    );
    assert_test!(
        uts_field_is(&uts.nodename, b"slopos"),
        "nodename must be a zero-padded \"slopos\""
    );
    assert_test!(
        uts_field_is(&uts.machine, b"x86_64"),
        "machine must be a zero-padded \"x86_64\""
    );
    assert_test!(
        uts.release[0] != 0 && uts.version[0] != 0,
        "release and version must be populated"
    );
    pass!()
}

fn current_cwd_copy(buf: &mut [u8; 128]) -> usize {
    let Some(current) = Current::get() else {
        return 0;
    };
    current.task().with_cwd(&current, |cwd| {
        let trimmed = match cwd.iter().position(|&b| b == 0) {
            Some(end) => &cwd[..end],
            None => cwd,
        };
        let n = trimmed.len().min(buf.len());
        buf[..n].copy_from_slice(&trimmed[..n]);
        n
    })
}

/// The stored cwd is prefixed onto every relative lookup, so a `..` left in
/// it would name a different directory than the one that was checked.
pub fn test_chdir_validates_and_canonicalises() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };

    // `chdir` resolves against, and writes, the *current* task's cwd, so the
    // fixture task has to be the one running.
    if !make_task_current(fx.task_id) {
        park_bootstrap_on_current_cpu();
        fx.teardown();
        return fail!("could not dispatch the fixture task as current");
    }

    let stage = |bytes: &[u8]| -> bool {
        let mut buf = [0u8; 64];
        if bytes.len() + 1 > buf.len() {
            return false;
        }
        buf[..bytes.len()].copy_from_slice(bytes);
        user_copy_out(fx.table, fx.page, &buf)
    };
    let chdir = || fx.call(syscall_chdir, [fx.page, 0, 0, 0, 0, 0]);

    let mut start = [0u8; 128];
    let start_len = current_cwd_copy(&mut start);
    let not_dir = stage(b"/dev/null").then(chdir);
    let ok = stage(b"/dev/../dev/").then(chdir);
    let mut stored = [0u8; 128];
    let stored_len = current_cwd_copy(&mut stored);
    // A missing final component under an existing parent, so the answer is
    // unambiguously ENOENT rather than a walk failure on the parent.
    let missing = stage(b"/dev/no_such_dir").then(chdir);
    let empty = fx.call(syscall_chdir, [0, 0, 0, 0, 0, 0]);

    park_bootstrap_on_current_cpu();
    fx.teardown();

    if &start[..start_len] != b"/" {
        klog_info!("PHASE1_PROC: a fresh task's cwd is not \"/\"");
        return TestResult::Fail;
    }
    assert_eq_test!(
        not_dir,
        Some(Errno::ENOTDIR.as_u64()),
        "chdir into a non-directory must be ENOTDIR"
    );
    assert_eq_test!(ok, Some(0), "chdir into /dev failed");
    if &stored[..stored_len] != b"/dev" {
        klog_info!(
            "PHASE1_PROC: chdir stored {} bytes that are not \"/dev\"",
            stored_len
        );
        return TestResult::Fail;
    }
    assert_eq_test!(
        missing,
        Some(Errno::ENOENT.as_u64()),
        "chdir into a missing directory must be ENOENT"
    );
    assert_eq_test!(
        empty,
        Errno::EFAULT.as_u64(),
        "a null path pointer must be EFAULT"
    );
    pass!()
}

/// Folding `..` against the spelling erases the symlink the `..` crossed, and
/// the stored cwd is prefixed onto every later relative lookup — so the task
/// would resolve against a directory `chdir` never checked. Here
/// `link -> /tmp/cd_real/inner`, so `link/..` is `/tmp/cd_real` by the walk
/// and `/tmp/cd_other` by the spelling.
pub fn test_chdir_stores_the_walked_path_not_the_lexical_one() -> TestResult {
    let _fixture = SyscallFixture::new();
    let Some(fx) = build_page_fixture() else {
        return fail!("could not build the page fixture");
    };

    let staged = vfs_mkdir(b"/tmp/cd_real").is_ok()
        && vfs_mkdir(b"/tmp/cd_real/inner").is_ok()
        && vfs_mkdir(b"/tmp/cd_other").is_ok()
        && vfs_symlink(b"/tmp/cd_real/inner", b"/tmp/cd_other/link").is_ok();
    if !staged || !make_task_current(fx.task_id) {
        park_bootstrap_on_current_cpu();
        fx.teardown();
        return fail!("could not stage the symlinked directory");
    }

    let mut buf = [0u8; 64];
    let arg = b"/tmp/cd_other/link/..";
    buf[..arg.len()].copy_from_slice(arg);
    let rc = user_copy_out(fx.table, fx.page, &buf)
        .then(|| fx.call(syscall_chdir, [fx.page, 0, 0, 0, 0, 0]));
    let mut stored = [0u8; 128];
    let stored_len = current_cwd_copy(&mut stored);

    park_bootstrap_on_current_cpu();
    fx.teardown();
    let _ = slopos_fs::vfs::ops::vfs_unlink(b"/tmp/cd_other/link");
    let _ = slopos_fs::vfs::ops::vfs_rmdir(b"/tmp/cd_other");
    let _ = slopos_fs::vfs::ops::vfs_rmdir(b"/tmp/cd_real/inner");
    let _ = slopos_fs::vfs::ops::vfs_rmdir(b"/tmp/cd_real");

    assert_eq_test!(rc, Some(0), "chdir through a directory symlink failed");
    if &stored[..stored_len] != b"/tmp/cd_real" {
        klog_info!(
            "PHASE1_PROC: chdir stored a lexical parent, {} bytes, not \"/tmp/cd_real\"",
            stored_len
        );
        return TestResult::Fail;
    }
    pass!()
}

slopos_testing::stest!(
    name = test_waitpid_returns_the_pid_and_writes_an_exited_status,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_reports_a_signal_death_in_the_low_seven_bits,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_wait4_reports_the_reaped_childs_usage,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_wnohang_on_a_live_child_returns_zero,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_without_children_is_echild,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_wuntraced_reports_a_stop_exactly_once,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_stop_report_survives_a_faulting_status_pointer,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_wcontinued_reports_a_resume,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_accepts_a_null_status_pointer,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_bad_status_pointer_faults_without_reaping,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_waitpid_rejects_unknown_option_bits,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_exit_group_terminates_every_thread_of_the_group,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_getpid_is_the_tgid_and_gettid_is_the_task_id,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_getppid_is_the_group_parent_for_every_thread,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_exit_group_terminates_the_caller_when_the_fanout_misses,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_private_flag_is_not_enosys,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_clock_realtime_needs_an_absolute_timeout,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_zero_bitset_is_einval,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_wait_bitset_past_absolute_deadline_times_out,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_wake_bitset_wakes_only_the_intersecting_waiter,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_requeue_moves_the_surplus_to_the_second_word,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_futex_key_carries_the_address_space,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_thread_cputime_is_nondecreasing,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_clock_settime_refuses_unsettable_clocks,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_uname_reports_a_nul_terminated_identity,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_chdir_validates_and_canonicalises,
    suite = syscall_proc_phase1
);
slopos_testing::stest!(
    name = test_chdir_stores_the_walked_path_not_the_lexical_one,
    suite = syscall_proc_phase1
);
