//! The `CommitPages` axis: what the kernel has promised a frame for. An
//! extent is charged when it is created and a placed page when it lands, so
//! the ledger moves at `mmap`, `mprotect`, `exec` and teardown, and at a
//! fault only for the page a `Frames` region places.

use slopos_abi::quota::{CommitPagesAxis, QuotaMode, ResourceKind};
use slopos_abi::syscall::{
    MAP_ANONYMOUS, MAP_NORESERVE, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE,
};
use slopos_ostd::process::quota::{quota_mode, root, set_limit, set_quota_mode, stats, try_charge};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use super::tests::resolve_pid;
use crate::memory_layout_defs::PROCESS_STACK_SIZE_BYTES;
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_end_prepay, process_vm_mmap,
    process_vm_mprotect, process_vm_munmap, process_vm_prepay_commit, process_vm_reset_stack,
};

const MAP_FLAGS: u64 = MAP_ANONYMOUS | MAP_PRIVATE;
const PROT_RW: u64 = PROT_READ | PROT_WRITE;

struct Scratch {
    pid: u32,
    restore: QuotaMode,
}

impl Scratch {
    fn new() -> Option<Self> {
        let restore = quota_mode();
        set_quota_mode(QuotaMode::Enforce);
        let pid = create_process_vm();
        if pid == slopos_abi::task::INVALID_PROCESS_ID {
            set_quota_mode(restore);
            return None;
        }
        Some(Self { pid, restore })
    }

    fn committed(&self) -> u32 {
        stats(resolve_pid(self.pid).account(), ResourceKind::CommitPages).map_or(0, |s| s.used)
    }

    fn mmap(&self, pages: u64, prot: u64, flags: u64) -> u64 {
        process_vm_mmap(
            resolve_pid(self.pid),
            0,
            pages * PAGE_SIZE_4KB,
            prot,
            flags,
            -1,
            0,
        )
    }

    fn munmap(&self, addr: u64, pages: u64) -> i32 {
        process_vm_munmap(resolve_pid(self.pid), addr, pages * PAGE_SIZE_4KB)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        destroy_process_vm(resolve_pid(self.pid));
        set_quota_mode(self.restore);
    }
}

fn root_committed() -> u32 {
    stats(root(), ResourceKind::CommitPages).map_or(0, |s| s.used)
}

fn root_limit() -> u32 {
    stats(root(), ResourceKind::CommitPages).map_or(u32::MAX, |s| s.limit)
}

/// The ceiling is a share of usable frames; zero is no ceiling and no share
/// ever reads as the sentinel by accident.
pub fn test_commit_limit_is_a_share_of_usable_frames() -> TestResult {
    use crate::commit::commit_limit_for;
    use slopos_abi::quota::NO_LIMIT_SENTINEL;
    assert_test!(commit_limit_for(1000, 100) == 1000, "100% of 1000 is 1000");
    assert_test!(commit_limit_for(1000, 50) == 500, "50% of 1000 is 500");
    assert_test!(commit_limit_for(1000, 150) == 1500, "150% of 1000 is 1500");
    assert_test!(
        commit_limit_for(1000, 0) == NO_LIMIT_SENTINEL,
        "0% is no ceiling"
    );
    assert_test!(
        commit_limit_for(u32::MAX, 400) != NO_LIMIT_SENTINEL,
        "a saturated share must not read as the sentinel"
    );
    pass!()
}

/// A fresh address space has promised exactly its eagerly mapped stack.
pub fn test_commit_fresh_space_promises_its_stack() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    let stack_pages = (PROCESS_STACK_SIZE_BYTES / PAGE_SIZE_4KB) as u32;
    assert_test!(
        scratch.committed() == stack_pages,
        "a fresh address space has {} pages committed, want the {} of its stack",
        scratch.committed(),
        stack_pages
    );
    pass!()
}

pub fn test_commit_extent_is_charged_at_mmap_and_returned_at_munmap() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 16;
    let before = scratch.committed();
    let addr = scratch.mmap(PAGES, PROT_RW, MAP_FLAGS);
    assert_test!(addr != 0, "mmap refused");
    assert_test!(
        scratch.committed() == before + PAGES as u32,
        "mmap of {} pages moved the commit by {}",
        PAGES,
        scratch.committed() as i64 - before as i64
    );
    assert_test!(scratch.munmap(addr, PAGES) == 0, "munmap failed");
    assert_test!(
        scratch.committed() == before,
        "munmap left {} pages committed, want {}",
        scratch.committed(),
        before
    );
    pass!()
}

/// `PROT_NONE` can populate nothing, so it owes nothing until `mprotect`
/// makes it accessible; narrowing it again keeps the promise.
pub fn test_commit_prot_none_owes_nothing_until_made_accessible() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 8;
    let before = scratch.committed();
    let addr = scratch.mmap(PAGES, PROT_NONE, MAP_FLAGS);
    assert_test!(addr != 0, "mmap refused");
    assert_test!(
        scratch.committed() == before,
        "a PROT_NONE mapping committed {} pages",
        scratch.committed() - before
    );
    let process = resolve_pid(scratch.pid);
    assert_test!(
        process_vm_mprotect(process, addr, PAGES * PAGE_SIZE_4KB, PROT_RW) == 0,
        "mprotect to RW failed"
    );
    assert_test!(
        scratch.committed() == before + PAGES as u32,
        "mprotect to RW committed {} pages, want {}",
        scratch.committed() - before,
        PAGES
    );
    assert_test!(
        process_vm_mprotect(process, addr, PAGES * PAGE_SIZE_4KB, PROT_NONE) == 0,
        "mprotect back to NONE failed"
    );
    assert_test!(
        scratch.committed() == before + PAGES as u32,
        "narrowing to PROT_NONE gave the promise back"
    );
    assert_test!(scratch.munmap(addr, PAGES) == 0, "munmap failed");
    assert_test!(
        scratch.committed() == before,
        "munmap did not return the promise"
    );
    pass!()
}

