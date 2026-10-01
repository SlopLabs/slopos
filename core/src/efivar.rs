//! UEFI variables for user space: how a booted system asks its boot loader
//! for the next boot, through the Boot Loader Interface's EFI variables, and
//! how an installer registers that loader with the firmware's boot manager.
//!
//! `Power`, which both syscalls are gated on, reaches the Boot Loader
//! Interface's namespace and SlopOS's own. The global namespace holds Secure
//! Boot's keys and every firmware setting besides the boot entries, so only
//! `Boot####`, `BootOrder`, `BootNext` and `BootCurrent` are reachable there,
//! with `BootEntry`, and a write is held to the format the firmware will parse
//! on every boot after it.
//!
//! The firmware is mapped only into the kernel master address space and may
//! use the vector registers, so a call runs on a kernel thread: there no user
//! task's page tables or vector state are live, and the scheduler restores the
//! next user task's own state on the way back to it. A syscall hands its
//! request to that thread and sleeps until the answer is back; callers are
//! served one at a time, because runtime services are not reentrant.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::Errno;
use slopos_boot_core::Guid;
use slopos_boot_core::variables::{self, BootVariable, Refusal, check_boot_write};
use slopos_ostd::authority::{BootEntry, Cap};
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, Mutex, SpinLock, WaitQueue};
use slopos_ostd::uefi::{EfiError, EfiGuid};
use slopos_ostd::{KBox, KVec, lock_class};

/// Longest variable name accepted, in UTF-16 units before the terminator.
pub const EFIVAR_NAME_MAX: usize = 128;
pub use slopos_abi::syscall::EFIVAR_DATA_MAX;

static SYSTEM_TABLE: AtomicU64 = AtomicU64::new(0);

static EFIVAR_STOP: KernelIoStop = KernelIoStop::new(
    "efivar",
    lock_class!("EFIVAR_STOP.waiters", LOCK_LEVEL_RESOURCE),
);
static THREAD_STARTED: InitFlag = InitFlag::new();

/// One caller at a time holds the thread.
static CALLER: Mutex<()> = Mutex::new((), lock_class!("EFIVAR_CALLER", LOCK_LEVEL_RESOURCE));

enum Op {
    Get,
    Set { attributes: u32 },
}

struct Request {
    op: Op,
    name: KVec<u16>,
    guid: EfiGuid,
    data: KVec<u8>,
    result: Option<Result<(usize, u32), EfiError>>,
}

/// The request in flight: posted by a caller, answered by the thread, taken
/// back by the caller. A caller killed while waiting leaves its request to
/// be answered and dropped by the next one.
static SLOT: SpinLock<Option<KBox<Request>>> =
    SpinLock::new(None, lock_class!("EFIVAR_SLOT", LOCK_LEVEL_RESOURCE));
static DONE: WaitQueue = WaitQueue::new(lock_class!("EFIVAR_DONE", LOCK_LEVEL_RESOURCE));

/// Record where the firmware's system table is; zero on a BIOS boot, which
/// leaves every call answering `ENODEV`.
pub fn efivar_init(system_table: u64) {
    SYSTEM_TABLE.store(system_table, Ordering::Release);
}

fn start_thread() -> Result<(), Errno> {
    if !THREAD_STARTED.init_once() {
        return Ok(());
    }
    if slopos_ostd::spawn_kernel_io!(&EFIVAR_STOP, efivar_thread).is_err() {
        THREAD_STARTED.reset();
        return Err(Errno::ENOMEM);
    }
    Ok(())
}

fn pending() -> bool {
    SLOT.lock().as_ref().is_some_and(|r| r.result.is_none())
}

fn efivar_thread(token: KernelIoToken<'static>) {
    loop {
        if token.park(&EFIVAR_STOP, pending) == KthreadWait::Stop {
            EFIVAR_STOP.note_exited();
            return;
        }
        let Some(mut request) = SLOT.lock().take() else {
            continue;
        };
        if request.result.is_none() {
            request.result = Some(serve(&mut request));
        }
        // A caller killed while this ran left the slot to the next one, whose
        // request must not be covered by an answer nobody waits for.
        let mut slot = SLOT.lock();
        if slot.is_none() {
            *slot = Some(request);
        }
        drop(slot);
        let _ = DONE.wake_all();
    }
}

fn serve(request: &mut Request) -> Result<(usize, u32), EfiError> {
    let table = SYSTEM_TABLE.load(Ordering::Acquire);
    match request.op {
        Op::Get => slopos_ostd::uefi::get_variable(
            table,
            &request.name,
            &request.guid,
            request.data.as_mut_slice(),
        ),
        Op::Set { attributes } => slopos_ostd::uefi::set_variable(
            table,
            &request.name,
            &request.guid,
            attributes,
            request.data.as_slice(),
        )
        .map(|()| (0, attributes)),
    }
}

