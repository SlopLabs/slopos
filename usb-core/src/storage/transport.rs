//! The Bulk-Only transport as a state machine over a device's bulk pipes and
//! EP0, which a [`Wire`] reaches: the kernel's rings or the simulator.
//!
//! One command is on the wire at a time, as a Command Block Wrapper, its data
//! stage and a Command Status Wrapper; the rest wait in the queue. Submission
//! and the drain move a command along and never wait. What needs commands and
//! control requests the device's own completion cannot carry — clearing a
//! halted pipe, Reset Recovery (§5.3.4), a command past its deadline — is left
//! to [`Transport::serve`], which the USB thread runs.

use super::bot::{self, BadStatus, CBW_LEN, CSW_LEN, Direction, Status};
use super::scsi::{self, Command, Sense, Verdict, sense_key};
use crate::device::request::Setup;
use crate::hub;
use crate::xhci::ring::{CommandResult, SubmitError, Ticket};
use crate::xhci::transfer::{PushError, Transfer, TransferError, TransferResult};
use crate::xhci::trb::CompletionCode;

/// Commands waiting or on the wire: two engine slots, two the engine
/// quarantined, a direct command and one spare.
pub const QUEUE: usize = 6;
/// A command's USB-side deadline, counted from its CBW.
pub const COMMAND_MS: u64 = 20_000;
/// Each recovery step's: a control request's (USB 2.0 §9.2.6.4), or a
/// command's, which a working controller completes in milliseconds.
const STEP_MS: u64 = 5000;
/// Reset Recovery's longest run: two steps per pipe to quiesce it, the reset,
/// a transaction translator's clear and a halt's per pipe, the reconfigure.
pub const RECOVERY_MS: u64 = 10 * STEP_MS;
/// A command, its recovery, its re-issue and that one's recovery.
pub const TAG_MS: u64 = 2 * (COMMAND_MS + RECOVERY_MS);

/// Where the wire page holds each wrapper, the data of a direct command, and
/// sense data, which never lands on a direct command's data.
pub const CBW_AT: usize = 0;
pub const CSW_AT: usize = 64;
pub const SCRATCH_AT: usize = 128;
pub const SCRATCH_LEN: usize = 256;
pub const SENSE_AT: usize = SCRATCH_AT + SCRATCH_LEN;

/// Below the tags the engine's test hooks stage (`0xfff0` up).
const MAX_TAG: u16 = 0xffe0;
const STATUS_RETRIES: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipe {
    In,
    Out,
}

impl Pipe {
    pub fn bit(self) -> u8 {
        match self {
            Pipe::In => 1,
            Pipe::Out => 2,
        }
    }
}

