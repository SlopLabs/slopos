use slopos_abi::Errno;
use slopos_abi::task::TASK_FLAG_USER_MODE;
use slopos_ostd::user::context::UserContext;
use slopos_sched::task_struct::{Current, Task};

use slopos_ostd::authority::{AuthorityDecision, decide};

use crate::syscall::common::SyscallEntry;
use crate::syscall::context::SyscallContext;
use crate::syscall::handlers::syscall_lookup;
use crate::syscall::result::SyscallResult;
use crate::syscall::signal::Delivered;

/// Whether `task` may invoke the operation `entry` classifies.
///
/// Returns `true` for every ungated operation without reading anything, so the
/// common path is one compare on a byte already in cache.
///
/// The failure mode is split by kind, deliberately: invoking an operation your
/// authority does not name is a *program bug* and is loud, because a program
/// that asks for something it can never have wants to be told. Acting on an
/// object you were not given is an ordinary `EPERM` and is silent — that one is
/// an attacker probing, and a log line per probe is the denial-of-service.
#[inline]
fn authorize(task: &Task, entry: &SyscallEntry, sysno: u64) -> bool {
    if !entry.cap.is_gated() {
        return true;
    }
    match decide(task_caps(task), entry.cap) {
        AuthorityDecision::Allow => true,
        AuthorityDecision::WarnAndAllow => {
            // Also userland-driven: a loop on a syscall it lacks the
            // capability for would otherwise drive the log at line rate.
            slopos_ostd::klog_warn_ratelimited!(
                "AUTHORITY: task {} invoked syscall {} without {} (authority=warn)",
                task.task_id,
                sysno,
                entry.cap.name(),
            );
            true
        }
        AuthorityDecision::Deny => false,
    }
}

/// The caller's effective capability mask.
///
/// Derived from `task.flags` until the `Cred` lands: the flags *are* the whole
/// of today's privilege model, so deriving keeps one source of truth rather
/// than adding a second that could disagree with it. The `Process`-owned
/// `Cred` replaces this body without moving the call site.
#[inline]
fn task_caps(task: &Task) -> u64 {
    slopos_ostd::task::ops::task_caps(task)
}

pub fn syscall_handle(user_ctx: &UserContext) {
    let sysno = user_ctx.rax();

    let Some(current) = slopos_sched::task_struct::Current::get() else {
        return;
    };
    let task = current.task();
    if (task.flags & TASK_FLAG_USER_MODE) == 0 {
        return;
    }

    // A handler that forgets to write a return value must not leak stale
    // register contents to userland.
    user_ctx.set_rax(slopos_abi::syscall::ERRNO_EINVAL as u64);

    let entry = syscall_lookup(sysno);
    let handler = entry.and_then(|e| e.handler);
    let mut restartable = None;

    match handler {
        Some(func) => {
            // Here, not in the handler: the entry is already in cache, so the
            // check is one compare beside the pointer about to be called — and
            // a handler cannot forget what it does not perform.
            if let Some(entry) = entry
                && !authorize(task, entry, sysno)
            {
                user_ctx.set_rax(Errno::EPERM.as_u64());
                crate::syscall::signal::deliver_pending_signal(&current, user_ctx);
                return;
            }
            let ctx = SyscallContext::from_current(&current, user_ctx);
            let began = slopos_sched::profile::stamp();
            if began != 0 && sysno == slopos_abi::syscall::SYSCALL_FUTEX {
                slopos_sched::profile::note_futex(user_ctx.rdi(), user_ctx.rsi());
            }
            let result = func(&ctx);
            slopos_sched::profile::note_syscall(sysno, began);
            restartable = match result {
                SyscallResult::Err(e @ (Errno::ERESTARTSYS | Errno::ERESTARTNOHAND)) => Some(e),
                _ => None,
            };
            ctx.write_result(result);
        }
        None => {
            if entry.is_none() {
                // Userland picks the syscall number, so this site is a
                // one-line loop away from monopolising the log lock.
                slopos_ostd::klog_warn_ratelimited!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
            }
            user_ctx.set_rax(slopos_abi::syscall::ENOSYS_RETURN);
        }
    }

    return_from_syscall(&current, user_ctx, sysno, restartable);
}

