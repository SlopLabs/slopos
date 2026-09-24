use core::sync::atomic::Ordering;
use slopos_abi::signal::{
    WAIT_OPTIONS_MASK, WAIT_STATUS_CONTINUED, WCONTINUED, WNOHANG, WUNTRACED, wait_status_exited,
    wait_status_signalled, wait_status_stopped,
};
use slopos_abi::spawn::{SPAWN_MAX_FD_ACTIONS, SpawnAttrs, SpawnFdAction, SpawnFdActionKind};
use slopos_abi::syscall::{
    ARCH_GET_FS, ARCH_SET_FS, FUTEX_CLOCK_REALTIME, FUTEX_CMD_MASK, FUTEX_CMP_REQUEUE,
    FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET, Rusage, Timespec,
    Timeval,
};
use slopos_abi::task::{
    INVALID_TASK_ID, SPAWN_PRIVILEGED, SPAWN_RESERVED, SPAWN_USER_SETTABLE, TASK_FLAG_KERNEL_MODE,
    TaskExitReason, TaskPriority, TaskStatus,
};
use slopos_abi::{Errno, PAGE_SIZE};
use slopos_fs::fileio::FdTable;
use slopos_fs::vfs::canon::{CanonPath, canonicalise_at};
use slopos_fs::vfs::path::RESOLVE_MUST_BE_DIR;
use slopos_fs::vfs::traits::VfsError;
use slopos_ostd::KVec;
use slopos_ostd::task::{ExitInfo, new_group_in_session, new_session_group};
use slopos_sched::scheduler::task_apply_affinity;
use slopos_sched::task::{
    task_consume_zombie, task_default_signals_in_mask, task_find_by_id, task_fork,
    task_peek_exit_info, task_reset_caught_handlers,
};
use slopos_sched::task_struct::{Current, Task};

use slopos_arch::cpu;
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr as MmUserPtr;

use crate::exec;
use crate::syscall::args::{UserBytes, UserPath, UserPtr};
use crate::syscall::common::{
    USER_PATH_MAX, syscall_bounded_from_user, syscall_copy_to_user_bounded, syscall_copy_user_str,
};
use crate::syscall::result::SyscallResult;

fn read_user_ptr_array_terminated(base_ptr: u64, max_count: usize) -> Result<KVec<u64>, ()> {
    let mut out = KVec::<u64>::new();

    for idx in 0..max_count {
        let slot_addr = base_ptr
            .checked_add((idx * core::mem::size_of::<u64>()) as u64)
            .ok_or(())?;
        let user_slot = MmUserPtr::<u64>::try_new(slot_addr).map_err(|_| ())?;
        let value = copy_from_user(user_slot).map_err(|_| ())?;
        if value == 0 {
            return Ok(out);
        }
        out.push(value).map_err(|_| ())?;
    }

    Err(())
}

fn read_user_ptr_array_count(
    base_ptr: u64,
    count: usize,
    max_count: usize,
) -> Result<KVec<u64>, ()> {
    if count > max_count {
        return Err(());
    }

    let mut out = KVec::<u64>::with_capacity(count).map_err(|_| ())?;

    for idx in 0..count {
        let slot_addr = base_ptr
            .checked_add((idx * core::mem::size_of::<u64>()) as u64)
            .ok_or(())?;
        let user_slot = MmUserPtr::<u64>::try_new(slot_addr).map_err(|_| ())?;
        let value = copy_from_user(user_slot).map_err(|_| ())?;
        if value == 0 {
            break;
        }
        out.push(value).map_err(|_| ())?;
    }

    Ok(out)
}

fn read_user_cstr_list(ptrs: &[u64]) -> Result<KVec<KVec<u8>>, Errno> {
    let mut out = KVec::<KVec<u8>>::with_capacity(ptrs.len()).map_err(|_| Errno::ENOMEM)?;

    let mut buf = KVec::<u8>::zeroed(exec::EXEC_MAX_ARG_STRLEN).map_err(|_| Errno::ENOMEM)?;

    // The pointer walk is bounded at `EXEC_MAX_ARG_STRINGS`, so without this
    // running total a long array would have the kernel hold far more than one
    // argument budget before `exec_arg_bytes_fit` ever sees it.
    let mut staged = 0usize;

    for &ptr in ptrs {
        for b in buf.as_mut_slice().iter_mut() {
            *b = 0;
        }
        syscall_copy_user_str(buf.as_mut_slice(), ptr).map_err(|_| Errno::EFAULT)?;
        let len = buf
            .as_slice()
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(buf.len());

        staged = staged
            .checked_add(len)
            .and_then(|t| t.checked_add(1))
            .ok_or(Errno::E2BIG)?;
        if staged > exec::EXEC_MAX_ARG_BYTES {
            return Err(Errno::E2BIG);
        }

        let mut s = KVec::<u8>::with_capacity(len).map_err(|_| Errno::ENOMEM)?;
        s.extend_from_slice(&buf.as_slice()[..len])
            .map_err(|_| Errno::ENOMEM)?;
        out.push(s).map_err(|_| Errno::ENOMEM)?;
    }

    Ok(out)
}

/// Decode the spawn fd-action array from user memory into kernel-owned
/// [`exec::FdAction`]s. Bounded by [`SPAWN_MAX_FD_ACTIONS`].
fn read_user_spawn_actions(attrs: &SpawnAttrs) -> Result<KVec<exec::FdAction>, Errno> {
    let count = attrs.actions_len as usize;
    if count == 0 {
        return Ok(KVec::new());
    }
    if count > SPAWN_MAX_FD_ACTIONS {
        return Err(Errno::EINVAL);
    }
    if attrs.actions_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let mut out = KVec::<exec::FdAction>::with_capacity(count).map_err(|_| Errno::ENOMEM)?;
    for idx in 0..count {
        let slot_addr = attrs
            .actions_ptr
            .checked_add((idx * core::mem::size_of::<SpawnFdAction>()) as u64)
            .ok_or(Errno::EFAULT)?;
        let user_slot =
            MmUserPtr::<SpawnFdAction>::try_new(slot_addr).map_err(|_| Errno::EFAULT)?;
        let raw = copy_from_user(user_slot).map_err(|_| Errno::EFAULT)?;
        let action = match SpawnFdActionKind::from_u32(raw.kind).ok_or(Errno::EINVAL)? {
            SpawnFdActionKind::CloneFd => exec::FdAction::Clone {
                src_fd: raw.src_fd,
                target_fd: raw.target_fd,
            },
            SpawnFdActionKind::TransferFd => exec::FdAction::Transfer {
                src_fd: raw.src_fd,
                target_fd: raw.target_fd,
            },
            SpawnFdActionKind::Close => exec::FdAction::Close {
                target_fd: raw.target_fd,
            },
        };
        out.push(action).map_err(|_| Errno::ENOMEM)?;
    }
    Ok(out)
}

