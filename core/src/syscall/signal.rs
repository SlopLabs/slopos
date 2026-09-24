use core::sync::atomic::Ordering;

use slopos_abi::Errno;
use slopos_abi::signal::{
    MINSIGSTKSZ, NSIG, REG_CR2, REG_CSGSFS, REG_EFL, REG_ERR, REG_OLDMASK, REG_R8, REG_R9, REG_R10,
    REG_R11, REG_R12, REG_R13, REG_R14, REG_R15, REG_RAX, REG_RBP, REG_RBX, REG_RCX, REG_RDI,
    REG_RDX, REG_RIP, REG_RSI, REG_RSP, REG_TRAPNO, SA_NODEFER, SA_ONSTACK, SA_RESETHAND,
    SA_SIGINFO, SI_USER, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIG_UNCATCHABLE, SIGKILL,
    SIGNAL_MASK, SIGSEGV, SS_DISABLE, SS_ONSTACK, SigDefault, SigSet, SignalFrame, UserSigAltStack,
    UserSigaction, UserSiginfo, UserUcontext, sig_bit, sig_default_action,
};
use slopos_abi::task::{
    INVALID_TASK_ID, SPAWN_PRIVILEGED, TASK_FLAG_USER_MODE, TaskExitReason, TaskFaultReason,
};
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr as MmUserPtr};
use slopos_ostd::irq::InterruptFrame;
use slopos_ostd::task::FPU_STATE_SIZE;
use slopos_ostd::user::context::{
    USER_RFLAGS_FORCED, USER_RFLAGS_PERMITTED, UserContext, UserRegs,
};

use crate::syscall::args::{Signum, UserPtr};
use crate::syscall::result::SyscallResult;
use slopos_sched::scheduler::{schedule, unblock_task};
use slopos_sched::task::{
    task_find_by_id, task_for_each_active, task_group_signal_from, task_group_stop,
    task_kill_and_wake, task_resume_if_stopped, task_signal_post_from, task_terminate,
};
use slopos_sched::task_struct::{SignalAction, Task};
use slopos_sched::trap::trap_running_on_exception_stack;

fn parse_signum(raw: u64) -> Option<u8> {
    if raw == 0 || raw as usize > NSIG {
        None
    } else {
        Some(raw as u8)
    }
}

/// Heap-backed list of unique task IDs.
struct TargetSet {
    ids: slopos_ostd::KVec<u32>,
}

impl TargetSet {
    fn new() -> Self {
        Self {
            ids: slopos_ostd::KVec::new(),
        }
    }

    fn push(&mut self, task_id: u32) {
        if task_id == INVALID_TASK_ID {
            return;
        }
        for id in self.ids.iter() {
            if *id == task_id {
                return;
            }
        }
        let _ = self.ids.push(task_id);
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn iter(&self) -> core::slice::Iter<'_, u32> {
        self.ids.iter()
    }
}

/// Whether `kill` may name this task at all.
///
/// Kernel tasks are excluded from signal *delivery*, so naming one could only
/// reach the SIGKILL arm — not signal-gated — and tear down a driver thread
/// owning device state and an interrupt line.
pub(crate) fn signal_may_name(flags: u16) -> bool {
    (flags & TASK_FLAG_USER_MODE) != 0
}

/// Whether a task holding `caller_flags` may signal one holding `target_flags`.
///
/// A sender may name a task whose privileges it already holds, and no other —
/// the relation standing in for the user ids POSIX asks about.
///
/// Stated on flags rather than on capability masks *deliberately*, and the
/// distinction matters after exec: `exec` narrows `Task::caps` but leaves
/// `Task::flags` alone, so a deprivileged process keeps the flag word it was
/// spawned with. Reading flags here is the conservative direction — the
/// exec'd process stays *protected* from its unprivileged peers rather than
/// gaining the ability to signal them — and it keeps this relation answering
/// the question it has always answered: who may be named, not what may be
/// invoked.
///
/// A capability-mask version would be the wrong shape: `caps` is about
/// operations, and two tasks with identical operational authority can still
/// stand in a spawn relation where one should not signal the other.
pub(crate) fn signal_dominates(caller_flags: u16, target_flags: u16) -> bool {
    target_flags & SPAWN_PRIVILEGED & !caller_flags == 0
}

/// Init is never a signal target: a terminating signal there takes the system
/// down undebuggably. Dominance already covers init today; the guarantee should
/// not rest on that.
pub(crate) fn signal_is_init(task_id: u32) -> bool {
    let init = crate::exec::init_task_id();
    init != INVALID_TASK_ID && task_id == init
}

/// Whether `target` may be named by a sender holding `caller_flags`, ignoring
/// the category check that answers `ESRCH` on its own.
fn signal_permitted(caller_flags: u16, target_id: u32, target_flags: u16) -> bool {
    !signal_is_init(target_id) && signal_dominates(caller_flags, target_flags)
}

struct Fanout {
    /// A task matched the selector but the caller may not signal it.
    denied: bool,
}

fn collect_targets_for_group(pgid: u32, caller_flags: u16, targets: &mut TargetSet) -> Fanout {
    let mut denied = false;
    task_for_each_active(|task| {
        if !signal_may_name(task.flags) || task.pgid() != pgid {
            return;
        }
        if signal_permitted(caller_flags, task.task_id, task.flags) {
            targets.push(task.task_id);
        } else {
            denied = true;
        }
    });
    Fanout { denied }
}

