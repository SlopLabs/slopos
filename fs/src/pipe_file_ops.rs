use slopos_abi::Errno;
use slopos_abi::event::{KernelEvent, PipeSlot};
use slopos_abi::file_ops::{FileKind, FileOps, FusedPollResult};
use slopos_abi::fs::{S_IFIFO, UserFsStat};
use slopos_abi::io::{IO_STAGING_SIZE, IoBufRead, IoBufWrite};
use slopos_abi::syscall::{POLLERR, POLLHUP};
use slopos_kernel_services::driver_runtime::scheduler_is_enabled;
use slopos_ostd::KArc;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{AliasOf, FileBacking};
use slopos_ostd::sync::BUS;
use slopos_ostd::sync::wait_queue::WaitAbort;

use crate::pipe;
use crate::pipe::{FifoNode, PipeHandle};

/// A blocked pipe transfer or FIFO open a signal cut short restarts under
/// `SA_RESTART` and is `EINTR` otherwise, as POSIX has it; one a kill cut short
/// is `EINTR`.
fn interrupted_errno(abort: WaitAbort) -> Errno {
    match abort {
        WaitAbort::Interrupted => Errno::ERESTARTSYS,
        _ => Errno::EINTR,
    }
}

fn interrupted(abort: WaitAbort) -> isize {
    interrupted_errno(abort).as_isize()
}

pub struct PipeReadOps;
pub struct PipeWriteOps;
/// A FIFO opened `O_RDWR`: both ends of the one pipe in a single description.
pub struct PipeReadWriteOps;

pub static PIPE_READ_OPS: PipeReadOps = PipeReadOps;
pub static PIPE_WRITE_OPS: PipeWriteOps = PipeWriteOps;
pub static PIPE_READ_WRITE_OPS: PipeReadWriteOps = PipeReadWriteOps;

/// The read-side event for a pipe handle (data became available to read).
#[inline]
fn read_ev(h: PipeHandle) -> KernelEvent {
    KernelEvent::PipeRead {
        pipe: PipeSlot(h.slot() as u32),
    }
}

/// The write-side event for a pipe handle (buffer space became available).
#[inline]
fn write_ev(h: PipeHandle) -> KernelEvent {
    KernelEvent::PipeWrite {
        pipe: PipeSlot(h.slot() as u32),
    }
}

/// Owner of the pipe's read end: dropping it retires one reader, waking
/// blocked writers on the last-reader edge and freeing the pipe slot once
/// both ends are gone.
#[derive(slopos_ostd::Charged)]
pub(crate) struct PipeReadBacking {
    handle: PipeHandle,
    object_charge: AliasOf,
}

slopos_ostd::charge_audit!(PipeReadBacking);

impl FileBacking for PipeReadBacking {}

impl Drop for PipeReadBacking {
    fn drop(&mut self) {
        pipe_release_reader(self.handle);
    }
}

/// Owner of the pipe's write end — the reader-side EOF edge lives in its
/// `Drop`.
#[derive(slopos_ostd::Charged)]
pub(crate) struct PipeWriteBacking {
    handle: PipeHandle,
    object_charge: AliasOf,
}

slopos_ostd::charge_audit!(PipeWriteBacking);

impl FileBacking for PipeWriteBacking {}

impl Drop for PipeWriteBacking {
    fn drop(&mut self) {
        pipe_release_writer(self.handle);
    }
}

/// Wrap ownership of both ends of a freshly-allocated, primed pipe
/// (readers == writers == 1). Consumes both primed references: on
/// allocation failure they are retired here, freeing the pipe slot —
/// the caller must not free it itself.
pub(crate) fn pipe_backings(
    handle: PipeHandle,
) -> Option<(KArc<dyn FileBacking>, KArc<dyn FileBacking>)> {
    let read: KArc<dyn FileBacking> = match KArc::try_new(PipeReadBacking {
        handle,
        object_charge: AliasOf {
            owner: "the pipe registry row",
        },
    }) {
        Ok(backing) => backing,
        Err(_) => {
            pipe_release_reader(handle);
            pipe_release_writer(handle);
            return None;
        }
    };
    let write: KArc<dyn FileBacking> = match KArc::try_new(PipeWriteBacking {
        handle,
        object_charge: AliasOf {
            owner: "the pipe registry row",
        },
    }) {
        Ok(backing) => backing,
        Err(_) => {
            drop(read);
            pipe_release_writer(handle);
            return None;
        }
    };
    Some((read, write))
}