fn errno_of(e: EfiError) -> Errno {
    match e {
        EfiError::Unavailable | EfiError::Unsupported => Errno::ENODEV,
        EfiError::NotFound => Errno::ENOENT,
        EfiError::BufferTooSmall(_) => Errno::ENOBUFS,
        EfiError::InvalidParameter => Errno::EINVAL,
        EfiError::WriteProtected => Errno::EROFS,
        EfiError::SecurityViolation => Errno::EPERM,
        EfiError::OutOfResources => Errno::ENOSPC,
        EfiError::DeviceError | EfiError::Other(_) => Errno::EIO,
    }
}

/// `name` as the NUL-terminated UTF-16 the firmware takes.
fn utf16_name(name: &[u8]) -> Result<KVec<u16>, Errno> {
    let text = core::str::from_utf8(name).map_err(|_| Errno::EINVAL)?;
    let mut out = KVec::new();
    for unit in text.encode_utf16() {
        if out.len() >= EFIVAR_NAME_MAX {
            return Err(Errno::ENAMETOOLONG);
        }
        out.push(unit).map_err(|_| Errno::ENOMEM)?;
    }
    if out.is_empty() {
        return Err(Errno::EINVAL);
    }
    out.push(0).map_err(|_| Errno::ENOMEM)?;
    Ok(out)
}

/// Whether a caller may read `name` under `guid`, or write it with `write`'s
/// attributes and value.
pub(crate) fn admit(
    name: &[u8],
    guid: Guid,
    write: Option<(u32, &[u8])>,
    boot_entry: Option<&Cap<'_, BootEntry>>,
) -> Result<(), Errno> {
    if guid == variables::LOADER || guid == variables::SLOPOS {
        return Ok(());
    }
    if guid != variables::GLOBAL || boot_entry.is_none() {
        return Err(Errno::EPERM);
    }
    let variable = core::str::from_utf8(name)
        .ok()
        .and_then(BootVariable::from_name)
        .ok_or(Errno::EPERM)?;
    let Some((attributes, value)) = write else {
        return Ok(());
    };
    check_boot_write(variable, attributes, value).map_err(|refusal| match refusal {
        Refusal::ReadOnly => Errno::EPERM,
        Refusal::Attributes | Refusal::Malformed => Errno::EINVAL,
    })
}

/// Run one request on the thread and answer it with the buffer it carried.
fn call(op: Op, name: &[u8], guid: [u8; 16], data: KVec<u8>) -> Result<(usize, KVec<u8>), Errno> {
    if SYSTEM_TABLE.load(Ordering::Acquire) == 0 {
        return Err(Errno::ENODEV);
    }
    let request = KBox::try_new(Request {
        op,
        name: utf16_name(name)?,
        guid: EfiGuid::from_bytes(guid),
        data,
        result: None,
    })
    .map_err(|_| Errno::ENOMEM)?;
    start_thread()?;

    let _serial = CALLER.lock().map_err(|_| Errno::EINTR)?;
    // Whatever a killed predecessor left is answered and stale.
    let _ = SLOT.lock().take();
    *SLOT.lock() = Some(request);
    EFIVAR_STOP.wake_one_for_work();
    let done = DONE
        .wait_event_until(|| {
            let mut slot = SLOT.lock();
            if slot.as_ref().is_some_and(|r| r.result.is_some()) {
                slot.take()
            } else {
                None
            }
        })
        .map_err(|_| Errno::EINTR)?;
    let Request { result, data, .. } = KBox::into_inner(done);
    match result {
        Some(Ok((len, _))) => Ok((len, data)),
        Some(Err(e)) => Err(errno_of(e)),
        None => Err(Errno::EIO),
    }
}

/// Read variable `name` under `guid` into a buffer of `capacity` bytes,
/// answering the value.
pub fn efivar_get(
    name: &[u8],
    guid: [u8; 16],
    capacity: usize,
    boot_entry: Option<&Cap<'_, BootEntry>>,
) -> Result<KVec<u8>, Errno> {
    admit(name, Guid(guid), None, boot_entry)?;
    let data = KVec::zeroed(capacity.min(EFIVAR_DATA_MAX)).map_err(|_| Errno::ENOMEM)?;
    let (len, mut data) = call(Op::Get, name, guid, data)?;
    data.truncate(len);
    Ok(data)
}

/// Write variable `name` under `guid`; an empty `value` deletes it.
pub fn efivar_set(
    name: &[u8],
    guid: [u8; 16],
    attributes: u32,
    value: &[u8],
    boot_entry: Option<&Cap<'_, BootEntry>>,
) -> Result<(), Errno> {
    if value.len() > EFIVAR_DATA_MAX {
        return Err(Errno::E2BIG);
    }
    admit(name, Guid(guid), Some((attributes, value)), boot_entry)?;
    let mut data = KVec::new();
    data.extend_from_slice(value).map_err(|_| Errno::ENOMEM)?;
    call(Op::Set { attributes }, name, guid, data).map(|_| ())
}