fn collect_targets_for_all(
    exclude_task_id: u32,
    caller_flags: u16,
    targets: &mut TargetSet,
) -> Fanout {
    let mut denied = false;
    task_for_each_active(|task| {
        if !signal_may_name(task.flags)
            || task.task_id == INVALID_TASK_ID
            || task.task_id == exclude_task_id
        {
            return;
        }
        if signal_permitted(caller_flags, task.task_id, task.flags) {
            targets.push(task.task_id);
        } else {
            denied = true;
        }
    });
    Fanout { denied }
}

fn action_from_user(new_action: UserSigaction) -> SignalAction {
    SignalAction {
        handler: new_action.sa_handler,
        flags: new_action.sa_flags,
        restorer: new_action.sa_restorer,
        mask: new_action.sa_mask & !SIG_UNCATCHABLE,
    }
}

fn action_to_user(action: &SignalAction) -> UserSigaction {
    UserSigaction {
        sa_handler: action.handler,
        sa_flags: action.flags,
        sa_restorer: action.restorer,
        sa_mask: action.mask,
    }
}

define_syscall!(syscall_rt_sigaction
    (ctx, signum: Signum, new_act_ptr: u64, old_act_ptr: u64, sigsetsize: u64)
    cap(NoneSelf)
    -> Result<(), Errno>
{
    if sigsetsize != core::mem::size_of::<SigSet>() as u64 {
        return Err(Errno::EINVAL);
    }
    let signum = signum.raw();
    let task_ref = ctx.task();
    let idx = (signum - 1) as usize;

    if old_act_ptr != 0 {
        let old_ptr = MmUserPtr::<UserSigaction>::try_new(old_act_ptr).map_err(|_| Errno::EFAULT)?;
        // `Signum` already bounded this; the checked read keeps the bound structural.
        let current = task_ref.signal_action(idx).ok_or(Errno::EINVAL)?;
        let old_action = action_to_user(&current);
        copy_to_user(old_ptr, &old_action).map_err(|_| Errno::EFAULT)?;
    }

    if new_act_ptr != 0 {
        if (sig_bit(signum) & SIG_UNCATCHABLE) != 0 {
            return Err(Errno::EINVAL);
        }
        let new_ptr = MmUserPtr::<UserSigaction>::try_new(new_act_ptr).map_err(|_| Errno::EFAULT)?;
        let new_action = copy_from_user(new_ptr).map_err(|_| Errno::EFAULT)?;
        if new_action.sa_handler != SIG_DFL
            && new_action.sa_handler != SIG_IGN
            && new_action.sa_restorer == 0
        {
            return Err(Errno::EINVAL);
        }
        if !task_ref.set_signal_action(idx, action_from_user(new_action)) {
            return Err(Errno::EINVAL);
        }
    }

    Ok(())
});

define_syscall!(syscall_rt_sigprocmask
    (ctx, how: u32, set_ptr: u64, oldset_ptr: u64, sigsetsize: u64)
    cap(NoneSelf)
    -> Result<(), Errno>
{
    if sigsetsize != core::mem::size_of::<SigSet>() as u64 {
        return Err(Errno::EINVAL);
    }
    let task_ref = ctx.task();

    if oldset_ptr != 0 {
        let old_ptr = MmUserPtr::<SigSet>::try_new(oldset_ptr).map_err(|_| Errno::EFAULT)?;
        copy_to_user(old_ptr, &task_ref.signal_blocked()).map_err(|_| Errno::EFAULT)?;
    }

    if set_ptr != 0 {
        let new_ptr = MmUserPtr::<SigSet>::try_new(set_ptr).map_err(|_| Errno::EFAULT)?;
        let set = copy_from_user(new_ptr).map_err(|_| Errno::EFAULT)?;

        let mut blocked = task_ref.signal_blocked();
        match how {
            slopos_abi::signal::SIG_BLOCK => blocked |= set,
            SIG_UNBLOCK => blocked &= !set,
            SIG_SETMASK => blocked = set,
            _ => return Err(Errno::EINVAL),
        }
        // `SIGNAL_MASK` as well as the uncatchable pair: `set` is user-authored,
        // and bit `NSIG` is the kernel-private kill flag. Every reader masks
        // already, so this is inert today — and that is the fragile direction.
        // Filtering at the writer makes the private bit unrepresentable in
        // `signal_blocked` instead of merely unread.
        task_ref.set_signal_blocked(blocked & SIGNAL_MASK & !SIG_UNCATCHABLE);
    }

    Ok(())
});