/// Owner of one FIFO opener's ends and of its hold on the filesystem node.
///
/// The ends are released in `drop`, before the node reference field is: the
/// pipe is keyed on the node, so it must be gone before the inode can be freed
/// and its number reused.
#[derive(slopos_ostd::Charged)]
pub(crate) struct FifoBacking {
    handle: PipeHandle,
    reads: bool,
    writes: bool,
    #[expect(dead_code, reason = "held for ownership; dropping it closes the node")]
    node: KArc<dyn FileBacking>,
    object_charge: AliasOf,
}

slopos_ostd::charge_audit!(FifoBacking);

impl FileBacking for FifoBacking {}

impl Drop for FifoBacking {
    fn drop(&mut self) {
        if self.reads {
            pipe_release_reader(self.handle);
        }
        if self.writes {
            pipe_release_writer(self.handle);
        }
    }
}

/// Open the FIFO at `node` with Linux fifo(7)'s rules: every opener of the
/// node shares one pipe, which lives while any of them holds it; a reader
/// waits for a writer and a writer for a reader unless `nonblock`, when a
/// reader succeeds at once and a writer is `ENXIO`; `O_RDWR` never waits.
///
/// `vnode` is the opener's hold on the node, kept for as long as the ends are.
/// Answers the ops, handle and backing for the new description.
pub(crate) fn fifo_open(
    node: FifoNode,
    vnode: KArc<dyn FileBacking>,
    account: AccountId,
    reads: bool,
    writes: bool,
    nonblock: bool,
) -> Result<(&'static dyn FileOps, PipeHandle, KArc<dyn FileBacking>), Errno> {
    let joined = pipe::fifo_join(node, account, reads, writes, nonblock)?;
    let h = joined.handle;
    // A failed allocation drops the value, and its `Drop` retires the ends.
    let backing: KArc<dyn FileBacking> = KArc::try_new(FifoBacking {
        handle: h,
        reads,
        writes,
        node: vnode,
        object_charge: AliasOf {
            owner: "the pipe registry row",
        },
    })
    .map_err(|_| Errno::ENOMEM)?;

    if reads {
        BUS.publish(write_ev(h));
    }
    if writes {
        BUS.publish(read_ev(h));
    }

    if !joined.partner_present && !nonblock {
        if scheduler_is_enabled() == 0 {
            return Err(Errno::EAGAIN);
        }
        let own_end = if reads { read_ev(h) } else { write_ev(h) };
        let waited = BUS.subscribe(own_end).wait_event_interruptible(|| {
            pipe::fifo_partner_arrived(h, reads, joined.partner_opens)
        });
        if let Err(abort) = waited {
            return Err(interrupted_errno(abort));
        }
    }

    let ops: &'static dyn FileOps = match (reads, writes) {
        (true, true) => &PIPE_READ_WRITE_OPS,
        (true, false) => &PIPE_READ_OPS,
        _ => &PIPE_WRITE_OPS,
    };
    Ok((ops, h, backing))
}

fn pipe_release_reader(h: PipeHandle) {
    if h != PipeHandle::INVALID && pipe::retire_end(h, true) {
        BUS.publish(write_ev(h));
    }
}

fn pipe_release_writer(h: PipeHandle) {
    if h != PipeHandle::INVALID && pipe::retire_end(h, false) {
        BUS.publish(read_ev(h));
    }
}

/// What `fstat` says about any end: a named pipe's own node, and for an
/// anonymous one a FIFO whose size is what is buffered. A jobserver client
/// checks exactly this before trusting an inherited fd.
fn pipe_stat(handle: usize, out: &mut UserFsStat) -> i32 {
    let h = PipeHandle::from_usize(handle);
    let Some((buffered, fifo)) = pipe::with_pipe(h, |slot| (slot.len, slot.fifo)) else {
        return Errno::EBADF.raw();
    };
    *out = UserFsStat::default();
    if let Some(node) = fifo
        && let Ok(stat) = node.fs.stat(node.inode)
    {
        stat.fill_user_stat(out);
        return 0;
    }
    out.st_ino = handle as u64;
    out.st_nlink = 1;
    out.st_mode = S_IFIFO | 0o600;
    out.st_size = buffered as i64;
    out.st_blksize = pipe::PIPE_BUFFER_SIZE as i64;
    0
}

