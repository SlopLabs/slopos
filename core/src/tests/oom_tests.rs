//! The task side of the OOM killer: init is never a victim, a process whose
//! tasks are all killed is already dying, a process the writer could not
//! signal is never taken, and a kill is the kill flag.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_abi::task::{
    INVALID_PROCESS_ID, INVALID_TASK_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_LAUNCH,
    TASK_FLAG_USER_MODE,
};
use slopos_mm::memfd::{memfd_create, memfd_ftruncate};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::oom::{
    Killed, OomOps, OomTrigger, OomVerdict, Standing, oom_choose_for_test, oom_decide_for_test,
    oom_forget_victim_for_test, oom_serve_for_test,
};
use slopos_mm::page_fault::{FaultOutcome, try_resolve_user_fault};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm::{
    ProcessVm, pack_process_vm_handle, process_vm_handle, process_vm_mmap, process_vm_mmap_shared,
};
use slopos_ostd::KArc;
use slopos_ostd::handle::Handle;
use slopos_ostd::process::quota::FileBacking;
use slopos_ostd::process::{AccountId, Process, ProcessId};
use slopos_sched::task::{
    TaskRef, fail_task_snapshots_for_test, task_create, task_find_by_id, task_terminate,
};
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::oom::{OOM_OPS, standing_of};

const UNGRANTED: u16 = TASK_FLAG_USER_MODE;
const PRIVILEGED: u16 = TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR | TASK_FLAG_LAUNCH;
const WRITE_USER_ABSENT: u64 = 0x06;

fn create_user_task() -> u32 {
    task_create(
        b"OomVictim\0".as_ptr() as *const c_char,
        slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    )
}

pub fn test_oom_spares_init_and_passes_over_the_dying() -> TestResult {
    let _scope = KernelTestScope::new();

    let task_id = create_user_task();
    if task_id == INVALID_TASK_ID {
        return fail!("could not create a user task");
    }
    let Some(task) = task_find_by_id(task_id) else {
        task_terminate(task_id);
        return fail!("the new task is not findable");
    };
    let Some(process) = task.process() else {
        drop(task);
        task_terminate(task_id);
        return fail!("the user task has no process");
    };

    let plain = standing_of(&process, INVALID_TASK_ID, UNGRANTED);
    let as_init = standing_of(&process, task_id, PRIVILEGED);
    let killed = OOM_OPS.kill(&process);
    let marked = task.is_killed();
    let after_kill = standing_of(&process, INVALID_TASK_ID, PRIVILEGED);
    let second = OOM_OPS.kill(&process);

    drop(task);
    task_terminate(task_id);

    assert_test!(
        plain == Standing::Killable,
        "a live process stood {:?}",
        plain
    );
    assert_test!(as_init == Standing::Exempt, "init stood {:?}", as_init);
    assert_test!(
        killed.as_ref().is_some_and(|k| k.pid == task_id) && marked,
        "the kill reached pid {:?}, and marked the task: {}",
        killed.as_ref().map(|k| k.pid),
        marked
    );
    assert_test!(
        after_kill == Standing::Dying,
        "a process whose tasks are all killed stood {:?}",
        after_kill
    );
    assert_test!(
        second.is_none(),
        "a second kill of a dying process claimed another victim"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_oom_spares_init_and_passes_over_the_dying,
    suite = oom_killer
);

/// The real task side, confined to the processes a test made.
struct Among {
    members: [u32; 2],
    writer: u16,
}

impl Among {
    fn one(pid: u32, writer: u16) -> Self {
        Self {
            members: [pid, INVALID_PROCESS_ID],
            writer,
        }
    }
}

impl OomOps for Among {
    fn standing(&self, process: &Process) -> Standing {
        if self.members.contains(&process.id()) {
            standing_of(process, INVALID_TASK_ID, self.writer)
        } else {
            Standing::Exempt
        }
    }

    fn kill(&self, process: &Process) -> Option<Killed> {
        self.members
            .contains(&process.id())
            .then(|| OOM_OPS.kill(process))
            .flatten()
    }
}

fn touch_fresh_pages(process: ProcessId, task_id: u32, pages: u64) -> bool {
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
            try_resolve_user_fault(
                addr + page * PAGE_SIZE_4KB,
                WRITE_USER_ABSENT,
                packed,
                task_id,
            ) == FaultOutcome::Resolved
        })
}