define_syscall!(syscall_kill
    (ctx, raw_pid_arg: i64, sig: u64) cap(NoneRelation)
    -> SyscallResult
{
    let caller_id = ctx.task_id();
    // `si_pid` names the sending process, as `getpid` would.
    let sender = match ctx.task().tgid {
        INVALID_TASK_ID => caller_id,
        tgid => tgid,
    };

    if raw_pid_arg < i32::MIN as i64 || raw_pid_arg > i32::MAX as i64 {
        return SyscallResult::Err(Errno::ESRCH);
    }
    let pid = raw_pid_arg as i32;

    let caller_flags = ctx.task().flags;

    let mut targets = TargetSet::new();

    if pid > 0 {
        // Resolved and authorized in one step, holding an owning reference to
        // the target: without it the id could be recycled onto a stranger
        // between this check and the delivery below.
        let target = match crate::syscall::signalable::resolve_signal_target(
            caller_flags,
            pid as u32,
        ) {
            Ok(target) => target,
            Err(e) => return SyscallResult::Err(e),
        };
        // Deliberately after the permission check: `kill(pid, 0)` is the
        // existence-and-permission probe.
        if sig == 0 {
            return SyscallResult::Ok(0);
        }
        let Some(signum) = parse_signum(sig) else {
            return SyscallResult::Err(Errno::EINVAL);
        };
        // POSIX `kill(pid)` names a *process*: the signal reaches every thread
        // in the group. The permission relation is answered once, on the named
        // task — a thread-group fan-out crosses no session.
        if task_group_signal_from(target.id(), signum, sender) == 0 {
            return SyscallResult::Err(Errno::ESRCH);
        }
        return SyscallResult::Ok(0);
    }
    let fanout = if pid == 0 {
        if caller_id == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        let Some(caller) = task_find_by_id(caller_id) else {
            return SyscallResult::Err(Errno::ESRCH);
        };
        let caller_pgid = caller.pgid();
        if caller_pgid == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        collect_targets_for_group(caller_pgid, caller_flags, &mut targets)
    } else if pid == -1 {
        if caller_id == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        collect_targets_for_all(caller_id, caller_flags, &mut targets)
    } else {
        if pid == i32::MIN {
            return SyscallResult::Err(Errno::ESRCH);
        }
        let group_id = (-pid) as u32;
        if group_id == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        collect_targets_for_group(group_id, caller_flags, &mut targets)
    };

    if targets.len() == 0 {
        // A selector that matched only unsignalable tasks is a permission
        // answer; one that matched nothing at all is not.
        return SyscallResult::Err(if fanout.denied {
            Errno::EPERM
        } else {
            Errno::ESRCH
        });
    }

    // Deliberately after the permission check: `kill(pid, 0)` is the
    // existence-and-permission probe, so it must be able to answer EPERM.
    if sig == 0 {
        return SyscallResult::Ok(0);
    }

    let Some(signum) = parse_signum(sig) else {
        return SyscallResult::Err(Errno::EINVAL);
    };

    // A stop or a continue is a state change of a whole *process*, applied per
    // thread group even when the selector named one thread. Idempotent, so a
    // group with several matching threads still stops once.
    let job_control = matches!(
        sig_default_action(signum),
        SigDefault::Stop | SigDefault::Continue
    );

    let mut signaled = 0usize;

    for target_id in targets.iter() {
        let Some(target) = task_find_by_id(*target_id) else {
            continue;
        };

        if job_control {
            if task_group_signal_from(*target_id, signum, sender) != 0 {
                signaled += 1;
            }
            continue;
        }

        if signum == SIGKILL {
            // SIG_UNCATCHABLE is stripped from every mask and rt_sigaction
            // refuses a handler, so SIGKILL is always deliverable. The kill
            // flag is what a target parked in a blocking primitive sees: it
            // unwinds by returning rather than being abandoned mid-stack.
            let _ = task_signal_post_from(&target, SIGKILL, sender);
            task_kill_and_wake(&target);
            // A stopped target reaches no boundary at which to act on the kill
            // flag until something resumes it.
            task_resume_if_stopped(&target);
            signaled += 1;
            continue;
        }

        // POSIX: kill() succeeds even when the disposition discards the signal.
        if task_signal_post_from(&target, signum, sender) {
            let _ = unblock_task(&target);
        }
        signaled += 1;
    }

    if signaled == 0 {
        return SyscallResult::Err(Errno::ESRCH);
    }

    // A self-kill returns normally and dies one frame later, in the signal
    // delivery at the end of `syscall_handle`.
    SyscallResult::Ok(0)
});

fn read_signal_frame(rsp: u64) -> Option<SignalFrame> {
    let ptr = MmUserPtr::<SignalFrame>::try_new(rsp).ok()?;
    copy_from_user(ptr).ok()
}

/// User signal-frame geometry, from the restorer word up:
/// `[restorer u64][SignalFrame][UserSiginfo][UserUcontext][FPU]`.
///
/// The `SignalFrame` must stay immediately after the restorer word: the
/// userland restorer's contract is that once the handler's `ret` pops the
/// restorer, RSP points directly at the frame.
const SIGINFO_SIZE: u64 = core::mem::size_of::<UserSiginfo>() as u64;
const UCONTEXT_SIZE: u64 = core::mem::size_of::<UserUcontext>() as u64;
const SIGFRAME_SIZE: u64 = core::mem::size_of::<SignalFrame>() as u64;

pub(crate) const SIGFRAME_TOTAL: u64 =
    8 + SIGFRAME_SIZE + SIGINFO_SIZE + UCONTEXT_SIZE + FPU_STATE_SIZE as u64;

// The floor must admit one whole frame, or `sigaltstack`'s refusal defers to
// a silent overflow below `ss_sp`.
const _: () = assert!(MINSIGSTKSZ as u64 >= SIGFRAME_TOTAL);

/// Base of the frame delivery pushes below `stack_top` — the address the
/// handler enters with in RSP, holding the restorer word.
///
/// The restorer pointer doubles as the handler's return address, so SysV wants
/// `frame_addr % 16 == 8`; aligning to 16 instead faults #GP on the first
/// aligned vector spill the handler emits.
#[inline]
pub(crate) fn sigframe_base_for_stack_top(stack_top: u64) -> u64 {
    (stack_top.wrapping_sub(SIGFRAME_TOTAL) & !0xF).wrapping_sub(8)
}