fn pipe_read(handle: usize, buf: &mut dyn IoBufWrite, flags: u32) -> isize {
    if buf.is_empty() {
        return 0;
    }
    let h = PipeHandle::from_usize(handle);
    let is_nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
    // Sized to the request, capped at the staging bound: a one-byte read
    // must not cost a 4 KiB kernel allocation.
    let mut local = match slopos_ostd::KVec::<u8>::zeroed(buf.len().min(IO_STAGING_SIZE)) {
        Ok(v) => v,
        Err(_) => return Errno::ENOMEM.as_isize(),
    };
    let mut total = 0usize;
    let mut remaining = buf.len();

    loop {
        let (consumed, no_writers, slot_gone) = pipe::with_pipe_mut(h, |slot| {
            let consumed = if remaining > 0 && slot.len > 0 {
                let chunk = remaining.min(local.len());
                slot.read_into(&mut local[..chunk])
            } else {
                0
            };
            (consumed, slot.writers == 0)
        })
        .map_or((0, true, true), |(consumed, no_writers)| {
            (consumed, no_writers, false)
        });

        if slot_gone {
            return if total > 0 {
                total as isize
            } else {
                Errno::EBADF.as_isize()
            };
        }

        if consumed > 0 {
            match buf.copy_in(total, &local[..consumed]) {
                Ok(n) => {
                    total += n;
                    remaining -= n;
                }
                Err(_) => {
                    return if total > 0 {
                        total as isize
                    } else {
                        Errno::EFAULT.as_isize()
                    };
                }
            }
            BUS.publish_one(write_ev(h));
            continue;
        }

        if total > 0 {
            return total as isize;
        }
        if no_writers {
            return 0;
        }
        if is_nonblock {
            return Errno::EAGAIN.as_isize();
        }
        if scheduler_is_enabled() == 0 {
            return Errno::EAGAIN.as_isize();
        }

        let waited = BUS.subscribe(read_ev(h)).wait_event_interruptible(|| {
            // A vanished slot falls out of the wait so the next
            // iteration's lookup reports EBADF.
            pipe::with_pipe(h, |slot| slot.len > 0 || slot.writers == 0).unwrap_or(true)
        });
        // Nothing transferred: the short-count return above already took
        // that case.
        if let Err(abort) = waited {
            return interrupted(abort);
        }
    }
}

/// Room in the buffer, or no reader left to fill it for: either lets a
/// blocked writer make progress, by pushing more or by reporting `EPIPE`.
fn pipe_writable_or_broken(h: PipeHandle) -> bool {
    pipe::with_pipe(h, |slot| {
        slot.len < pipe::PIPE_BUFFER_SIZE || slot.readers == 0
    })
    .unwrap_or(true)
}

impl FileOps for PipeReadOps {
    fn kind(&self) -> FileKind {
        FileKind::PipeRead
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        pipe_stat(handle, out)
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        pipe_read(handle, buf, flags)
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EBADF.as_isize()
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = PipeHandle::from_usize(handle);
        // Register FIRST, then check readiness.
        let registered = BUS.subscribe_current(read_ev(h));
        let revents =
            pipe::with_pipe(h, |slot| slot.revents(true, false, events)).unwrap_or(POLLERR);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        pipe::with_pipe(h, |slot| slot.revents(true, false, events)).unwrap_or(POLLERR)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        BUS.subscribe_current(read_ev(PipeHandle::from_usize(handle)))
    }

    fn poll_unwait(&self, handle: usize) {
        BUS.unsubscribe_current(read_ev(PipeHandle::from_usize(handle)));
    }
}

