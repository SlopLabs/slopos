//! A queue pair kept apart for the panic path: created at probe with no
//! interrupt vector, its DMA pages allocated then, and driven by polling. It
//! is only ever taken with `try_lock`, so a panic that finds it in use gives
//! up rather than waits.

use slopos_nvme_core::command::Command;
use slopos_ostd::lock_class;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock, SpinLockGuard};

use super::io;
use super::ring::Ring;
use crate::block::engine::{BlkError, Namespace, Op, PAGE_SIZE, Request, RequestPages};

/// 64 KiB, a panic report and a kernel log tail in one command, where the
/// controller's largest transfer allows.
const PANIC_TRANSFER: usize = 16 * PAGE_SIZE;
const PANIC_TIMEOUT_MS: u32 = 5000;

struct PolledRing {
    ring: Ring,
    next_cid: u16,
    /// A command went unanswered and may still write the pages, so the queue
    /// is never used again.
    wedged: bool,
}

pub struct PanicQueue {
    ring: SpinLock<PolledRing>,
    pages: RequestPages,
    max_transfer: usize,
    flushes: bool,
}

impl PanicQueue {
    pub(super) fn new(ring: Ring, flushes: bool, max_transfer: usize) -> Option<Self> {
        let max_transfer = PANIC_TRANSFER.min(max_transfer);
        Some(Self {
            ring: SpinLock::new(
                PolledRing {
                    ring,
                    next_cid: 0,
                    wedged: false,
                },
                lock_class!("NvmePanic.ring", LOCK_LEVEL_RESOURCE),
            ),
            pages: RequestPages::allocate(max_transfer / PAGE_SIZE)?,
            max_transfer,
            flushes,
        })
    }

    /// The most one command moves.
    pub fn max_transfer(&self) -> usize {
        self.max_transfer
    }

    /// The queue, if nobody holds it and no command it was given is still
    /// unanswered. Interrupts stay off while it is held.
    pub fn take(&self) -> Option<PanicSession<'_>> {
        let ring = self.ring.try_lock()?;
        if ring.wedged {
            return None;
        }
        Some(PanicSession { ring, queue: self })
    }
}

/// Exclusive use of a [`PanicQueue`] until dropped.
pub struct PanicSession<'a> {
    ring: SpinLockGuard<'a, PolledRing>,
    queue: &'a PanicQueue,
}

impl PanicSession<'_> {
    /// Write whole blocks at `offset` of `ns`, and wait for them by polling.
    pub fn write(&mut self, ns: Namespace, offset: u64, src: &[u8]) -> Result<(), BlkError> {
        self.check(ns, offset, src.len())?;
        // An unanswered command may still be reading the pages.
        if self.ring.wedged {
            return Err(BlkError::Timeout);
        }
        if !self.queue.pages.copy_in(0, src) {
            return Err(BlkError::BadRequest);
        }
        self.execute(&Request {
            op: Op::Write,
            ns,
            offset,
            len: src.len(),
        })
    }

    /// Read whole blocks at `offset` of `ns` into `dst`.
    pub fn read(&mut self, ns: Namespace, offset: u64, dst: &mut [u8]) -> Result<(), BlkError> {
        self.check(ns, offset, dst.len())?;
        self.execute(&Request {
            op: Op::Read,
            ns,
            offset,
            len: dst.len(),
        })?;
        if !self.queue.pages.copy_out(0, dst) {
            return Err(BlkError::BadRequest);
        }
        Ok(())
    }

    /// Empty the volatile write cache, if the controller has one.
    pub fn flush(&mut self, ns: Namespace) -> Result<(), BlkError> {
        if !self.queue.flushes {
            return Ok(());
        }
        self.execute(&Request {
            op: Op::Flush,
            ns,
            offset: 0,
            len: 0,
        })
    }

    /// One command, polled to completion.
    fn execute(&mut self, req: &Request) -> Result<(), BlkError> {
        if self.ring.wedged {
            return Err(BlkError::Timeout);
        }
        let cmd: Command = io::build(req, &self.queue.pages).ok_or(BlkError::BadRequest)?;
        let polled = &mut *self.ring;
        let cid = polled.next_cid;
        polled.next_cid = (cid + 1) & 0x7FFF;
        if !polled.ring.push(&cmd.with_cid(cid)) {
            return Err(BlkError::Busy);
        }
        let mut status = None;
        crate::hpet::spin_until(
            &mut || {
                while let Some(c) = polled.ring.pop() {
                    if c.cid == cid {
                        status = Some(c.status);
                    }
                }
                status.is_some()
            },
            PANIC_TIMEOUT_MS,
        );
        polled.ring.release();
        polled.wedged = status.is_none();
        io::outcome(status.ok_or(BlkError::Timeout)?)
    }

    fn check(&self, ns: Namespace, offset: u64, len: usize) -> Result<(), BlkError> {
        let block = 1u64 << ns.block_shift;
        if len == 0 || len > self.queue.max_transfer {
            return Err(BlkError::BadRequest);
        }
        if !offset.is_multiple_of(block) || !(len as u64).is_multiple_of(block) {
            return Err(BlkError::BadRequest);
        }
        Ok(())
    }
}