#[inline]
fn sigframe_siginfo_addr(sigframe_addr: u64) -> u64 {
    sigframe_addr.wrapping_add(SIGFRAME_SIZE)
}

#[inline]
fn sigframe_ucontext_addr(sigframe_addr: u64) -> u64 {
    sigframe_siginfo_addr(sigframe_addr).wrapping_add(SIGINFO_SIZE)
}

/// The FPU/vector save area, last in the frame. Kernel-internal; the userland
/// restorer never touches it, but `rt_sigreturn` must compute it the same way
/// delivery did.
#[inline]
pub(crate) fn sigframe_fpu_addr(sigframe_addr: u64) -> u64 {
    sigframe_ucontext_addr(sigframe_addr).wrapping_add(UCONTEXT_SIZE)
}

/// Save the interrupted task's live FPU/vector state into its user signal frame
/// so a handler cannot corrupt it. The kernel is `+soft-float` and has not
/// touched the vector file since entry, so the live CPU state is still the
/// user's. Returns false on a user-copy fault.
fn save_fpu_to_sigframe(current: &slopos_sched::task_struct::Current, sigframe_addr: u64) -> bool {
    // Not a switch-out: the state stays live in the register file, so the save
    // keeps the owner tag rather than releasing it.
    let task = current.task();
    task.fpu_save_in_place(current, slopos_ostd::cpu::x86_64::xsave::active_xcr0());
    let Ok(bytes) = UserBytes::try_new(sigframe_fpu_addr(sigframe_addr), FPU_STATE_SIZE) else {
        return false;
    };
    task.with_fpu_bytes_mut(current, |data| copy_bytes_to_user(bytes, data).is_ok())
}

/// Copy the image written by [`save_fpu_to_sigframe`] back into the task's
/// save area and check that `XRSTOR64` will accept it. Returns false on a
/// user-copy fault or a malformed image.
///
/// The 2.6 KiB image is borrowed in place rather than staged through a scratch
/// buffer (2 KiB stack-frame ceiling), so rejection leaves the task owning
/// bytes it did not author and the reset is what takes them back. `xcr0` comes
/// from the caller so the image is validated against the mask it is then
/// restored under.
fn stage_fpu_from_sigframe(
    current: &slopos_sched::task_struct::Current,
    sigframe_addr: u64,
    xcr0: u64,
) -> bool {
    let Ok(bytes) = UserBytes::try_new(sigframe_fpu_addr(sigframe_addr), FPU_STATE_SIZE) else {
        return false;
    };
    let mxcsr_mask = slopos_ostd::cpu::x86_64::xsave::mxcsr_feature_mask();

    let task = current.task();
    let staged = task.with_fpu_bytes_mut(current, |data| {
        copy_bytes_from_user(bytes, data).is_ok()
            && slopos_ostd::task::validate_xsave_image(data, xcr0, mxcsr_mask).is_ok()
    });
    if !staged {
        task.fpu_reset(current);
    }
    staged
}

define_syscall!(syscall_rt_sigreturn (ctx) cap(NoneSelf)
    -> SyscallResult {
    // After the handler's `ret` pops the restorer address, RSP points
    // directly at the SignalFrame.
    let rsp = ctx.user_rsp();
    let sigframe = match read_signal_frame(rsp) {
        Some(sf) => sf,
        None => return SyscallResult::Err(Errno::EFAULT),
    };

    let Some(current) = slopos_sched::task_struct::Current::get() else {
        return SyscallResult::Err(Errno::EFAULT);
    };
    let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();

    // Committed in one IRQ-off window: a context switch between the copy-in and
    // the XRSTOR would save the live register file over the staged image. FPU
    // before GPRs, so a refused frame leaves the task exactly where it was.
    let committed = slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        if !stage_fpu_from_sigframe(&current, rsp, xcr0) {
            return false;
        }
        if !current.task().fpu_restore_to_cpu(&current, xcr0) {
            return false;
        }

        let mut regs = ctx.user_ctx().regs();
        regs.rax = sigframe.rax;
        regs.rbx = sigframe.rbx;
        regs.rcx = sigframe.rcx;
        regs.rdx = sigframe.rdx;
        regs.rsi = sigframe.rsi;
        regs.rdi = sigframe.rdi;
        regs.rbp = sigframe.rbp;
        regs.rsp = sigframe.rsp;
        regs.r8 = sigframe.r8;
        regs.r9 = sigframe.r9;
        regs.r10 = sigframe.r10;
        regs.r11 = sigframe.r11;
        regs.r12 = sigframe.r12;
        regs.r13 = sigframe.r13;
        regs.r14 = sigframe.r14;
        regs.r15 = sigframe.r15;
        regs.rip = sigframe.rip;
        regs.rflags_user_subset = sigframe.rflags;
        ctx.user_ctx().set_regs(regs);

        // The mask came out of a sigframe the caller wrote: masked to
        // `SIGNAL_MASK` for the reason `rt_sigprocmask` above masks.
        ctx.task()
            .set_signal_blocked(sigframe.saved_mask & SIGNAL_MASK & !SIG_UNCATCHABLE);
        true
    });

    if !committed {
        return SyscallResult::Err(Errno::EFAULT);
    }

    // sigreturn fully replaced the user-mode register state — the
    // dispatcher must not overwrite RAX after we return.
    SyscallResult::NoReturn
});

