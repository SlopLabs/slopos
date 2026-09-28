//! Kernel pipe implementation.
//!
//! Pipes live in a single [`HandleTable`] behind one [`SpinLock`], each owning a
//! heap ring buffer. Handles are generation-checked, so one left over from a
//! recycled slot resolves to an error rather than aliasing the new pipe.
//!
//! Blocking and wakeups go through the kernel event bus keyed by
//! `KernelEvent::PipeRead` / `KernelEvent::PipeWrite`. The accessors below
//! never let a table guard escape their closure, so a sleeper or waker
//! never holds the table lock across a wait-queue operation.

use slopos_abi::Errno;
use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use slopos_ostd::KVec;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::lock_class;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, try_charge};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::vfs::traits::same_filesystem;
use crate::vfs::{FileSystem, InodeId};

pub(crate) use slopos_abi::event::MAX_PIPES;
pub(crate) const PIPE_BUFFER_SIZE: usize = 4096;

/// Slot-index width in the packed handle encoding; the rest is generation.
const SLOT_BITS: u32 = MAX_PIPES.trailing_zeros();
const _: () = assert!(MAX_PIPES.is_power_of_two());

/// Opaque handle identifying a kernel pipe.
///
/// `OpenFile::handle` is one `usize` shared by every file backend, so the
/// encoding packs into it as `(generation << SLOT_BITS) | slot_index`; the low
/// bits are the slot, which also keys the event bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct PipeHandle(u64);

impl PipeHandle {
    pub const INVALID: Self = Self(u64::MAX);

    pub(crate) fn pack(h: Handle<Pipe>) -> Self {
        Self(h.pack(SLOT_BITS) as u64)
    }

    /// `None` for the sentinel or an out-of-range slot.
    pub(crate) fn to_internal(self) -> Option<Handle<Pipe>> {
        if self == Self::INVALID {
            return None;
        }
        let h = Handle::unpack(self.0 as usize, SLOT_BITS);
        if h.slot() as usize >= MAX_PIPES {
            return None;
        }
        Some(h)
    }

    /// The slot index — also the event-bus key for this pipe.
    pub(crate) fn slot(self) -> usize {
        Handle::<Pipe>::unpack(self.0 as usize, SLOT_BITS).slot() as usize
    }

    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    pub fn from_usize(v: usize) -> Self {
        Self(v as u64)
    }
}

/// The node a named pipe belongs to. Every opener holds it open, so its inode
/// number cannot be reused while a pipe carries this key.
#[derive(Clone, Copy)]
pub(crate) struct FifoNode {
    pub(crate) fs: &'static dyn FileSystem,
    pub(crate) inode: InodeId,
}

impl FifoNode {
    fn names(&self, other: &FifoNode) -> bool {
        self.inode == other.inode && same_filesystem(self.fs, other.fs)
    }
}

pub(crate) struct Pipe {
    read_pos: usize,
    write_pos: usize,
    pub(crate) len: usize,
    pub(crate) readers: u32,
    pub(crate) writers: u32,
    /// Opens of each end ever, so a blocked FIFO opener sees a partner that
    /// came and went before it looked.
    pub(crate) reader_opens: u32,
    pub(crate) writer_opens: u32,
    pub(crate) fifo: Option<FifoNode>,
    buffer: KVec<u8>,
    /// The registry row and its buffer, charged to the pipe's creator.
    ///
    /// Here rather than in either backing: a pipe has **two** backings releasing
    /// into **one** slot, so a charge in each would refund twice, and a charge
    /// in one would refund while the object was still alive behind the other.
    #[expect(dead_code, reason = "held for ownership; dropping it is the refund")]
    object_charge: Charge<ObjectRow>,
}

impl Pipe {
    fn new(buffer: KVec<u8>, object_charge: Charge<ObjectRow>) -> Self {
        Self {
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 0,
            writers: 0,
            reader_opens: 0,
            writer_opens: 0,
            fifo: None,
            buffer,
            object_charge,
        }
    }

    fn attach(&mut self, reads: bool, writes: bool) {
        if reads {
            self.readers += 1;
            self.reader_opens = self.reader_opens.wrapping_add(1);
        }
        if writes {
            self.writers += 1;
            self.writer_opens = self.writer_opens.wrapping_add(1);
        }
    }

    /// Consumes into the kernel staging buffer `out`; the caller transfers to
    /// userspace *after* releasing the table lock.
    pub(crate) fn read_into(&mut self, out: &mut [u8]) -> usize {
        let mut copied = 0usize;
        while copied < out.len() && self.len > 0 {
            out[copied] = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % PIPE_BUFFER_SIZE;
            self.len -= 1;
            copied += 1;
        }
        copied
    }

    pub(crate) fn write_from(&mut self, input: &[u8]) -> usize {
        let mut written = 0usize;
        while written < input.len() && self.len < PIPE_BUFFER_SIZE {
            self.buffer[self.write_pos] = input[written];
            self.write_pos = (self.write_pos + 1) % PIPE_BUFFER_SIZE;
            self.len += 1;
            written += 1;
        }
        written
    }

    pub(crate) fn revents(&self, is_read_end: bool, is_write_end: bool, events: u16) -> u16 {
        let mut revents = 0u16;

        if is_read_end {
            if self.len > 0 {
                revents |= events & (POLLIN | POLLPRI);
            }
            if self.writers == 0 {
                revents |= POLLHUP;
                if (events & POLLIN) != 0 {
                    revents |= POLLIN;
                }
            }
        }

        if is_write_end {
            if self.readers == 0 {
                revents |= POLLERR | POLLHUP;
            } else if self.len < PIPE_BUFFER_SIZE {
                revents |= events & POLLOUT;
            }
        }

        revents
    }
}

