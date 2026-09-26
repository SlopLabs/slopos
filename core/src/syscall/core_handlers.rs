use core::ffi::c_char;
use core::ops::ControlFlow;
use core::sync::atomic::Ordering as AtomicOrdering;

use slopos_abi::Errno;
use slopos_abi::syscall::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
    LINUX_REBOOT_CMD_CAD_OFF, LINUX_REBOOT_CMD_CAD_ON, LINUX_REBOOT_CMD_HALT,
    LINUX_REBOOT_CMD_POWER_OFF, LINUX_REBOOT_CMD_RESTART, LINUX_REBOOT_MAGIC1, Timespec,
    UserSysInfo, UserUtsname, linux_reboot_magic2,
};
use slopos_abi::task::{INVALID_TASK_ID, TaskExitReason, TaskFaultReason};
use slopos_abi::tty_error::TtyError;
use slopos_ostd::klog_debug;

use crate::syscall::args::{UserBytes, UserPtr};
use crate::syscall::common::{
    USER_IO_MAX_BYTES, syscall_bounded_from_user, syscall_copy_to_user_bounded,
};
use crate::syscall::result::SyscallResult;
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::tty;
use slopos_ostd::platform::power;
use slopos_sched::scheduler::{
    get_scheduler_stats, schedule, scheduler_is_preemption_enabled, sleep_current_task_ms, yield_,
};
use slopos_sched::task::{get_task_stats, task_group_exit, task_terminate};

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::user_copy::{copy_from_user, copy_to_user};

define_syscall!(syscall_sched_yield (ctx) cap(NoneSelf)
    -> SyscallResult {
    // rax is written before yielding and the handler returns `NoReturn`:
    // `yield_()` suspends, and the dispatcher's own `write_ok` on resume would
    // double-account the WL balance.
    ctx.write_ok(0);
    yield_();
    SyscallResult::NoReturn
});

/// Consumed CPU time in TSC ticks, the slice in progress included.
pub(crate) fn task_cpu_ticks(task: &slopos_sched::task_struct::Task, now: u64) -> u64 {
    let mut ticks = task.total_runtime();
    let last = task.last_run_timestamp();
    if last != 0 && now > last {
        ticks = ticks.saturating_add(now - last);
    }
    ticks
}

/// `#[inline(never)]`: the group walk's closure must not land in the
/// handler's frame.
#[inline(never)]
fn cpu_clock_ns(clock_id: u64, task: &slopos_sched::task_struct::Task) -> u64 {
    let now = slopos_ostd::kdiag_timestamp();
    let ticks = if clock_id == CLOCK_THREAD_CPUTIME_ID {
        task_cpu_ticks(task, now)
    } else {
        // Summed in ticks and converted once: per-task conversion would round
        // every thread's microsecond down separately.
        let tgid = task.tgid;
        let mut total = 0u64;
        slopos_sched::task::task_for_each_active(|member| {
            if member.tgid == tgid {
                total = total.saturating_add(task_cpu_ticks(member, now));
            }
        });
        total
    };
    slopos_kernel_services::clock::ticks_to_microseconds(ticks).saturating_mul(1_000)
}

