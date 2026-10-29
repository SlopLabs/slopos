//! `/bin/endtask PID NAME USER` — privd's `org.slopos.task.end`, run as uid 0
//! holding `ProcSignal`, which waives both halves of the signal rule. It keeps
//! the half that guards the system: it ends only a task of a person's session
//! that runs as a person, holds no more than every program holds and still
//! runs under the `NAME` and `USER` the person was shown. A task's
//! capabilities only shrink, its login tag never changes and its id is never
//! given again, so what was checked is what the signal reaches.

use slopos_abi::caps::UNIVERSAL;
use slopos_abi::errno::Errno;
use slopos_abi::fs::PERSON_UIDS;
use slopos_abi::signal::SIGKILL;
use slopos_abi::task::{MAX_TASKS, SESSION_LOGINS};

use crate::syscall::error::SyscallError;
use crate::syscall::{UserTaskEntry, core as sys_core, process as sys_proc};

pub const ACTION: &str = "org.slopos.task.end";

/// Exit statuses beside 0, the task ended.
pub const GONE: i32 = 1;
pub const USAGE: i32 = 2;
pub const POWERFUL: i32 = 3;
pub const FAILED: i32 = 4;
pub const SYSTEM: i32 = 5;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Endable {
    /// Under the name it runs as and its user's, which a request names.
    Yes {
        name: String,
        user: String,
    },
    /// It holds a capability beyond those every program holds.
    Powerful,
    /// The system runs it: it is of no person's session, or runs as no person.
    System,
    Gone,
    /// What it holds, or whose it is, could not be read.
    Unknown,
}

/// What this program would make of task `pid`.
pub fn endable(pid: u32) -> Endable {
    let Some(task) = listed(pid) else {
        return Endable::Gone;
    };
    let held = match sys_core::caps_get(pid) {
        Ok(held) => held,
        // Still listed, it is a kernel task.
        Err(SyscallError::ESRCH) if listed(pid).is_some() => return Endable::System,
        Err(SyscallError::ESRCH) => return Endable::Gone,
        Err(_) => return Endable::Unknown,
    };
    if held & !UNIVERSAL != 0 {
        return Endable::Powerful;
    }
    if !PERSON_UIDS.contains(&task.uid) || !SESSION_LOGINS.contains(&task.login) {
        return Endable::System;
    }
    match crate::accounts::name_of(task.uid) {
        Some(user) => Endable::Yes {
            name: crate::apps::sysmon::task_name_string(&task),
            user,
        },
        None => Endable::Unknown,
    }
}

pub fn endtask_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Positive as `kill` reads it: past `i32::MAX` it would name a group.
    let (pid, name, user) = match args.as_slice() {
        [pid, name, user] => match pid.parse::<i32>() {
            Ok(pid) if pid > 0 => (pid as u32, name.as_str(), user.as_str()),
            _ => usage(),
        },
        _ => usage(),
    };
    crate::privd::elevate(
        Some(ACTION),
        &crate::privd::own_args(),
        crate::privd::Input::Closed,
    );
    sys_core::exit_with_code(end(pid, name, user))
}

fn usage() -> ! {
    eprintln!("usage: endtask PID NAME USER");
    sys_core::exit_with_code(USAGE)
}

fn end(pid: u32, name: &str, user: &str) -> i32 {
    match endable(pid) {
        Endable::Yes {
            name: now,
            user: whose,
        } if now == name && whose == user => {}
        Endable::Yes { .. } | Endable::Gone => {
            eprintln!("endtask: no task {pid} named {name} is running as {user}");
            return GONE;
        }
        Endable::Powerful => {
            eprintln!("endtask: task {pid} ({name}) holds capabilities, so it is not ended");
            return POWERFUL;
        }
        Endable::System => {
            eprintln!("endtask: task {pid} ({name}) is the system's, so it is not ended");
            return SYSTEM;
        }
        Endable::Unknown => {
            eprintln!("endtask: what task {pid} holds, or whose it is, could not be read");
            return FAILED;
        }
    }
    match sys_proc::kill(pid, SIGKILL) {
        0 => 0,
        rc if rc == Errno::ESRCH.raw() => GONE,
        rc => {
            let why = Errno::from_raw(rc).map_or("unknown error", Errno::description);
            eprintln!("endtask: task {pid}: {why}");
            FAILED
        }
    }
}

fn listed(pid: u32) -> Option<UserTaskEntry> {
    let mut tasks = vec![UserTaskEntry::default(); MAX_TASKS];
    let count = sys_core::process_list(&mut tasks).clamp(0, MAX_TASKS as i64) as usize;
    tasks[..count]
        .iter()
        .find(|task| task.task_id == pid)
        .copied()
}