/// All live pipes, capped at [`MAX_PIPES`] by [`alloc_slot`] so slot indices
/// stay within [`PipeHandle`]'s [`SLOT_BITS`]-wide field and the event-bus key.
static PIPE_TABLE: SpinLock<HandleTable<Pipe>> = SpinLock::new(
    HandleTable::new(),
    lock_class!("PIPE_TABLE", LOCK_LEVEL_RESOURCE),
);

/// The ring buffer is allocated before the table lock is taken, so the locked
/// region stays allocation-light. `None` if the pipe table is full.
pub(crate) fn alloc_slot(account: AccountId) -> Option<PipeHandle> {
    let pipe = new_pipe(account)?;
    let mut table = PIPE_TABLE.lock();
    if table.len() >= MAX_PIPES {
        return None;
    }
    let handle = table.insert(pipe).ok()?;
    Some(PipeHandle::pack(handle))
}

fn new_pipe(account: AccountId) -> Option<Pipe> {
    let buffer = KVec::<u8>::zeroed(PIPE_BUFFER_SIZE).ok()?;
    // Charged before the table lock: a refusal must not unwind under it.
    let reservation = try_charge::<ObjectRow>(account, 1).ok()?;
    Some(Pipe::new(buffer, Charge::commit(reservation)))
}

pub(crate) struct FifoJoined {
    pub(crate) handle: PipeHandle,
    /// The partner end's open count at the join, for [`fifo_partner_arrived`].
    pub(crate) partner_opens: u32,
    pub(crate) partner_present: bool,
}

/// Join, or create, the pipe `node`'s openers share, as the ends asked for. A
/// non-blocking writer with no reader is `ENXIO`, per Linux fifo(7).
pub(crate) fn fifo_join(
    node: FifoNode,
    account: AccountId,
    reads: bool,
    writes: bool,
    nonblock: bool,
) -> Result<FifoJoined, Errno> {
    let mut fresh: Option<Pipe> = None;
    loop {
        let mut table = PIPE_TABLE.lock();
        let found = table
            .iter_mut()
            .find(|(_, p)| p.fifo.as_ref().is_some_and(|f| f.names(&node)));
        let (handle, pipe) = match found {
            Some(entry) => entry,
            None => {
                if writes && !reads && nonblock {
                    return Err(Errno::ENXIO);
                }
                let Some(mut pipe) = fresh.take() else {
                    drop(table);
                    fresh = Some(new_pipe(account).ok_or(Errno::ENOMEM)?);
                    continue;
                };
                if table.len() >= MAX_PIPES {
                    return Err(Errno::ENFILE);
                }
                pipe.fifo = Some(node);
                let handle = table.insert(pipe).map_err(|_| Errno::ENFILE)?;
                let pipe = table.get_mut(handle).map_err(|_| Errno::ENFILE)?;
                (handle, pipe)
            }
        };
        if writes && !reads && nonblock && pipe.readers == 0 {
            return Err(Errno::ENXIO);
        }
        pipe.attach(reads, writes);
        let (partner_opens, partner_present) = if reads && !writes {
            (pipe.writer_opens, pipe.writers > 0)
        } else if writes && !reads {
            (pipe.reader_opens, pipe.readers > 0)
        } else {
            (0, true)
        };
        return Ok(FifoJoined {
            handle: PipeHandle::pack(handle),
            partner_opens,
            partner_present,
        });
    }
}

/// Whether a blocked FIFO opener's partner is present or came and went since
/// the join; `reader` is the waiter's end. A vanished pipe releases it.
pub(crate) fn fifo_partner_arrived(handle: PipeHandle, reader: bool, opens_at_join: u32) -> bool {
    with_pipe(handle, |p| {
        if reader {
            p.writers > 0 || p.writer_opens != opens_at_join
        } else {
            p.readers > 0 || p.reader_opens != opens_at_join
        }
    })
    .unwrap_or(true)
}

/// `None` if the handle is stale, recycled, or invalid.
pub(crate) fn with_pipe<R>(handle: PipeHandle, f: impl FnOnce(&Pipe) -> R) -> Option<R> {
    let internal = handle.to_internal()?;
    let table = PIPE_TABLE.lock();
    let pipe = table.get(internal).ok()?;
    Some(f(pipe))
}

/// Callers must perform any wait or wake outside this closure.
pub(crate) fn with_pipe_mut<R>(handle: PipeHandle, f: impl FnOnce(&mut Pipe) -> R) -> Option<R> {
    let internal = handle.to_internal()?;
    let mut table = PIPE_TABLE.lock();
    let pipe = table.get_mut(internal).ok()?;
    Some(f(pipe))
}

/// Retire one end, removing the pipe once neither end is held; answers whether
/// it was the last of this end. One lock hold, so no opener joins a dying pipe.
pub(crate) fn retire_end(handle: PipeHandle, reader: bool) -> bool {
    let Some(internal) = handle.to_internal() else {
        return false;
    };
    let mut table = PIPE_TABLE.lock();
    let Ok(pipe) = table.get_mut(internal) else {
        return false;
    };
    let count = if reader {
        &mut pipe.readers
    } else {
        &mut pipe.writers
    };
    if *count == 0 {
        return false;
    }
    *count -= 1;
    let last_of_end = *count == 0;
    if pipe.readers == 0 && pipe.writers == 0 {
        let _ = table.remove(internal);
    }
    last_of_end
}

/// Bumps the slot generation, so any surviving handle to it becomes stale.
pub(crate) fn free_slot(handle: PipeHandle) {
    if let Some(internal) = handle.to_internal() {
        let mut table = PIPE_TABLE.lock();
        let _ = table.remove(internal);
    }
}
