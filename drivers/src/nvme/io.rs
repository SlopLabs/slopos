//! The I/O queue pair as a block-layer transport.

use slopos_nvme_core::command::{self, Command};
use slopos_nvme_core::completion::{Disposition, Status};

use super::ring::Ring;
use crate::block::engine::{BlkError, Op, PAGE_SIZE, QueueOps, Request, RequestPages};

/// Commands the queue may hold at once: every engine slot and every request
/// a timeout quarantined.
const MAX_OUTSTANDING: usize = 16;
/// CIDs cycle through `0..CID_SPACE`, far above what can be outstanding, so a
/// quarantined command's CID is not handed out again while it may complete.
const CID_SPACE: u16 = 0x8000;

pub struct IoQueue {
    ring: Ring,
    next_cid: u16,
    outstanding: [Option<u16>; MAX_OUTSTANDING],
}

impl IoQueue {
    pub fn new(ring: Ring) -> Self {
        Self {
            ring,
            next_cid: 0,
            outstanding: [None; MAX_OUTSTANDING],
        }
    }

    fn allocate_cid(&mut self) -> Option<u16> {
        let free = self.outstanding.iter().position(Option::is_none)?;
        let mut cid = self.next_cid;
        while self.outstanding.contains(&Some(cid)) {
            cid = (cid + 1) % CID_SPACE;
        }
        self.next_cid = (cid + 1) % CID_SPACE;
        self.outstanding[free] = Some(cid);
        Some(cid)
    }

    fn free_cid(&mut self, cid: u16) {
        if let Some(entry) = self.outstanding.iter_mut().find(|e| **e == Some(cid)) {
            *entry = None;
        }
    }
}

/// The command for `req`, its data described by PRPs over the slot's pages.
/// A transfer of more than two pages lists the rest in the slot's own page.
pub fn build(req: &Request, pages: &RequestPages) -> Option<Command> {
    let shift = req.ns.block_shift;
    let lba = req.offset >> shift;
    let blocks = u16::try_from(req.len >> shift).ok()?;
    if req.op != Op::Flush && (blocks == 0 || req.len.div_ceil(PAGE_SIZE) > pages.data.len()) {
        return None;
    }
    let cmd = match req.op {
        Op::Flush => return Some(Command::flush(req.ns.nsid)),
        Op::Read => Command::read(req.ns.nsid, lba, blocks),
        Op::Write => Command::write(req.ns.nsid, lba, blocks),
    };
    let mut phys = [0u64; crate::block::engine::MAX_DATA_PAGES];
    let data = pages.span(req.len);
    for (slot, page) in phys.iter_mut().zip(data) {
        *slot = page.phys_u64();
    }
    let phys = &phys[..data.len()];
    for (i, entry) in command::prp_list(phys).iter().enumerate() {
        if !pages.aux.write_at(i * 8, entry) {
            return None;
        }
    }
    let (prp1, prp2) = command::prp_pair(phys, pages.aux.phys_u64());
    Some(cmd.with_prp(prp1, prp2))
}

/// What a completion's status means for its request.
pub fn outcome(status: Status) -> Result<(), BlkError> {
    match status.disposition() {
        Disposition::Done => Ok(()),
        Disposition::Unsupported => Err(BlkError::Unsupported),
        Disposition::OutOfRange => Err(BlkError::OutOfBounds),
        Disposition::Retry => Err(BlkError::DeviceFault { retry: true }),
        Disposition::Failed => Err(BlkError::DeviceFault { retry: false }),
    }
}

impl QueueOps for IoQueue {
    fn submit(&mut self, req: &Request, pages: &RequestPages) -> Result<u16, BlkError> {
        if self.ring.full() {
            return Err(BlkError::Busy);
        }
        let cmd = build(req, pages).ok_or(BlkError::BadRequest)?;
        let cid = self.allocate_cid().ok_or(BlkError::Busy)?;
        if !self.ring.push(&cmd.with_cid(cid)) {
            self.free_cid(cid);
            return Err(BlkError::Busy);
        }
        Ok(cid)
    }

    fn pop(&mut self) -> Option<(u16, u32)> {
        self.ring.pop().map(|c| (c.cid, u32::from(c.status.0)))
    }

    fn end_harvest(&mut self) {
        self.ring.release();
    }

    fn retire(&mut self, cid: u16) {
        self.free_cid(cid);
    }

    fn outcome(&self, _pages: &RequestPages, status: u32) -> Result<(), BlkError> {
        outcome(Status(status as u16))
    }
}
