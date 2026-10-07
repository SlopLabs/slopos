//! Ring bookkeeping (§4.9): cycle bits, the Link TRB that closes a producer
//! ring, the full and empty tests, and the commands a command ring has
//! outstanding.

use super::bus::RegisterBus;
use super::memory::{DmaPage, PAGE_SIZE};
use super::trb::{CompletionCode, TRB_BYTES, Trb};

/// TRBs one page holds: an event ring's segment, or a producer ring with its
/// Link TRB last.
pub const RING_TRBS: u16 = (PAGE_SIZE / TRB_BYTES) as u16;
const LINK: u16 = RING_TRBS - 1;

fn at(index: u16) -> usize {
    usize::from(index) * TRB_BYTES
}

/// Write `trb`'s parameter and status, then, after a release fence, the
/// control dword whose cycle bit hands the TRB to its consumer.
fn publish<P: DmaPage>(page: &mut P, index: u16, trb: Trb) {
    let offset = at(index);
    page.write64(offset, trb.parameter);
    page.write32(offset + 8, trb.status);
    page.release();
    page.write32(offset + 12, trb.control);
}

/// The TRB at `index` if its cycle bit is `cycle`; nothing else is read
/// before the cycle bit says it was written.
fn consume<P: DmaPage>(page: &P, index: u16, cycle: bool) -> Option<Trb> {
    let offset = at(index);
    let control = page.read32(offset + 12);
    if (control & 1 != 0) != cycle {
        return None;
    }
    page.acquire();
    let parameter = u64::from(page.read32(offset)) | u64::from(page.read32(offset + 4)) << 32;
    Some(Trb {
        parameter,
        status: page.read32(offset + 8),
        control,
    })
}

/// A ring software produces into, one page whose last TRB links back to
/// its first.
pub struct ProducerRing<P> {
    page: P,
    enqueue: u16,
    cycle: bool,
    /// Where the consumer has reached, as its completions report: every TRB
    /// from here to `enqueue` is still the controller's.
    dequeue: u16,
}

impl<P: DmaPage> ProducerRing<P> {
    /// `page` must be zeroed, so no TRB in it carries the first lap's cycle.
    pub fn new(mut page: P) -> Self {
        let link = Trb::link(page.phys());
        publish(&mut page, LINK, link);
        Self {
            page,
            enqueue: 0,
            cycle: true,
            dequeue: 0,
        }
    }

    pub fn phys(&self) -> u64 {
        self.page.phys()
    }

    /// The producer cycle state, which the consumer's starts equal to.
    pub fn cycle(&self) -> bool {
        self.cycle
    }

    pub fn in_flight(&self) -> u16 {
        (self.enqueue + LINK - self.dequeue) % LINK
    }

    pub fn is_empty(&self) -> bool {
        self.in_flight() == 0
    }

    /// One TRB short of a lap: an enqueue that caught the dequeue up would
    /// read as empty.
    pub fn is_full(&self) -> bool {
        self.in_flight() == LINK - 1
    }

    /// Hand `trb` to the consumer; its address, or `None` when full.
    pub fn push(&mut self, trb: Trb) -> Option<u64> {
        if self.is_full() {
            return None;
        }
        let index = self.enqueue;
        publish(&mut self.page, index, trb.with_cycle(self.cycle));
        self.enqueue += 1;
        if self.enqueue == LINK {
            let link = Trb::link(self.page.phys()).with_cycle(self.cycle);
            publish(&mut self.page, LINK, link);
            self.cycle = !self.cycle;
            self.enqueue = 0;
        }
        Some(self.address(index))
    }

    pub fn address(&self, index: u16) -> u64 {
        self.page.phys() + at(index) as u64
    }

    /// The index of the TRB at `trb`, if that address is one this ring
    /// produces into.
    pub fn index_of(&self, trb: u64) -> Option<u16> {
        let offset = trb.checked_sub(self.page.phys())?;
        let index = offset / TRB_BYTES as u64;
        (offset % TRB_BYTES as u64 == 0 && index < u64::from(LINK)).then_some(index as u16)
    }

    /// Whether the TRB at `index` is one the consumer has yet to finish.
    pub fn in_flight_at(&self, index: u16) -> bool {
        index < LINK && (index + LINK - self.dequeue) % LINK < self.in_flight()
    }

    fn distance(&self, index: u16) -> u16 {
        (index + LINK - self.dequeue) % LINK
    }

    /// The consumer finished the TRB at `index`: it and every TRB before it
    /// are free. An index not in flight changes nothing.
    pub fn retire_through(&mut self, index: u16) -> bool {
        if !self.in_flight_at(index) {
            return false;
        }
        self.dequeue = (index + 1) % LINK;
        true
    }
}

/// The event ring's one segment, which only the controller produces into.
pub struct EventRing<P> {
    page: P,
    dequeue: u16,
    cycle: bool,
}

impl<P: DmaPage> EventRing<P> {
    /// `page` must be zeroed.
    pub fn new(page: P) -> Self {
        Self {
            page,
            dequeue: 0,
            cycle: true,
        }
    }

    pub fn phys(&self) -> u64 {
        self.page.phys()
    }

