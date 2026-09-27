//! The OOM killer's choice: the killable process holding the most resident
//! pages, one victim at a time, and another once a victim outstays its grace.
//!
//! The task side is swapped for one that names exactly the processes a test
//! made, so nothing else bound at the time can be chosen.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use slopos_abi::task::{INVALID_PROCESS_ID, TASK_NAME_MAX_LEN};
use slopos_ostd::handle::Handle;
use slopos_ostd::process::Process;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use super::tests::resolve_pid;
use crate::oom::{
    Decision, Killed, OomOps, OomTrigger, Standing, choose_victim, decide,
    oom_expire_victim_for_test, oom_forget_victim_for_test, oom_swap_ops,
};
use crate::page_fault::{FaultOutcome, try_resolve_user_fault};
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm::{
    ProcessVm, create_process_vm, destroy_process_vm, pack_process_vm_handle, process_vm_handle,
    process_vm_mmap, process_vm_released, unbind_process_vm,
};

const MADE: usize = 3;

static MADE_PIDS: [AtomicU32; MADE] = [const { AtomicU32::new(INVALID_PROCESS_ID) }; MADE];
static EXEMPT_PID: AtomicU32 = AtomicU32::new(INVALID_PROCESS_ID);
static DYING_MASK: AtomicU32 = AtomicU32::new(0);
static KILL_CALLS: AtomicU32 = AtomicU32::new(0);
/// Made processes whose last task leaves just before the kill reaches it.
static GONE_MASK: AtomicU32 = AtomicU32::new(0);

fn made_index(pid: u32) -> Option<usize> {
    MADE_PIDS
        .iter()
        .position(|slot| slot.load(Ordering::Acquire) == pid)
}

/// Stands in for the task side: a kill marks its process dying, as a real
/// one marks every task.
struct MadeOnly;

impl OomOps for MadeOnly {
    fn standing(&self, process: &Process) -> Standing {
        let Some(index) = made_index(process.id()) else {
            return Standing::Exempt;
        };
        if EXEMPT_PID.load(Ordering::Acquire) == process.id() {
            Standing::Exempt
        } else if DYING_MASK.load(Ordering::Acquire) & (1 << index) != 0 {
            Standing::Dying
        } else {
            Standing::Killable
        }
    }

    fn kill(&self, process: &Process) -> Option<Killed> {
        let index = made_index(process.id())?;
        DYING_MASK.fetch_or(1 << index, Ordering::AcqRel);
        if GONE_MASK.load(Ordering::Acquire) & (1 << index) != 0 {
            return None;
        }
        KILL_CALLS.fetch_add(1, Ordering::AcqRel);
        let mut name = [0u8; TASK_NAME_MAX_LEN];
        name[..8].copy_from_slice(b"oom-test");
        Some(Killed {
            pid: process.id(),
            name,
        })
    }
}

static MADE_ONLY: MadeOnly = MadeOnly;

/// Three address spaces, each holding more resident pages than the last,
/// with the killer's task side swapped for [`MadeOnly`] until drop.
struct Ladder {
    pids: [u32; MADE],
    restore: Option<&'static dyn OomOps>,
}

impl Ladder {
    fn new() -> Option<Self> {
        oom_forget_victim_for_test();
        EXEMPT_PID.store(INVALID_PROCESS_ID, Ordering::Release);
        DYING_MASK.store(0, Ordering::Release);
        KILL_CALLS.store(0, Ordering::Release);
        GONE_MASK.store(0, Ordering::Release);
        let mut ladder = Self {
            pids: [INVALID_PROCESS_ID; MADE],
            restore: oom_swap_ops(Some(&MADE_ONLY)),
        };
        for (rung, extra) in [16u64, 48, 96].into_iter().enumerate() {
            let pid = create_process_vm();
            if pid == INVALID_PROCESS_ID {
                return None;
            }
            ladder.pids[rung] = pid;
            MADE_PIDS[rung].store(pid, Ordering::Release);
            if !touch_fresh_pages(pid, extra) {
                return None;
            }
        }
        Some(ladder)
    }