fn pipe_write(handle: usize, buf: &dyn IoBufRead, flags: u32) -> isize {
    if buf.is_empty() {
        return 0;
    }
    let h = PipeHandle::from_usize(handle);
    let is_nonblock = (flags & slopos_abi::syscall::O_NONBLOCK as u32) != 0;
    let buf_len = buf.len();
    let mut total = 0usize;
    let mut local = match slopos_ostd::KVec::<u8>::zeroed(IO_STAGING_SIZE) {
        Ok(v) => v,
        Err(_) => return Errno::ENOMEM.as_isize(),
    };

    let drain_or_close = || pipe_writable_or_broken(h);

    loop {
        let (can_write, no_readers, slot_gone) = pipe::with_pipe(h, |slot| {
            (slot.len < pipe::PIPE_BUFFER_SIZE, slot.readers == 0)
        })
        .map_or((false, true, true), |(can_write, no_readers)| {
            (can_write, no_readers, false)
        });

        if slot_gone {
            return if total > 0 {
                total as isize
            } else {
                Errno::EBADF.as_isize()
            };
        }
        if no_readers {
            return if total > 0 {
                total as isize
            } else {
                Errno::EPIPE.as_isize()
            };
        }

        if !can_write {
            if total >= buf_len {
                return total as isize;
            }
            if is_nonblock {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EAGAIN.as_isize()
                };
            }
            if scheduler_is_enabled() == 0 {
                return Errno::EAGAIN.as_isize();
            }
            if let Err(abort) = BUS
                .subscribe(write_ev(h))
                .wait_event_interruptible(drain_or_close)
            {
                return if total > 0 {
                    total as isize
                } else {
                    interrupted(abort)
                };
            }
            continue;
        }

        if total >= buf_len {
            return total as isize;
        }
        let chunk = (buf_len - total).min(local.len());
        let staged = match buf.copy_out(total, &mut local[..chunk]) {
            Ok(n) => n,
            Err(_) => {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EFAULT.as_isize()
                };
            }
        };
        if staged == 0 {
            return total as isize;
        }

        // Push under the slot lock, release it, and only then wake: the
        // reader's `wait_event` closure takes this slot lock under the
        // wait-queue lock, so waking while holding it is an AB-BA pair.
        enum PushOutcome {
            Wrote {
                written: usize,
                no_readers_after: bool,
            },
            NoReaders,
            Gone,
        }
        let outcome = pipe::with_pipe_mut(h, |slot| {
            if slot.readers == 0 {
                return PushOutcome::NoReaders;
            }
            let written = slot.write_from(&local[..staged]);
            PushOutcome::Wrote {
                written,
                no_readers_after: slot.readers == 0,
            }
        })
        .unwrap_or(PushOutcome::Gone);

        let (written, no_readers_after) = match outcome {
            PushOutcome::Wrote {
                written,
                no_readers_after,
            } => {
                total += written;
                (written, no_readers_after)
            }
            PushOutcome::NoReaders => {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EPIPE.as_isize()
                };
            }
            PushOutcome::Gone => {
                return if total > 0 {
                    total as isize
                } else {
                    Errno::EBADF.as_isize()
                };
            }
        };
        if written > 0 && !no_readers_after {
            BUS.publish_one(read_ev(h));
        }

        if total >= buf_len {
            return total as isize;
        }
        if is_nonblock {
            return if total > 0 {
                total as isize
            } else {
                Errno::EAGAIN.as_isize()
            };
        }
        if scheduler_is_enabled() == 0 {
            return Errno::EAGAIN.as_isize();
        }
        if let Err(abort) = BUS
            .subscribe(write_ev(h))
            .wait_event_interruptible(drain_or_close)
        {
            return if total > 0 {
                total as isize
            } else {
                interrupted(abort)
            };
        }
    }
}

impl FileOps for PipeWriteOps {
    fn kind(&self) -> FileKind {
        FileKind::PipeWrite
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        pipe_stat(handle, out)
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        Errno::EBADF.as_isize()
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, flags: u32) -> isize {
        pipe_write(handle, buf, flags)
    }

    fn poll_fused(&self, handle: usize, events: u16) -> slopos_abi::file_ops::FusedPollResult {
        let h = PipeHandle::from_usize(handle);
        let registered = BUS.subscribe_current(write_ev(h));
        let revents = pipe::with_pipe(h, |slot| slot.revents(false, true, events))
            .unwrap_or(POLLERR | POLLHUP);
        slopos_abi::file_ops::FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        pipe::with_pipe(h, |slot| slot.revents(false, true, events)).unwrap_or(POLLERR | POLLHUP)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        BUS.subscribe_current(write_ev(PipeHandle::from_usize(handle)))
    }

    fn poll_unwait(&self, handle: usize) {
        BUS.unsubscribe_current(write_ev(PipeHandle::from_usize(handle)));
    }
}

/// Reported as the read end: the kind decides nothing a read-write FIFO
/// description does differently, and poll registration goes through the ops.
impl FileOps for PipeReadWriteOps {
    fn kind(&self) -> FileKind {
        FileKind::PipeRead
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        pipe_stat(handle, out)
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, flags: u32) -> isize {
        pipe_read(handle, buf, flags)
    }

    fn write(&self, handle: usize, buf: &dyn IoBufRead, _offset: u64, flags: u32) -> isize {
        pipe_write(handle, buf, flags)
    }

    fn poll_fused(&self, handle: usize, events: u16) -> FusedPollResult {
        let registered = self.poll_wait(handle);
        FusedPollResult {
            revents: self.poll_events(handle, events),
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, events: u16) -> u16 {
        let h = PipeHandle::from_usize(handle);
        pipe::with_pipe(h, |slot| slot.revents(true, true, events)).unwrap_or(POLLERR | POLLHUP)
    }

    fn poll_wait(&self, handle: usize) -> bool {
        let h = PipeHandle::from_usize(handle);
        let on_read = BUS.subscribe_current(read_ev(h));
        let on_write = BUS.subscribe_current(write_ev(h));
        on_read || on_write
    }

    fn poll_unwait(&self, handle: usize) {
        let h = PipeHandle::from_usize(handle);
        BUS.unsubscribe_current(read_ev(h));
        BUS.unsubscribe_current(write_ev(h));
    }
}