/// Classify the caller-supplied `SpawnAttrs::flags`, returning the subset the
/// child is allowed to inherit from the request.
///
/// Order is load-bearing: an undefined bit is answered as malformed before
/// anything is said about privilege, so probing reserved bits never learns from
/// an `EPERM` that a bit *means* something. The privileged bits a child ends up
/// with come from [`crate::exec::grants`].
fn validate_spawn_flags(flags: u16) -> Result<u16, Errno> {
    if flags & SPAWN_RESERVED != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & SPAWN_PRIVILEGED != 0 {
        return Err(Errno::EPERM);
    }
    // `USER_MODE` is dropped rather than refused: `spawn_program_with_attrs`
    // ORs it back in unconditionally.
    Ok(flags & SPAWN_USER_SETTABLE)
}

/// The directory a spawned child starts in: the `SpawnAttrs` one when given,
/// otherwise the spawner's own. Resolved in the spawner's context, because
/// the child has none yet, and a bad cwd must fail the spawn.
#[inline(never)]
fn spawn_child_cwd(
    ctx: &crate::syscall::context::SyscallContext,
    attrs: &SpawnAttrs,
) -> Result<CanonPath, Errno> {
    if attrs.cwd_ptr == 0 {
        return ctx
            .with_cwd(|cwd| canonicalise_at(b".", cwd))
            .map_err(VfsError::to_errno);
    }
    if attrs.cwd_len == 0 {
        return Err(Errno::EINVAL);
    }
    if attrs.cwd_len as usize > USER_PATH_MAX {
        return Err(Errno::ENAMETOOLONG);
    }
    let mut buf = KVec::<u8>::zeroed(attrs.cwd_len as usize).map_err(|_| Errno::ENOMEM)?;
    let copied = syscall_bounded_from_user(
        buf.as_mut_slice(),
        attrs.cwd_ptr,
        attrs.cwd_len,
        USER_PATH_MAX,
    )
    .map_err(|_| Errno::EFAULT)?;
    let requested = match buf[..copied].iter().position(|&b| b == 0) {
        Some(end) => &buf[..end],
        None => &buf[..copied],
    };
    if requested.is_empty() {
        return Err(Errno::EINVAL);
    }
    ctx.with_cwd(|cwd| resolve_new_cwd(requested, cwd))
}

/// Stage the program path off the caller's buffer. Length-delimited rather
/// than [`UserPath`], because the spawn ABI passes a length, not a NUL.
/// `#[inline(never)]`: the handler's own frame is already at the 2 KiB gate.
#[inline(never)]
fn spawn_program_path(path: &UserBytes) -> Result<KVec<u8>, Errno> {
    let mut buf = KVec::<u8>::zeroed(USER_PATH_MAX).map_err(|_| Errno::ENOMEM)?;
    let copied = syscall_bounded_from_user(
        buf.as_mut_slice(),
        path.base_u64(),
        path.len() as u64,
        USER_PATH_MAX,
    )
    .map_err(|_| Errno::EFAULT)?;
    buf.truncate(copied);
    Ok(buf)
}

define_syscall!(syscall_spawn_path
    (ctx, path: UserBytes, argv_ptr: u64, argc_raw: u32, attrs_ptr: u64)
    cap(NoneSelf)
    -> Result<u64, Errno>
{
    if path.base_u64() == 0 || path.len() == 0 || path.len() > USER_PATH_MAX {
        return Err(Errno::EINVAL);
    }
    if attrs_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let attrs_user = MmUserPtr::<SpawnAttrs>::try_new(attrs_ptr).map_err(|_| Errno::EFAULT)?;
    let attrs = copy_from_user(attrs_user).map_err(|_| Errno::EFAULT)?;

    let priority = TaskPriority::try_from_u8(attrs.priority).ok_or(Errno::EINVAL)?;
    // Userland picks between the two ordinary tiers and nothing else. `High` is
    // handed out by program identity (`exec::grants`) — a `loop {}` binary at
    // that tier starves the machine.
    if !matches!(priority, TaskPriority::Normal | TaskPriority::Low) {
        return Err(Errno::EINVAL);
    }
    let flags = validate_spawn_flags(attrs.flags)?;
    let argc = argc_raw as usize;

    let path_buf = spawn_program_path(&path)?;

    let argv_storage = if argv_ptr != 0 && argc > 0 {
        let argv_ptrs = read_user_ptr_array_count(argv_ptr, argc, exec::EXEC_MAX_ARG_STRINGS)
            .map_err(|_| Errno::EINVAL)?;
        Some(read_user_cstr_list(argv_ptrs.as_slice())?)
    } else {
        None
    };

    let argv_refs = match argv_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return Err(Errno::ENOMEM),
        None => None,
    };

    let envp_storage = if attrs.envp_ptr != 0 && attrs.envp_len > 0 {
        let envp_ptrs = read_user_ptr_array_count(
            attrs.envp_ptr,
            attrs.envp_len as usize,
            exec::EXEC_MAX_ARG_STRINGS,
        )
        .map_err(|_| Errno::EINVAL)?;
        Some(read_user_cstr_list(envp_ptrs.as_slice())?)
    } else {
        None
    };

    let envp_refs = match envp_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return Err(Errno::ENOMEM),
        None => None,
    };

    if !exec::exec_arg_bytes_fit(argv_refs.as_deref(), envp_refs.as_deref()) {
        return Err(Errno::E2BIG);
    }

    let actions = read_user_spawn_actions(&attrs)?;
    let child_cwd = spawn_child_cwd(ctx, &attrs)?;

    // The spawner's own table, so the child's fd actions clone from the process
    // that asked rather than from whoever holds its number by then.
    let parent_table = ctx.require_process().ok();
    let parent_tid = ctx.task_id();
    match exec::spawn_program_with_cwd(
        path_buf.as_slice(),
        argv_refs.as_deref(),
        envp_refs.as_deref(),
        priority,
        flags,
        actions.as_slice(),
        attrs.sigdefault_mask,
        parent_table,
        parent_tid,
        child_cwd.as_bytes(),
    ) {
        Ok(task_id) => Ok(task_id as u64),
        Err(err) => Ok((err as i32) as u64),
    }
});