/// Deliver what is pending on `sysno`'s way out, settling an `ERESTARTSYS` or
/// `ERESTARTNOHAND` it returned on what the delivery did.
///
/// `restartable` is what the handler itself returned, not what `rax` reads:
/// `rt_sigreturn` may restore a user `rax` of that value, which Linux keeps
/// from rewinding with `orig_ax = -1`.
pub(crate) fn return_from_syscall(
    current: &Current,
    user_ctx: &UserContext,
    sysno: u64,
    restartable: Option<Errno>,
) {
    crate::syscall::signal::deliver_pending_signal_on_syscall_exit(
        current,
        user_ctx,
        |delivered| {
            if let Some(kind) = restartable {
                settle_restart(user_ctx, sysno, kind, delivered);
            }
        },
    );
}

/// The x86_64 `syscall` instruction is 2 bytes (`0F 05`), so rewinding
/// `frame.rip` by that points back at it for transparent re-execution.
const SYSCALL_INSN_SIZE: u64 = 2;

/// Syscalls that carry a caller-supplied timeout they do not hand back.
///
/// A restart re-arms the *original* timeout, so under signal pressure each
/// delivery starts a fresh full-length wait. These must report `EINTR`.
const TIMEOUT_BEARING: &[u64] = &[
    slopos_abi::syscall::SYSCALL_NANOSLEEP,
    slopos_abi::syscall::SYSCALL_POLL,
    slopos_abi::syscall::SYSCALL_FUTEX,
    slopos_abi::syscall::SYSCALL_RING_ENTER,
];

/// Runs before the signal frame is built, so the frame captures the rewound
/// state.
fn settle_restart(user_ctx: &UserContext, sysno: u64, kind: Errno, delivered: Delivered) {
    debug_assert_eq!(user_ctx.rax(), kind.as_u64());
    debug_assert!(
        !TIMEOUT_BEARING.contains(&sysno),
        "syscall {sysno} returned {kind:?} with a caller-supplied timeout; \
         it must return EINTR so the remaining time is not re-armed"
    );

    let restart = match (kind, delivered) {
        (_, Delivered::Nothing) => true,
        (Errno::ERESTARTNOHAND, Delivered::Handler { .. }) => false,
        (_, Delivered::Handler { restarting }) => restarting,
    };
    if restart {
        let mut regs = user_ctx.regs();
        regs.rip = regs.rip.wrapping_sub(SYSCALL_INSN_SIZE);
        regs.rax = sysno;
        user_ctx.set_regs(regs);
    } else {
        user_ctx.set_rax(Errno::EINTR.as_u64());
    }
}

/// Invoke a handler with a caller-built `UserContext`, bypassing ISR entry.
///
/// **Runs no authority check.** For a caller that has already made the
/// decision, or one invoking an operation classified as needing nothing. A
/// test reaching a gated handler through this proves the handler's own logic
/// and says nothing about whether the operation is reachable — use
/// [`dispatch_entry`] for that.
pub fn dispatch_handler(
    handler: crate::syscall::common::SyscallHandler,
    task: &slopos_sched::task::TaskRef,
    frame: &mut UserContext,
) -> SyscallResult {
    let ctx = SyscallContext::from_task_ref(task, frame);
    let result = handler(&ctx);
    ctx.write_result(result);
    result
}

/// Invoke a syscall through its table entry, applying the same authority
/// decision `syscall_handle` makes.
///
/// The difference from [`dispatch_handler`] is the whole point: the capability
/// check lives in the dispatcher, so a path that calls a handler directly is
/// *not* the syscall. Anything asserting what userland can reach must come
/// through here.
pub fn dispatch_entry(
    entry: &SyscallEntry,
    task: &slopos_sched::task::TaskRef,
    frame: &mut UserContext,
) -> SyscallResult {
    let Some(handler) = entry.handler else {
        return SyscallResult::Err(Errno::ENOSYS);
    };
    if !authorize(task, entry, u64::MAX) {
        let ctx = SyscallContext::from_task_ref(task, frame);
        let result = SyscallResult::Err(Errno::EPERM);
        ctx.write_result(result);
        return result;
    }
    dispatch_handler(handler, task, frame)
}
