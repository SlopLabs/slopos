use core::sync::atomic::Ordering;

use slopos_abi::signal::{
    NSIG, SA_NODEFER, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIG_UNCATCHABLE, SIGKILL,
    SigDefault, SigSet, SignalFrame, UserSigaction, sig_bit, sig_default_action,
};
use slopos_abi::syscall::{ERRNO_EFAULT, ERRNO_EINVAL, ERRNO_ESRCH};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE, TaskExitReason, TaskFaultReason};
use slopos_lib::InterruptFrame;
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr;

use crate::sched::{clear_scheduler_current_task, schedule, unblock_task};
use crate::scheduler::task::{task_find_by_id, task_terminate};
use crate::scheduler::task_struct::{SignalAction, Task};
use crate::syscall::common::{SyscallDisposition, syscall_return_err};
use crate::syscall::context::SyscallContext;

fn errno(ctx: &SyscallContext, value: u64) -> SyscallDisposition {
    let disp = ctx.err();
    unsafe {
        (*ctx.frame_ptr()).rax = value;
    }
    disp
}

fn parse_signum(raw: u64) -> Option<u8> {
    if raw == 0 || raw as usize > NSIG {
        None
    } else {
        Some(raw as u8)
    }
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

pub fn syscall_rt_sigaction(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let args = ctx.args();
    if args.arg3 != core::mem::size_of::<SigSet>() as u64 {
        return errno(&ctx, ERRNO_EINVAL);
    }

    let Some(signum) = parse_signum(args.arg0) else {
        return errno(&ctx, ERRNO_EINVAL);
    };

    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return errno(&ctx, ERRNO_EINVAL),
    };
    let idx = (signum - 1) as usize;

    if args.arg2 != 0 {
        let old_ptr = match UserPtr::<UserSigaction>::try_new(args.arg2) {
            Ok(p) => p,
            Err(_) => return errno(&ctx, ERRNO_EFAULT),
        };
        let old_action = action_to_user(&task_ref.signal_actions[idx]);
        if copy_to_user(old_ptr, &old_action).is_err() {
            return errno(&ctx, ERRNO_EFAULT);
        }
    }

    if args.arg1 != 0 {
        if (sig_bit(signum) & SIG_UNCATCHABLE) != 0 {
            return errno(&ctx, ERRNO_EINVAL);
        }
        let new_ptr = match UserPtr::<UserSigaction>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => return errno(&ctx, ERRNO_EFAULT),
        };
        let new_action = match copy_from_user(new_ptr) {
            Ok(a) => a,
            Err(_) => return errno(&ctx, ERRNO_EFAULT),
        };
        if new_action.sa_handler != SIG_DFL
            && new_action.sa_handler != SIG_IGN
            && new_action.sa_restorer == 0
        {
            return errno(&ctx, ERRNO_EINVAL);
        }
        task_ref.signal_actions[idx] = action_from_user(new_action);
    }

    ctx.ok(0)
}

pub fn syscall_rt_sigprocmask(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let args = ctx.args();
    if args.arg3 != core::mem::size_of::<SigSet>() as u64 {
        return errno(&ctx, ERRNO_EINVAL);
    }

    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return errno(&ctx, ERRNO_EINVAL),
    };

    if args.arg2 != 0 {
        let old_ptr = match UserPtr::<SigSet>::try_new(args.arg2) {
            Ok(p) => p,
            Err(_) => return errno(&ctx, ERRNO_EFAULT),
        };
        if copy_to_user(old_ptr, &task_ref.signal_blocked).is_err() {
            return errno(&ctx, ERRNO_EFAULT);
        }
    }

    if args.arg1 != 0 {
        let new_ptr = match UserPtr::<SigSet>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => return errno(&ctx, ERRNO_EFAULT),
        };
        let set = match copy_from_user(new_ptr) {
            Ok(v) => v,
            Err(_) => return errno(&ctx, ERRNO_EFAULT),
        };

        let mut blocked = task_ref.signal_blocked;
        match args.arg0 as u32 {
            slopos_abi::signal::SIG_BLOCK => blocked |= set,
            SIG_UNBLOCK => blocked &= !set,
            SIG_SETMASK => blocked = set,
            _ => return errno(&ctx, ERRNO_EINVAL),
        }
        task_ref.signal_blocked = blocked & !SIG_UNCATCHABLE;
    }

    ctx.ok(0)
}

pub fn syscall_kill(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let args = ctx.args();
    let caller_id = ctx.task_id().unwrap_or(INVALID_TASK_ID);
    let target_id = if args.arg0_u32() == 0 {
        caller_id
    } else {
        args.arg0_u32()
    };
    if target_id == INVALID_TASK_ID {
        return errno(&ctx, ERRNO_ESRCH);
    }

    let target = task_find_by_id(target_id);
    if target.is_null() {
        return errno(&ctx, ERRNO_ESRCH);
    }

    if args.arg1 == 0 {
        return ctx.ok(0);
    }

    let Some(signum) = parse_signum(args.arg1) else {
        return errno(&ctx, ERRNO_EINVAL);
    };

    if signum == SIGKILL {
        if task_terminate(target_id) != 0 {
            return errno(&ctx, ERRNO_ESRCH);
        }
        if target_id == caller_id {
            clear_scheduler_current_task();
            schedule();
            return SyscallDisposition::NoReturn;
        }
        return ctx.ok(0);
    }

    unsafe {
        (*target)
            .signal_pending
            .fetch_or(sig_bit(signum), Ordering::AcqRel);
    }
    let _ = unblock_task(target);
    ctx.ok(0)
}