define_syscall!(syscall_sigaltstack
    (ctx, new_ptr: Option<UserPtr<UserSigAltStack>>, old_ptr: Option<UserPtr<UserSigAltStack>>)
    cap(NoneSelf)
    -> Result<(), Errno>
{
    let task_ref = ctx.task();
    let (ss_sp, ss_size) = task_ref.sigaltstack();
    // POSIX asks whether the task is *executing* on the alternate stack, which
    // is a range test on the interrupted stack pointer: a stored flag is wrong
    // for a nested handler, whose return would retire the outer frame's mark.
    let on_stack = task_ref.rsp_on_sigaltstack(ctx.user_rsp());

    if let Some(old) = old_ptr {
        let reported = UserSigAltStack {
            ss_sp,
            ss_flags: if on_stack {
                SS_ONSTACK
            } else if ss_sp == 0 {
                SS_DISABLE
            } else {
                0
            },
            _pad: 0,
            ss_size,
        };
        copy_to_user(old.inner(), &reported).map_err(|_| Errno::EFAULT)?;
    }

    let Some(new) = new_ptr else {
        return Ok(());
    };

    // A change while the task is running on the alternate stack would move the
    // ground out from under the frame it is executing on.
    if on_stack {
        return Err(Errno::EPERM);
    }

    let requested = copy_from_user(new.inner()).map_err(|_| Errno::EFAULT)?;
    if (requested.ss_flags & !(SS_DISABLE | SS_ONSTACK)) != 0 {
        return Err(Errno::EINVAL);
    }
    if (requested.ss_flags & SS_DISABLE) != 0 {
        task_ref.set_sigaltstack(0, 0);
        return Ok(());
    }
    if requested.ss_size < MINSIGSTKSZ as u64 {
        return Err(Errno::ENOMEM);
    }
    if requested.ss_sp == 0 || requested.ss_sp.checked_add(requested.ss_size).is_none() {
        return Err(Errno::EINVAL);
    }
    // Validated here rather than at delivery: a frame push is the wrong place
    // to discover the stack was never a user address.
    UserBytes::try_new(requested.ss_sp, requested.ss_size as usize).map_err(|_| Errno::EFAULT)?;
    task_ref.set_sigaltstack(requested.ss_sp, requested.ss_size);
    Ok(())
});

/// Read/write view over the user-mode register file signal delivery mutates,
/// shared by the syscall-exit and IRQ-exit paths so both stay in lockstep.
trait UserRegView {
    /// Snapshot the GPR file, with `rflags_user_subset` already masked to the
    /// user-permitted bits so both delivery sites store the same value.
    fn snapshot(&self) -> UserRegs;

    /// Commit a redirected register file. Re-applies RFLAGS masking and the
    /// user CS/SS selectors so a redirect cannot escape the sandbox.
    fn commit_redirect(&mut self, regs: &UserRegs);
}

/// Syscall-exit register view over the per-task [`UserContext`].
struct UserContextRegs<'a> {
    ctx: &'a UserContext,
}

impl UserRegView for UserContextRegs<'_> {
    fn snapshot(&self) -> UserRegs {
        self.ctx.regs()
    }

    fn commit_redirect(&mut self, regs: &UserRegs) {
        self.ctx.set_regs(*regs);
    }
}

/// IRQ-exit register view over the CPU-pushed [`InterruptFrame`]. The `iretq`
/// that resumes user mode loads RIP/RSP/RFLAGS from this frame, so a redirect
/// mutates it in place.
struct InterruptFrameRegs<'a> {
    frame: &'a mut InterruptFrame,
}

impl UserRegView for InterruptFrameRegs<'_> {
    fn snapshot(&self) -> UserRegs {
        let f = &*self.frame;
        UserRegs {
            rax: f.rax,
            rbx: f.rbx,
            rcx: f.rcx,
            rdx: f.rdx,
            rsi: f.rsi,
            rdi: f.rdi,
            rbp: f.rbp,
            rsp: f.rsp,
            r8: f.r8,
            r9: f.r9,
            r10: f.r10,
            r11: f.r11,
            r12: f.r12,
            r13: f.r13,
            r14: f.r14,
            r15: f.r15,
            rip: f.rip,
            rflags_user_subset: f.rflags & USER_RFLAGS_PERMITTED,
            fs_base: 0,
            gs_base: 0,
            cs: f.cs as u16,
            ss: f.ss as u16,
            _pad: [0; 3],
        }
    }

    fn commit_redirect(&mut self, regs: &UserRegs) {
        let f = &mut *self.frame;
        f.rax = regs.rax;
        f.rbx = regs.rbx;
        f.rcx = regs.rcx;
        f.rdx = regs.rdx;
        f.rsi = regs.rsi;
        f.rdi = regs.rdi;
        f.rbp = regs.rbp;
        f.rsp = regs.rsp;
        f.r8 = regs.r8;
        f.r9 = regs.r9;
        f.r10 = regs.r10;
        f.r11 = regs.r11;
        f.r12 = regs.r12;
        f.r13 = regs.r13;
        f.r14 = regs.r14;
        f.r15 = regs.r15;
        f.rip = regs.rip;
        f.rflags = (regs.rflags_user_subset & USER_RFLAGS_PERMITTED) | USER_RFLAGS_FORCED;
    }
}