define_syscall!(syscall_sigdefault
    (ctx, mask: u64) cap(NoneSelf)
    -> Result<u64, Errno>
{
    if let Some(task) = Some(ctx.task()) {
        task_default_signals_in_mask(task, mask);
    }
    Ok(0)
});

/// What `finish_wait` owes the child once the status word has reached user
/// memory. Every variant is idempotent, so a losing racer commits nothing.
enum WaitCommit {
    Reap,
    ConsumeStop,
    ConsumeContinue,
}

struct WaitReport {
    child_id: u32,
    status: u32,
    commit: WaitCommit,
}

/// A death caused by a signal reports `WIFSIGNALED`, whichever path recorded
/// it. `TaskExitReason::UserFault` is a *diagnostic* distinction — which
/// vector, for the klog line — not a different kind of death: a task the
/// kernel killed on an unresolvable #PF died of `SIGSEGV`, and a waiter that
/// read `exited(139)` for it could not tell that from a program that called
/// `exit(139)`. Both arms already stamp `exit_signal`; only this word was
/// keyed on the reason.
fn exit_status_word(info: &ExitInfo) -> u32 {
    let signalled = matches!(
        info.exit_reason,
        TaskExitReason::Signalled | TaskExitReason::UserFault
    );
    if signalled && info.signal != 0 {
        wait_status_signalled(info.signal)
    } else {
        wait_status_exited(info.exit_code as u8)
    }
}

/// Non-destructive, like the zombie peek it sits beside: a stop or continue
/// report is consume-once, and consuming it here would lose it outright when
/// the status word turns out to be unwritable. `finish_wait` commits.
fn child_report(child: &Task, wuntraced: bool, wcontinued: bool) -> Option<WaitReport> {
    let child_id = child.task_id;
    if child.status() == TaskStatus::Zombie
        && let Some(info) = task_peek_exit_info(child_id)
    {
        return Some(WaitReport {
            child_id,
            status: exit_status_word(&info),
            commit: WaitCommit::Reap,
        });
    }
    // Keyed on the report rather than on `is_stopped()`: a stop posted to a
    // member still executing parks it only at its next return-to-user
    // boundary, and the parent must not wait for that to learn of it.
    if wuntraced && let Some(signum) = child.stop_report() {
        return Some(WaitReport {
            child_id,
            status: wait_status_stopped(signum),
            commit: WaitCommit::ConsumeStop,
        });
    }
    if wcontinued && child.has_continue_report() {
        return Some(WaitReport {
            child_id,
            status: WAIT_STATUS_CONTINUED,
            commit: WaitCommit::ConsumeContinue,
        });
    }
    None
}

/// Whether `wait4` may report `child` at all.
///
/// A non-leader thread is linked into its group leader's parent's children
/// list, but `wait` reports *processes*: returning one would hand its tid back
/// as a pid and reap a task the caller never spawned.
fn is_waitable_child(child: &Task) -> bool {
    child.tgid == INVALID_TASK_ID || child.tgid == child.task_id
}

/// `target` of `None` is wait-any. Every step is a peek: nothing is claimed
/// until `finish_wait`.
fn scan_children(
    caller_id: u32,
    target: Option<u32>,
    wuntraced: bool,
    wcontinued: bool,
) -> Option<WaitReport> {
    if let Some(id) = target {
        let child = task_find_by_id(id)?;
        if child.parent_task_id() != caller_id || !is_waitable_child(&child) {
            return None;
        }
        return child_report(&child, wuntraced, wcontinued);
    }

    if let Some(id) = slopos_sched::task::task_first_exited_child(caller_id)
        && let Some(child) = task_find_by_id(id)
        && is_waitable_child(&child)
        && let Some(report) = child_report(&child, false, false)
    {
        return Some(report);
    }
    let id = slopos_sched::task::task_first_reported_child(caller_id, wuntraced, wcontinued)?;
    let child = task_find_by_id(id)?;
    if !is_waitable_child(&child) {
        return None;
    }
    child_report(&child, wuntraced, wcontinued)
}

fn rusage_of(cpu_ticks: u64, peak_resident_pages: u32) -> Rusage {
    let micros = slopos_kernel_services::clock::ticks_to_microseconds(cpu_ticks);
    Rusage {
        ru_utime: Timeval {
            tv_sec: (micros / 1_000_000) as i64,
            tv_usec: (micros % 1_000_000) as i64,
        },
        ru_maxrss: i64::from(peak_resident_pages) * (PAGE_SIZE / 1024) as i64,
        ..Rusage::default()
    }
}

