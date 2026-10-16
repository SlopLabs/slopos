//! `base_monitor`: a descriptor that turns readable when the base's
//! generation changes, as `/proc/self/mountinfo` turns pollable on a mount
//! change. A read answers the current generation, eight bytes little-endian,
//! and what it answered is what the next change is measured against.

use slopos_abi::Errno;
use slopos_abi::event::KernelEvent;
use slopos_abi::file_ops::{FileKind, FileOps, FusedPollResult};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::{POLLIN, POLLNVAL};
use slopos_ostd::KArc;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::process::quota::{Charge, FileBacking, try_charge};
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};

use super::BASE_FS;
use crate::fileio::{FdFlags, FdTable};

/// The generation each open monitor last answered. Each one is charged to
/// its opener's account, which is what bounds the table.
static SEEN: SpinLock<HandleTable<u32>> = SpinLock::new(
    HandleTable::new(),
    lock_class!("BASE_MONITORS", LOCK_LEVEL_REGISTRY),
);

const SLOT_BITS: u32 = 32;

fn handle_of(raw: usize) -> Handle<u32> {
    Handle::unpack(raw, SLOT_BITS)
}

const EVENT: KernelEvent = KernelEvent::BaseGeneration;

/// Wake every monitor's poller: the base's generation changed.
pub(super) fn changed() {
    BUS.publish(EVENT);
}

fn changed_since(handle: usize) -> Option<bool> {
    let seen = *SEEN.lock().get(handle_of(handle)).ok()?;
    Some(seen != BASE_FS.current_generation())
}

pub struct BaseMonitorFileOps;

pub static BASE_MONITOR_FILE_OPS: BaseMonitorFileOps = BaseMonitorFileOps;

#[derive(slopos_ostd::Charged)]
struct MonitorBacking {
    handle: usize,
    object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(MonitorBacking);

impl FileBacking for MonitorBacking {}

impl Drop for MonitorBacking {
    fn drop(&mut self) {
        let _ = SEEN.lock().remove(handle_of(self.handle));
    }
}

impl FileOps for BaseMonitorFileOps {
    fn kind(&self) -> FileKind {
        FileKind::BaseMonitor
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        if buf.len() < 8 {
            return Errno::EINVAL.as_isize();
        }
        let current = BASE_FS.current_generation();
        if let Err(e) = buf.copy_in(0, &u64::from(current).to_le_bytes()) {
            return e.as_isize();
        }
        match SEEN.lock().get_mut(handle_of(handle)) {
            Ok(seen) => *seen = current,
            Err(_) => return Errno::EBADF.as_isize(),
        }
        8
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_fused(&self, handle: usize, events: u16) -> FusedPollResult {
        let registered = self.poll_wait(handle);
        FusedPollResult {
            revents: self.poll_events(handle, events),
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, _events: u16) -> u16 {
        match changed_since(handle) {
            Some(true) => POLLIN,
            Some(false) => 0,
            None => POLLNVAL,
        }
    }

    fn poll_wait(&self, _handle: usize) -> bool {
        BUS.subscribe_current(EVENT)
    }

    fn poll_unwait(&self, _handle: usize) {
        BUS.unsubscribe_current(EVENT);
    }
}

/// Open a monitor for `table`'s process and install it as a close-on-exec
/// fd: the fd, or a negated errno.
pub fn monitor_create(table: FdTable) -> i32 {
    let Ok(reservation) = try_charge::<ObjectRow>(table.account(), 1) else {
        return Errno::ENFILE.raw();
    };
    let Ok(inserted) = SEEN.lock().insert(BASE_FS.current_generation()) else {
        return Errno::ENOMEM.raw();
    };
    let handle = inserted.pack(SLOT_BITS);
    let backing: KArc<dyn FileBacking> = match KArc::try_new(MonitorBacking {
        handle,
        object_charge: Charge::commit(reservation),
    }) {
        Ok(backing) => backing,
        Err(_) => {
            let _ = SEEN.lock().remove(inserted);
            return Errno::ENOMEM.raw();
        }
    };
    crate::fileio::fileio_open_fd_with_ops(
        table,
        &BASE_MONITOR_FILE_OPS,
        handle,
        Some(backing),
        FdFlags {
            cloexec: true,
            close_on_fork: false,
        },
    )
}