fn read_signal_frame(rsp: u64) -> Option<SignalFrame> {
    let ptr = UserPtr::<SignalFrame>::try_new(rsp).ok()?;
    copy_from_user(ptr).ok()
}

pub fn syscall_rt_sigreturn(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, ERRNO_EINVAL);
    };

    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return errno(&ctx, ERRNO_EINVAL),
    };

    let rsp = unsafe { (*frame).rsp };
    let sigframe = match read_signal_frame(rsp).or_else(|| read_signal_frame(rsp.wrapping_sub(8))) {
        Some(sf) => sf,
        None => return errno(&ctx, ERRNO_EFAULT),
    };

    task_ref.signal_blocked = sigframe.saved_mask & !SIG_UNCATCHABLE;

    unsafe {
        (*frame).rax = sigframe.rax;
        (*frame).rbx = sigframe.rbx;
        (*frame).rcx = sigframe.rcx;
        (*frame).rdx = sigframe.rdx;
        (*frame).rsi = sigframe.rsi;
        (*frame).rdi = sigframe.rdi;
        (*frame).rbp = sigframe.rbp;
        (*frame).rsp = sigframe.rsp;
        (*frame).r8 = sigframe.r8;
        (*frame).r9 = sigframe.r9;
        (*frame).r10 = sigframe.r10;
        (*frame).r11 = sigframe.r11;
        (*frame).r12 = sigframe.r12;
        (*frame).r13 = sigframe.r13;
        (*frame).r14 = sigframe.r14;
        (*frame).r15 = sigframe.r15;
        (*frame).rip = sigframe.rip;
        (*frame).rflags = sigframe.rflags;
    }

    ctx.ok(0)
}

pub fn deliver_pending_signal(task: *mut Task, frame: *mut InterruptFrame) {
    if task.is_null() || frame.is_null() {
        return;
    }

    unsafe {
        if ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            return;
        }

        let pending = (*task).signal_pending.load(Ordering::Acquire);
        let deliverable = pending & !(*task).signal_blocked;
        if deliverable == 0 {
            return;
        }

        let signum = (deliverable.trailing_zeros() + 1) as u8;
        let bit = sig_bit(signum);
        (*task).signal_pending.fetch_and(!bit, Ordering::AcqRel);

        let action = (*task).signal_actions[(signum - 1) as usize];
        if action.handler == SIG_IGN {
            return;
        }

        if action.handler == SIG_DFL {
            match sig_default_action(signum) {
                SigDefault::Ignore | SigDefault::Stop | SigDefault::Continue => return,
                SigDefault::Terminate => {
                    let task_id = (*task).task_id;
                    (*task).exit_reason = TaskExitReason::Normal;
                    (*task).fault_reason = TaskFaultReason::None;
                    (*task).exit_code = 128 + signum as u32;
                    if task_terminate(task_id) == 0 {
                        clear_scheduler_current_task();
                        schedule();
                    }
                    return;
                }
            }
        }

        if action.restorer == 0 {
            return;
        }

        let frame_addr = ((*frame)
            .rsp
            .wrapping_sub(core::mem::size_of::<SignalFrame>() as u64))
            & !0xF;
        let sigframe_ptr = match UserPtr::<SignalFrame>::try_new(frame_addr) {
            Ok(p) => p,
            Err(_) => return,
        };

        let saved_mask = (*task).signal_blocked;
        let sigframe = SignalFrame {
            restorer: action.restorer,
            signum: signum as u64,
            rax: (*frame).rax,
            rbx: (*frame).rbx,
            rcx: (*frame).rcx,
            rdx: (*frame).rdx,
            rsi: (*frame).rsi,
            rdi: (*frame).rdi,
            rbp: (*frame).rbp,
            rsp: (*frame).rsp,
            r8: (*frame).r8,
            r9: (*frame).r9,
            r10: (*frame).r10,
            r11: (*frame).r11,
            r12: (*frame).r12,
            r13: (*frame).r13,
            r14: (*frame).r14,
            r15: (*frame).r15,
            rip: (*frame).rip,
            rflags: (*frame).rflags,
            saved_mask,
        };

        if copy_to_user(sigframe_ptr, &sigframe).is_err() {
            return;
        }

        let mut blocked = saved_mask | action.mask;
        if (action.flags & SA_NODEFER) == 0 {
            blocked |= bit;
        }
        (*task).signal_blocked = blocked & !SIG_UNCATCHABLE;

        (*frame).rsp = frame_addr;
        (*frame).rip = action.handler;
        (*frame).rdi = signum as u64;
        (*frame).rsi = 0;
        (*frame).rdx = 0;
    }
}