/// What [`claim_pending_signal`] decided, for a caller holding no borrow.
enum SignalDisposition {
    /// Nothing deliverable, or the disposition needs no further work.
    Done,
    /// Terminate; the exit fields are already stamped.
    Terminate(u32),
    /// Job control: park this task's whole thread group.
    Stop { signum: u8 },
    Handle {
        signum: u8,
        bit: u64,
        action: SignalAction,
        saved_mask: SigSet,
        si_code: i32,
        si_addr: u64,
        /// The sending process for `si_pid`, or 0 when the kernel raised it.
        sender: u32,
    },
}

/// Pick the lowest deliverable signal, consume its pending bit, and decide what
/// happens to it.
///
/// Split from the delivery below so the task borrow ends before anything acts
/// on the decision: terminating re-enters the task through the registry and
/// context-switches.
fn claim_pending_signal(task_ref: &Task) -> SignalDisposition {
    if (task_ref.flags & TASK_FLAG_USER_MODE) == 0 {
        return SignalDisposition::Done;
    }

    let pending = task_ref.signal_pending.load(Ordering::Acquire);
    let deliverable = pending & SIGNAL_MASK & !task_ref.signal_blocked();
    if deliverable == 0 {
        return SignalDisposition::Done;
    }

    let signum = (deliverable.trailing_zeros() + 1) as u8;
    let bit = sig_bit(signum);
    // Taken before the bit clears: a post finds the bit still set and drops
    // its signal, so the sender read here belongs to this delivery.
    let sender = task_ref.take_signal_sender(signum);
    task_ref.signal_pending.fetch_and(!bit, Ordering::AcqRel);

    let idx = (signum - 1) as usize;
    let Some(action) = task_ref.signal_action(idx) else {
        return SignalDisposition::Done;
    };
    if action.handler == SIG_IGN {
        return SignalDisposition::Done;
    }

    if action.handler == SIG_DFL {
        return match sig_default_action(signum) {
            // A `SIGCONT` that only pends here could never have woken this
            // task: a stopped task reaches no delivery point, so the resume
            // already happened at the send site.
            SigDefault::Ignore | SigDefault::Continue => SignalDisposition::Done,
            SigDefault::Stop => SignalDisposition::Stop { signum },
            SigDefault::Terminate => {
                slopos_ostd::klog_info!(
                    "SIGNAL: task {} of process {} dies of signal {} from task {}",
                    task_ref.task_id,
                    task_ref.tgid,
                    signum,
                    sender
                );
                task_ref
                    .exit_reason
                    .store(TaskExitReason::Signalled.as_u16(), Ordering::Release);
                task_ref
                    .fault_reason
                    .store(TaskFaultReason::None.as_u16(), Ordering::Release);
                task_ref
                    .exit_code
                    .store(128 + signum as u32, Ordering::Release);
                // `exit(139)` is not a segfault, so the signal number cannot
                // be recovered from the exit code.
                task_ref.set_exit_signal(signum);
                SignalDisposition::Terminate(task_ref.task_id)
            }
        };
    }

    if action.restorer == 0 {
        return SignalDisposition::Done;
    }

    let (si_code, si_addr) = task_ref.fault_siginfo_for(signum).unwrap_or((SI_USER, 0));

    SignalDisposition::Handle {
        signum,
        bit,
        action,
        saved_mask: task_ref.signal_blocked(),
        si_code,
        si_addr,
        sender,
    }
}

/// Write the `siginfo_t` an `SA_SIGINFO` handler receives.
///
/// `#[inline(never)]`: 128 bytes of `siginfo` must materialise in *this*
/// frame, not in a delivery frame already near the 2 KiB ceiling.
#[inline(never)]
fn push_siginfo(addr: u64, signum: u8, si_code: i32, si_addr: u64, sender: u32) -> bool {
    let Ok(ptr) = MmUserPtr::<UserSiginfo>::try_new(addr) else {
        return false;
    };
    let info = if si_code == SI_USER {
        UserSiginfo::sent(signum as i32, si_code, sender, 0)
    } else {
        UserSiginfo::new(signum as i32, si_code, si_addr)
    };
    copy_to_user(ptr, &info).is_ok()
}

/// Write the `ucontext_t` an `SA_SIGINFO` handler receives. Same frame-size
/// reason for `#[inline(never)]` as [`push_siginfo`], at ~250 bytes.
#[inline(never)]
fn push_ucontext(
    addr: u64,
    regs: &UserRegs,
    saved_mask: SigSet,
    altstack: (u64, u64),
    on_altstack: bool,
    si_addr: u64,
) -> bool {
    let Ok(ptr) = MmUserPtr::<UserUcontext>::try_new(addr) else {
        return false;
    };
    let mut uc = UserUcontext {
        uc_flags: 0,
        uc_link: 0,
        uc_stack: UserSigAltStack {
            ss_sp: altstack.0,
            ss_flags: if on_altstack {
                SS_ONSTACK
            } else if altstack.0 == 0 {
                SS_DISABLE
            } else {
                0
            },
            _pad: 0,
            ss_size: altstack.1,
        },
        uc_mcontext_gregs: [0; 23],
        uc_sigmask: saved_mask,
    };
    let g = &mut uc.uc_mcontext_gregs;
    g[REG_R8] = regs.r8;
    g[REG_R9] = regs.r9;
    g[REG_R10] = regs.r10;
    g[REG_R11] = regs.r11;
    g[REG_R12] = regs.r12;
    g[REG_R13] = regs.r13;
    g[REG_R14] = regs.r14;
    g[REG_R15] = regs.r15;
    g[REG_RDI] = regs.rdi;
    g[REG_RSI] = regs.rsi;
    g[REG_RBP] = regs.rbp;
    g[REG_RBX] = regs.rbx;
    g[REG_RDX] = regs.rdx;
    g[REG_RAX] = regs.rax;
    g[REG_RCX] = regs.rcx;
    g[REG_RSP] = regs.rsp;
    g[REG_RIP] = regs.rip;
    g[REG_EFL] = regs.rflags_user_subset;
    g[REG_CSGSFS] = regs.cs as u64;
    g[REG_ERR] = 0;
    g[REG_TRAPNO] = 0;
    g[REG_OLDMASK] = saved_mask;
    g[REG_CR2] = si_addr;
    copy_to_user(ptr, &uc).is_ok()
}

