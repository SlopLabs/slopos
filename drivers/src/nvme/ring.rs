//! One submission queue and the completion queue it posts to, each a single
//! zeroed page: 64 submission entries or 256 completion entries fit one, and
//! a single page is physically contiguous whatever CAP.CQR demands.

use core::sync::atomic::{Ordering, fence};

use slopos_mm::mmio::MmioRegion;
use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_nvme_core::command::{Command, SQE_BYTES};
use slopos_nvme_core::completion::{CQE_BYTES, Completion};
use slopos_nvme_core::regs::{self, Cap, PAGE_SIZE};

/// The deepest queue one page holds.
pub const MAX_DEPTH: u16 = (PAGE_SIZE / SQE_BYTES) as u16;

pub struct Ring {
    regs: MmioRegion,
    sq: OwnedPageFrame,
    cq: OwnedPageFrame,
    depth: u16,
    sq_tail: u16,
    /// The last SQ head the controller reported: entries behind it are free.
    sq_head: u16,
    cq_head: u16,
    phase: bool,
    sq_doorbell: usize,
    cq_doorbell: usize,
}

impl Ring {
    pub fn new(regs: &MmioRegion, cap: Cap, qid: u16, depth: u16) -> Option<Self> {
        Some(Self {
            regs: regs.clone(),
            sq: OwnedPageFrame::alloc_zeroed()?,
            cq: OwnedPageFrame::alloc_zeroed()?,
            depth: depth.clamp(2, MAX_DEPTH),
            sq_tail: 0,
            sq_head: 0,
            cq_head: 0,
            phase: true,
            sq_doorbell: regs::sq_tail_doorbell(cap, qid),
            cq_doorbell: regs::cq_head_doorbell(cap, qid),
        })
    }

    pub fn depth(&self) -> u16 {
        self.depth
    }

    pub fn sq_phys(&self) -> u64 {
        self.sq.phys_u64()
    }

    pub fn cq_phys(&self) -> u64 {
        self.cq.phys_u64()
    }

    /// An N-entry queue holds N−1 commands: a tail one behind the head would
    /// read as empty.
    pub fn full(&self) -> bool {
        (self.sq_tail + 1) % self.depth == self.sq_head
    }

    /// Write `cmd` at the tail and ring the doorbell; `false` when full.
    pub fn push(&mut self, cmd: &Command) -> bool {
        if self.full() {
            return false;
        }
        let at = usize::from(self.sq_tail) * SQE_BYTES;
        if !self.sq.write_at(at, &cmd.dw) {
            return false;
        }
        self.sq_tail = (self.sq_tail + 1) % self.depth;
        // The entry must be in memory before the controller learns of it.
        fence(Ordering::Release);
        self.regs
            .write::<u32>(self.sq_doorbell, u32::from(self.sq_tail));
        true
    }

    /// The next completion the controller posted, if its phase tag says it
    /// is new. The head doorbell is left to [`Self::release`].
    pub fn pop(&mut self) -> Option<Completion> {
        let at = usize::from(self.cq_head) * CQE_BYTES;
        let dw3 = self.cq.read_volatile_at::<u32>(at + 12)?;
        if (dw3 & (1 << 16) != 0) != self.phase {
            return None;
        }
        // The controller writes the phase tag last: nothing before it may be
        // read ahead of it.
        fence(Ordering::Acquire);
        let dw = [
            self.cq.read_volatile_at::<u32>(at)?,
            self.cq.read_volatile_at::<u32>(at + 4)?,
            self.cq.read_volatile_at::<u32>(at + 8)?,
            dw3,
        ];
        let completion = Completion::from_dwords(dw);
        self.cq_head += 1;
        if self.cq_head == self.depth {
            self.cq_head = 0;
            self.phase = !self.phase;
        }
        if completion.sq_head < self.depth {
            self.sq_head = completion.sq_head;
        }
        Some(completion)
    }

    /// Hand the consumed completion entries back to the controller.
    pub fn release(&self) {
        self.regs
            .write::<u32>(self.cq_doorbell, u32::from(self.cq_head));
    }
}
