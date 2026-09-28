//! The out-of-memory killer.
//!
//! `mmap`, `brk`, `mprotect` and `exec` are charged when they are made, so a
//! page can be missing only where it is charged as it is written: a forked
//! copy, stack growth, `MAP_NORESERVE`. When such a write finds the commit
//! ceiling refusing its page, or the buddy empty after reclaim, the writer is
//! not faulted for it. A process is killed instead and the writer waits for
//! that process's address space to be torn down, then writes again. That is
//! the behaviour Linux documents for its OOM killer, down to its two guards:
//! while a victim is still dying nobody picks a second, and a victim that
//! frees nothing within [`VICTIM_GRACE_MS`] stops holding the others back.
//!
//! The victim is the process whose own account owes the most committed pages
//! — its private mappings, the forked pages it holds as its own, and every
//! memfd it sized, mapped or not. A shared page is charged once, to whoever
//! sized it, so mapping another process's memory never makes the mapper the
//! victim. Among them, the processes the writer could `kill` come first; one
//! holding privileged flags the writer lacks is taken only when none of those
//! owes anything, and init, like a kernel task, which has no address space,
//! never is.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::quota::ResourceKind;
use slopos_abi::task::{INVALID_TASK_ID, TASK_NAME_MAX_LEN};
use slopos_ostd::handle::Handle;
use slopos_ostd::mm::KArc;
use slopos_ostd::process::Process;
use slopos_ostd::process::quota::held_by;
use slopos_ostd::sync::wait_queue::current_task_is_killed;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, SpinLock, WaitAbort, WaitQueue};
use slopos_ostd::{klog_warn, lock_class};

use crate::process_vm::{ProcessVm, for_each_bound, process_vm_released};

/// What a write found missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OomTrigger {
    /// The commit ceiling refused the page's charge.
    Commit,
    /// The buddy had no frame, even after reclaim.
    Frames,
}

impl OomTrigger {
    fn describe(self) -> &'static str {
        match self {
            Self::Commit => "the commit ceiling refused a page",
            Self::Frames => "no free frame after reclaim",
        }
    }
}

/// Where a process stands with the killer, as seen from the writer it serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Never a victim: init.
    Exempt,
    /// Every task already killed or gone: a second kill frees nothing sooner.
    Dying,
    /// Holds privileged flags the writer lacks, which `kill` would refuse it:
    /// taken only when no [`Killable`](Self::Killable) process owes anything.
    Shielded,
    Killable,
}

/// A kill that reached a live task: the victim's pid as `getpid` reports it,
/// and its name, for the log.
pub struct Killed {
    pub pid: u32,
    pub name: [u8; TASK_NAME_MAX_LEN],
}

/// The part of the killer that needs tasks, which `mm` sits below.
pub trait OomOps: Sync {
    /// Where `process` stands with the writer the killer runs for — the task
    /// whose write found no page, which is the one running.
    fn standing(&self, process: &Process) -> Standing;

    /// Kill every task of `process` the way `SIGKILL` does: a flag each one
    /// unwinds from. `None` when none of them was live.
    fn kill(&self, process: &Process) -> Option<Killed>;
}

static OOM_OPS: SpinLock<Option<&'static dyn OomOps>> =
    SpinLock::new(None, lock_class!("OOM_OPS", LOCK_LEVEL_RESOURCE));

/// Publish the task side of the killer. Called once, from boot.
pub fn oom_register_ops(ops: &'static dyn OomOps) {
    *OOM_OPS.lock() = Some(ops);
}

/// Swap the task side, answering the previous one, so a test can decide who
/// is init and who is dying.
#[cfg(feature = "test-hooks")]
pub fn oom_swap_ops(ops: Option<&'static dyn OomOps>) -> Option<&'static dyn OomOps> {
    core::mem::replace(&mut *OOM_OPS.lock(), ops)
}

fn ops() -> Option<&'static dyn OomOps> {
    *OOM_OPS.lock()
}

/// How long a victim may hold its memory after the kill before it stops
/// holding back the choice of another.
pub const VICTIM_GRACE_MS: u64 = 5000;

/// The longest one faulting write waits before it tries again.
const RELEASE_WAIT_MS: u64 = 1000;

/// Pages reclaim is asked for before a frame shortage costs a process.
const RECLAIM_BEFORE_KILL: u32 = 32;