/// An exited child's usage is the one its exit recorded; a stopped or
/// continued one's is its process's so far: the CPU time its departed tasks
/// banked, what its live ones have run, and its resident peak.
#[inline(never)]
fn child_rusage(child: &Task) -> Rusage {
    if let Some(info) = task_peek_exit_info(child.task_id) {
        return rusage_of(info.cpu_ticks, info.peak_resident_pages);
    }
    let Some(process) = child.process() else {
        return Rusage::default();
    };
    let now = slopos_ostd::kdiag_timestamp();
    let handle = child.process_handle_raw();
    let mut ticks = process.exited_cpu_ticks();
    slopos_sched::task::task_for_each_active(|member| {
        if member.process_handle_raw() == handle
            && !member.exit_cleanup_claimed(slopos_ostd::task::ops::TASK_EXIT_CPU_BANKED)
        {
            ticks =
                ticks.saturating_add(crate::syscall::core_handlers::task_cpu_ticks(member, now));
        }
    });
    let peak = slopos_ostd::process::ProcessId::of(&process)
        .map_or(0, slopos_mm::process_vm::process_vm_peak_resident_pages);
    rusage_of(ticks, peak)
}

/// The status word and the usage land first, then the child pays. An `EFAULT`
/// here leaves the zombie unreaped and the stop or continue report still
/// pending, so the caller's retry sees exactly the same event. The only place
/// either pointer is touched, which is where Linux checks them too.
fn finish_wait(
    report: WaitReport,
    status: Option<UserPtr<i32>>,
    rusage: Option<UserPtr<Rusage>>,
) -> Result<u64, Errno> {
    if let Some(out) = status {
        copy_to_user(out.inner(), &(report.status as i32)).map_err(|_| Errno::EFAULT)?;
    }
    if let Some(out) = rusage {
        let usage = match task_find_by_id(report.child_id) {
            Some(child) => child_rusage(&child),
            None => Rusage::default(),
        };
        copy_to_user(out.inner(), &usage).map_err(|_| Errno::EFAULT)?;
    }
    match report.commit {
        WaitCommit::Reap => {
            let _ = task_consume_zombie(report.child_id);
        }
        WaitCommit::ConsumeStop => {
            if let Some(child) = task_find_by_id(report.child_id) {
                let _ = child.take_stop_report();
            }
        }
        WaitCommit::ConsumeContinue => {
            if let Some(child) = task_find_by_id(report.child_id) {
                let _ = child.take_continue_report();
            }
        }
    }
    Ok(report.child_id as u64)
}

define_syscall!(syscall_wait4
    (ctx, pid: i32, status: Option<UserPtr<i32>>, options: u32, rusage: Option<UserPtr<Rusage>>)
    cap(NoneRelation)
    -> Result<u64, Errno>
{
    if options & !WAIT_OPTIONS_MASK != 0 {
        return Err(Errno::EINVAL);
    }
    // `0` and `< -1` name a process group, and SlopOS implements no group
    // wait; folding them into wait-any would answer a different question.
    if pid == 0 || pid < -1 {
        return Err(Errno::ESRCH);
    }
    let caller_id = ctx.task_id();
    let target = (pid > 0).then_some(pid as u32);
    let wnohang = options & WNOHANG != 0;
    let wuntraced = options & WUNTRACED != 0;
    let wcontinued = options & WCONTINUED != 0;

    if let Some(report) = scan_children(caller_id, target, wuntraced, wcontinued) {
        return finish_wait(report, status, rusage);
    }

    // Reaping is the parent's alone: `task_consume_zombie` drops the parent's
    // owning reference, so a stranger's wait would leave the real parent with
    // `ECHILD`. Re-looked-up here because the id could name a different task
    // by now.
    match target {
        Some(id) => match task_find_by_id(id) {
            Some(t) if t.parent_task_id() == caller_id && is_waitable_child(&t) => {}
            _ => return Err(Errno::ECHILD),
        },
        None => {
            if !slopos_sched::task::task_has_children(caller_id) {
                return Err(Errno::ECHILD);
            }
        }
    }

    if wnohang {
        return Ok(0);
    }

    let event = match target {
        Some(id) => slopos_ostd::task::ops::child_exit_event(id),
        None => slopos_ostd::task::ops::any_child_exit_event(caller_id),
    };
    let mut latched: Option<WaitReport> = None;
    let waited = slopos_ostd::sync::BUS
        .subscribe(event)
        .wait_event_interruptible(|| {
            latched = scan_children(caller_id, target, wuntraced, wcontinued);
            latched.is_some()
        });
    // Latch first: a predicate pass that found an event and then lost the race
    // with a signal must still deliver it rather than answer EINTR.
    match latched {
        Some(report) => finish_wait(report, status, rusage),
        None if waited.is_err() => Err(Errno::EINTR),
        None => Err(Errno::ECHILD),
    }
});