    pub fn pop(&mut self) -> Option<Trb> {
        let trb = consume(&self.page, self.dequeue, self.cycle)?;
        self.dequeue += 1;
        if self.dequeue == RING_TRBS {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(trb)
    }

    /// ERDP's pointer: the next TRB software will read.
    pub fn dequeue_pointer(&self) -> u64 {
        self.page.phys() + at(self.dequeue) as u64
    }
}

/// Commands one controller may have outstanding at once.
pub const MAX_COMMANDS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandCompletion {
    pub code: CompletionCode,
    pub parameter: u32,
    pub slot: u8,
}

/// Why a command has no completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unanswered {
    /// The controller died first.
    Dead,
    /// A later command completed first, so this one's event was lost.
    Lost,
}

pub type CommandResult = Result<CommandCompletion, Unanswered>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// The ring, or the table of outstanding commands, is full.
    Busy,
    Dead,
}

/// Names one submitted command; the serial keeps a ticket from matching a
/// later command that reuses its ring index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    index: u16,
    serial: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
    Free,
    Waiting(Ticket),
    /// The submitter stopped waiting; the completion only frees the entry.
    Abandoned(Ticket),
    Done(Ticket, CommandResult),
}

/// The command ring and what it has outstanding.
pub struct CommandRing<P> {
    ring: ProducerRing<P>,
    entries: [Entry; MAX_COMMANDS],
    serial: u32,
    dead: bool,
}

/// What a Command Completion Event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completed {
    /// It completed an outstanding command.
    Command,
    /// It names no outstanding command: the ring stopping, or a pointer
    /// outside the ring.
    Stray,
}

impl<P: DmaPage> CommandRing<P> {
    pub fn new(page: P) -> Self {
        Self {
            ring: ProducerRing::new(page),
            entries: [Entry::Free; MAX_COMMANDS],
            serial: 0,
            dead: false,
        }
    }

    /// CRCR's value: the ring's base and the consumer's starting cycle.
    pub fn crcr(&self) -> u64 {
        self.ring.phys() | u64::from(self.ring.cycle())
    }

    /// Queue `trb` and ring the host controller's doorbell, `doorbell` being
    /// doorbell 0's offset.
    pub fn submit<B: RegisterBus>(
        &mut self,
        bus: &mut B,
        doorbell: usize,
        trb: Trb,
    ) -> Result<Ticket, SubmitError> {
        if self.dead {
            return Err(SubmitError::Dead);
        }
        let slot = self
            .entries
            .iter()
            .position(|e| *e == Entry::Free)
            .ok_or(SubmitError::Busy)?;
        let address = self.ring.push(trb).ok_or(SubmitError::Busy)?;
        let index = self.ring.index_of(address).ok_or(SubmitError::Busy)?;
        self.serial = self.serial.wrapping_add(1);
        let ticket = Ticket {
            index,
            serial: self.serial,
        };
        self.entries[slot] = Entry::Waiting(ticket);
        bus.write32(doorbell, 0);
        Ok(ticket)
    }

    pub fn complete(&mut self, trb: u64, completion: CommandCompletion) -> Completed {
        if completion.code == CompletionCode::COMMAND_RING_STOPPED {
            return Completed::Stray;
        }
        let Some(index) = self.ring.index_of(trb) else {
            return Completed::Stray;
        };
        if !self.ring.in_flight_at(index) {
            return Completed::Stray;
        }
        let behind = self.ring.distance(index);
        for entry in &mut self.entries {
            *entry = match *entry {
                Entry::Waiting(t) if self.ring.distance(t.index) < behind => {
                    Entry::Done(t, Err(Unanswered::Lost))
                }
                Entry::Abandoned(t) if self.ring.distance(t.index) < behind => Entry::Free,
                other => other,
            };
        }
        self.ring.retire_through(index);
        for entry in &mut self.entries {
            match *entry {
                Entry::Waiting(t) if t.index == index => {
                    *entry = Entry::Done(t, Ok(completion));
                    return Completed::Command;
                }
                Entry::Abandoned(t) if t.index == index => {
                    *entry = Entry::Free;
                    return Completed::Command;
                }
                _ => {}
            }
        }
        Completed::Stray
    }

    /// The command's result, once it has one; taking it frees the entry.
    pub fn take(&mut self, ticket: Ticket) -> Option<CommandResult> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| matches!(e, Entry::Done(t, _) if *t == ticket))?;
        let Entry::Done(_, result) = *entry else {
            return None;
        };
        *entry = Entry::Free;
        Some(result)
    }

    /// Stop waiting for `ticket`; its completion, if it comes, frees it.
    pub fn abandon(&mut self, ticket: Ticket) {
        for entry in &mut self.entries {
            match *entry {
                Entry::Waiting(t) if t == ticket => *entry = Entry::Abandoned(t),
                Entry::Done(t, _) if t == ticket => *entry = Entry::Free,
                _ => {}
            }
        }
    }

    /// The controller is dead: every outstanding command fails, and no
    /// other is accepted.
    pub fn fail_all(&mut self) {
        self.dead = true;
        for entry in &mut self.entries {
            *entry = match *entry {
                Entry::Waiting(t) => Entry::Done(t, Err(Unanswered::Dead)),
                Entry::Abandoned(_) => Entry::Free,
                other => other,
            };
        }
    }

    /// Commands the controller has yet to complete.
    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| matches!(e, Entry::Waiting(_) | Entry::Abandoned(_)))
            .count()
    }
}