#[derive(Clone, Copy)]
struct Victim {
    vm: Handle<ProcessVm>,
    pid: u32,
    killed_ms: u64,
    /// When it stops holding back the choice of another.
    grace_ends_ms: u64,
}

/// The victim still dying, if any. A sleeping lock, because deciding walks
/// every address space and reaches the task registry to kill.
static VICTIM: Mutex<Option<Victim>> =
    Mutex::new(None, lock_class!("OOM_VICTIM", LOCK_LEVEL_RESOURCE));

static RELEASED: WaitQueue = WaitQueue::new(lock_class!("OOM_RELEASED", LOCK_LEVEL_RESOURCE));

static KILLS: AtomicU32 = AtomicU32::new(0);
static LAST_VICTIM: AtomicU32 = AtomicU32::new(INVALID_TASK_ID);

/// What the killer has done since boot, for `sys_info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OomStats {
    pub kills: u32,
    /// The pid of the last victim, [`INVALID_TASK_ID`] before the first.
    pub last_victim: u32,
}

pub fn oom_stats() -> OomStats {
    OomStats {
        kills: KILLS.load(Ordering::Acquire),
        last_victim: LAST_VICTIM.load(Ordering::Acquire),
    }
}

/// An address space's frames are back: whoever waits for a victim's memory
/// looks again.
pub fn note_released() {
    RELEASED.wake_all();
}

/// What became of a write that found no page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OomVerdict {
    /// Write again: memory was freed, a victim died or is dying, or the writer
    /// is itself killed and unwinds before it gets there.
    Retry,
    /// Nothing may be killed — only init, the dying and processes owing no
    /// committed page are left — so the write cannot be served.
    Unresolved,
}