    fn vm(&self, rung: usize) -> Option<Handle<ProcessVm>> {
        process_vm_handle(resolve_pid(self.pids[rung]))
    }
}

impl Drop for Ladder {
    fn drop(&mut self) {
        for (rung, pid) in self.pids.iter().enumerate() {
            MADE_PIDS[rung].store(INVALID_PROCESS_ID, Ordering::Release);
            if *pid != INVALID_PROCESS_ID {
                destroy_process_vm(resolve_pid(*pid));
            }
        }
        oom_forget_victim_for_test();
        oom_swap_ops(self.restore);
    }
}

/// Map `pages` fresh pages into `pid` and write every one, as its owner would.
fn touch_fresh_pages(pid: u32, pages: u64) -> bool {
    let process = resolve_pid(pid);
    let addr = process_vm_mmap(
        process,
        0,
        pages * PAGE_SIZE_4KB,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE,
        -1,
        0,
    );
    let Some(handle) = process_vm_handle(process) else {
        return false;
    };
    let packed = pack_process_vm_handle(handle);
    addr != 0
        && (0..pages).all(|page| {
            // 0x06: a user write to an absent page.
            try_resolve_user_fault(addr + page * PAGE_SIZE_4KB, 0x06, packed, 1)
                == FaultOutcome::Resolved
        })
}

fn chosen_pid() -> Option<u32> {
    choose_victim(&MADE_ONLY).map(|candidate| candidate.process.id())
}

/// The largest killable process is the victim; init, however large, never
/// is, and neither is one already dying.
pub fn test_oom_takes_the_largest_killable_process() -> TestResult {
    let Some(ladder) = Ladder::new() else {
        return fail!("could not build three address spaces");
    };
    let [small, middle, large] = ladder.pids;

    let first = chosen_pid();
    EXEMPT_PID.store(large, Ordering::Release);
    let beside_init = chosen_pid();
    DYING_MASK.fetch_or(1 << 1, Ordering::AcqRel);
    let beside_the_dying = chosen_pid();
    DYING_MASK.fetch_or(1 << 0, Ordering::AcqRel);
    let none_left = chosen_pid();
    drop(ladder);

    assert_test!(
        first == Some(large),
        "chose {:?}, want the largest, {}",
        first,
        large
    );
    assert_test!(
        beside_init == Some(middle),
        "with the largest exempt as init, chose {:?}, want {}",
        beside_init,
        middle
    );
    assert_test!(
        beside_the_dying == Some(small),
        "with the next one dying, chose {:?}, want {}",
        beside_the_dying,
        small
    );
    assert_test!(
        none_left.is_none(),
        "with only init and the dying left, chose {:?}",
        none_left
    );
    pass!()
}

/// While a victim is still dying nobody picks a second; once it is gone, or
/// once it has held its memory past its grace, the next largest goes.
pub fn test_oom_waits_for_one_victim_at_a_time() -> TestResult {
    let Some(mut ladder) = Ladder::new() else {
        return fail!("could not build three address spaces");
    };
    let (Some(small), Some(middle), Some(large)) = (ladder.vm(0), ladder.vm(1), ladder.vm(2))
    else {
        return fail!("a made address space has no handle");
    };

    let first = decide(&MADE_ONLY, OomTrigger::Commit);
    let again = decide(&MADE_ONLY, OomTrigger::Commit);
    let kills_while_dying = KILL_CALLS.load(Ordering::Acquire);

    destroy_process_vm(resolve_pid(ladder.pids[2]));
    ladder.pids[2] = INVALID_PROCESS_ID;
    let after_release = decide(&MADE_ONLY, OomTrigger::Commit);

    oom_expire_victim_for_test();
    let after_grace = decide(&MADE_ONLY, OomTrigger::Frames);
    let kills = KILL_CALLS.load(Ordering::Acquire);
    drop(ladder);

    assert_test!(
        first == Decision::Await(large),
        "the first shortage decided {:?}, want the largest",
        first
    );
    assert_test!(
        again == Decision::Await(large) && kills_while_dying == 1,
        "a second shortage while the victim was dying decided {:?} after {} kills",
        again,
        kills_while_dying
    );
    assert_test!(
        after_release == Decision::Await(middle),
        "once the victim was gone, decided {:?}, want the next largest",
        after_release
    );
    assert_test!(
        after_grace == Decision::Await(small),
        "a victim past its grace still held the choice: decided {:?}",
        after_grace
    );
    assert_test!(kills == 3, "{} kills, want one per victim", kills);
    pass!()
}

