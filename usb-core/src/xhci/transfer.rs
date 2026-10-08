//! Transfer rings (§4.9.2, §4.11): one endpoint's ring and its outstanding
//! transfers, each completed by one event, or two when a data stage is short.

use super::memory::DmaPage;
use super::ring::ProducerRing;
use super::trb::{CompletionCode, Trb};
use crate::device::request::Setup;

pub const MAX_TRANSFERS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Transfer {
    last: u16,
    serial: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferError {
    Stall,
    /// More than the buffer holds.
    Babble,
    /// No valid handshake: a CRC error, a bus timeout, a device that left.
    Transaction,
    Gone,
    Dead,
    /// A later transfer completed first, so this one's event was lost.
    Lost,
    /// The ring was moved past it after another transfer halted the endpoint.
    Cancelled,
    Other(CompletionCode),
}

impl TransferError {
    fn from_code(code: CompletionCode) -> Self {
        match code {
            CompletionCode::STALL => TransferError::Stall,
            CompletionCode::BABBLE => TransferError::Babble,
            CompletionCode::TRANSACTION => TransferError::Transaction,
            other => TransferError::Other(other),
        }
    }

    /// A removed device or a dead controller never answers again.
    pub fn is_final(self) -> bool {
        matches!(self, TransferError::Gone | TransferError::Dead)
    }
}

/// Bytes moved, or why not.
pub type TransferResult = Result<u32, TransferError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Free,
    Waiting,
    /// The submitter stopped waiting; the completion only frees the entry.
    Abandoned,
    Done(TransferResult),
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    state: State,
    ticket: Transfer,
    first: u16,
    /// The data stage's TRB, whose short packet is reported before the status
    /// stage completes.
    data: Option<u16>,
    length: u32,
    short: Option<u32>,
}

