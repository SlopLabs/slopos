use slopos_abi::addr::VirtAddr;
use slopos_abi::task::TaskFaultReason;
use slopos_ostd::handle::HandleError;
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::{klog_info, klog_warn};

use crate::error::MmError;
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::ProcessVm;
use crate::user_mappings::ostd_get_pte_flags_4kb;
use crate::{cow, demand, filemap_hook, process_vm};
use slopos_ostd::handle::Handle;

/// What became of a user page fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultOutcome {
    /// Serviced; the faulting instruction can be retried.
    Resolved,
    /// Exclusive access was unavailable; nothing changed and the instruction re-faults.
    Retry,
    /// A file-backed page that must be read from the device. The caller opens
    /// the blocking window and finishes through [`complete_file_fault`], by
    /// which point the deep plan phase has unwound off the trap frame.
    NeedsIo(demand::FileFaultPlan),
    /// Not serviceable; the task dies with this reason, which `waitpid` needs
    /// to tell an out-of-memory kill from a wild dereference.
    Fatal(TaskFaultReason),
}

pub(crate) const RETRY_WARN_MS: u64 = 50;

#[derive(Clone, Copy)]
pub(crate) struct RetryEpisode {
    task_id: u32,
    fault_addr: u64,
    since_ms: u64,
    warned: bool,
}

impl RetryEpisode {
    pub(crate) const IDLE: Self = Self {
        task_id: 0,
        fault_addr: 0,
        since_ms: 0,
        warned: false,
    };
}

slopos_ostd::cpu_local! {
    static RETRY_EPISODE: RetryEpisode = RetryEpisode::IDLE;
}

/// Keyed on the task, not the address: alternating addresses would reset the episode.
pub(crate) fn note_retry(
    ep: &mut RetryEpisode,
    task_id: u32,
    fault_addr: u64,
    now_ms: u64,
) -> bool {
    if ep.task_id != task_id {
        *ep = RetryEpisode {
            task_id,
            fault_addr,
            since_ms: now_ms,
            warned: false,
        };
        return false;
    }
    if ep.warned || now_ms.wrapping_sub(ep.since_ms) < RETRY_WARN_MS {
        return false;
    }
    ep.fault_addr = fault_addr;
    ep.warned = true;
    true
}

/// No escalation: any threshold here is user-reachable, and a stuck retry is a defect.
fn retry(task_id: u32, fault_addr: u64) -> FaultOutcome {
    let warn = {
        let mut episode = RETRY_EPISODE.get_mut();
        // The clock read is uncached MMIO taken with `IF` clear; skip it once warned.
        if episode.task_id == task_id && episode.warned {
            false
        } else {
            let now = slopos_kernel_services::clock::uptime_ms();
            note_retry(&mut episode, task_id, fault_addr, now)
        }
    };
    if warn {
        klog_warn!(
            "PF: task {} has been retrying at cr2=0x{:x} for {} ms — an address-space \
             reader is not draining",
            task_id,
            fault_addr,
            RETRY_WARN_MS
        );
    }
    FaultOutcome::Retry
}

enum Prefault {
    Spurious,
    Cow(Result<(), MmError>),
    /// Neither; the region walk decides.
    Unclaimed,
}

/// Whether the page tables already permit the access that faulted.
///
/// A sibling can resolve the page between the trap and this handler, and a
/// permission upgrade is published with a local invalidation only, so a peer
/// CPU's stale entry faults on an access its tables now allow. Both are the
/// architecture's spurious fault (SDM Vol. 3 §4.10.4.3), not a violation.
fn fault_is_spurious(error_code: u64, vm_space: &KArc<VmSpace>, fault_addr: u64) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_ifetch = (error_code & 0x10) != 0;

    let Some(flags) = ostd_get_pte_flags_4kb(vm_space, VirtAddr::new(fault_addr)) else {
        return false;
    };
    if !flags.contains(PageFlags::PRESENT) || !flags.contains(PageFlags::USER) {
        return false;
    }
    if is_write && !flags.contains(PageFlags::WRITABLE) {
        return false;
    }
    !(is_ifetch && flags.contains(PageFlags::NO_EXECUTE))
}