/// Where the frame for `action` is based.
///
/// A task already on the alternate stack keeps pushing downward from the
/// interrupted stack pointer, so a nested handler does not land on the frame
/// the outer one is using. An alternate stack too small for a frame is not
/// used at all, so the failure is a refused delivery rather than a silent
/// write below `ss_sp`.
fn frame_base_for(task_ref: &Task, action: &SignalAction, rsp: u64) -> u64 {
    if (action.flags & SA_ONSTACK) == 0 || task_ref.rsp_on_sigaltstack(rsp) {
        return rsp;
    }
    let (ss_sp, ss_size) = task_ref.sigaltstack();
    if ss_sp == 0 || ss_size < SIGFRAME_TOTAL {
        return rsp;
    }
    ss_sp.wrapping_add(ss_size)
}

/// A frame push that faulted while delivering a signal the task cannot
/// decline.
///
/// Re-pending is not an option: the interrupted instruction re-executes on
/// return to user mode, so a fault signal whose frame cannot be pushed would
/// fault forever. The task dies of `SIGSEGV` whatever the original signal was.
fn force_death_on_frame_fault(task_ref: &Task) {
    task_ref
        .exit_reason
        .store(TaskExitReason::Signalled.as_u16(), Ordering::Release);
    task_ref
        .fault_reason
        .store(TaskFaultReason::None.as_u16(), Ordering::Release);
    task_ref
        .exit_code
        .store(128 + SIGSEGV as u32, Ordering::Release);
    task_ref.set_exit_signal(SIGSEGV);
    let task_id = task_ref.task_id;
    if task_terminate(task_id) == 0 {
        schedule();
    }
}

/// Resolve the frame's pages as a user write would, so the copies below meet
/// a present, writable leaf. `#[inline(never)]` for the same frame-size
/// reason as [`push_siginfo`].
#[inline(never)]
fn populate_sigframe_range(task_ref: &Task, frame_addr: u64) -> bool {
    slopos_mm::page_fault::populate_user_range_for_write(
        task_ref.process_vm_handle_raw(),
        frame_addr,
        SIGFRAME_TOTAL,
        task_ref.task_id,
        slopos_mm::page_fault::FileIo::Refuse,
    )
}