define_syscall!(syscall_execve
    (ctx, path_ptr: u64, argv_ptr: u64, envp_ptr: u64)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> SyscallResult
{
    let path = match UserPath::from_user_addr(path_ptr) {
        Ok(path) => path,
        Err(err) => return SyscallResult::Err(err),
    };
    // Resolved against the caller's cwd here: `do_exec` cannot see the cwd,
    // and the grant lookup below keys on the canonical name.
    let program = match ctx.with_cwd(|cwd| exec::resolve_program(path.as_bytes(), cwd)) {
        Ok(program) => program,
        Err(e) => {
            return SyscallResult::Err(Errno::from_raw(e as i32).unwrap_or(Errno::EINVAL));
        }
    };

    let argv_storage = if argv_ptr != 0 {
        match read_user_ptr_array_terminated(argv_ptr, exec::EXEC_MAX_ARG_STRINGS) {
            Ok(argv_ptrs) => match read_user_cstr_list(argv_ptrs.as_slice()) {
                Ok(values) => Some(values),
                Err(err) => return SyscallResult::Err(err),
            },
            Err(_) => return SyscallResult::Err(Errno::EINVAL),
        }
    } else {
        None
    };

    let envp_storage = if envp_ptr != 0 {
        match read_user_ptr_array_terminated(envp_ptr, exec::EXEC_MAX_ARG_STRINGS) {
            Ok(envp_ptrs) => match read_user_cstr_list(envp_ptrs.as_slice()) {
                Ok(values) => Some(values),
                Err(err) => return SyscallResult::Err(err),
            },
            Err(_) => return SyscallResult::Err(Errno::EINVAL),
        }
    } else {
        None
    };

    let argv_refs = match argv_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return SyscallResult::Err(Errno::ENOMEM),
        None => None,
    };
    let envp_refs = match envp_storage
        .as_ref()
        .map(|values| KVec::<&[u8]>::from_iter_fallible(values.iter().map(|v| v.as_slice())))
    {
        Some(Ok(refs)) => Some(refs),
        Some(Err(_)) => return SyscallResult::Err(Errno::ENOMEM),
        None => None,
    };

    if !exec::exec_arg_bytes_fit(argv_refs.as_deref(), envp_refs.as_deref()) {
        return SyscallResult::Err(Errno::E2BIG);
    }

    let mut entry_point = 0u64;
    let mut stack_ptr = 0u64;
    let mut tls_tp = 0u64;

    let irq_was_enabled = cpu::are_interrupts_enabled();
    if !irq_was_enabled {
        cpu::enable_interrupts();
    }

    let exec_result = exec::do_exec(
        process_id,
        &program,
        argv_refs.as_deref(),
        envp_refs.as_deref(),
        &mut entry_point,
        &mut stack_ptr,
        &mut tls_tp,
    );

    if !irq_was_enabled {
        cpu::disable_interrupts();
    }

    match exec_result {
        Ok(()) => {
            if tls_tp != 0 {
                let user_tp = match MmUserPtr::<u64>::try_new(tls_tp) {
                    Ok(ptr) => ptr,
                    Err(_) => return SyscallResult::Err(Errno::EFAULT),
                };
                if copy_to_user(user_tp, &tls_tp).is_err() {
                    return SyscallResult::Err(Errno::EFAULT);
                }
            }

            // Point of no return: the old image is gone.
            let task_id = ctx.task_id();

            // Here rather than in `do_exec`: past every fallible step, before
            // the new image's first instruction.
            {
                let (granted_flags, _) = exec::grants::grant_for(program.as_bytes());
                let granted = slopos_ostd::authority::caps_from_task_flags(
                    granted_flags | slopos_abi::task::TASK_FLAG_USER_MODE,
                );
                let before = slopos_ostd::task::ops::task_caps(ctx.task());
                let after =
                    slopos_ostd::task::ops::task_restrict_caps(ctx.task(), granted);
                if after != before {
                    slopos_ostd::klog_info!(
                        "exec: task {} authority {:#x} -> {:#x}",
                        task_id,
                        before,
                        after,
                    );
                }
            }

            slopos_sched::task::task_cleanup_for_exec(task_id);

            // SIG_DFL so no stale handler pointer survives into the new image;
            // SIG_IGN and blocked/pending state stay (POSIX exec semantics).
            if let Some(task) = Some(ctx.task()) {
                task_reset_caught_handlers(task);
            }

            // Unconditionally, `tls_tp == 0` included: slibc's startup adopts
            // a non-zero FS base as an installed TCB, and the old image's
            // thread pointer names memory the new image does not have.
            {
                let t = ctx.task();
                t.set_fs_base(tls_tp);
            }
            slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, tls_tp);
            let uc = ctx.user_ctx();
            let mut regs = uc.regs();
            regs.rip = entry_point;
            regs.rsp = stack_ptr;
            regs.rax = 0;
            regs.rdi = 0;
            regs.rsi = 0;
            regs.rdx = 0;
            regs.rcx = 0;
            regs.r8 = 0;
            regs.r9 = 0;
            regs.r10 = 0;
            regs.r11 = 0;
            uc.set_regs(regs);

            // The new image must never see the previous program's vector
            // registers. Reset and load the default under IRQ-off, so a context
            // switch cannot re-save the old live registers over the reset.
            let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
            slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
                if let Some(current) = slopos_sched::task_struct::Current::get() {
                    current.task().fpu_reset(&current);
                    let restored = current.task().fpu_restore_to_cpu(&current, xcr0);
                    debug_assert!(restored, "XRSTOR64 rejected the FPU init image");
                }
            });
            SyscallResult::NoReturn
        }
        Err(e) => SyscallResult::Err(Errno::from_raw(e as i32).unwrap_or(Errno::EINVAL)),
    }
});

/// The kernel's own affinity mask is a `u32`, so that is the widest cpu set a
/// `sched_*affinity` call can carry. A caller naming a wider `cpusetsize` —
/// glibc's `cpu_set_t` is 128 bytes — has the excess ignored on `set`, and on
/// `get` receives this many bytes and the count as the return value; clearing
/// the remainder is the caller's, as it is on Linux past `cpumask_size()`.
const AFFINITY_MASK_BYTES: usize = core::mem::size_of::<u32>();

/// One bit per online CPU. `Task::cpu_affinity` stores 0 for "any CPU", and
/// this is the set that means.
fn online_cpu_mask() -> u32 {
    let count = slopos_arch::pcr::get_cpu_count();
    if count >= u32::BITS as usize {
        u32::MAX
    } else {
        (1u32 << count) - 1
    }
}

/// Resolve a `sched_*affinity` `pid` (0 is the caller) inside the caller's own
/// address space.
///
/// Unchecked, any task could pin a `NO_PREEMPT` spinner per CPU and wedge
/// every core, so pinning is confined to a shared address space. Compared as
/// tables, not numbers — a recycled id would let a *later* process pass.
fn affinity_target(
    pid: u32,
    task_id: u32,
    process_id: FdTable,
) -> Result<slopos_sched::task::TaskRef, Errno> {
    let resolved = if pid == 0 { task_id } else { pid };
    let task_ref = task_find_by_id(resolved).ok_or(Errno::ESRCH)?;
    if task_ref.process().as_deref().and_then(FdTable::of) != Some(process_id) {
        return Err(Errno::EPERM);
    }
    Ok(task_ref)
}