/// Try to service a user page fault in the address space named by
/// `process_vm_handle`, returning whether it was resolved.
///
/// Keyed by handle rather than by process id because ids recycle: a handle
/// names the address space the faulting task was built against and fails to
/// resolve once that slot is rebound, where a recycled id would service the
/// fault inside a stranger's page tables.
pub fn try_resolve_user_fault(
    fault_addr: u64,
    error_code: u64,
    process_vm_handle: u64,
    task_id: u32,
) -> FaultOutcome {
    let Some(handle) = process_vm::unpack_process_vm_handle(process_vm_handle) else {
        return FaultOutcome::Fatal(TaskFaultReason::UserPage);
    };

    // One hold of the per-process lock, not two: a sibling can resolve the page between.
    let prefault = process_vm::process_vm_with_vm_space_by_handle(handle, |vs| {
        if cow::is_cow_fault(error_code, vs, fault_addr) {
            return Prefault::Cow(cow::handle_cow_fault(vs, fault_addr));
        }
        if fault_is_spurious(error_code, vs, fault_addr) {
            return Prefault::Spurious;
        }
        Prefault::Unclaimed
    });

    match prefault {
        // Nothing to publish: only this CPU's cached translation is stale.
        Ok(Prefault::Spurious) => {
            crate::tlb::flush_page_local(VirtAddr::new(fault_addr & !(PAGE_SIZE_4KB - 1)));
            return FaultOutcome::Resolved;
        }
        Ok(Prefault::Cow(Ok(()))) => return FaultOutcome::Resolved,
        Ok(Prefault::Cow(Err(MmError::Retry))) => return retry(task_id, fault_addr),
        Ok(Prefault::Cow(Err(MmError::NoMemory))) => {
            klog_info!(
                "PF: COW copy for task {} at cr2=0x{:x} found no memory",
                task_id,
                fault_addr
            );
            return FaultOutcome::Fatal(TaskFaultReason::UserOom);
        }
        Ok(Prefault::Cow(Err(_))) => {
            klog_info!(
                "PF: COW resolution FAILED for task {} at cr2=0x{:x}",
                task_id,
                fault_addr
            );
        }
        Ok(Prefault::Unclaimed) => {}
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            return FaultOutcome::Fatal(TaskFaultReason::UserPage);
        }
    }

    let demanded = process_vm::process_vm_with_fault_context_by_handle(
        handle,
        fault_addr,
        |vs, map, region| {
            if !demand::is_demand_fault_in_region(error_code, &region) || !region.is_anonymous() {
                return None;
            }
            Some(demand::handle_demand_fault(
                vs, map, fault_addr, error_code, &region,
            ))
        },
    );

    match demanded {
        Ok(Some(Ok(()))) => return FaultOutcome::Resolved,
        Ok(Some(Err(MmError::Retry))) => return retry(task_id, fault_addr),
        Ok(Some(Err(MmError::NoMemory))) => {
            klog_info!(
                "PF: demand fault for task {} at cr2=0x{:x} found no memory after reclaim",
                task_id,
                fault_addr
            );
            return FaultOutcome::Fatal(TaskFaultReason::UserOom);
        }
        Ok(Some(Err(_))) => return FaultOutcome::Fatal(TaskFaultReason::UserPage),
        Ok(None) => {}
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            return FaultOutcome::Fatal(TaskFaultReason::UserPage);
        }
    }

    plan_file_fault(handle, fault_addr, error_code, task_id)
}

/// Plan a file-backed fault under the per-process lock. Answers
/// [`FaultOutcome::NeedsIo`] rather than reading: this runs with interrupts off
/// on the trap's own stack, and the read must not.
fn plan_file_fault(
    handle: Handle<ProcessVm>,
    fault_addr: u64,
    error_code: u64,
    task_id: u32,
) -> FaultOutcome {
    let planned = process_vm::process_vm_with_vm_space_and_area_by_handle(
        handle,
        fault_addr,
        |vs, start, _end, region| {
            if !demand::is_demand_fault_in_region(error_code, region) {
                return None;
            }
            Some(demand::plan_file_fault(
                vs, start, fault_addr, error_code, region,
            ))
        },
    );

    match planned {
        Ok(Some(Ok(Some(plan)))) => FaultOutcome::NeedsIo(plan),
        // Someone else installed it while this fault was in flight.
        Ok(Some(Ok(None))) => FaultOutcome::Resolved,
        Ok(Some(Err(MmError::Retry))) => retry(task_id, fault_addr),
        Ok(Some(Err(_))) | Ok(None) => FaultOutcome::Fatal(TaskFaultReason::UserPage),
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            FaultOutcome::Fatal(TaskFaultReason::UserPage)
        }
    }
}