/// Serve a user write that found no page. Blocks, so the caller must hold no
/// lock and may sleep; the address space the write faulted in is not held.
pub fn out_of_memory(trigger: OomTrigger) -> OomVerdict {
    if current_task_is_killed() {
        return OomVerdict::Retry;
    }
    if trigger == OomTrigger::Frames && crate::reclaim_pages(RECLAIM_BEFORE_KILL) != 0 {
        return OomVerdict::Retry;
    }
    let Some(ops) = ops() else {
        return OomVerdict::Unresolved;
    };
    let awaited = match decide(ops, trigger) {
        Decision::Await(vm) => vm,
        Decision::Abandoned => return OomVerdict::Retry,
        Decision::NoVictim => return OomVerdict::Unresolved,
    };
    // Bounded, and killable: a victim is woken out of this by its own kill.
    match RELEASED.wait_event_timeout(|| process_vm_released(awaited), RELEASE_WAIT_MS) {
        // The frames are back, or a round is spent: the write looks again, and
        // a round's end is what lets a victim outstay its grace.
        Ok(()) | Err(WaitAbort::Timeout) => OomVerdict::Retry,
        // The writer unwinds on the way back to the write.
        Err(WaitAbort::Killed) => OomVerdict::Retry,
        // Nothing to park: the write retries rather than failing a program
        // for memory the victim is still returning.
        Err(WaitAbort::NoRuntime | WaitAbort::Interrupted) => OomVerdict::Retry,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Wait for this address space to go.
    Await(Handle<ProcessVm>),
    /// The caller was killed while it waited to decide.
    Abandoned,
    NoVictim,
}

/// Keep the victim still dying, or kill the next one. Never waits for memory.
pub(crate) fn decide(ops: &dyn OomOps, trigger: OomTrigger) -> Decision {
    let Ok(mut current) = VICTIM.lock() else {
        return Decision::Abandoned;
    };
    let now = slopos_kernel_services::clock::uptime_ms();
    if let Some(victim) = *current {
        if !process_vm_released(victim.vm) {
            if now < victim.grace_ends_ms {
                return Decision::Await(victim.vm);
            }
            // Its tasks are killed, so the choice below passes it over.
            report_stuck_victim(victim.pid, now.saturating_sub(victim.killed_ms));
        }
        *current = None;
    }
    let Some(chosen) = choose_victim(ops) else {
        report_no_victim(trigger);
        return Decision::NoVictim;
    };
    let pid = match ops.kill(&chosen.process) {
        Some(killed) => {
            KILLS.fetch_add(1, Ordering::AcqRel);
            LAST_VICTIM.store(killed.pid, Ordering::Release);
            report_kill(&killed, &chosen, trigger);
            killed.pid
        }
        // Its last task left between the choice and the kill. Nothing was
        // killed, but its memory is on its way back all the same, so it holds
        // the choice exactly as a victim would.
        None => chosen.process.id(),
    };
    *current = Some(Victim {
        vm: chosen.vm,
        pid,
        killed_ms: now,
        grace_ends_ms: now.saturating_add(VICTIM_GRACE_MS),
    });
    Decision::Await(chosen.vm)
}

/// [`decide`] for a test outside `mm`: the address space the writer would
/// wait for, `None` for anything else.
#[cfg(feature = "test-hooks")]
pub fn oom_decide_for_test(ops: &dyn OomOps, trigger: OomTrigger) -> Option<Handle<ProcessVm>> {
    match decide(ops, trigger) {
        Decision::Await(vm) => Some(vm),
        Decision::Abandoned | Decision::NoVictim => None,
    }
}

/// The address space [`decide`] would take next, with nothing killed.
#[cfg(feature = "test-hooks")]
pub fn oom_choose_for_test(ops: &dyn OomOps) -> Option<Handle<ProcessVm>> {
    choose_victim(ops).map(|candidate| candidate.vm)
}

/// Forget the victim being waited for, so a test starts from none.
#[cfg(feature = "test-hooks")]
pub fn oom_forget_victim_for_test() {
    if let Ok(mut current) = VICTIM.lock() {
        *current = None;
    }
}

/// End the victim's grace now, as if it had held its memory that long.
#[cfg(feature = "test-hooks")]
pub fn oom_expire_victim_for_test() {
    if let Ok(mut current) = VICTIM.lock()
        && let Some(victim) = current.as_mut()
    {
        victim.grace_ends_ms = 0;
    }
}

/// A process the killer may take, with what its own account owes.
pub(crate) struct Candidate {
    pub vm: Handle<ProcessVm>,
    pub process: KArc<Process>,
    pub owed: u32,
    pub standing: Standing,
}

/// The process whose own account owes the most committed pages among those
/// the writer may kill or, only when none of those owes any, among those
/// shielded from it. A tie goes to the one met first, and a process owing
/// nothing is never worth a kill: killing it would free no promise.
pub(crate) fn choose_victim(ops: &dyn OomOps) -> Option<Candidate> {
    let mut killable: Option<Candidate> = None;
    let mut shielded: Option<Candidate> = None;
    for_each_bound(|vm, process| {
        let owed = held_by(process.account(), ResourceKind::CommitPages);
        let outranks = |best: &Option<Candidate>| best.as_ref().is_none_or(|b| owed > b.owed);
        if owed == 0 || !outranks(&killable) {
            return;
        }
        let standing = ops.standing(process);
        let best = match standing {
            Standing::Killable => &mut killable,
            Standing::Shielded if killable.is_none() && outranks(&shielded) => &mut shielded,
            _ => return,
        };
        *best = Some(Candidate {
            vm,
            process: process.clone(),
            owed,
            standing,
        });
    });
    killable.or(shielded)
}

/// Out of line and `#[cold]`: `format_args!` builds its argument array in the
/// caller's frame, which is measured against the 2 KiB stack gate.
#[cold]
#[inline(never)]
fn report_kill(killed: &Killed, chosen: &Candidate, trigger: OomTrigger) {
    let len = killed
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(killed.name.len());
    let resort = if chosen.standing == Standing::Shielded {
        " (privileged: nothing the writer may signal owed a page)"
    } else {
        ""
    };
    klog_warn!(
        "OOM: killed pid {} ('{}') owing {} committed pages{}: {}",
        killed.pid,
        core::str::from_utf8(&killed.name[..len]).unwrap_or("?"),
        chosen.owed,
        resort,
        trigger.describe()
    );
}

#[cold]
#[inline(never)]
fn report_stuck_victim(pid: u32, held_ms: u64) {
    klog_warn!(
        "OOM: pid {} still holds its memory {} ms after the kill; choosing another victim",
        pid,
        held_ms
    );
}

#[cold]
#[inline(never)]
fn report_no_victim(trigger: OomTrigger) {
    klog_warn!(
        "OOM: {} and nothing is left to kill; the write fails",
        trigger.describe()
    );
}