fn deliver_pending_signal_core(
    current: &slopos_sched::task_struct::Current,
    regs: &mut impl UserRegView,
) {
    let task_ref = current.task();

    let (signum, bit, action, saved_mask, si_code, si_addr, sender) =
        match claim_pending_signal(task_ref) {
            SignalDisposition::Done => {
                // A task marked for death leaves here rather than returning to
                // userland; the mark is deliberately not a signal. This frame
                // returns to CPL3 off an exception stack and owns no Rust value,
                // so abandoning it across the switch leaks nothing.
                if task_ref.is_killed() {
                    let task_id = task_ref.task_id;
                    if task_terminate(task_id) == 0 {
                        schedule();
                    }
                }
                return;
            }
            SignalDisposition::Terminate(task_id) => {
                if task_terminate(task_id) == 0 {
                    schedule();
                }
                return;
            }
            SignalDisposition::Stop { signum } => {
                // Parks this task's whole thread group, this task last, and does
                // not return until a `SIGCONT` resumes it.
                let _ = task_group_stop(task_ref.task_id, signum);
                return;
            }
            SignalDisposition::Handle {
                signum,
                bit,
                action,
                saved_mask,
                si_code,
                si_addr,
                sender,
            } => (signum, bit, action, saved_mask, si_code, si_addr, sender),
        };

    let regs_snapshot = regs.snapshot();
    let fault_signal = task_ref.fault_siginfo_for(signum).is_some();

    let stack_top = frame_base_for(task_ref, &action, regs_snapshot.rsp);
    let frame_addr = sigframe_base_for_stack_top(stack_top);
    let sigframe_addr = frame_addr.wrapping_add(8);

    // A push that faults while delivering a *fault* signal must kill rather
    // than re-pend: the interrupted instruction re-executes and faults again.
    // Anything else gets one deferral and then dies, as Linux's
    // `force_sigsegv` does — a signal retried forever is never reported.
    let refuse = |task_ref: &Task| {
        if fault_signal || task_ref.note_sigframe_push_failure() >= 2 {
            force_death_on_frame_fault(task_ref);
            return;
        }
        task_ref.set_signal_sender(signum, sender);
        task_ref.signal_pending.fetch_or(bit, Ordering::AcqRel);
    };

    // The copies below refuse an absent or write-protected leaf rather than
    // faulting it in, and the frame lands wherever the interrupted RSP put it:
    // below a `fork`ed child's untouched COW stack pages as often as not.
    if !populate_sigframe_range(task_ref, frame_addr) {
        refuse(task_ref);
        return;
    }

    let restorer_ptr = match MmUserPtr::<u64>::try_new(frame_addr) {
        Ok(p) => p,
        Err(_) => {
            refuse(task_ref);
            return;
        }
    };
    if copy_to_user(restorer_ptr, &action.restorer).is_err() {
        refuse(task_ref);
        return;
    }

    if !push_signal_frame(sigframe_addr, signum, &regs_snapshot, saved_mask) {
        refuse(task_ref);
        return;
    }

    if (action.flags & SA_SIGINFO) != 0
        && (!push_siginfo(
            sigframe_siginfo_addr(sigframe_addr),
            signum,
            si_code,
            si_addr,
            sender,
        ) || !push_ucontext(
            sigframe_ucontext_addr(sigframe_addr),
            &regs_snapshot,
            saved_mask,
            task_ref.sigaltstack(),
            task_ref.rsp_on_sigaltstack(regs_snapshot.rsp),
            si_addr,
        ))
    {
        refuse(task_ref);
        return;
    }

    if !save_fpu_to_sigframe(current, sigframe_addr) {
        refuse(task_ref);
        return;
    }

    // Everything below this point commits: no path from here refuses the
    // delivery, which is what lets the disposition reset and the mask move
    // together with the redirect.
    task_ref.clear_sigframe_push_failures();
    // Consumed, so the next delivery from a `kill` reports no stale address.
    task_ref.clear_fault_siginfo();

    // `SA_RESETHAND` reverts here rather than at the claim, so a refused
    // frame push leaves the handler installed instead of converting a caught
    // signal into its default action on the retry.
    if (action.flags & SA_RESETHAND) != 0 {
        let _ = task_ref.set_signal_action((signum - 1) as usize, SignalAction::default());
    }

    let mut blocked = saved_mask | action.mask;
    if (action.flags & SA_NODEFER) == 0 {
        blocked |= bit;
    }
    // `action.mask` is `sa_mask` as userland wrote it, so the same filter as
    // the other two `set_signal_blocked` writers applies.
    task_ref.set_signal_blocked(blocked & SIGNAL_MASK & !SIG_UNCATCHABLE);

    let mut redirected = regs_snapshot;
    redirected.rsp = frame_addr;
    redirected.rip = action.handler;
    redirected.rdi = signum as u64;
    if (action.flags & SA_SIGINFO) != 0 {
        redirected.rsi = sigframe_siginfo_addr(sigframe_addr);
        redirected.rdx = sigframe_ucontext_addr(sigframe_addr);
    } else {
        redirected.rsi = 0;
        redirected.rdx = 0;
    }
    regs.commit_redirect(&redirected);
}

/// Write the [`SignalFrame`] `rt_sigreturn` restores from. `#[inline(never)]`
/// for the same frame-size reason as [`push_siginfo`].
#[inline(never)]
fn push_signal_frame(addr: u64, signum: u8, regs: &UserRegs, saved_mask: SigSet) -> bool {
    let Ok(ptr) = MmUserPtr::<SignalFrame>::try_new(addr) else {
        return false;
    };
    let frame = SignalFrame {
        signum: signum as u64,
        rax: regs.rax,
        rbx: regs.rbx,
        rcx: regs.rcx,
        rdx: regs.rdx,
        rsi: regs.rsi,
        rdi: regs.rdi,
        rbp: regs.rbp,
        rsp: regs.rsp,
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
        r11: regs.r11,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        rip: regs.rip,
        rflags: regs.rflags_user_subset,
        saved_mask,
    };
    copy_to_user(ptr, &frame).is_ok()
}

pub fn deliver_pending_signal(
    current: &slopos_sched::task_struct::Current,
    user_ctx: &UserContext,
) {
    deliver_pending_signal_core(current, &mut UserContextRegs { ctx: user_ctx });
}

/// Deliver a pending signal on the IRQ/timer/IPI return-to-user path.
///
/// Without a check here a user task spinning in pure userspace (no syscalls)
/// would never act on a pending signal and would be unkillable.
///
/// No-op unless the frame is non-null, returns to user (`cs & 3 == 3`), a
/// user-mode task is current, and we are not on an IST/exception stack —
/// vectors 0-31 run under `IstPreemptHold` and must never deliver here.
pub fn deliver_pending_signal_on_irq_exit(frame: *mut InterruptFrame) {
    // The frame lives for exactly this invocation, so a frame-local anchors the borrow.
    let mut frame_anchor = ();
    let Some(frame_ref) = InterruptFrame::from_ptr_mut(&mut frame_anchor, frame) else {
        return;
    };
    if (frame_ref.cs & 3) != 3 {
        return;
    }
    if trap_running_on_exception_stack() {
        return;
    }

    let Some(current) = slopos_sched::task_struct::Current::get() else {
        return;
    };

    let mut view = InterruptFrameRegs { frame: frame_ref };
    deliver_pending_signal_core(&current, &mut view);
}
