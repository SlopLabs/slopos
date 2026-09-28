//! `FileKind::Signalfd` file operations: a pollable view of the pending
//! signals of the task using the descriptor — its own and its process's —
//! filtered to a subscribed mask. Like Linux's, a descriptor inherited across
//! `fork` serves the child's signals, never its creator's.
//!
//! Paired with the caller blocking those signals (`rt_sigprocmask`), delivery
//! becomes in-band: `(pending & !blocked)` excludes them from the harvest's
//! EINTR check, while `poll_events` tests raw `pending` and still reports them.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_abi::signal::SignalfdSiginfo;
use slopos_abi::syscall::{O_NONBLOCK, POLLIN, POLLNVAL};
use slopos_ostd::process::quota::{Charge, FileBacking};
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::sync::wait_queue::WaitAbort;
use slopos_ostd::task::ops::signal_pending_event;
use slopos_sched::task::task_wait_for_signal;
use slopos_sched::task_struct::Current;

use crate::registry;

pub struct SignalfdFileOps;

pub static SIGNALFD_FILE_OPS: SignalfdFileOps = SignalfdFileOps;

/// Owns one signalfd registry entry; dropping the last fd alias removes it.
#[derive(slopos_ostd::Charged)]
pub(crate) struct SignalfdBacking {
    pub(crate) handle: usize,
    pub(crate) object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(SignalfdBacking);

impl FileBacking for SignalfdBacking {}

impl Drop for SignalfdBacking {
    fn drop(&mut self) {
        registry::remove(self.handle);
    }
}

impl FileOps for SignalfdFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Signalfd
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        let Some(state) = registry::get(handle) else {
            return Errno::EBADF.as_isize();
        };
        if buf.len() < SignalfdSiginfo::SERIALIZED_LEN {
            return Errno::EINVAL.as_isize();
        }
        let Some(current) = Current::get() else {
            return Errno::ESRCH.as_isize();
        };
        let reader = current.task();
        let taken = if flags & O_NONBLOCK as u32 != 0 {
            reader.dequeue_signal(state.mask).ok_or(Errno::EAGAIN)
        } else {
            task_wait_for_signal(reader, state.mask, None).map_err(|abort| match abort {
                WaitAbort::Interrupted => Errno::ERESTARTSYS,
                _ => Errno::EINTR,
            })
        };
        let taken = match taken {
            Ok(taken) => taken,
            Err(e) => return e.as_isize(),
        };
        let record = SignalfdSiginfo::new(taken.signum, &taken.info);
        match buf.copy_in(0, &record.to_bytes()) {
            Ok(n) => n as isize,
            Err(e) => e.as_isize(),
        }
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match (registry::get(handle), Current::get()) {
            (Some(_), Some(current)) => BUS.subscribe_current(signal_pending_event(current.id())),
            _ => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let (Some(_), Some(current)) = (registry::get(handle), Current::get()) {
            BUS.unsubscribe_current(signal_pending_event(current.id()));
        }
    }

    fn poll_events(&self, handle: usize, _events: u16) -> u16 {
        let Some(state) = registry::get(handle) else {
            return POLLNVAL;
        };
        let pending = Current::get().map_or(0, |current| current.task().signal_pending());
        if pending & state.mask != 0 { POLLIN } else { 0 }
    }
}