define_syscall!(syscall_getcpu
    (ctx, cpu: Option<UserPtr<u32>>, node: Option<UserPtr<u32>>, _unused: u64)
    cap(NoneSelf)
    -> Result<(), Errno>
{
    if let Some(out) = cpu {
        let id = slopos_arch::pcr::get_current_cpu() as u32;
        copy_to_user(out.inner(), &id).map_err(|_| Errno::EFAULT)?;
    }
    // No NUMA topology is discovered, so every CPU sits on node 0.
    if let Some(out) = node {
        copy_to_user(out.inner(), &0u32).map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
});

define_syscall!(syscall_sched_setaffinity
    (ctx, pid: u32, cpusetsize: u64, mask: u64)
    cap(NoneRelation)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<(), Errno>
{
    // A short mask zero-extends and a long one truncates, as Linux's
    // `get_user_cpu_mask` does.
    let mut bytes = [0u8; AFFINITY_MASK_BYTES];
    let want = cpusetsize.min(AFFINITY_MASK_BYTES as u64);
    if want != 0 {
        syscall_bounded_from_user(&mut bytes, mask, want, AFFINITY_MASK_BYTES)
            .map_err(|_| Errno::EFAULT)?;
    }
    let task_ref = affinity_target(pid, task_id, process_id)?;
    // An empty intersection with the online set names no CPU the task could
    // ever run on. It cannot be stored either: 0 is this kernel's "any CPU".
    let requested = u32::from_le_bytes(bytes) & online_cpu_mask();
    if requested == 0 {
        return Err(Errno::EINVAL);
    }
    task_ref.set_cpu_affinity(requested);
    // Stamping the mask is not enough — re-place the task so the new mask
    // actually governs where it runs.
    task_apply_affinity(&task_ref, requested);
    Ok(())
});

define_syscall!(syscall_sched_getaffinity
    (ctx, pid: u32, cpusetsize: u64, mask: u64)
    cap(NoneRelation)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    // A buffer that cannot name every online CPU is refused rather than
    // answered with a truncated set the caller would read as complete.
    if cpusetsize.saturating_mul(8) < slopos_arch::pcr::get_cpu_count() as u64 {
        return Err(Errno::EINVAL);
    }
    let task_ref = affinity_target(pid, task_id, process_id)?;
    // A stored 0 is "any CPU", reported as the online set — what it means, and
    // what Linux answers for a task that was never pinned.
    let stored = task_ref.cpu_affinity();
    let online = online_cpu_mask();
    let effective = if stored == 0 { online } else { stored & online };
    let write_len = (cpusetsize as usize).min(AFFINITY_MASK_BYTES);
    syscall_copy_to_user_bounded(mask, &effective.to_le_bytes()[..write_len])
        .map_err(|_| Errno::EFAULT)?;
    Ok(write_len as u64)
});

// POSIX `getpid` is the *thread group* id: every thread of a process must see
// one pid.
define_syscall!(syscall_getpid (ctx)
    cap(NoneSelf)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let tgid = ctx.task().tgid;
    Ok(if tgid == INVALID_TASK_ID { task_id } else { tgid })
});

define_syscall!(syscall_gettid (ctx)
    cap(NoneSelf)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    Ok(task_id)
});

// Per *process*, like `getpid`. The per-task parent link names the thread's
// creator, which for a `CLONE_THREAD` sibling is a sibling thread.
define_syscall!(syscall_getppid (ctx) cap(NoneSelf)
    -> Result<u32, Errno> {
    let task = ctx.task();
    let tgid = task.tgid;
    if tgid == INVALID_TASK_ID || tgid == task.task_id {
        return Ok(task.parent_task_id());
    }
    // A reaped leader leaves no group to ask; the thread's own link is then
    // the only answer left.
    Ok(task_find_by_id(tgid).map_or_else(
        || task.parent_task_id(),
        |leader| leader.parent_task_id(),
    ))
});

define_syscall!(syscall_getpgid
    (ctx, target: u32)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    Ok(task_ref.pgid())
});

define_syscall!(syscall_getsid
    (ctx, target: u32)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let resolved = if target == 0 { task_id } else { target };
    let Some(task_ref) = task_find_by_id(resolved) else {
        return Err(Errno::ESRCH);
    };
    Ok(task_ref.sid())
});

define_syscall!(syscall_setpgid
    (ctx, pid: u32, pgid_arg: u32)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let resolved_pid = if pid == 0 { task_id } else { pid };
    let resolved_pgid = if pgid_arg == 0 { resolved_pid } else { pgid_arg };

    let Some(target_ref) = task_find_by_id(resolved_pid) else {
        return Err(Errno::EINVAL);
    };
    if resolved_pgid == 0 {
        return Err(Errno::EINVAL);
    }

    let caller_sid = ctx.task().sid();
    let target = &*target_ref;
    if resolved_pid != task_id && target.parent_task_id() != task_id {
        return Err(Errno::EINVAL);
    }
    if target.sid() != caller_sid {
        return Err(Errno::EINVAL);
    }

    // The group object must mirror the integer pgid the target is about to hold.
    let new_group = if resolved_pgid == resolved_pid {
        match target.process_group.load() {
            Some(existing) if existing.id() == resolved_pgid => Some(existing),
            _ => {
                let session = target
                    .process_group
                    .load()
                    .map(|pg| pg.session().clone())
                    .ok_or(Errno::EPERM)?;
                Some(new_group_in_session(resolved_pgid, session).ok_or(Errno::ENOMEM)?)
            }
        }
    } else {
        let Some(leader_ref) = task_find_by_id(resolved_pgid) else {
            return Err(Errno::EINVAL);
        };
        if leader_ref.sid() != caller_sid {
            return Err(Errno::EINVAL);
        }
        Some(leader_ref.process_group.load().ok_or(Errno::EINVAL)?)
    };

    // `target` is generally *not* the calling task, so another CPU may be
    // reading these fields. Integer first, membership second: the slot's
    // Release store orders the pair and defers the displaced handle's release
    // past any concurrent reader's clone.
    target.set_pgid(resolved_pgid);
    target.process_group.store(new_group);
    Ok(())
});