define_syscall!(syscall_clock_gettime
    (ctx, clock_id: u64, ts: UserPtr<Timespec>) cap(NoneSelf)
    -> Result<(), Errno>
{
    // `CLOCK_REALTIME` answers the wall clock the bootloader anchored, and
    // falls back to monotonic only when the boot established none — a machine
    // whose firmware reported no date has no better answer, and `EINVAL` for a
    // clock POSIX requires every system to have is worse than an uptime.
    let ns = match clock_id {
        CLOCK_MONOTONIC => slopos_kernel_services::clock::monotonic_ns(),
        CLOCK_REALTIME => slopos_kernel_services::clock::realtime_ns()
            .unwrap_or_else(slopos_kernel_services::clock::monotonic_ns),
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => cpu_clock_ns(clock_id, ctx.task()),
        _ => return Err(Errno::EINVAL),
    };
    let value = Timespec {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    };
    copy_to_user(ts.inner(), &value).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_clock_settime
    (ctx, clock_id: u64, ts: UserPtr<Timespec>) cap(Clock)
    -> Result<(), Errno>
{
    // Monotonic is defined not to move, and a CPU-time clock is an accounting
    // total rather than a coordinate.
    if clock_id != CLOCK_REALTIME {
        return Err(Errno::EINVAL);
    }
    let value = copy_from_user(ts.inner()).map_err(|_| Errno::EFAULT)?;
    if !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    slopos_kernel_services::clock::set_realtime(value.tv_sec, value.tv_nsec as u32)
        .map_err(|_| Errno::EINVAL)
});

#[cfg(debug_assertions)]
const KERNEL_VERSION: &str = concat!("SlopOS ", env!("CARGO_PKG_VERSION"), " debug");
#[cfg(not(debug_assertions))]
const KERNEL_VERSION: &str = concat!("SlopOS ", env!("CARGO_PKG_VERSION"), " release");

fn set_uts_field(field: &mut [u8; 65], value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(field.len() - 1);
    field[..len].copy_from_slice(&bytes[..len]);
    field[len] = 0;
}

define_syscall!(syscall_uname
    (ctx, out: UserPtr<UserUtsname>) cap(NoneSelf)
    -> Result<(), Errno>
{
    let mut uts = UserUtsname::new();
    set_uts_field(&mut uts.sysname, "SlopOS");
    set_uts_field(&mut uts.nodename, "slopos");
    set_uts_field(&mut uts.release, env!("CARGO_PKG_VERSION"));
    set_uts_field(&mut uts.version, KERNEL_VERSION);
    set_uts_field(&mut uts.machine, "x86_64");
    set_uts_field(&mut uts.domainname, "(none)");
    copy_to_user(out.inner(), &uts).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

/// A variable name the caller's buffer holds, bounded before it is copied.
fn efivar_name(name: &UserBytes) -> Result<slopos_ostd::KVec<u8>, Errno> {
    if name.is_empty() || name.len() > crate::efivar::EFIVAR_NAME_MAX * 3 {
        return Err(Errno::EINVAL);
    }
    let mut out = slopos_ostd::KVec::zeroed(name.len()).map_err(|_| Errno::ENOMEM)?;
    slopos_mm::user_copy::copy_bytes_from_user(*name.inner(), &mut out)
        .map_err(|_| Errno::EFAULT)?;
    Ok(out)
}

define_syscall!(syscall_efivar_get
    (ctx, name: UserBytes, guid: UserPtr<[u8; 16]>, buf: UserBytes) cap(Power)
    -> Result<u64, Errno> {
    let _ = ctx;
    let name = efivar_name(&name)?;
    let guid = copy_from_user(guid.inner()).map_err(|_| Errno::EFAULT)?;
    let value = crate::efivar::efivar_get(&name, guid, buf.len())?;
    slopos_mm::user_copy::copy_bytes_to_user(*buf.inner(), &value).map_err(|_| Errno::EFAULT)?;
    Ok(value.len() as u64)
});

define_syscall!(syscall_efivar_set
    (ctx, name: UserBytes, guid: UserPtr<[u8; 16]>, attributes: u32, data: UserBytes) cap(Power)
    -> Result<u64, Errno> {
    let _ = ctx;
    let name = efivar_name(&name)?;
    let guid = copy_from_user(guid.inner()).map_err(|_| Errno::EFAULT)?;
    if data.len() > crate::efivar::EFIVAR_DATA_MAX {
        return Err(Errno::E2BIG);
    }
    let mut value = slopos_ostd::KVec::zeroed(data.len()).map_err(|_| Errno::ENOMEM)?;
    slopos_mm::user_copy::copy_bytes_from_user(*data.inner(), &mut value)
        .map_err(|_| Errno::EFAULT)?;
    crate::efivar::efivar_set(&name, guid, attributes, &value)?;
    Ok(0)
});

// Linux's `reboot(2)`: the magics are what separate a deliberate call from a
// stray one, and `cmd` picks the action. `POWER_OFF` and `HALT` both stop the
// machine — the platform layer draws no further distinction.
define_syscall!(syscall_reboot
    (ctx, magic1: u64, magic2: u64, cmd: u64, _arg: u64) cap(Power)
    -> SyscallResult {
    if magic1 != LINUX_REBOOT_MAGIC1 || !linux_reboot_magic2(magic2) {
        return SyscallResult::Err(Errno::EINVAL);
    }
    // The dispatcher already refused a caller lacking `Power`. The witness is
    // what the primitive itself demands, so a path reaching it without one
    // does not compile -- which is the whole point of moving the primitive
    // into ostd.
    let Ok(cap) = ctx.require_cap::<slopos_ostd::authority::Power>() else {
        return SyscallResult::Err(Errno::EPERM);
    };
    match cmd {
        LINUX_REBOOT_CMD_RESTART => {
            power::reboot(&cap, b"user reboot\0".as_ptr() as *const c_char)
        }
        LINUX_REBOOT_CMD_POWER_OFF | LINUX_REBOOT_CMD_HALT => {
            power::shutdown(&cap, b"user halt\0".as_ptr() as *const c_char)
        }
        // No Ctrl-Alt-Del handler to toggle, so the toggle succeeds and
        // changes nothing; an init program calls it early and expects 0.
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => {
            return SyscallResult::Ok(0);
        }
        _ => return SyscallResult::Err(Errno::EINVAL),
    }
    #[allow(unreachable_code)]
    SyscallResult::NoReturn
});

// The sleep is driven by the scheduler's millisecond timer, so the request is
// rounded *up* to the next millisecond — POSIX asks for at least the interval
// named, never less — and the remainder reported on `EINTR` carries the same
// granularity. No upper bound: a deadline is re-derived each pass, so a long
// sleep is many bounded waits rather than one truncated one.
define_syscall!(syscall_nanosleep
    (ctx, req: Option<UserPtr<Timespec>>, rem: Option<UserPtr<Timespec>>) cap(NoneSelf)
    -> Result<(), Errno> {
    let req = req.ok_or(Errno::EFAULT)?;
    let want = copy_from_user(req.inner()).map_err(|_| Errno::EFAULT)?;
    if want.tv_sec < 0 || !(0..1_000_000_000).contains(&want.tv_nsec) {
        return Err(Errno::EINVAL);
    }
    let ms = (want.tv_sec as u64)
        .saturating_mul(1_000)
        .saturating_add((want.tv_nsec as u64).div_ceil(1_000_000));
    if ms == 0 {
        return Ok(());
    }
    if scheduler_is_preemption_enabled() == 0 {
        // Nothing to park on and no signal that could arrive, so the interval
        // is burned in polls bounded by what the timer call accepts.
        let mut left = ms;
        while left != 0 {
            let chunk = left.min(u32::MAX as u64);
            slopos_kernel_services::platform::timer_poll_delay_ms(chunk as u32);
            left -= chunk;
        }
        return Ok(());
    }

    // An absolute deadline, re-derived each pass: an early wake (a signal's
    // unblock, a kill's) must not end the sleep reporting success. EINTR rather
    // than ERESTARTSYS, because a restart re-arms the whole original duration.
    let deadline_ms = slopos_kernel_services::platform::get_time_ms().saturating_add(ms);
    loop {
        let now_ms = slopos_kernel_services::platform::get_time_ms();
        if now_ms >= deadline_ms {
            return Ok(());
        }
        let task = ctx.task();
        if task.is_killed() || slopos_sched::task::task_has_deliverable_signal(task) {
            if let Some(out) = rem {
                let left_ms = deadline_ms - now_ms;
                let value = Timespec {
                    tv_sec: (left_ms / 1_000) as i64,
                    tv_nsec: ((left_ms % 1_000) * 1_000_000) as i64,
                };
                copy_to_user(out.inner(), &value).map_err(|_| Errno::EFAULT)?;
            }
            return Err(Errno::EINTR);
        }
        let remaining = (deadline_ms - now_ms).min(u32::MAX as u64) as u32;
        if sleep_current_task_ms(remaining) != 0 {
            return Err(Errno::EINVAL);
        }
    }
});

define_syscall!(syscall_exit (ctx, code: u32) cap(NoneSelf)
    -> SyscallResult {
    let task_id = ctx.task_id();
    klog_debug!("SYSCALL_EXIT: task {} entering exit", task_id);
    {
        let t = ctx.task();
        // Atomic: written by any CPU terminating any task, never only by the owner.
        t.exit_reason
            .store(TaskExitReason::Normal.as_u16(), AtomicOrdering::Release);
        t.fault_reason
            .store(TaskFaultReason::None.as_u16(), AtomicOrdering::Release);
        t.exit_code.store(code, AtomicOrdering::Release);
    }
    klog_debug!("SYSCALL_EXIT: task {} calling task_terminate", task_id);
    task_terminate(task_id);
    schedule();
    klog_debug!(
        "SYSCALL_EXIT: task {} schedule returned (should not happen)",
        task_id
    );
    SyscallResult::NoReturn
});

/// End every thread of `tgid`'s group, then the caller. Returns how many the
/// group fan-out matched.
///
/// The caller's own exit must not depend on the fan-out: `NoReturn` is a
/// promise, and a group that resolved to nothing — a reaped leader, a tgid
/// nobody claims — would otherwise let `exit_group` return to userland.
/// `task_terminate` is idempotent, so the ordinary path pays one lookup.
pub(crate) fn exit_group_terminate(task_id: u32, tgid: u32, code: u32) -> usize {
    let group = if tgid == INVALID_TASK_ID {
        task_id
    } else {
        tgid
    };
    let terminated = task_group_exit(group, code);
    task_terminate(task_id);
    terminated
}

// The caller's own exit goes last, so the marking pass runs on a live stack.
define_syscall!(syscall_exit_group (ctx, code: u32) cap(NoneSelf)
    -> SyscallResult {
    let task_id = ctx.task_id();
    {
        let t = ctx.task();
        t.exit_reason
            .store(TaskExitReason::Normal.as_u16(), AtomicOrdering::Release);
        t.fault_reason
            .store(TaskFaultReason::None.as_u16(), AtomicOrdering::Release);
        t.exit_code.store(code, AtomicOrdering::Release);
    }
    let terminated = exit_group_terminate(task_id, ctx.task().tgid, code);
    klog_debug!(
        "SYSCALL_EXIT_GROUP: task {} terminated {} group member(s)",
        task_id,
        terminated
    );
    schedule();
    SyscallResult::NoReturn
});

// `klog_write`, not `write(2)`: there is no descriptor, and the destination is
// the kernel log rather than anything in the caller's fd table. It exists for
// output that must survive a broken or absent fd 1. Deliberately
// unprivileged, like Linux's `/dev/kmsg`, and it reaches the same serialised
// writer klog uses, so a caller cannot interleave into a klog line or the
// harness's KTAP framing.
define_syscall!(syscall_klog_write (ctx, buf: UserBytes) cap(ConsoleIo)
    -> Result<u64, Errno> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let write_len = syscall_bounded_from_user(
        &mut tmp,
        buf.base_u64(),
        buf.len() as u64,
        USER_IO_MAX_BYTES,
    )
    .map_err(|_| Errno::EFAULT)?;
    platform::console_write_serialized(&tmp[..write_len]);
    Ok(write_len as u64)
});

// `/dev/tty` semantics: the terminal resolves per process, so a task in a PTY
// session reads its own PTY and one with no controlling terminal reads nothing
// rather than the operator's console — `ENXIO`, as opening `/dev/tty` answers.
define_syscall!(syscall_ctty_read (ctx, buf: UserBytes) cap(NoneSelf)
    -> Result<u64, Errno> {
    if buf.len() == 0 {
        return Ok(0);
    }
    let tty_idx = ctx.task().controlling_tty().ok_or(Errno::ENXIO)?;
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let max_len = buf.len().min(USER_IO_MAX_BYTES);
    let read_len = tty::read_cooked(tty_idx, tmp.as_mut_ptr(), max_len, false);
    let n = match read_len {
        Ok(n) => n,
        Err(TtyError::Restart) => return Err(Errno::ERESTARTSYS),
        Err(_) => return Err(Errno::EINVAL),
    };
    syscall_copy_to_user_bounded(buf.base_u64(), &tmp[..n]).map_err(|_| Errno::EFAULT)?;
    Ok(n as u64)
});

define_syscall!(syscall_sys_info (ctx, info_out: UserPtr<UserSysInfo>) cap(SysInspect)
    -> Result<(), Errno> {
    let pages = get_page_allocator_stats();
    let tasks = get_task_stats();
    let sched = get_scheduler_stats();
    let commit = slopos_mm::commit::commit_stats();
    let commit_headroom_pages = if commit.limit == u32::MAX {
        u32::MAX
    } else {
        commit.limit.saturating_sub(commit.committed)
    };

    let info = UserSysInfo {
        total_pages: pages.total,
        free_pages: pages.free,
        allocated_pages: pages.allocated,
        total_tasks: tasks.total_tasks,
        active_tasks: tasks.active_tasks,
        _pad0: 0,
        task_context_switches: tasks.context_switches,
        scheduler_context_switches: sched.context_switches,
        scheduler_yields: sched.yields,
        ready_tasks: sched.ready_tasks,
        schedule_calls: sched.schedule_calls,
        wl_balance: slopos_ostd::wl_currency::check_balance(),
        boot_flags: slopos_ostd::boot_flags::get_flags(),
        commit_headroom_pages,
        commit_limit_pages: commit.limit,
        committed_pages: commit.committed,
    };

    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_process_list
    (ctx, buf: UserPtr<slopos_abi::syscall::UserTaskEntry>, max: u64) cap(SysInspect)
    -> Result<u64, Errno>
{
    use slopos_abi::syscall::UserTaskEntry;
    use slopos_abi::task::{INVALID_TASK_ID, MAX_TASKS};
    use slopos_ostd::KVec;
    use slopos_sched::task::task_try_for_each_enumerable;
    use crate::syscall::signal::{signal_dominates, signal_is_init, signal_may_name};

    // Enumeration answers to the same relation `kill` does, so an id this
    // refuses to report is also one `kill` would refuse to act on. `PROC_ADMIN`
    // (held by `/bin/sysmon`) sees everything.
    let caller_flags = ctx.task().flags;
    let unrestricted = ctx.is_proc_admin();
    let visible = |task: &slopos_sched::task_struct::Task| {
        unrestricted
            || (signal_may_name(task.flags)
                && !signal_is_init(task.task_id)
                && signal_dominates(caller_flags, task.flags))
    };

    let max_entries = (max as usize).min(MAX_TASKS);
    let mut entries = match KVec::<UserTaskEntry>::with_capacity(max_entries) {
        Ok(mut v) => {
            for _ in 0..max_entries {
                if v.push(UserTaskEntry::default()).is_err() {
                    return Err(Errno::ENOMEM);
                }
            }
            v
        }
        Err(_) => return Err(Errno::ENOMEM),
    };

    let mut count = 0usize;
    task_try_for_each_enumerable(|task| {
        if count >= max_entries {
            return ControlFlow::Break(());
        }
        if task.task_id == INVALID_TASK_ID {
            return ControlFlow::Continue(());
        }
        if !visible(task) {
            return ControlFlow::Continue(());
        }
        let entry = &mut entries[count];
        entry.task_id = task.task_id;
        entry.parent_task_id = task.parent_task_id();
        entry.process_id = task.process_id;
        entry.state = task.status().as_u8();
        entry.block_reason = task.load_block_reason().as_u8();
        entry.priority = task.priority.as_u8();
        entry.last_cpu = task.last_cpu();
        entry.cpu_affinity = task.cpu_affinity();
        entry.total_runtime_us =
            slopos_kernel_services::clock::ticks_to_microseconds(task.total_runtime());
        entry.creation_time_ms = task.creation_time;
        entry.yield_count = task.yield_count();
        entry.name = task.name;
        count += 1;
        ControlFlow::Continue(())
    });

    for i in 0..count {
        let dst_addr = buf
            .as_u64()
            .wrapping_add((i * core::mem::size_of::<UserTaskEntry>()) as u64);
        let user_ptr = slopos_mm::user_ptr::UserPtr::<UserTaskEntry>::try_new(dst_addr)
            .map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_ptr, &entries[i]).map_err(|_| Errno::EFAULT)?;
    }

    Ok(count as u64)
});

define_syscall!(syscall_cpu_info
    (ctx, info_out: UserPtr<slopos_abi::syscall::UserCpuInfo>) cap(SysInspect)
    -> Result<(), Errno>
{
    use slopos_abi::syscall::UserCpuInfo;
    use slopos_arch::cpu::cpuid;

    let mut info = UserCpuInfo::default();
    info.vendor = cpuid::cpu_vendor_string();
    info.brand_string = cpuid::cpu_brand_string();
    info.cpu_count = slopos_arch::pcr::get_cpu_count() as u32;
    let (family, model, stepping) = cpuid::cpu_family_model_stepping();
    info.family = family;
    info.model = model;
    info.stepping = stepping;
    info.features = cpuid::cpu_features_bitmask();

    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_percpu_stats
    (ctx, buf: UserPtr<slopos_abi::syscall::UserPerCpuStats>, max: u64) cap(SysInspect)
    -> Result<u64, Errno>
{
    use core::sync::atomic::Ordering;
    use slopos_abi::syscall::UserPerCpuStats;

    let cpu_count = slopos_arch::pcr::get_cpu_count();
    let max_entries = (max as usize).min(cpu_count);

    for i in 0..max_entries {
        let stats = slopos_sched::per_cpu::with_cpu_scheduler(i, |sched| UserPerCpuStats {
            cpu_id: i as u32,
            _pad: 0,
            total_switches: sched.total_switches.load(Ordering::Relaxed),
            total_ticks: sched.total_ticks.load(Ordering::Relaxed),
            idle_ticks: sched.idle_time.load(Ordering::Relaxed),
            ready_count: sched.total_ready_count(),
            _pad2: 0,
        })
        .unwrap_or(UserPerCpuStats {
            cpu_id: i as u32,
            ..UserPerCpuStats::default()
        });

        let dst_addr = buf
            .as_u64()
            .wrapping_add((i * core::mem::size_of::<UserPerCpuStats>()) as u64);
        let user_ptr = slopos_mm::user_ptr::UserPtr::<UserPerCpuStats>::try_new(dst_addr)
            .map_err(|_| Errno::EFAULT)?;
        copy_to_user(user_ptr, &stats).map_err(|_| Errno::EFAULT)?;
    }

    Ok(max_entries as u64)
});