/// What a TD moves: a wrapper, the scratch area, or the pages an engine
/// request was staged in, held by its queue slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Buffer {
    Wrapper,
    Status,
    Scratch,
    Sense,
    Held(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeCommand {
    /// Reset Endpoint: a halted pipe stops, its toggle back at zero.
    Reset(Pipe),
    Stop(Pipe),
    /// Configure Endpoint dropping and adding the pipes in the mask, each
    /// added with its dequeue at its ring's enqueue pointer: the host's
    /// toggle reset and whatever the ring held skipped.
    Reconfigure(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Device,
    /// The high-speed hub whose transaction translator a full- or low-speed
    /// device is reached through.
    TranslatorHub,
}

/// The device's pipes, EP0 and the controller's commands.
pub trait Wire {
    fn now_ms(&mut self) -> u64;
    /// The device left, or its controller died.
    fn gone(&mut self) -> bool;
    /// Keep `pages` for queue slot `slot`'s data stage.
    fn hold(&mut self, slot: u8, pages: &[u64]);
    fn write_wrapper(&mut self, cbw: &[u8; CBW_LEN]);
    /// Bytes of the wire page from offset `at`: [`CSW_AT`], [`SCRATCH_AT`]
    /// or [`SENSE_AT`].
    fn read_wire(&mut self, at: usize, out: &mut [u8]);
    fn bulk(&mut self, pipe: Pipe, buffer: Buffer, length: u32) -> Result<Transfer, PushError>;
    fn bulk_result(&mut self, pipe: Pipe, transfer: Transfer) -> Option<TransferResult>;
    /// The pipe takes nothing more until a recovery moves its ring past it.
    fn abandon(&mut self, pipe: Pipe, transfer: Transfer);
    /// A request with no data stage.
    fn control(&mut self, target: Target, setup: Setup) -> Result<Transfer, PushError>;
    fn control_result(&mut self, target: Target, transfer: Transfer) -> Option<TransferResult>;
    fn abandon_control(&mut self, target: Target, transfer: Transfer);
    fn command(&mut self, command: PipeCommand) -> Result<Ticket, SubmitError>;
    fn command_result(&mut self, ticket: Ticket) -> Option<CommandResult>;
    fn abandon_command(&mut self, ticket: Ticket);
    /// A command never completed: the controller is taken for dead.
    fn stuck(&mut self);
    /// The ring of the pipe runs again, empty.
    fn recovered(&mut self, pipe: Pipe);
    /// Recovery failed: the port is reset and the device enumerated again.
    fn escalate(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interface {
    pub interface: u8,
    pub bulk_in: u8,
    pub bulk_out: u8,
    /// The device's address, when a transaction translator is between it
    /// and the controller.
    pub translated: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// May succeed if sent again.
    Retry,
    Fail,
    Gone,
}

impl Outcome {
    pub fn code(self) -> u32 {
        self as u32
    }

    pub fn from_code(code: u32) -> Option<Self> {
        [Outcome::Ok, Outcome::Retry, Outcome::Fail, Outcome::Gone]
            .into_iter()
            .find(|o| o.code() == code)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Done {
    pub tag: u16,
    pub outcome: Outcome,
    /// Of its data stage, as the device's residue states it.
    pub moved: u32,
    pub sense: Option<Sense>,
    direct: bool,
}

#[derive(Clone, Copy, Debug)]
struct Queued {
    tag: u16,
    lun: u8,
    command: Command,
    direct: bool,
    held: bool,
    order: u32,
    /// Its submitter let it go: it runs, and is reported to nobody.
    retired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Wrapper(Transfer),
    Data(Transfer),
    Status(Transfer),
    /// Waiting for [`Transport::serve`].
    Recovering,
}

/// What the original command's CSW said, kept while REQUEST SENSE runs.
#[derive(Clone, Copy, Debug)]
struct Sensing {
    moved: u32,
}

#[derive(Clone, Copy, Debug)]
struct Current {
    slot: u8,
    phase: Phase,
    cbw_tag: u32,
    deadline: u64,
    moved: u32,
    /// CSW reads after the first: one more is made after a zero-length
    /// packet or a stall.
    status_tries: u8,
    reissued: bool,
    /// It went past its deadline: the device hung rather than answered.
    hung: bool,
    sensing: Option<Sensing>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Reset(Pipe),
    Stop(Pipe),
    MassStorageReset,
    ClearTranslator(Pipe),
    ClearHalt(Pipe),
    Reconfigure(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// Read the command's CSW: its data stage stalled, or the CSW did.
    Status,
    /// Send the command again, once.
    Reissue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    Command(Ticket),
    Control(Target, Transfer),
}

/// Reset Recovery's: quiesce both pipes, the Bulk-Only reset, a transaction
/// translator's clear and a halt's per pipe, the reconfigure.
const MAX_ACTIONS: usize = 8;

#[derive(Clone, Copy, Debug)]
struct Recovery {
    actions: [Action; MAX_ACTIONS],
    len: u8,
    at: u8,
    pending: Option<Pending>,
    deadline: u64,
    then: Then,
}

impl Recovery {
    fn new(then: Then) -> Self {
        Self {
            actions: [Action::MassStorageReset; MAX_ACTIONS],
            len: 0,
            at: 0,
            pending: None,
            deadline: 0,
            then,
        }
    }

    fn push(&mut self, action: Action) {
        self.actions[usize::from(self.len)] = action;
        self.len += 1;
    }

    fn action(&self) -> Option<Action> {
        (self.at < self.len).then(|| self.actions[usize::from(self.at)])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unissued {
    /// The command ring or EP0 is full: the step waits for room.
    Busy,
    Refused,
}

/// Why a command needed more than its pipes could give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    /// One pipe stalled: clear it, then read the CSW.
    Clear(Pipe),
    Reset,
}

/// What the transport has done, for the kernel log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub clears: u32,
    pub resets: u32,
    pub escalations: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stepped {
    /// A command finished: wake whoever waits for one.
    pub finished: bool,
    /// [`Transport::serve`] has work.
    pub service: bool,
}

#[derive(Debug)]
pub struct Transport {
    interface: Interface,
    queue: [Option<Queued>; QUEUE],
    done: [Option<Done>; QUEUE],
    current: Option<Current>,
    want: Option<Want>,
    recovery: Option<Recovery>,
    next_tag: u16,
    next_cbw: u32,
    order: u32,
    /// LUNs that answered SYNCHRONIZE CACHE with ILLEGAL REQUEST: no cache.
    uncached: u16,
    /// Recovery failed, or is forbidden and was needed.
    broken: bool,
    recovery_allowed: bool,
    gone: bool,
    pub counts: Counts,
}

impl Transport {
    pub fn new(interface: Interface) -> Self {
        Self {
            interface,
            queue: [None; QUEUE],
            done: [None; QUEUE],
            current: None,
            want: None,
            recovery: None,
            next_tag: 1,
            next_cbw: 1,
            order: 0,
            uncached: 0,
            broken: false,
            recovery_allowed: true,
            gone: false,
            counts: Counts::default(),
        }
    }

    /// Nothing queued, on the wire or being recovered.
    pub fn is_idle(&self) -> bool {
        self.queue.iter().all(Option::is_none) && self.current.is_none() && self.recovery.is_none()
    }

    pub fn is_broken(&self) -> bool {
        self.broken || self.gone
    }

    pub fn has_cache(&self, lun: u8) -> bool {
        self.uncached & 1 << (lun & bot::MAX_LUN) == 0
    }

    /// From here on a command that needs recovery fails instead, as a
    /// shutdown needs.
    pub fn forbid_recovery(&mut self) {
        self.recovery_allowed = false;
    }

    /// Queue `command` for `lun`, its data stage in `pages`, or the scratch
    /// area for a direct command; it goes on the wire at once when nothing
    /// is ahead of it. `None` when the queue is full.
    pub fn submit<W: Wire>(
        &mut self,
        wire: &mut W,
        lun: u8,
        command: Command,
        pages: Option<&[u64]>,
        direct: bool,
    ) -> Option<u16> {
        let outstanding = self.queue.iter().flatten().count() + self.done.iter().flatten().count();
        if outstanding >= QUEUE {
            return None;
        }
        let slot = self.queue.iter().position(Option::is_none)?;
        let tag = self.next_tag;
        self.next_tag = if tag >= MAX_TAG { 1 } else { tag + 1 };
        self.order = self.order.wrapping_add(1);
        if let Some(pages) = pages {
            wire.hold(slot as u8, pages);
        }
        self.queue[slot] = Some(Queued {
            tag,
            lun,
            command,
            direct,
            held: pages.is_some(),
            order: self.order,
            retired: false,
        });
        let immediate = if self.gone || wire.gone() {
            Some(Outcome::Gone)
        } else if self.broken {
            Some(Outcome::Fail)
        } else if command.is_synchronize_cache() && !self.has_cache(lun) {
            Some(Outcome::Ok)
        } else {
            None
        };
        match immediate {
            Some(outcome) => self.finish_slot(slot as u8, outcome, 0, None),
            None => self.start_next(wire),
        }
        Some(tag)
    }

    /// The next finished engine command: its tag and outcome.
    pub fn pop(&mut self) -> Option<(u16, Outcome)> {
        let done = self
            .done
            .iter_mut()
            .find(|d| d.is_some_and(|d| !d.direct))?
            .take()?;
        Some((done.tag, done.outcome))
    }

    /// A finished direct command.
    pub fn take(&mut self, tag: u16) -> Option<Done> {
        self.done
            .iter_mut()
            .find(|d| d.is_some_and(|d| d.direct && d.tag == tag))?
            .take()
    }

    /// Whoever submitted `tag` has let it go: nothing it did is reported
    /// again, so a direct command no waiter will [`take`](Self::take) keeps
    /// no completion slot.
    pub fn retire(&mut self, tag: u16) {
        for queued in self.queue.iter_mut().flatten() {
            if queued.tag == tag {
                queued.retired = true;
            }
        }
        for done in self.done.iter_mut() {
            if done.is_some_and(|d| d.tag == tag) {
                *done = None;
            }
        }
    }

    /// A transfer on one of the pipes completed: move the command on.
    pub fn completed<W: Wire>(&mut self, wire: &mut W) -> Stepped {
        let before = self.finished_count();
        if !self.notice_gone(wire) {
            while self.advance(wire) {}
        }
        self.stepped(before)
    }

    /// The USB thread's turn: a command past its deadline, a pipe to clear,
    /// Reset Recovery. When it next needs a turn.
    pub fn serve<W: Wire>(&mut self, wire: &mut W) -> (Stepped, Option<u64>) {
        let before = self.finished_count();
        if !self.notice_gone(wire) {
            while self.advance(wire) {}
            let now = wire.now_ms();
            self.check_deadline(wire, now);
            if let Some(want) = self.want.take() {
                self.begin_recovery(wire, now, want);
            }
            while self.recovery.is_some() && self.step_recovery(wire, now) {}
            while self.advance(wire) {}
        }
        let next = match (&self.recovery, &self.current) {
            (Some(recovery), _) => Some(recovery.deadline),
            (None, Some(current)) => Some(current.deadline),
            (None, None) => None,
        };
        (self.stepped(before), next)
    }

    fn stepped(&self, before: usize) -> Stepped {
        Stepped {
            finished: self.finished_count() != before,
            service: self.want.is_some(),
        }
    }

    fn finished_count(&self) -> usize {
        self.done.iter().flatten().count()
    }

    fn notice_gone<W: Wire>(&mut self, wire: &mut W) -> bool {
        if !self.gone && !wire.gone() {
            return false;
        }
        self.gone = true;
        self.want = None;
        if let Some(recovery) = self.recovery.take() {
            match recovery.pending {
                Some(Pending::Command(ticket)) => wire.abandon_command(ticket),
                Some(Pending::Control(target, transfer)) => wire.abandon_control(target, transfer),
                None => {}
            }
        }
        self.current = None;
        self.fail_queued(Outcome::Gone);
        true
    }

    fn fail_queued(&mut self, outcome: Outcome) {
        for slot in 0..QUEUE as u8 {
            if self.queue[usize::from(slot)].is_some() {
                self.finish_slot(slot, outcome, 0, None);
            }
        }
    }

    fn finish_slot(&mut self, slot: u8, outcome: Outcome, moved: u32, sense: Option<Sense>) {
        let Some(queued) = self.queue[usize::from(slot)].take() else {
            return;
        };
        if queued.retired {
            return;
        }
        if let Some(done) = self.done.iter_mut().find(|d| d.is_none()) {
            *done = Some(Done {
                tag: queued.tag,
                outcome,
                moved,
                sense,
                direct: queued.direct,
            });
        }
    }

    fn queued(&self, slot: u8) -> Option<Queued> {
        self.queue[usize::from(slot)]
    }

    /// The command the current CBW carries: the queued one, or REQUEST SENSE
    /// for it.
    fn active(&self, current: &Current) -> Option<(Queued, Command)> {
        let queued = self.queued(current.slot)?;
        let command = match current.sensing {
            Some(_) => Command::request_sense(),
            None => queued.command,
        };
        Some((queued, command))
    }

    fn start_next<W: Wire>(&mut self, wire: &mut W) {
        if self.current.is_some() || self.recovery.is_some() || self.want.is_some() {
            return;
        }
        let next = self
            .queue
            .iter()
            .enumerate()
            .filter_map(|(slot, q)| q.map(|q| (slot, q.order)))
            .max_by_key(|&(_, order)| self.order.wrapping_sub(order));
        let Some((slot, _)) = next else {
            return;
        };
        self.current = Some(Current {
            slot: slot as u8,
            phase: Phase::Recovering,
            cbw_tag: 0,
            deadline: 0,
            moved: 0,
            status_tries: 0,
            reissued: false,
            hung: false,
            sensing: None,
        });
        let now = wire.now_ms();
        self.send_wrapper(wire, now);
    }

    fn send_wrapper<W: Wire>(&mut self, wire: &mut W, now: u64) {
        let Some(mut current) = self.current else {
            return;
        };
        let Some((queued, command)) = self.active(&current) else {
            return;
        };
        current.cbw_tag = self.next_cbw;
        self.next_cbw = self.next_cbw.wrapping_add(1);
        current.deadline = now + COMMAND_MS;
        current.moved = 0;
        current.status_tries = 0;
        let cbw = bot::command_block_wrapper(
            current.cbw_tag,
            command.length,
            command.direction,
            queued.lun,
            &command.cdb,
        );
        wire.write_wrapper(&cbw);
        current.phase = match wire.bulk(Pipe::Out, Buffer::Wrapper, CBW_LEN as u32) {
            Ok(transfer) => Phase::Wrapper(transfer),
            Err(_) => {
                self.want = Some(Want::Reset);
                Phase::Recovering
            }
        };
        self.current = Some(current);
    }

    /// One step of the command on the wire; whether it moved.
    fn advance<W: Wire>(&mut self, wire: &mut W) -> bool {
        let Some(current) = self.current else {
            self.start_next(wire);
            return self.current.is_some_and(|c| c.phase != Phase::Recovering)
                && self.want.is_none();
        };
        let Some((pipe, transfer)) = self.outstanding(&current) else {
            return false;
        };
        let Some(result) = wire.bulk_result(pipe, transfer) else {
            return false;
        };
        if matches!(result, Err(e) if e.is_final()) {
            self.notice_gone_now(wire);
            return false;
        }
        match current.phase {
            Phase::Wrapper(_) => self.wrapper_sent(wire, current, result),
            Phase::Data(_) => self.data_moved(wire, pipe, result),
            Phase::Status(_) => self.status_read(wire, current, result),
            Phase::Recovering => {}
        }
        true
    }

    fn outstanding(&self, current: &Current) -> Option<(Pipe, Transfer)> {
        match current.phase {
            Phase::Wrapper(t) => Some((Pipe::Out, t)),
            Phase::Data(t) => {
                let pipe = match self.active(current).map(|(_, c)| c.direction) {
                    Some(Direction::Out) => Pipe::Out,
                    _ => Pipe::In,
                };
                Some((pipe, t))
            }
            Phase::Status(t) => Some((Pipe::In, t)),
            Phase::Recovering => None,
        }
    }

    /// The driver lets the device go while it may still be there: what the
    /// pipes and EP0 hold is abandoned, and every command fails as gone.
    pub fn leave<W: Wire>(&mut self, wire: &mut W) {
        if let Some((pipe, transfer)) = self.current.and_then(|c| self.outstanding(&c)) {
            wire.abandon(pipe, transfer);
        }
        self.notice_gone_now(wire);
    }

    fn notice_gone_now<W: Wire>(&mut self, wire: &mut W) {
        self.gone = true;
        self.notice_gone(wire);
    }

    fn set_phase(&mut self, phase: Phase) {
        if let Some(current) = self.current.as_mut() {
            current.phase = phase;
        }
    }

    fn want(&mut self, want: Want) {
        self.set_phase(Phase::Recovering);
        self.want = Some(want);
    }

    fn wrapper_sent<W: Wire>(&mut self, wire: &mut W, current: Current, result: TransferResult) {
        if result.is_err() {
            return self.want(Want::Reset);
        }
        let Some((queued, command)) = self.active(&current) else {
            return;
        };
        let pipe = match command.direction {
            Direction::None => return self.read_status(wire),
            Direction::In => Pipe::In,
            Direction::Out => Pipe::Out,
        };
        let buffer = match (current.sensing, queued.held) {
            (Some(_), _) => Buffer::Sense,
            (None, true) => Buffer::Held(current.slot),
            (None, false) => Buffer::Scratch,
        };
        let length = match buffer {
            Buffer::Scratch => command.length.min(SCRATCH_LEN as u32),
            Buffer::Sense => command.length.min(u32::from(scsi::SENSE_LEN)),
            _ => command.length,
        };
        match wire.bulk(pipe, buffer, length) {
            Ok(transfer) => self.set_phase(Phase::Data(transfer)),
            Err(_) => self.want(Want::Reset),
        }
    }

    fn data_moved<W: Wire>(&mut self, wire: &mut W, pipe: Pipe, result: TransferResult) {
        match result {
            Ok(moved) => {
                if let Some(c) = self.current.as_mut() {
                    c.moved = moved;
                }
                self.read_status(wire);
            }
            Err(TransferError::Stall) => self.want(Want::Clear(pipe)),
            Err(_) => self.want(Want::Reset),
        }
    }

    fn read_status<W: Wire>(&mut self, wire: &mut W) {
        match wire.bulk(Pipe::In, Buffer::Status, CSW_LEN as u32) {
            Ok(transfer) => self.set_phase(Phase::Status(transfer)),
            Err(_) => self.want(Want::Reset),
        }
    }

    /// A zero-length packet or a stall where the CSW should be earns one more
    /// read, behind a clear for the stall.
    fn status_read<W: Wire>(&mut self, wire: &mut W, current: Current, result: TransferResult) {
        let again = current.status_tries < STATUS_RETRIES;
        if again && matches!(result, Ok(0) | Err(TransferError::Stall)) {
            if let Some(c) = self.current.as_mut() {
                c.status_tries += 1;
            }
            return match result {
                Ok(_) => self.read_status(wire),
                Err(_) => self.want(Want::Clear(Pipe::In)),
            };
        }
        let Ok(received) = result else {
            return self.want(Want::Reset);
        };
        let Some((_, command)) = self.active(&current) else {
            return;
        };
        let mut csw = [0u8; CSW_LEN];
        wire.read_wire(CSW_AT, &mut csw);
        let status =
            match bot::command_status(&csw, received as usize, current.cbw_tag, command.length) {
                Ok(status) => status,
                Err(BadStatus::Invalid | BadStatus::Meaningless) => return self.want(Want::Reset),
            };
        let moved = command
            .length
            .saturating_sub(status.residue)
            .min(current.moved);
        match (status.status, current.sensing) {
            (Status::PhaseError, _) => self.want(Want::Reset),
            (Status::Passed, None) => self.passed(wire, current, moved, status.residue),
            (Status::Failed, None) => self.sense(wire, moved),
            (Status::Passed, Some(sensing)) => {
                let mut bytes = [0u8; scsi::SENSE_LEN as usize];
                let length = (moved as usize).min(bytes.len());
                wire.read_wire(SENSE_AT, &mut bytes[..length]);
                self.sensed(wire, current, sensing, Sense::parse(&bytes[..length]));
            }
            (Status::Failed, Some(sensing)) => self.sensed(wire, current, sensing, None),
        }
    }

    fn passed<W: Wire>(&mut self, wire: &mut W, current: Current, moved: u32, residue: u32) {
        let Some(queued) = self.queued(current.slot) else {
            return;
        };
        let short = residue != 0 || moved < queued.command.length;
        let outcome = if queued.held && short {
            Outcome::Retry
        } else {
            Outcome::Ok
        };
        self.finish_current(wire, outcome, moved, None);
    }

    fn sense<W: Wire>(&mut self, wire: &mut W, moved: u32) {
        let now = wire.now_ms();
        if let Some(c) = self.current.as_mut() {
            c.sensing = Some(Sensing { moved });
        }
        self.send_wrapper(wire, now);
    }

    fn sensed<W: Wire>(
        &mut self,
        wire: &mut W,
        current: Current,
        sensing: Sensing,
        sense: Option<Sense>,
    ) {
        let Some(queued) = self.queued(current.slot) else {
            return;
        };
        let outcome = match sense {
            Some(s)
                if queued.command.is_synchronize_cache() && s.key == sense_key::ILLEGAL_REQUEST =>
            {
                self.uncached |= 1 << (queued.lun & bot::MAX_LUN);
                Outcome::Ok
            }
            Some(s) => match s.verdict() {
                Verdict::Done if queued.held && sensing.moved < queued.command.length => {
                    Outcome::Retry
                }
                Verdict::Done => Outcome::Ok,
                Verdict::Retry => Outcome::Retry,
                Verdict::Fail => Outcome::Fail,
            },
            None => Outcome::Retry,
        };
        self.finish_current(wire, outcome, sensing.moved, sense);
    }

    fn finish_current<W: Wire>(
        &mut self,
        wire: &mut W,
        outcome: Outcome,
        moved: u32,
        sense: Option<Sense>,
    ) {
        let Some(current) = self.current.take() else {
            return;
        };
        self.finish_slot(current.slot, outcome, moved, sense);
        self.start_next(wire);
    }

    fn check_deadline<W: Wire>(&mut self, wire: &mut W, now: u64) {
        let Some(current) = self.current else {
            return;
        };
        if now < current.deadline || self.want.is_some() || self.recovery.is_some() {
            return;
        }
        if let Some((pipe, transfer)) = self.outstanding(&current) {
            wire.abandon(pipe, transfer);
        }
        if let Some(c) = self.current.as_mut() {
            c.hung = true;
        }
        self.want(Want::Reset);
    }

    fn begin_recovery<W: Wire>(&mut self, wire: &mut W, now: u64, want: Want) {
        if !self.recovery_allowed {
            return self.give_up(wire, false);
        }
        let (mut recovery, pipes) = match want {
            Want::Clear(pipe) => {
                self.counts.clears += 1;
                (Recovery::new(Then::Status), [Some(pipe), None])
            }
            Want::Reset => {
                self.counts.resets += 1;
                (
                    Recovery::new(Then::Reissue),
                    [Some(Pipe::In), Some(Pipe::Out)],
                )
            }
        };
        for pipe in pipes.into_iter().flatten() {
            recovery.push(Action::Reset(pipe));
        }
        if want == Want::Reset {
            recovery.push(Action::MassStorageReset);
        }
        let mut mask = 0;
        for pipe in pipes.into_iter().flatten() {
            if self.interface.translated.is_some() {
                recovery.push(Action::ClearTranslator(pipe));
            }
            recovery.push(Action::ClearHalt(pipe));
            mask |= pipe.bit();
        }
        recovery.push(Action::Reconfigure(mask));
        recovery.deadline = now + STEP_MS;
        self.set_phase(Phase::Recovering);
        self.recovery = Some(recovery);
    }

    /// Issue the next step or collect the one out; whether to go on now.
    fn step_recovery<W: Wire>(&mut self, wire: &mut W, now: u64) -> bool {
        let Some(mut recovery) = self.recovery else {
            return false;
        };
        let Some(action) = recovery.action() else {
            self.recovery = None;
            self.recovered(wire, recovery.then);
            return false;
        };
        let Some(pending) = recovery.pending else {
            let issued = match self.issue(wire, action) {
                Ok(pending) => pending,
                Err(Unissued::Busy) if now < recovery.deadline => return false,
                Err(_) => {
                    self.recovery = None;
                    self.give_up(wire, true);
                    return false;
                }
            };
            recovery.pending = Some(issued);
            self.recovery = Some(recovery);
            return true;
        };
        let succeeded = match pending {
            Pending::Command(ticket) => match wire.command_result(ticket) {
                None if now >= recovery.deadline => {
                    wire.abandon_command(ticket);
                    wire.stuck();
                    self.recovery = None;
                    self.notice_gone_now(wire);
                    return false;
                }
                None => return false,
                Some(result) => self.command_done(wire, &mut recovery, action, result),
            },
            Pending::Control(target, transfer) => match wire.control_result(target, transfer) {
                None if now >= recovery.deadline => {
                    wire.abandon_control(target, transfer);
                    false
                }
                None => return false,
                Some(Err(e)) if e.is_final() => {
                    self.recovery = None;
                    self.notice_gone_now(wire);
                    return false;
                }
                Some(result) => {
                    recovery.at += u8::from(result.is_ok());
                    result.is_ok()
                }
            },
        };
        recovery.pending = None;
        if !succeeded {
            self.recovery = None;
            self.give_up(wire, true);
            return false;
        }
        recovery.deadline = now + STEP_MS;
        self.recovery = Some(recovery);
        true
    }

    fn issue<W: Wire>(&mut self, wire: &mut W, action: Action) -> Result<Pending, Unissued> {
        let command = match action {
            Action::Reset(pipe) => PipeCommand::Reset(pipe),
            Action::Stop(pipe) => PipeCommand::Stop(pipe),
            Action::Reconfigure(mask) => PipeCommand::Reconfigure(mask),
            Action::MassStorageReset => {
                let setup = Setup::mass_storage_reset(self.interface.interface);
                return self.control(wire, Target::Device, setup);
            }
            Action::ClearHalt(pipe) => {
                let setup = Setup::clear_halt(self.address(pipe));
                return self.control(wire, Target::Device, setup);
            }
            Action::ClearTranslator(pipe) => {
                let device = self.interface.translated.unwrap_or(0);
                let setup = hub::clear_tt_buffer(device, self.address(pipe), hub::TT_BULK, 1);
                return self.control(wire, Target::TranslatorHub, setup);
            }
        };
        wire.command(command)
            .map(Pending::Command)
            .map_err(|e| match e {
                SubmitError::Busy => Unissued::Busy,
                SubmitError::Dead => Unissued::Refused,
            })
    }

    fn control<W: Wire>(
        &self,
        wire: &mut W,
        target: Target,
        setup: Setup,
    ) -> Result<Pending, Unissued> {
        wire.control(target, setup)
            .map(|t| Pending::Control(target, t))
            .map_err(|e| match e {
                PushError::Busy => Unissued::Busy,
                PushError::Halted => Unissued::Refused,
            })
    }

    fn address(&self, pipe: Pipe) -> u8 {
        match pipe {
            Pipe::In => self.interface.bulk_in,
            Pipe::Out => self.interface.bulk_out,
        }
    }

    /// A Reset Endpoint that found the pipe not halted stops it instead.
    fn command_done<W: Wire>(
        &mut self,
        wire: &mut W,
        recovery: &mut Recovery,
        action: Action,
        result: CommandResult,
    ) -> bool {
        let code = match result {
            Ok(done) => done.code,
            Err(_) => return false,
        };
        match action {
            Action::Reset(pipe) if code == CompletionCode::CONTEXT_STATE => {
                recovery.actions[usize::from(recovery.at)] = Action::Stop(pipe);
                return true;
            }
            Action::Stop(_) => {}
            Action::Reconfigure(mask) if code.is_success() => {
                for pipe in [Pipe::In, Pipe::Out] {
                    if mask & pipe.bit() != 0 {
                        wire.recovered(pipe);
                    }
                }
            }
            _ if !code.is_success() => return false,
            _ => {}
        }
        recovery.at += 1;
        true
    }

    fn recovered<W: Wire>(&mut self, wire: &mut W, then: Then) {
        let now = wire.now_ms();
        let Some(current) = self.current else {
            return self.start_next(wire);
        };
        match then {
            Then::Status => self.read_status(wire),
            Then::Reissue if current.sensing.is_some() => {
                let moved = current.sensing.map_or(0, |s| s.moved);
                self.finish_current(wire, Outcome::Retry, moved, None);
            }
            Then::Reissue if current.reissued => {
                self.finish_current(wire, Outcome::Fail, 0, None);
            }
            Then::Reissue if self.queued(current.slot).is_some_and(|q| q.direct) => {
                let outcome = if current.hung {
                    Outcome::Fail
                } else {
                    Outcome::Retry
                };
                self.finish_current(wire, outcome, 0, None);
            }
            Then::Reissue => {
                if let Some(c) = self.current.as_mut() {
                    c.reissued = true;
                }
                self.send_wrapper(wire, now);
            }
        }
    }

    /// Every command fails; past a recovery that failed, the device is
    /// reset and enumerated again.
    fn give_up<W: Wire>(&mut self, wire: &mut W, escalate: bool) {
        self.broken = true;
        self.current = None;
        self.want = None;
        self.fail_queued(Outcome::Fail);
        if escalate {
            self.counts.escalations += 1;
            wire.escalate();
        }
    }
}
