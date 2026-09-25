//! User-copy primitives — thin shim over [`slopos_ostd::user::copy`].
//!
//! All the byte-copy logic (`rep movsb`, SMAP STAC/CLAC, page-fault
//! recovery) lives in OSTD. This module adapts the PCR-implicit signature
//! (`copy_from_user(ptr) -> Result<T, UserPtrError>`) onto OSTD's
//! explicit-`&VmSpace` API: it resolves `pcr.syscall_pid` to a
//! [`KArc<VmSpace>`] (the per-slot lock is dropped before the copy runs) and
//! maps [`slopos_ostd::user::copy::UserCopyError`] back onto the single
//! [`UserPtrError`] type kernel callers expect.

use slopos_ostd::cpu::preempt::PreemptGuard;
use slopos_ostd::sync::InitFlag;

use crate::page_fault::FileIo;
use crate::user_ptr::{UserBytes, UserPtr, UserPtrError};

static KERNEL_GUARD_CHECKED: InitFlag = InitFlag::new();

#[inline]
fn current_process_id() -> u32 {
    slopos_arch::pcr::current_syscall_pid()
}

pub fn set_test_process_id(pid: u32) {
    slopos_arch::pcr::set_current_syscall_pid(pid);
}

/// One-shot probe (latched after the first run) confirming the kernel half
/// cannot be reached through the user-VA validator — catches a page table
/// whose user/kernel boundary is shifted, before any fault-recovering copy is
/// handed a kernel address.
fn check_kernel_guard(process: slopos_ostd::process::ProcessId) -> Result<(), UserPtrError> {
    if KERNEL_GUARD_CHECKED.is_set() {
        return Ok(());
    }
    let kernel_probe = crate::memory_layout_defs::KERNEL_HALF_PROBE_VA;
    if crate::process_vm::process_vm_user_va_is_user_accessible(process, kernel_probe) {
        return Err(UserPtrError::NotMapped);
    }
    KERNEL_GUARD_CHECKED.mark_set();
    Ok(())
}

#[inline]
fn current_vm_space() -> Result<slopos_ostd::KArc<slopos_ostd::mm::vm_space::VmSpace>, UserPtrError>
{
    // The PCR carries a bare pid across the syscall boundary; resolving it here
    // to a generation-checked designator means a pid naming no live process
    // fails before it can reach a slot lookup.
    let Some(process) = slopos_ostd::process::ProcessId::resolve(current_process_id()) else {
        return Err(UserPtrError::NotMapped);
    };
    check_kernel_guard(process)?;
    crate::process_vm::process_vm_get_vm_space(process).ok_or(UserPtrError::NotMapped)
}

/// Resolve `[addr, addr + len)` the way the faulting instruction would.
///
/// OSTD's copy primitives validate the leaf and refuse; they take no fault.
/// A user buffer is therefore only copyable once its pages are *already*
/// present — and since `brk`, `mmap` and `fork` all leave pages absent or COW
/// by design, a first `read(2)` into a fresh `Vec` had no present leaf to
/// write and answered `EFAULT`. Linux services a kernel fault on a user
/// address through the ordinary fault path; this is that, run from
/// [`copy_then_populate`] on the failure that path would have fixed. A range
/// that cannot be resolved is left alone: the copy then fails exactly as it
/// did before, so this only ever turns an `EFAULT` into a transfer.
fn populate(access: Access, addr: u64, len: usize) {
    if len == 0 {
        return;
    }
    let _ = populate_inner(access, addr, len);
}

fn populate_inner(access: Access, addr: u64, len: usize) -> Option<()> {
    let process = slopos_ostd::process::ProcessId::resolve(current_process_id())?;
    let handle = crate::process_vm::process_vm_handle(process)?;
    let packed = crate::process_vm::pack_process_vm_handle(handle);
    let task_id = slopos_arch::pcr::current_task_id();
    let io = file_io();
    let ok = match access {
        Access::Read => {
            crate::page_fault::populate_user_range_for_read(packed, addr, len as u64, task_id, io)
        }
        Access::Write => {
            crate::page_fault::populate_user_range_for_write(packed, addr, len as u64, task_id, io)
        }
    };
    ok.then_some(())
}

/// A copy may wait for a file page wherever its caller could block. Filesystem
/// code stages user data outside its own locks, so a caller here holds none a
/// file read takes.
fn file_io() -> FileIo {
    if slopos_ostd::cpu::x86_64::interrupts::are_interrupts_enabled()
        && !slopos_ostd::cpu::x86_64::pcr::in_interrupt_context()
        && !PreemptGuard::is_active()
    {
        FileIo::Read
    } else {
        FileIo::Refuse
    }
}

#[cfg(feature = "test-hooks")]
static FAULT_NEXT_COPY: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(slopos_abi::task::INVALID_PROCESS_ID);