const FREE: Entry = Entry {
    state: State::Free,
    ticket: Transfer { last: 0, serial: 0 },
    first: 0,
    data: None,
    length: 0,
    short: None,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushError {
    Busy,
    /// A transfer halted or was abandoned on the endpoint, which takes Set TR
    /// Dequeue Pointer before the ring runs again.
    Halted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completed {
    Transfer,
    /// A short data stage; the status stage is still to come.
    Partial,
    /// Names nothing outstanding.
    Stray,
}

fn halts(code: CompletionCode) -> bool {
    !matches!(
        code,
        CompletionCode::SUCCESS
            | CompletionCode::SHORT_PACKET
            | CompletionCode::STOPPED
            | CompletionCode::STOPPED_LENGTH_INVALID
            | CompletionCode::STOPPED_SHORT_PACKET
    )
}

pub struct TransferRing<P> {
    ring: ProducerRing<P>,
    entries: [Entry; MAX_TRANSFERS],
    serial: u32,
    halted: bool,
}

impl<P: DmaPage> TransferRing<P> {
    /// `page` must be zeroed.
    pub fn new(page: P) -> Self {
        Self {
            ring: ProducerRing::new(page),
            entries: [FREE; MAX_TRANSFERS],
            serial: 0,
            halted: false,
        }
    }

    /// The endpoint context's initial TR Dequeue Pointer and cycle state.
    pub fn dequeue(&self) -> (u64, bool) {
        self.ring.enqueue_pointer()
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Whether a transfer pushed now would be taken, so its buffer may be
    /// filled first.
    pub fn accepts(&self) -> bool {
        !self.halted && self.entries.iter().any(|e| e.state == State::Free)
    }

    /// Transfers submitted and not yet taken.
    pub fn outstanding(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e.state, State::Waiting | State::Abandoned))
            .count()
    }

    fn submit(
        &mut self,
        trbs: &[Trb],
        data: Option<u16>,
        length: u32,
    ) -> Result<Transfer, PushError> {
        if self.halted {
            return Err(PushError::Halted);
        }
        let slot = self
            .entries
            .iter()
            .position(|e| e.state == State::Free)
            .ok_or(PushError::Busy)?;
        let (first, last) = self.ring.push_all(trbs).ok_or(PushError::Busy)?;
        self.serial = self.serial.wrapping_add(1);
        let ticket = Transfer {
            last,
            serial: self.serial,
        };
        self.entries[slot] = Entry {
            state: State::Waiting,
            ticket,
            first,
            data: data.map(|offset| (first + offset) % (super::ring::RING_TRBS - 1)),
            length,
            short: None,
        };
        Ok(ticket)
    }

    /// A data stage moves `setup.length` bytes to or from `buffer`.
    pub fn control(&mut self, setup: Setup, buffer: u64) -> Result<Transfer, PushError> {
        let length = u32::from(setup.length);
        let data_in = setup.is_in();
        if length == 0 {
            let trbs = [Trb::setup(setup.bytes(), None), Trb::status_stage(false)];
            return self.submit(&trbs, None, 0);
        }
        let trbs = [
            Trb::setup(setup.bytes(), Some(data_in)),
            Trb::data(buffer, length, data_in),
            Trb::status_stage(data_in),
        ];
        self.submit(&trbs, Some(1), length)
    }

    pub fn normal(&mut self, buffer: u64, length: u32) -> Result<Transfer, PushError> {
        self.submit(&[Trb::normal(buffer, length, false)], None, length)
    }

    pub fn complete(&mut self, trb: u64, code: CompletionCode, residual: u32) -> Completed {
        let Some(index) = self.ring.index_of(trb) else {
            return Completed::Stray;
        };
        if !self.ring.in_flight_at(index) {
            return Completed::Stray;
        }
        let at = self.ring.distance(index);
        let mut found = None;
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if !matches!(entry.state, State::Waiting | State::Abandoned) {
                continue;
            }
            let first = self.ring.distance(entry.first);
            let last = self.ring.distance(entry.ticket.last);
            if last < at {
                entry.state = match entry.state {
                    State::Waiting => State::Done(Err(TransferError::Lost)),
                    _ => State::Free,
                };
            } else if first <= at {
                found = Some(i);
            }
        }
        let Some(i) = found else {
            return Completed::Stray;
        };
        let entry = &mut self.entries[i];
        let delivered = entry.length - residual.min(entry.length);
        let result = if code == CompletionCode::SHORT_PACKET && Some(index) == entry.data {
            entry.short = Some(delivered);
            if index != entry.ticket.last {
                return Completed::Partial;
            }
            Ok(delivered)
        } else if code == CompletionCode::SHORT_PACKET || code == CompletionCode::SUCCESS {
            if index != entry.ticket.last {
                return Completed::Stray;
            }
            Ok(match (entry.short, code) {
                (Some(short), _) => short,
                (None, CompletionCode::SHORT_PACKET) => delivered,
                (None, _) => entry.length,
            })
        } else {
            if halts(code) {
                self.halted = true;
            }
            Err(TransferError::from_code(code))
        };
        let last = entry.ticket.last;
        entry.state = match entry.state {
            State::Waiting => State::Done(result),
            _ => State::Free,
        };
        self.ring.retire_through(last);
        Completed::Transfer
    }

    /// Taking a result frees the entry.
    pub fn take(&mut self, ticket: Transfer) -> Option<TransferResult> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| matches!(e.state, State::Done(_)) && e.ticket == ticket)?;
        let State::Done(result) = entry.state else {
            return None;
        };
        *entry = FREE;
        Some(result)
    }

    /// A transfer the controller may still run halts the ring until it is
    /// moved past, so nothing reuses its buffer under it.
    pub fn abandon(&mut self, ticket: Transfer) {
        for entry in &mut self.entries {
            if entry.ticket != ticket {
                continue;
            }
            match entry.state {
                State::Waiting => {
                    entry.state = State::Abandoned;
                    self.halted = true;
                }
                State::Done(_) => *entry = FREE,
                _ => {}
            }
        }
    }

    /// Nothing more is submitted.
    pub fn fail_all(&mut self, error: TransferError) {
        self.halted = true;
        self.finish_outstanding(error);
    }

    fn finish_outstanding(&mut self, error: TransferError) {
        for entry in &mut self.entries {
            entry.state = match entry.state {
                State::Waiting => State::Done(Err(error)),
                State::Abandoned => State::Free,
                other => other,
            };
        }
    }

    /// Past every TRB written, so whatever the halt left is cancelled.
    pub fn recovery_dequeue(&self) -> (u64, bool) {
        self.ring.enqueue_pointer()
    }

    /// Set TR Dequeue Pointer completed: the ring runs again, empty.
    pub fn recovered(&mut self) {
        self.finish_outstanding(TransferError::Cancelled);
        self.ring.skip_to_enqueue();
        self.halted = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::descriptor::kind;
    use crate::xhci::sim::{Memory, SimPage};
    use crate::xhci::trb::kind as trb_kind;

    fn ring() -> (Memory, TransferRing<SimPage>) {
        let mem = Memory::default();
        let page = mem.page();
        (mem, TransferRing::new(page))
    }

    fn trb_at(mem: &Memory, address: u64) -> Trb {
        Trb {
            parameter: mem.read64(address),
            status: mem.read32(address + 8),
            control: mem.read32(address + 12),
        }
    }

    #[test]
    fn a_control_read_is_three_stages_handed_over_first_last() {
        let (mem, mut r) = ring();
        let (base, cycle) = r.dequeue();
        assert!(cycle);
        let setup = Setup::get_descriptor(kind::DEVICE, 0, 0, 18);
        let ticket = r.control(setup, 0x9000).unwrap();
        let stages: std::vec::Vec<Trb> = (0..3).map(|i| trb_at(&mem, base + i * 16)).collect();
        assert_eq!(stages[0].kind(), trb_kind::SETUP);
        assert_eq!(stages[1].kind(), trb_kind::DATA);
        assert_eq!(stages[2].kind(), trb_kind::STATUS);
        assert!(stages.iter().all(Trb::cycle));
        assert_eq!(
            r.complete(base + 32, CompletionCode::SUCCESS, 0),
            Completed::Transfer
        );
        assert_eq!(r.take(ticket), Some(Ok(18)));
        assert_eq!(r.take(ticket), None);
        assert_eq!(r.outstanding(), 0);
    }

    #[test]
    fn a_short_data_stage_reports_what_arrived() {
        let (_mem, mut r) = ring();
        let (base, _) = r.dequeue();
        let ticket = r
            .control(
                Setup::get_descriptor(kind::CONFIGURATION, 0, 0, 255),
                0x9000,
            )
            .unwrap();
        assert_eq!(
            r.complete(base + 16, CompletionCode::SHORT_PACKET, 223),
            Completed::Partial
        );
        assert_eq!(r.take(ticket), None, "the status stage is still to come");
        assert_eq!(
            r.complete(base + 32, CompletionCode::SUCCESS, 0),
            Completed::Transfer
        );
        assert_eq!(r.take(ticket), Some(Ok(32)));
    }

    #[test]
    fn a_residual_past_the_buffer_reports_nothing_moved() {
        let (_mem, mut r) = ring();
        let (base, _) = r.dequeue();
        let ticket = r.normal(0x9000, 8).unwrap();
        r.complete(base, CompletionCode::SHORT_PACKET, 0xff_ffff);
        assert_eq!(r.take(ticket), Some(Ok(0)));
    }

    #[test]
    fn a_stall_halts_the_ring_until_it_is_moved_past() {
        let (_mem, mut r) = ring();
        let (base, _) = r.dequeue();
        let stalled = r.control(Setup::get_status(), 0x9000).unwrap();
        let queued = r.control(Setup::get_status(), 0x9000).unwrap();
        assert_eq!(
            r.complete(base + 16, CompletionCode::STALL, 2),
            Completed::Transfer
        );
        assert_eq!(r.take(stalled), Some(Err(TransferError::Stall)));
        assert!(r.is_halted());
        assert_eq!(
            r.control(Setup::get_status(), 0x9000),
            Err(PushError::Halted)
        );
        let (skip, cycle) = r.recovery_dequeue();
        assert_eq!((skip, cycle), (base + 6 * 16, true));
        r.recovered();
        assert_eq!(r.take(queued), Some(Err(TransferError::Cancelled)));
        assert!(!r.is_halted());
        let next = r.normal(0x9000, 4).unwrap();
        r.complete(skip, CompletionCode::SUCCESS, 0);
        assert_eq!(r.take(next), Some(Ok(4)));
    }

    #[test]
    fn a_later_completion_loses_an_earlier_one_and_strays_are_ignored() {
        let (_mem, mut r) = ring();
        let (base, _) = r.dequeue();
        let first = r.normal(0x9000, 4).unwrap();
        let second = r.normal(0x9000, 4).unwrap();
        let abandoned = r.normal(0x9000, 4).unwrap();
        r.abandon(abandoned);
        assert_eq!(
            r.complete(base + 0x800, CompletionCode::SUCCESS, 0),
            Completed::Stray
        );
        assert_eq!(
            r.complete(base + 3, CompletionCode::SUCCESS, 0),
            Completed::Stray
        );
        assert_eq!(
            r.complete(base + 16, CompletionCode::SUCCESS, 0),
            Completed::Transfer
        );
        assert_eq!(r.take(first), Some(Err(TransferError::Lost)));
        assert_eq!(r.take(second), Some(Ok(4)));
        assert_eq!(
            r.complete(base + 32, CompletionCode::SUCCESS, 0),
            Completed::Transfer
        );
        assert_eq!(
            r.outstanding(),
            0,
            "an abandoned transfer's completion frees it"
        );
        assert_eq!(
            r.complete(base + 32, CompletionCode::SUCCESS, 0),
            Completed::Stray
        );
    }

    #[test]
    fn a_full_table_is_busy_and_fail_all_ends_everything() {
        let (_mem, mut r) = ring();
        let tickets: std::vec::Vec<Transfer> = (0..MAX_TRANSFERS)
            .map(|_| r.normal(0x9000, 1).unwrap())
            .collect();
        assert_eq!(r.normal(0x9000, 1), Err(PushError::Busy));
        r.fail_all(TransferError::Gone);
        for t in tickets {
            assert_eq!(r.take(t), Some(Err(TransferError::Gone)));
        }
        assert_eq!(r.normal(0x9000, 1), Err(PushError::Halted));
        assert!(TransferError::Gone.is_final() && !TransferError::Stall.is_final());
    }

    #[test]
    fn transfers_lap_the_ring() {
        let (_mem, mut r) = ring();
        let (base, _) = r.dequeue();
        let link = u64::from(crate::xhci::ring::RING_TRBS - 1);
        for _ in 0..200 {
            let (pointer, _) = r.dequeue();
            let status = base + ((pointer - base) / 16 + 1) % link * 16;
            let ticket = r.control(Setup::set_configuration(1), 0).unwrap();
            assert_eq!(
                r.complete(status, CompletionCode::SUCCESS, 0),
                Completed::Transfer
            );
            assert_eq!(r.take(ticket), Some(Ok(0)));
        }
    }
}
