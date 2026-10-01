//! The admin queue: one command at a time, each waited out before the next is
//! sent. Admin commands run at probe and at shutdown, so the simplicity is
//! worth more than the concurrency.

use core::cell::Cell;
use slopos_mm::page_alloc::OwnedPageFrame;
use slopos_nvme_core::command::Command;
use slopos_nvme_core::completion::Completion;
use slopos_nvme_core::identify::IDENTIFY_BYTES;

use slopos_ostd::mm::AllocError;
use slopos_ostd::mm::init::{Init, Initialised, SlotPtr, init_struct_with};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, MutexGuard, SpinLock, WaitAbort, WaitQueue};
use slopos_ostd::{KVec, lock_class, write_field};

use super::ring::Ring;
use crate::hpet::poll_wait;

/// Identify and queue creation are otherwise microseconds, but a controller
/// recovering from an unsafe shutdown may hold its first commands for tens of
/// seconds.
const ADMIN_TIMEOUT_MS: u32 = 60_000;
/// What the shutdown path gives one command: the power goes regardless.
const POLLED_TIMEOUT_MS: u32 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminError {
    /// The queue was full or the entry could not be written.
    Submit,
    /// An earlier command is still unanswered, so this one was not sent.
    Blocked,
    /// No completion before the waiter gave up, at its deadline or killed.
    /// The command may still run, and the queue sends nothing more until its
    /// completion turns up.
    Unanswered,
    /// The controller completed it with this status.
    Failed(u16),
    NoMemory,
}

/// Whose turn it is to send a command, and what the last one left behind.
struct Turn {
    next_cid: u16,
    /// A command whose completion has not turned up: the controller may still
    /// write what it was given — the identify page — so nothing more is sent.
    unanswered: Option<u16>,
}

#[derive(slopos_ostd::SlotFields)]
pub struct AdminQueue {
    ring: SpinLock<Ring>,
    /// Serialises commands, so the one completion outstanding is the caller's.
    turn: Mutex<Turn>,
    waiters: WaitQueue,
    /// Identify's 4 KiB transfer lands here.
    page: OwnedPageFrame,
}

impl AdminQueue {
    /// Built in place: the locks and wait queues make it too large to stage
    /// on a probe's stack.
    pub fn init(ring: Ring) -> impl Init<Self, AllocError> {
        init_struct_with(
            move |slot: SlotPtr<Self>| -> Result<Initialised<Self>, AllocError> {
                let page = OwnedPageFrame::alloc_zeroed().ok_or(AllocError)?;
                write_field!(
                    slot,
                    ring,
                    SpinLock::new(ring, lock_class!("NvmeAdmin.ring", LOCK_LEVEL_RESOURCE))
                );
                write_field!(
                    slot,
                    turn,
                    Mutex::new(
                        Turn {
                            next_cid: 0,
                            unanswered: None,
                        },
                        lock_class!("NvmeAdmin.turn", LOCK_LEVEL_RESOURCE)
                    )
                );
                write_field!(
                    slot,
                    waiters,
                    WaitQueue::new(lock_class!("NvmeAdmin.waiters", LOCK_LEVEL_RESOURCE))
                );
                write_field!(slot, page, page);
                Ok(slot.finish())
            },
        )
    }

    pub fn depth(&self) -> u16 {
        self.ring.lock().depth()
    }

    pub fn sq_phys(&self) -> u64 {
        self.ring.lock().sq_phys()
    }

    pub fn cq_phys(&self) -> u64 {
        self.ring.lock().cq_phys()
    }

    /// The admin completion queue's interrupt.
    pub fn handle_irq(&self) {
        let _ = self.waiters.wake_all();
    }

    fn take_turn(&self) -> Result<MutexGuard<'_, Turn>, AdminError> {
        let mut turn = self.turn.lock().map_err(|_| AdminError::Submit)?;
        if let Some(cid) = turn.unanswered {
            if self.collect(cid).is_none() {
                return Err(AdminError::Blocked);
            }
            turn.unanswered = None;
        }
        Ok(turn)
    }

    /// Send `cmd` and wait for its completion.
    pub fn run(&self, cmd: Command) -> Result<Completion, AdminError> {
        let mut turn = self.take_turn()?;
        self.execute(&mut turn, cmd)
    }

    /// Identify into the queue's page; the structure is copied out before the
    /// next command may overwrite it.
    pub fn identify(&self, cmd: Command) -> Result<KVec<u8>, AdminError> {
        let mut out = KVec::zeroed(IDENTIFY_BYTES).map_err(|_| AdminError::NoMemory)?;
        let mut turn = self.take_turn()?;
        self.execute(&mut turn, cmd.with_prp(self.page.phys_u64(), 0))?;
        if !self.page.read_slice(0, &mut out) {
            return Err(AdminError::NoMemory);
        }
        Ok(out)
    }

    /// Send `cmd` and poll for its completion, sleeping on nothing: the
    /// shutdown path runs while tasks are being torn down.
    pub fn run_polled(&self, cmd: Command) -> Result<Completion, AdminError> {
        let mut turn = self.take_turn()?;
        let cid = self.submit(&mut turn, cmd)?;
        let mut done = None;
        crate::hpet::spin_until(
            &mut || {
                done = self.collect(cid);
                done.is_some()
            },
            POLLED_TIMEOUT_MS,
        );
        answered(&mut turn, cid, done)
    }

    fn submit(&self, turn: &mut Turn, cmd: Command) -> Result<u16, AdminError> {
        let cid = turn.next_cid;
        turn.next_cid = (cid + 1) & 0x7FFF;
        if !self.ring.lock().push(&cmd.with_cid(cid)) {
            return Err(AdminError::Submit);
        }
        Ok(cid)
    }

    /// Drain the completion queue, keeping `cid`'s entry: only one command
    /// is outstanding, so any other is one a waiter gave up on.
    fn collect(&self, cid: u16) -> Option<Completion> {
        let mut ring = self.ring.lock();
        let mut mine = None;
        let mut popped = false;
        while let Some(c) = ring.pop() {
            popped = true;
            if c.cid == cid {
                mine = Some(c);
            }
        }
        if popped {
            ring.release();
        }
        mine
    }

    fn execute(&self, turn: &mut Turn, cmd: Command) -> Result<Completion, AdminError> {
        let cid = self.submit(turn, cmd)?;
        let collect = || self.collect(cid);
        let done = match self
            .waiters
            .wait_event_timeout_until(collect, u64::from(ADMIN_TIMEOUT_MS))
        {
            Ok(c) => Some(c),
            Err(WaitAbort::NoRuntime) => {
                let got = Cell::new(None);
                poll_wait(
                    &|| {
                        if got.get().is_none() {
                            got.set(collect());
                        }
                        got.get().is_some()
                    },
                    ADMIN_TIMEOUT_MS,
                );
                got.get()
            }
            Err(_) => collect(),
        };
        answered(turn, cid, done)
    }
}

fn answered(turn: &mut Turn, cid: u16, done: Option<Completion>) -> Result<Completion, AdminError> {
    let Some(completion) = done else {
        turn.unanswered = Some(cid);
        return Err(AdminError::Unanswered);
    };
    checked(completion)
}

fn checked(completion: Completion) -> Result<Completion, AdminError> {
    if !completion.status.is_success() {
        return Err(AdminError::Failed(completion.status.0));
    }
    Ok(completion)
}