pub fn test_commit_noreserve_is_charged_at_the_touch_not_the_mmap() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 8;
    let before = scratch.committed();
    let addr = scratch.mmap(PAGES, PROT_RW, MAP_FLAGS | MAP_NORESERVE);
    assert_test!(addr != 0, "mmap refused");
    assert_test!(
        scratch.committed() == before,
        "MAP_NORESERVE committed {} pages at mmap",
        scratch.committed() - before
    );
    assert_test!(scratch.munmap(addr, PAGES) == 0, "munmap failed");
    assert_test!(scratch.committed() == before, "munmap moved the commit");
    pass!()
}

/// `MAP_NORESERVE` is a property of the region, not of the protection it was
/// mapped with: a reservation made accessible later is still charged at the
/// touch.
pub fn test_commit_noreserve_survives_a_prot_none_reservation() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    const PAGES: u64 = 8;
    let before = scratch.committed();
    let addr = scratch.mmap(PAGES, PROT_NONE, MAP_FLAGS | MAP_NORESERVE);
    assert_test!(addr != 0, "mmap refused");
    let process = resolve_pid(scratch.pid);
    assert_test!(
        process_vm_mprotect(process, addr, PAGES * PAGE_SIZE_4KB, PROT_RW) == 0,
        "mprotect to RW failed"
    );
    assert_test!(
        scratch.committed() == before,
        "mprotect of a MAP_NORESERVE reservation committed {} pages",
        scratch.committed() - before
    );
    assert_test!(scratch.munmap(addr, PAGES) == 0, "munmap failed");
    assert_test!(scratch.committed() == before, "munmap moved the commit");
    pass!()
}

/// An exec's frames draw on the advance charged beside the old image, and
/// what the loader does not draw comes back.
pub fn test_commit_exec_advance_is_drawn_before_the_ceiling() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    let process = resolve_pid(scratch.pid);
    let stack_pages = (PROCESS_STACK_SIZE_BYTES / PAGE_SIZE_4KB) as u32;
    const SPARE: u32 = 10;
    let before = scratch.committed();
    let Ok(funds) = try_charge::<CommitPagesAxis>(process.account(), stack_pages + SPARE) else {
        return fail!("the advance was refused");
    };
    process_vm_prepay_commit(process, funds);
    assert_test!(
        scratch.committed() == before + stack_pages + SPARE,
        "the advance is not held: {} committed, want {}",
        scratch.committed(),
        before + stack_pages + SPARE
    );
    assert_test!(process_vm_reset_stack(process) == 0, "reset_stack failed");
    assert_test!(
        scratch.committed() == before + SPARE,
        "the stack drew on the ceiling rather than the advance: {} committed, want {}",
        scratch.committed(),
        before + SPARE
    );
    process_vm_end_prepay(process);
    assert_test!(
        scratch.committed() == before,
        "the undrawn advance was not returned: {} committed, want {}",
        scratch.committed(),
        before
    );
    pass!()
}

/// The machine's ceiling refuses at `mmap`, and a refusal debits nothing.
pub fn test_commit_ceiling_refuses_at_mmap_and_leaves_no_debit() -> TestResult {
    let Some(scratch) = Scratch::new() else {
        return fail!("could not create an address space");
    };
    let restore = root_limit();
    let held = root_committed();
    set_limit(root(), ResourceKind::CommitPages, held + 4);

    let refused = scratch.mmap(8, PROT_RW, MAP_FLAGS);
    let after_refusal = root_committed();
    let granted = scratch.mmap(4, PROT_RW, MAP_FLAGS);
    let after_grant = root_committed();
    if granted != 0 {
        scratch.munmap(granted, 4);
    }
    set_limit(root(), ResourceKind::CommitPages, restore);

    assert_test!(refused == 0, "an mmap past the ceiling was granted");
    assert_test!(
        after_refusal == held,
        "the refusal left {} pages debited",
        after_refusal as i64 - held as i64
    );
    assert_test!(granted != 0, "an mmap inside the ceiling was refused");
    assert_test!(
        after_grant == held + 4,
        "the granted mmap did not debit its 4 pages"
    );
    pass!()
}

pub fn test_commit_teardown_returns_everything() -> TestResult {
    let restore = quota_mode();
    set_quota_mode(QuotaMode::Enforce);
    let held = root_committed();
    {
        let Some(scratch) = Scratch::new() else {
            set_quota_mode(restore);
            return fail!("could not create an address space");
        };
        assert_test!(scratch.mmap(64, PROT_RW, MAP_FLAGS) != 0, "mmap refused");
        assert_test!(scratch.mmap(16, PROT_NONE, MAP_FLAGS) != 0, "mmap refused");
        assert_test!(root_committed() > held, "nothing was committed");
    }
    let after = root_committed();
    set_quota_mode(restore);
    assert_test!(
        after == held,
        "teardown left {} pages committed at the root",
        after as i64 - held as i64
    );
    pass!()
}

slopos_testing::stest!(
    name = test_commit_limit_is_a_share_of_usable_frames,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_fresh_space_promises_its_stack,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_extent_is_charged_at_mmap_and_returned_at_munmap,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_prot_none_owes_nothing_until_made_accessible,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_noreserve_is_charged_at_the_touch_not_the_mmap,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_noreserve_survives_a_prot_none_reservation,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_exec_advance_is_drawn_before_the_ceiling,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_ceiling_refuses_at_mmap_and_leaves_no_debit,
    suite = quota_commit
);
slopos_testing::stest!(
    name = test_commit_teardown_returns_everything,
    suite = quota_commit
);