/// Read the planned page and install it. **Blocks**, so the caller must have
/// opened the window: interrupts on, interrupt-nesting left, the per-process
/// lock not held.
pub fn complete_file_fault(
    process_vm_handle: u64,
    plan: &demand::FileFaultPlan,
    fault_addr: u64,
    task_id: u32,
) -> FaultOutcome {
    let Some(handle) = process_vm::unpack_process_vm_handle(process_vm_handle) else {
        return FaultOutcome::Fatal(TaskFaultReason::UserPage);
    };

    let cached = match filemap_hook::filemap_fault_page(plan.map, plan.page_index) {
        Ok(phys) => phys,
        Err(errno) if errno == slopos_abi::Errno::EINTR.raw() => {
            // The instruction re-executes; the signal is delivered on the way out.
            return FaultOutcome::Retry;
        }
        Err(errno) => {
            klog_info!(
                "PF: file page {} for task {} at cr2=0x{:x} refused: errno {}",
                plan.page_index,
                task_id,
                fault_addr,
                errno
            );
            return FaultOutcome::Fatal(TaskFaultReason::UserPage);
        }
    };

    let installed = process_vm::process_vm_with_vm_space_and_area_by_handle(
        handle,
        fault_addr,
        |vs, start, _end, region| demand::install_file_page(vs, start, plan, cached, region),
    );

    // Balances the reference the read took to hold the page across the install.
    filemap_hook::filemap_release(plan.map, 1);

    match installed {
        Ok(Ok(())) => FaultOutcome::Resolved,
        Ok(Err(MmError::Retry)) => retry(task_id, fault_addr),
        Ok(Err(MmError::NoMemory)) => {
            klog_info!(
                "PF: file page install for task {} at cr2=0x{:x} found no memory",
                task_id,
                fault_addr
            );
            FaultOutcome::Fatal(TaskFaultReason::UserOom)
        }
        Ok(Err(_)) => FaultOutcome::Fatal(TaskFaultReason::UserPage),
        Err(err) => {
            report_unresolvable_address_space(err, task_id, fault_addr);
            FaultOutcome::Fatal(TaskFaultReason::UserPage)
        }
    }
}

/// Whether a populate may read a file-backed page in, which blocks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileIo {
    /// Leave such a page absent, and the range unpopulated.
    Refuse,
    /// Read it as the user fault would: the caller can block, holds no
    /// spinning lock and no lock the filesystem read takes.
    Read,
}

/// One step of a populate loop; `false` gives the range up.
fn resolve_for_populate(
    page: u64,
    error_code: u64,
    process_vm_handle: u64,
    task_id: u32,
    io: FileIo,
) -> bool {
    match try_resolve_user_fault(page, error_code, process_vm_handle, task_id) {
        FaultOutcome::Resolved | FaultOutcome::Retry => true,
        FaultOutcome::NeedsIo(plan) if io == FileIo::Read => matches!(
            complete_file_fault(process_vm_handle, &plan, page, task_id),
            FaultOutcome::Resolved | FaultOutcome::Retry
        ),
        FaultOutcome::NeedsIo(_) | FaultOutcome::Fatal(_) => false,
    }
}

/// x86-64 `#PF` error-code shapes a user write takes: against an absent page
/// (the demand-paging shape) and against a present one (the COW shape).
const USER_WRITE_ABSENT: u64 = 0x06;
/// The same shape for a user *read* against an absent page.
const USER_READ_ABSENT: u64 = 0x04;
const USER_WRITE_PRESENT: u64 = 0x07;

/// `Retry` means a peer holds the address space for a bounded window, so the
/// bound is generous; a stuck retry is a defect, not a reason to spin forever.
/// Kept generous rather than trimmed now that the user-copy path reaches here
/// only after a copy has already failed: exhausting the bound is a spurious
/// `EFAULT` handed to userland, so the cost of spinning too long is latency in
/// a rare case and the cost of spinning too little is a wrong answer.
const POPULATE_SPINS: u32 = 4096;