/// Report the first attempt of `pid`'s next copy as faulted midway, as a
/// stale TLB entry makes it.
#[cfg(feature = "test-hooks")]
pub fn fault_next_copy_for_test(pid: u32) {
    FAULT_NEXT_COPY.store(pid, core::sync::atomic::Ordering::Release);
}

/// Disarm [`fault_next_copy_for_test`], answering whether a copy took it.
#[cfg(feature = "test-hooks")]
pub fn copy_fault_taken_for_test() -> bool {
    FAULT_NEXT_COPY.swap(
        slopos_abi::task::INVALID_PROCESS_ID,
        core::sync::atomic::Ordering::AcqRel,
    ) == slopos_abi::task::INVALID_PROCESS_ID
}

#[derive(Clone, Copy)]
enum Access {
    Read,
    Write,
}

/// Run `copy` against the current address space; only if it fails the way a
/// page fault would have fixed, populate `[addr, addr + len)` and run it once
/// more.
///
/// Copy first, not populate first: the pages are already present on all but
/// the first touch, and populating unconditionally charged that majority a
/// second pid resolve, a slot lock and a per-page table walk before OSTD
/// walked the same leaves again. Every failure populate can repair — absent
/// leaf, non-user leaf, present-but-COW leaf refused to a write — arrives as
/// [`UserPtrError::NotMapped`]. A fault mid-`movsb`, `CopyFailed`, is retried
/// too: a peer that made a page writable flushed only its own TLB, and the
/// fault retired this CPU's stale entry.
///
/// The `KArc<VmSpace>` is dropped before `populate` and re-taken after, and
/// that ordering is load-bearing: the demand path refuses to install a page
/// while any other reference to the space is live, so populating with the
/// copy's own handle held could never succeed — and would make the faulting
/// task's own retries look like an address-space reader that is not draining.
///
/// Preemption is held off while the handle is held, and that is load-bearing
/// too: a holder switched out mid-copy pins the reference until it runs
/// again, the exclusive roads spin for that reference under the process-VM
/// lock, and dispatching the holder takes that same lock. The copy neither
/// faults nor blocks, so the span is the copy and nothing else.
#[inline]
fn copy_then_populate<T>(
    access: Access,
    addr: u64,
    len: usize,
    mut copy: impl FnMut(&slopos_ostd::mm::vm_space::VmSpace) -> Result<T, UserPtrError>,
) -> Result<T, UserPtrError> {
    {
        let _pinned = PreemptGuard::new();
        let space = current_vm_space()?;
        let first = copy(&space);
        #[cfg(feature = "test-hooks")]
        let first = match FAULT_NEXT_COPY.compare_exchange(
            current_process_id(),
            slopos_abi::task::INVALID_PROCESS_ID,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => Err(UserPtrError::CopyFailed),
            Err(_) => first,
        };
        match first {
            Err(UserPtrError::NotMapped | UserPtrError::CopyFailed) => {}
            result => return result,
        }
    }
    populate(access, addr, len);
    let _pinned = PreemptGuard::new();
    let space = current_vm_space()?;
    copy(&space)
}

/// Copy a `T: Copy` from user space into kernel space.
///
/// The transfer is fault-recoverable: a concurrent `munmap` on another CPU
/// surfaces as `UserPtrError::CopyFailed`, never a kernel panic.
///
/// `T: Copy` rather than OSTD's stricter `T: Pod`: the copy is byte-level, so
/// the caller is responsible for ensuring `T`'s representation tolerates
/// arbitrary byte patterns.
pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, UserPtrError> {
    copy_then_populate(
        Access::Read,
        src.addr().as_u64(),
        core::mem::size_of::<T>(),
        |space| slopos_ostd::user::copy::copy_value_from_user(space, src).map_err(Into::into),
    )
}

/// Copy a `T: Copy` from kernel space into user space.
pub fn copy_to_user<T: Copy>(dst: UserPtr<T>, value: &T) -> Result<(), UserPtrError> {
    copy_then_populate(
        Access::Write,
        dst.addr().as_u64(),
        core::mem::size_of::<T>(),
        |space| slopos_ostd::user::copy::copy_value_to_user(space, dst, value).map_err(Into::into),
    )
}

/// Copy raw bytes from user space.
pub fn copy_bytes_from_user(src: UserBytes, dst: &mut [u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }
    copy_then_populate(Access::Read, src.base().as_u64(), copy_len, |space| {
        slopos_ostd::user::copy::copy_bytes_from_user(space, src.base(), &mut dst[..copy_len])
            .map_err(Into::into)
    })?;
    Ok(copy_len)
}

/// Copy raw bytes to user space.
pub fn copy_bytes_to_user(dst: UserBytes, src: &[u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }
    copy_then_populate(Access::Write, dst.base().as_u64(), copy_len, |space| {
        slopos_ostd::user::copy::copy_bytes_to_user(space, dst.base(), &src[..copy_len])
            .map_err(Into::into)
    })?;
    Ok(copy_len)
}