/// The killer runs with the heap dry, so it must not need a task snapshot.
pub fn test_oom_kills_the_hog_without_a_task_snapshot() -> TestResult {
    let _scope = KernelTestScope::new();

    let task_id = create_user_task();
    if task_id == INVALID_TASK_ID {
        return fail!("could not create a user task");
    }
    let Some(task) = task_find_by_id(task_id) else {
        task_terminate(task_id);
        return fail!("the new task is not findable");
    };
    let Some(process) = task.process() else {
        drop(task);
        task_terminate(task_id);
        return fail!("the user task has no process");
    };
    let Some(designator) = ProcessId::of(&process) else {
        drop(task);
        task_terminate(task_id);
        return fail!("the user task's process is not live");
    };
    let hog_vm = process_vm_handle(designator);
    let touched = touch_fresh_pages(designator, task_id, 8);

    oom_forget_victim_for_test();
    fail_task_snapshots_for_test(true);
    let awaited = oom_decide_for_test(&Among::one(process.id(), UNGRANTED), OomTrigger::Frames);
    fail_task_snapshots_for_test(false);
    oom_forget_victim_for_test();
    let marked = task.is_killed();

    drop(task);
    task_terminate(task_id);

    assert_test!(
        touched && hog_vm.is_some(),
        "could not make the process resident"
    );
    assert_test!(
        awaited.is_some() && awaited == hog_vm,
        "the killer chose no victim with the hog resident"
    );
    assert_test!(marked, "the victim was chosen but its task never killed");
    pass!()
}

slopos_testing::stest!(
    name = test_oom_kills_the_hog_without_a_task_snapshot,
    suite = oom_killer
);

struct Member {
    task_id: u32,
    task: Option<TaskRef>,
    process: KArc<Process>,
    designator: ProcessId,
}

impl Member {
    fn spawn(flags: u16) -> Option<Self> {
        let task_id = task_create(
            b"OomMember\0".as_ptr() as *const c_char,
            slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
            ptr::null_mut(),
            1,
            flags,
        );
        if task_id == INVALID_TASK_ID {
            return None;
        }
        let member = task_find_by_id(task_id).and_then(|task| {
            let process = task.process()?;
            let designator = ProcessId::of(&process)?;
            Some(Self {
                task_id,
                task: Some(task),
                process,
                designator,
            })
        });
        if member.is_none() {
            task_terminate(task_id);
        }
        member
    }

    fn pid(&self) -> u32 {
        self.process.id()
    }

    fn vm(&self) -> Option<Handle<ProcessVm>> {
        process_vm_handle(self.designator)
    }

    fn killed(&self) -> bool {
        self.task.as_ref().is_some_and(|task| task.is_killed())
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        drop(self.task.take());
        task_terminate(self.task_id);
    }
}

/// A memfd of `pages` sized on `account`, as `ftruncate` sizes one, closed
/// when its backing drops.
fn sized_memfd(account: AccountId, pages: usize) -> Option<(usize, KArc<dyn FileBacking>)> {
    let (handle, _ops, backing) = memfd_create(0, account)?;
    (memfd_ftruncate(handle, pages * PAGE_SIZE_4KB as usize, account) == 0)
        .then_some((handle, backing))
}