/// A victim unbound from its slot still holds the choice until the frames
/// it held are back, and only then does the next largest go.
pub fn test_oom_victim_holds_the_choice_until_its_frames_are_back() -> TestResult {
    let Some(mut ladder) = Ladder::new() else {
        return fail!("could not build three address spaces");
    };
    let (Some(middle), Some(large)) = (ladder.vm(1), ladder.vm(2)) else {
        return fail!("a made address space has no handle");
    };

    let first = decide(&MADE_ONLY, OomTrigger::Frames);
    let unbound = unbind_process_vm(resolve_pid(ladder.pids[2]));
    ladder.pids[2] = INVALID_PROCESS_ID;
    let released_while_returning = process_vm_released(large);
    let while_returning = decide(&MADE_ONLY, OomTrigger::Frames);
    let unbound_at_all = unbound.is_some();
    drop(unbound);
    let released_after = process_vm_released(large);
    let after = decide(&MADE_ONLY, OomTrigger::Frames);
    drop(ladder);

    assert_test!(unbound_at_all, "the victim's slot would not unbind");
    assert_test!(
        first == Decision::Await(large),
        "the first shortage decided {:?}, want the largest",
        first
    );
    assert_test!(
        !released_while_returning && while_returning == Decision::Await(large),
        "with the victim's frames still out, it counted released: {}, and the \
         killer decided {:?}",
        released_while_returning,
        while_returning
    );
    assert_test!(
        released_after && after == Decision::Await(middle),
        "with the frames back, released: {}, decided {:?}, want the next largest",
        released_after,
        after
    );
    pass!()
}

/// A victim whose last task left before the kill reached it holds the choice
/// as a killed one does, and counts as no kill.
pub fn test_oom_victim_with_no_task_left_still_holds_the_choice() -> TestResult {
    let Some(ladder) = Ladder::new() else {
        return fail!("could not build three address spaces");
    };
    let Some(large) = ladder.vm(2) else {
        return fail!("a made address space has no handle");
    };
    GONE_MASK.store(1 << 2, Ordering::Release);
    let kills_before = crate::oom::oom_stats().kills;

    let first = decide(&MADE_ONLY, OomTrigger::Commit);
    let again = decide(&MADE_ONLY, OomTrigger::Commit);
    let kills = KILL_CALLS.load(Ordering::Acquire);
    let counted = crate::oom::oom_stats().kills - kills_before;
    drop(ladder);

    assert_test!(
        first == Decision::Await(large) && again == Decision::Await(large),
        "decided {:?} and then {:?}, want the taskless victim both times",
        first,
        again
    );
    assert_test!(
        kills == 0 && counted == 0,
        "{} kills reached a task and {} were counted, want none",
        kills,
        counted
    );
    pass!()
}

slopos_testing::stest!(
    name = test_oom_takes_the_largest_killable_process,
    suite = oom_killer
);
slopos_testing::stest!(
    name = test_oom_waits_for_one_victim_at_a_time,
    suite = oom_killer
);
slopos_testing::stest!(
    name = test_oom_victim_holds_the_choice_until_its_frames_are_back,
    suite = oom_killer
);
slopos_testing::stest!(
    name = test_oom_victim_with_no_task_left_still_holds_the_choice,
    suite = oom_killer
);