define_syscall!(syscall_setsid (ctx)
    cap(NoneSelf)
    requires(let task_id: task_id)
    -> Result<u32, Errno>
{
    let task = ctx.task();
    if task.pgid() == task.task_id || task.sid() == task.task_id {
        return Err(Errno::EPERM);
    }
    // Installing the fresh group drops the old membership, so the old session
    // and any terminal weak links to it die with it.
    let pg = new_session_group(task.task_id).ok_or(Errno::ENOMEM)?;
    if task.controlling_tty().is_some() {
        task.set_controlling_tty(None);
    }
    // Integers first, membership second — see `syscall_setpgid`.
    task.set_sid(task.task_id);
    task.set_pgid(task.task_id);
    task.process_group.store(Some(pg));
    Ok(task.sid())
});

define_syscall!(syscall_getuid (ctx) cap(NoneSelf)
    -> u32 { 0 });
define_syscall!(syscall_getgid (ctx) cap(NoneSelf)
    -> u32 { 0 });
define_syscall!(syscall_geteuid (ctx) cap(NoneSelf)
    -> u32 { 0 });
define_syscall!(syscall_getegid (ctx) cap(NoneSelf)
    -> u32 { 0 });

/// Resolve `path` against `cwd` and require a directory, returning the
/// canonical path the walk ended on.
///
/// Not `canonicalise_at`: that normaliser is lexical, folding `..` against
/// the spelling and erasing a symlink the `..` crossed. The stored cwd is
/// prefixed onto every later relative lookup, so a lexical answer would leave
/// the task resolving against a directory this call never checked.
#[inline(never)]
fn resolve_new_cwd(path: &[u8], cwd: &[u8]) -> Result<CanonPath, Errno> {
    slopos_fs::vfs::resolve_path_canon_at(path, cwd, RESOLVE_MUST_BE_DIR)
        .map(|(_, canon)| canon)
        .map_err(VfsError::to_errno)
}

define_syscall!(syscall_chdir
    (ctx, path: UserPath) cap(NoneSelf)
    -> Result<(), Errno>
{
    if path.is_empty() {
        return Err(Errno::EINVAL);
    }
    let canon = ctx.with_cwd(|cwd| resolve_new_cwd(path.as_bytes(), cwd))?;
    // The store, unlike the read, genuinely needs the owner's witness.
    let current = Current::get().ok_or(Errno::EINVAL)?;
    if !current.task().set_cwd(&current, canon.as_bytes()) {
        return Err(Errno::ENOMEM);
    }
    Ok(())
});

define_syscall!(syscall_getcwd
    (ctx, buf: UserBytes) cap(NoneSelf)
    -> Result<u64, Errno>
{
    let current = Current::get().ok_or(Errno::EINVAL)?;
    current.task().with_cwd(&current, |cwd| {
        if buf.len() < cwd.len() {
            return Err(Errno::ERANGE);
        }
        syscall_copy_to_user_bounded(buf.base_u64(), cwd).map_err(|_| Errno::EFAULT)?;
        Ok(cwd.len() as u64)
    })
});