#[derive(Clone, Copy)]
enum PageWriteState {
    Writable,
    /// Present but refused to a user write — the COW shape, or a read-only map.
    Present,
    Absent,
}

fn page_write_state(handle: Handle<ProcessVm>, page: u64) -> Option<PageWriteState> {
    use crate::paging_defs::PageFlags;
    let va = slopos_abi::addr::VirtAddr::new(page);
    process_vm::process_vm_with_vm_space_by_handle(handle, |vs| {
        match crate::user_mappings::ostd_get_pte_flags_4kb(vs, va) {
            Some(flags)
                if flags.contains(PageFlags::USER)
                    && flags.contains(PageFlags::WRITABLE)
                    && !flags.contains(PageFlags::COW) =>
            {
                PageWriteState::Writable
            }
            Some(_) => PageWriteState::Present,
            None => PageWriteState::Absent,
        }
    })
    .ok()
}

/// Make every page of `[addr, addr + len)` present and user-writable in the
/// address space `process_vm_handle` names.
///
/// The user-copy primitives take no page faults — they validate the leaf and
/// refuse — so a kernel→user write against an absent or still-COW page has to
/// fault the range in itself. Two callers: signal-frame delivery, which picks
/// an address below the interrupted RSP where a forked child's stack is still
/// COW, and [`crate::user_copy`], which comes here after a copy has already
/// refused.
///
/// A page that must be read from its file is read only under [`FileIo::Read`].
pub fn populate_user_range_for_write(
    process_vm_handle: u64,
    addr: u64,
    len: u64,
    task_id: u32,
    io: FileIo,
) -> bool {
    let Some(handle) = process_vm::unpack_process_vm_handle(process_vm_handle) else {
        return false;
    };
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    let page_size = crate::paging_defs::PAGE_SIZE_4KB;
    let mut page = addr & !(page_size - 1);
    while page < end {
        let mut spins = 0u32;
        loop {
            let error_code = match page_write_state(handle, page) {
                Some(PageWriteState::Writable) => break,
                Some(PageWriteState::Present) => USER_WRITE_PRESENT,
                Some(PageWriteState::Absent) => USER_WRITE_ABSENT,
                None => return false,
            };
            if !resolve_for_populate(page, error_code, process_vm_handle, task_id, io) {
                return false;
            }
            spins += 1;
            if spins == POPULATE_SPINS {
                return false;
            }
        }
        page += page_size;
    }
    true
}

/// Make every page of `[addr, addr + len)` present, as a user *read* would.
///
/// The write twin above breaks COW because a write must; a read must not, or
/// a reader would be handed a private copy nobody wrote to.
pub fn populate_user_range_for_read(
    process_vm_handle: u64,
    addr: u64,
    len: u64,
    task_id: u32,
    io: FileIo,
) -> bool {
    let Some(handle) = process_vm::unpack_process_vm_handle(process_vm_handle) else {
        return false;
    };
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    let page_size = crate::paging_defs::PAGE_SIZE_4KB;
    let mut page = addr & !(page_size - 1);
    while page < end {
        let mut spins = 0u32;
        loop {
            match page_write_state(handle, page) {
                Some(PageWriteState::Writable | PageWriteState::Present) => break,
                Some(PageWriteState::Absent) => {}
                None => return false,
            }
            if !resolve_for_populate(page, USER_READ_ABSENT, process_vm_handle, task_id, io) {
                return false;
            }
            spins += 1;
            if spins == POPULATE_SPINS {
                return false;
            }
        }
        page += page_size;
    }
    true
}

/// `HandleError::NoEntry` is the ordinary race a dying task loses; only
/// `Stale` is worth naming — a fault arriving for a task whose slot now
/// belongs to another process.
fn report_unresolvable_address_space(err: HandleError, task_id: u32, fault_addr: u64) {
    if err == HandleError::Stale {
        klog_info!(
            "PF: task {} faulted at cr2=0x{:x} against an address space that has \
             been rebound to another process",
            task_id,
            fault_addr
        );
    }
}