/// SLOPOS-2026-0058's scenario. One process holds a memfd it never mapped and
/// a surface another maps beside a few pages of its own, so that one counts
/// every surface page among its present leaves. Both memfds are the holder's
/// promises and the holder's frames: it is taken whichever ran short, and the
/// mapper is not.
pub fn test_oom_takes_the_holder_not_the_mapper() -> TestResult {
    const HOARD: usize = 48;
    const SURFACE: usize = 64;
    let _scope = KernelTestScope::new();

    let (Some(holder), Some(mapper)) = (Member::spawn(UNGRANTED), Member::spawn(UNGRANTED)) else {
        return fail!("could not create the two user tasks");
    };
    let account = holder.process.account();
    let hoard = sized_memfd(account, HOARD);
    let sized = hoard.is_some();
    let surface = sized_memfd(account, SURFACE);
    let mapped = surface.as_ref().is_some_and(|(handle, _)| {
        process_vm_mmap_shared(
            mapper.designator,
            0,
            (SURFACE as u64) * PAGE_SIZE_4KB,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            0,
            *handle,
        ) != 0
    });
    let working = touch_fresh_pages(mapper.designator, mapper.task_id, 4);
    let (holder_vm, mapper_vm) = (holder.vm(), mapper.vm());

    let ops = Among {
        members: [holder.pid(), mapper.pid()],
        writer: UNGRANTED,
    };
    let for_commit = oom_choose_for_test(&ops, OomTrigger::Commit);
    let for_frames = oom_choose_for_test(&ops, OomTrigger::Frames);

    drop(mapper);
    drop(holder);
    drop(surface);
    drop(hoard);

    assert_test!(
        sized && mapped && working && holder_vm.is_some() && mapper_vm.is_some(),
        "could not size the memfds, map the surface and touch the mapper's pages"
    );
    assert_test!(
        for_commit == holder_vm && for_frames == holder_vm,
        "the killer chose {:?} for the ceiling and {:?} for the frames (the holder is \
         {:?}, the mapper {:?})",
        for_commit,
        for_frames,
        holder_vm,
        mapper_vm
    );
    pass!()
}

slopos_testing::stest!(
    name = test_oom_takes_the_holder_not_the_mapper,
    suite = oom_killer
);

/// A process holding privileged flags the writer lacks is never taken for it,
/// however much it holds: with nothing else left the write fails — the
/// writer's `UserOom` — and the privileged process lives. A writer holding
/// those flags weighs it like any other.
pub fn test_oom_never_takes_a_process_the_writer_may_not_signal() -> TestResult {
    const OWN: usize = 16;
    const PRIVATE: u64 = 64;
    let _scope = KernelTestScope::new();

    let (Some(writer), Some(privileged)) = (Member::spawn(UNGRANTED), Member::spawn(PRIVILEGED))
    else {
        return fail!("could not create the two user tasks");
    };
    let own = sized_memfd(privileged.process.account(), OWN);
    let sized = own.is_some();
    let touched = touch_fresh_pages(privileged.designator, privileged.task_id, PRIVATE);
    let (writer_vm, privileged_vm) = (writer.vm(), privileged.vm());
    let both = [writer.pid(), privileged.pid()];

    let as_ungranted = oom_choose_for_test(
        &Among {
            members: both,
            writer: UNGRANTED,
        },
        OomTrigger::Commit,
    );
    let as_privileged = oom_choose_for_test(
        &Among {
            members: both,
            writer: PRIVILEGED,
        },
        OomTrigger::Commit,
    );
    let alone = Among::one(privileged.pid(), UNGRANTED);
    oom_forget_victim_for_test();
    let for_commit = oom_serve_for_test(&alone, OomTrigger::Commit);
    let for_frames = oom_serve_for_test(&alone, OomTrigger::Frames);
    oom_forget_victim_for_test();
    let privileged_killed = privileged.killed();

    drop(privileged);
    drop(writer);
    drop(own);

    assert_test!(
        sized && touched && writer_vm.is_some() && privileged_vm.is_some(),
        "could not make the privileged process hold its pages"
    );
    assert_test!(
        as_ungranted == writer_vm,
        "an ungranted writer's killer chose {:?}, want the writer's own {:?}, not the \
         privileged {:?}",
        as_ungranted,
        writer_vm,
        privileged_vm
    );
    assert_test!(
        as_privileged == privileged_vm,
        "a privileged writer's killer chose {:?}, want the larger {:?}",
        as_privileged,
        privileged_vm
    );
    assert_test!(
        for_commit == OomVerdict::Unresolved && for_frames == OomVerdict::Unresolved,
        "with only the privileged process left, the ungranted write got {:?} for the \
         ceiling and {:?} for the frames, want it failed",
        for_commit,
        for_frames
    );
    assert_test!(
        !privileged_killed,
        "the privileged process was killed for a write that could not signal it"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_oom_never_takes_a_process_the_writer_may_not_signal,
    suite = oom_killer
);
