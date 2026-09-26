//! UEFI variables for user space: how a booted system asks its boot loader
//! for the next boot, through the Boot Loader Interface's EFI variables.
//!
//! The firmware is mapped only into the kernel master address space and may
//! use the vector registers, so a call runs on a kernel thread: there no user
//! task's page tables or vector state are live, and the scheduler restores the
//! next user task's own state on the way back to it. A syscall hands its
//! request to that thread and sleeps until the answer is back; callers are
//! served one at a time, because runtime services are not reentrant.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::Errno;
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, LOCK_LEVEL_RESOURCE, Mutex, SpinLock, WaitQueue};
use slopos_ostd::uefi::{EfiError, EfiGuid};
use slopos_ostd::{KBox, KVec, lock_class};

/// The vendor GUIDs user space may touch: the Boot Loader Interface's
/// (4a67b082-0a4c-41cf-b6c7-440b29bb8c4f) and SlopOS's own
/// (5a1b0b05-5105-4e57-a11e-0000000000a1). Anything wider would let a holder
/// of `Power`, which exists to reboot, rewrite `BootOrder` or enrol Secure
/// Boot keys.
const ALLOWED_GUIDS: [[u8; 16]; 2] = [
    [
        0x82, 0xb0, 0x67, 0x4a, 0x4c, 0x0a, 0xcf, 0x41, 0xb6, 0xc7, 0x44, 0x0b, 0x29, 0xbb, 0x8c,
        0x4f,
    ],
    [
        0x05, 0x0b, 0x1b, 0x5a, 0x05, 0x51, 0x57, 0x4e, 0xa1, 0x1e, 0, 0, 0, 0, 0, 0xa1,
    ],
];

/// Longest variable name accepted, in UTF-16 units before the terminator.
pub const EFIVAR_NAME_MAX: usize = 128;
/// Largest variable value moved in one call.
pub const EFIVAR_DATA_MAX: usize = 4096;

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

/// Run one request on the thread and answer it with the buffer it carried.
fn call(op: Op, name: &[u8], guid: [u8; 16], data: KVec<u8>) -> Result<(usize, KVec<u8>), Errno> {
    if !ALLOWED_GUIDS.contains(&guid) {
        return Err(Errno::EPERM);
    }
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
pub fn efivar_get(name: &[u8], guid: [u8; 16], capacity: usize) -> Result<KVec<u8>, Errno> {
    let data = KVec::zeroed(capacity.min(EFIVAR_DATA_MAX)).map_err(|_| Errno::ENOMEM)?;
    let (len, mut data) = call(Op::Get, name, guid, data)?;
    data.truncate(len);
    Ok(data)
}

/// Write variable `name` under `guid`; an empty `value` deletes it.
pub fn efivar_set(name: &[u8], guid: [u8; 16], attributes: u32, value: &[u8]) -> Result<(), Errno> {
    if value.len() > EFIVAR_DATA_MAX {
        return Err(Errno::E2BIG);
    }
    let mut data = KVec::new();
    data.extend_from_slice(value).map_err(|_| Errno::ENOMEM)?;
    call(Op::Set { attributes }, name, guid, data).map(|_| ())
}
