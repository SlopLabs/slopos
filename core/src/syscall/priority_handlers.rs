//! `getpriority`/`setpriority`: the nice value of a process, a process group
//! or every process of a user. The value is recorded and reported; the
//! scheduler places a task by the tier it was spawned at.

use slopos_abi::Errno;
use slopos_abi::syscall::{PRIO_PGRP, PRIO_PROCESS, PRIO_USER};
use slopos_abi::task::TASK_FLAG_SYSTEM;
use slopos_sched::task::task_for_each_active;
use slopos_sched::task_struct::Task;

use crate::syscall::signal::{sender_pid, signal_dominates, signal_is_init, signal_may_name};

const NICE_MIN: i64 = -20;
const NICE_MAX: i64 = 19;

/// Which tasks a `(which, who)` pair names, resolved against the caller.
enum Selector {
    Process(u32),
    Group(u32),
    Everyone,
}

impl Selector {
    fn resolve(caller: &Task, which: u64, who: u64) -> Result<Self, Errno> {
        let who = u32::try_from(who).map_err(|_| Errno::ESRCH)?;
        match which {
            PRIO_PROCESS => Ok(Self::Process(if who == 0 {
                sender_pid(caller)
            } else {
                who
            })),
            PRIO_PGRP => Ok(Self::Group(if who == 0 { caller.pgid() } else { who })),
            // Every process runs as user 0.
            PRIO_USER if who == 0 => Ok(Self::Everyone),
            PRIO_USER => Err(Errno::ESRCH),
            _ => Err(Errno::EINVAL),
        }
    }

    fn names(&self, task: &Task) -> bool {
        signal_may_name(task.flags)
            && match *self {
                Self::Process(pid) => sender_pid(task) == pid,
                Self::Group(pgid) => task.pgid() == pgid,
                Self::Everyone => true,
            }
    }
}

// Linux's encoding: `20 - nice`, so the highest priority is the largest
// return and no success reads as an error.
define_syscall!(syscall_getpriority (ctx, which: u64, who: u64) cap(NoneRelation)
    -> Result<u64, Errno> {
    let selector = Selector::resolve(ctx.task(), which, who)?;
    let mut lowest: Option<i8> = None;
    task_for_each_active(|task| {
        if selector.names(task) {
            lowest = Some(lowest.map_or(task.nice(), |nice| nice.min(task.nice())));
        }
    });
    lowest
        .map(|nice| (20 - i64::from(nice)) as u64)
        .ok_or(Errno::ESRCH)
});

// A task's nice value is for whoever may signal it, and init's for init alone;
// one below both zero and the task's own also takes `TASK_FLAG_SYSTEM`.
define_syscall!(syscall_setpriority (ctx, which: u64, who: u64, value: i64) cap(NoneRelation)
    -> Result<(), Errno> {
    let caller = ctx.task();
    let selector = Selector::resolve(caller, which, who)?;
    let nice = value.clamp(NICE_MIN, NICE_MAX) as i8;
    let privileged = caller.flags & TASK_FLAG_SYSTEM != 0;
    let own = sender_pid(caller);
    let (mut found, mut denied, mut refused) = (false, false, false);
    task_for_each_active(|task| {
        if !selector.names(task) {
            return;
        }
        found = true;
        let target = sender_pid(task);
        if target != own
            && (signal_is_init(target) || !signal_dominates(caller.flags, task.flags))
        {
            denied = true;
        } else if nice < task.nice().min(0) && !privileged {
            refused = true;
        } else {
            task.set_nice(nice);
        }
    });
    match (found, denied, refused) {
        (false, _, _) => Err(Errno::ESRCH),
        (_, true, _) => Err(Errno::EPERM),
        (_, _, true) => Err(Errno::EACCES),
        _ => Ok(()),
    }
});