define_syscall!(syscall_arch_prctl
    (ctx, cmd: u64, addr: u64) cap(NoneSelf)
    -> Result<(), Errno>
{
    match cmd {
        ARCH_SET_FS => {
            if addr >= slopos_mm::memory_layout_defs::USER_SPACE_END_VA && addr != 0 {
                return Err(Errno::EINVAL);
            }
            let t = ctx.task();
            t.fs_base.store(addr, Ordering::Release);
            slopos_arch::cpu::msr::write_msr(slopos_arch::cpu::msr::Msr::FS_BASE, addr);
            Ok(())
        }
        ARCH_GET_FS => {
            if addr == 0 {
                return Err(Errno::EINVAL);
            }
            let t = ctx.task();
            let fs_base_val = t.fs_base.load(Ordering::Acquire);
            let user_ptr = MmUserPtr::<u64>::try_new(addr).map_err(|_| Errno::EFAULT)?;
            copy_to_user(user_ptr, &fs_base_val).map_err(|_| Errno::EFAULT)?;
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
});

define_syscall!(syscall_fork (ctx) cap(NoneSelf)
    -> Result<u64, Errno> {
    let task = ctx.task();
    let child_id = task_fork(task, Some(ctx.user_ctx()));
    if child_id == slopos_abi::task::INVALID_TASK_ID {
        Err(Errno::EAGAIN)
    } else {
        Ok(child_id as u64)
    }
});

define_syscall!(syscall_clone
    (ctx, flags: u64, child_stack: u64, parent_tidptr: u64, child_tidptr: u64, tls: u64)
    cap(NoneSelf)
    -> Result<u64, Errno>
{
    let parent = ctx.task();
    match slopos_sched::task::task_clone(
        parent,
        Some(ctx.user_ctx()),
        flags,
        child_stack,
        parent_tidptr,
        child_tidptr,
        tls,
    ) {
        Ok(child_id) => Ok(child_id as u64),
        Err(errno) => Err(Errno::from_raw(errno as i32).unwrap_or(Errno::EINVAL)),
    }
});

fn read_timeout(addr: u64) -> Result<Timespec, Errno> {
    let ptr = MmUserPtr::<Timespec>::try_new(addr).map_err(|_| Errno::EFAULT)?;
    let ts = copy_from_user(ptr).map_err(|_| Errno::EFAULT)?;
    if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    Ok(ts)
}

fn timespec_to_ms(ts: &Timespec) -> u64 {
    (ts.tv_sec as u64)
        .saturating_mul(1_000)
        .saturating_add((ts.tv_nsec as u64).div_ceil(1_000_000))
}

fn timespec_to_ns(ts: &Timespec) -> u64 {
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// `FUTEX_WAIT`'s timeout is relative, `FUTEX_WAIT_BITSET`'s is absolute
/// against `CLOCK_MONOTONIC` (or `CLOCK_REALTIME` under
/// `FUTEX_CLOCK_REALTIME`). Both reduce to relative milliseconds; a null
/// pointer blocks indefinitely.
fn futex_timeout_ms(addr: u64, absolute: bool, realtime: bool) -> Result<Option<u64>, Errno> {
    if addr == 0 {
        return Ok(None);
    }
    let ts = read_timeout(addr)?;
    if !absolute {
        return Ok(Some(timespec_to_ms(&ts)));
    }
    let now_ns = if realtime {
        slopos_kernel_services::clock::realtime_ns()
            .unwrap_or_else(slopos_kernel_services::clock::monotonic_ns)
    } else {
        slopos_kernel_services::clock::monotonic_ns()
    };
    let remaining_ns = timespec_to_ns(&ts).saturating_sub(now_ns);
    Ok(Some(remaining_ns.div_ceil(1_000_000)))
}

define_syscall!(syscall_futex
    (ctx, uaddr: u64, op: u64, val: u32, timeout: u64, uaddr2: u64, val3: u32) cap(NoneSelf)
    -> Result<u64, Errno>
{
    if (uaddr & 0x3) != 0 {
        return Err(Errno::EINVAL);
    }

    let user_word = MmUserPtr::<u32>::try_new(uaddr).map_err(|_| Errno::EFAULT)?;
    if copy_from_user(user_word).is_err() {
        return Err(Errno::EFAULT);
    }

    let cmd = op & FUTEX_CMD_MASK;
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    // Linux accepts the clock flag only where the timeout is absolute.
    if realtime && cmd != FUTEX_WAIT_BITSET {
        return Err(Errno::ENOSYS);
    }

    let rc = match cmd {
        FUTEX_WAIT => {
            let relative = futex_timeout_ms(timeout, false, false)?;
            slopos_sched::futex::futex_wait(uaddr, val, relative)
        }
        FUTEX_WAIT_BITSET => {
            if val3 == 0 {
                return Err(Errno::EINVAL);
            }
            let relative = futex_timeout_ms(timeout, true, realtime)?;
            slopos_sched::futex::futex_wait_bitset(uaddr, val, relative, val3)
        }
        FUTEX_WAKE => slopos_sched::futex::futex_wake(uaddr, val),
        FUTEX_WAKE_BITSET => {
            if val3 == 0 {
                return Err(Errno::EINVAL);
            }
            slopos_sched::futex::futex_wake_bitset(uaddr, val, val3)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            if (uaddr2 & 0x3) != 0 || uaddr2 == uaddr {
                return Err(Errno::EINVAL);
            }
            let second = MmUserPtr::<u32>::try_new(uaddr2).map_err(|_| Errno::EFAULT)?;
            if copy_from_user(second).is_err() {
                return Err(Errno::EFAULT);
            }
            // Arg 4 is `val2`, the requeue count, not a timeout.
            let max_requeue = timeout as u32;
            let expected = (cmd == FUTEX_CMP_REQUEUE).then_some(val3);
            slopos_sched::futex::futex_requeue(uaddr, uaddr2, val, max_requeue, expected)
        }
        _ => return Err(Errno::ENOSYS),
    };

    Ok(rc as u64)
});

define_syscall!(syscall_vhangup (ctx)
    cap(NoneRelation)
    requires(let task_id: task_id)
    -> Result<(), Errno>
{
    let ctty = match ctx.task().controlling_tty() {
        Some(idx) => idx,
        None => return Err(Errno::EPERM),
    };
    slopos_kernel_services::syscall_services::tty::hangup(ctty);
    Ok(())
});

define_syscall!(syscall_prlimit64
    (ctx, pid: u32, resource: u32, new_ptr: u64, old_ptr: u64)
    cap(NoneRelation)
    requires(let process_id: process_id)
    -> Result<(), Errno>
{
    use slopos_abi::quota::{RLIM64_INFINITY, RLimit64, rlimit_mapping};
    use slopos_ostd::process::quota::{KindStats, NO_LIMIT, set_limit, stats};

    let process = process_id.process().ok_or(Errno::ESRCH)?;
    // Self only: there is no privilege principal in this kernel (getuid returns
    // a literal 0), so a cross-process limit change has no answer but
    // "everyone may".
    if pid != 0 && pid != process.id() {
        return Err(Errno::EPERM);
    }

    let mapping = rlimit_mapping(resource).ok_or(Errno::EINVAL)?;
    let account = process.account();
    // An account row reaped mid-call reports the enforced default rather than
    // failing: `ESRCH` here would make the syscall depend on reap timing.
    let current = stats(account, mapping.kind).unwrap_or(KindStats {
        used: 0,
        limit: slopos_abi::quota::default_process_limit(mapping.kind),
        peak: 0,
        denials: 0,
    });
    let publish = |limit: u32| -> u64 {
        if limit == NO_LIMIT {
            RLIM64_INFINITY
        } else {
            (limit as u64).saturating_mul(mapping.scale)
        }
    };

    // Read before write, so a call that both queries and sets reports what was
    // in force when it was made rather than what it just installed.
    if old_ptr != 0 {
        let out = MmUserPtr::<RLimit64>::try_new(old_ptr).map_err(|_| Errno::EFAULT)?;
        // Soft and hard are the same number: there is no privileged path to
        // raise one above the other, so reporting them apart would imply
        // headroom that cannot be claimed.
        let value = publish(current.limit);
        copy_to_user(out, &RLimit64 { rlim_cur: value, rlim_max: value })
            .map_err(|_| Errno::EFAULT)?;
    }

    if new_ptr != 0 {
        let src = MmUserPtr::<RLimit64>::try_new(new_ptr).map_err(|_| Errno::EFAULT)?;
        let want = copy_from_user(src).map_err(|_| Errno::EFAULT)?;
        if want.rlim_cur > want.rlim_max {
            return Err(Errno::EINVAL);
        }
        // Lowering only: raising the ceiling is the privileged operation, and
        // granting it unconditionally would make every limit advisory.
        if want.rlim_max > publish(current.limit) {
            return Err(Errno::EPERM);
        }
        // Saturating, never `NO_LIMIT`: mapping an over-wide `rlim_cur` to the
        // no-limit sentinel would turn the widest possible *set* into a way to
        // switch enforcement off.
        let scaled = want.rlim_cur / mapping.scale.max(1);
        let requested = u32::try_from(scaled).unwrap_or(u32::MAX).min(current.limit);
        set_limit(account, mapping.kind, requested);
    }

    Ok(())
});
