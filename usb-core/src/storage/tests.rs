//! The transport over the simulated controller and a simulated disk whose
//! toggles are held to the controller's.

use super::scsi::{self, Command, sense_key};
use super::transport::*;
use crate::bus::tests::{SimHost, Tables, no_violations, settle};
use crate::device::Speed;
use crate::device::request::Setup;
use crate::xhci::context::{dci, write_readded};
use crate::xhci::controller;
use crate::xhci::memory::{DmaPage, PAGE_SIZE};
use crate::xhci::ring::{CommandResult, SubmitError, Ticket};
use crate::xhci::sim::{Config, SimDevice, SimPage, SimStorage};
use crate::xhci::transfer::{PushError, Transfer, TransferResult};
use crate::xhci::trb::Trb;
use std::vec;
use std::vec::Vec;

const IN: u8 = 0x81;
const OUT: u8 = 0x02;

fn pipe_dci(pipe: Pipe) -> u8 {
    match pipe {
        Pipe::In => dci(1, true),
        Pipe::Out => dci(2, false),
    }
}

struct Bench {
    rig: Rig,
    transport: Transport,
}

impl core::ops::Deref for Bench {
    type Target = Rig;
    fn deref(&self) -> &Rig {
        &self.rig
    }
}

impl core::ops::DerefMut for Bench {
    fn deref_mut(&mut self) -> &mut Rig {
        &mut self.rig
    }
}

struct Rig {
    host: SimHost,
    tables: Tables,
    slot: u8,
    hub: Option<u8>,
    root: u8,
    route: Vec<u8>,
    max_packet: u16,
    page: SimPage,
    held: Vec<Vec<u64>>,
    data: Vec<SimPage>,
    escalated: u32,
    stuck: u32,
    /// Reconfigure moves the rings with Set TR Dequeue instead, leaving the
    /// controller's toggles where they were.
    skip_toggle_reset: bool,
    /// Commands the controller answers `Busy` before it takes another.
    busy_commands: u32,
}

struct StickWire<'a> {
    bench: &'a mut Rig,
}

impl Bench {
    fn new(config: Config, speed: Speed, behind_hub: bool) -> Self {
        let mut host = SimHost::new(config);
        let root = if speed.is_super() { 1 } else { 3 };
        let route = if behind_hub {
            let mut hub = SimDevice::hub(Speed::High, 4);
            hub.plug(2, SimDevice::disk(speed, 4096));
            host.sim.plug(3, hub);
            vec![2]
        } else {
            host.sim.plug(root, SimDevice::disk(speed, 4096));
            vec![]
        };
        let root = if behind_hub { 3 } else { root };
        host.owned = 1 << pipe_dci(Pipe::In) | 1 << pipe_dci(Pipe::Out);
        let mut tables = Tables::new(&host);
        settle(&mut host, &mut tables);
        let slot = host.sim.device_at(root, &route).expect("a disk").slot;
        let hub = behind_hub.then(|| host.sim.device_at(root, &[]).unwrap().slot);
        let translated = (behind_hub && speed < Speed::High).then_some(slot);
        let page = host.mem.page();
        let data = (0..32).map(|_| host.mem.page()).collect();
        let max_packet = match speed {
            Speed::Low | Speed::Full => 64,
            Speed::High => 512,
            _ => 1024,
        };
        let rig = Rig {
            host,
            tables,
            slot,
            hub,
            root,
            route,
            max_packet,
            page,
            held: vec![Vec::new(); QUEUE],
            data,
            escalated: 0,
            stuck: 0,
            skip_toggle_reset: false,
            busy_commands: 0,
        };
        Self {
            rig,
            transport: Transport::new(Interface {
                interface: 0,
                bulk_in: IN,
                bulk_out: OUT,
                translated,
            }),
        }
    }

    fn qemu() -> Self {
        Self::new(crate::xhci::sim::Config::qemu(), Speed::High, false)
    }

    fn step(&mut self) {
        self.host.drain();
        self.rig.tables.tree().step(&mut self.rig.host);
        self.host.drain();
        let mut wire = StickWire {
            bench: &mut self.rig,
        };
        self.transport.completed(&mut wire);
        self.transport.serve(&mut wire);
    }

    /// Steps a millisecond at a time until `done`, for at most `limit_ms`.
    fn run(&mut self, limit_ms: u64, mut done: impl FnMut(&mut Self) -> bool) -> bool {
        for _ in 0..limit_ms {
            self.step();
            if done(self) {
                return true;
            }
            self.host.sim.advance_us(1000);
        }
        false
    }

    fn with_wire<R>(&mut self, f: impl FnOnce(&mut Transport, &mut StickWire<'_>) -> R) -> R {
        f(
            &mut self.transport,
            &mut StickWire {
                bench: &mut self.rig,
            },
        )
    }

    fn direct(&mut self, command: Command) -> Done {
        let tag = self
            .with_wire(|t, w| t.submit(w, 0, command, None, true))
            .expect("room");
        let mut done = None;
        let finished = self.run(4 * TAG_MS, |b| {
            done = b.transport.take(tag);
            done.is_some()
        });
        assert!(finished, "a direct command never finished");
        done.unwrap()
    }

    /// Submits an engine request over data pages `from..`, unfinished.
    fn submit(&mut self, write: bool, lba: u64, blocks: u32, from: usize) -> u16 {
        let command = Command::transfer(write, lba, blocks, 512).unwrap();
        let pages = self.pages(from, (blocks as usize * 512).div_ceil(PAGE_SIZE));
        self.with_wire(|t, w| t.submit(w, 0, command, Some(&pages), false))
            .expect("room")
    }

    fn finish(&mut self, tags: &[u16]) -> Vec<Outcome> {
        let mut outcomes = vec![None; tags.len()];
        let done = self.run(4 * TAG_MS, |b| {
            while let Some((tag, outcome)) = b.transport.pop() {
                if let Some(at) = tags.iter().position(|&t| t == tag) {
                    outcomes[at] = Some(outcome);
                }
            }
            outcomes.iter().all(Option::is_some)
        });
        assert!(
            done,
            "requests never finished: {:?} {:#?} {:?}",
            outcomes, self.transport, self.host.sim.violations
        );
        outcomes.into_iter().map(Option::unwrap).collect()
    }

    fn transfer(&mut self, write: bool, lba: u64, blocks: u32) -> Outcome {
        let tag = self.submit(write, lba, blocks, 0);
        self.finish(&[tag])[0]
    }
}

impl Rig {
    fn disk(&mut self) -> &mut SimStorage {
        self.host
            .sim
            .device_mut(self.root, route_of(&self.route))
            .expect("plugged")
            .disk_storage()
    }

    fn device(&self) -> &SimDevice {
        self.host
            .sim
            .device_at(self.root, &self.route)
            .expect("plugged")
    }

    fn pages(&self, from: usize, count: usize) -> Vec<u64> {
        self.data[from..from + count]
            .iter()
            .map(DmaPage::phys)
            .collect()
    }

    fn fill(&mut self, from: usize, count: usize, seed: u8) {
        for (i, page) in self.data[from..from + count].iter_mut().enumerate() {
            let bytes: Vec<u8> = (0..PAGE_SIZE)
                .map(|b| seed.wrapping_add((i * 7 + b) as u8))
                .collect();
            page.write_bytes(0, &bytes);
        }
    }

    fn read_page(&self, index: usize) -> Vec<u8> {
        let mut bytes = vec![0; PAGE_SIZE];
        self.data[index].read_bytes(0, &mut bytes);
        bytes
    }

    fn clear_halts(&self, address: u8) -> usize {
        self.device()
            .requests
            .iter()
            .filter(|&&r| r == Setup::clear_halt(address))
            .count()
    }

    fn mass_storage_resets(&self) -> usize {
        self.device()
            .requests
            .iter()
            .filter(|&&r| r == Setup::mass_storage_reset(0))
            .count()
    }
}

fn route_of(hubs: &[u8]) -> u32 {
    hubs.iter()
        .enumerate()
        .fold(0, |r, (tier, &p)| r | u32::from(p) << (4 * tier))
}

impl StickWire<'_> {
    fn ring(&mut self, pipe: Pipe) -> &mut crate::xhci::TransferRing<SimPage> {
        let dci = pipe_dci(pipe);
        let slot = self.bench.slot;
        &mut self
            .bench
            .host
            .slots
            .get_mut(&slot)
            .expect("a slot")
            .rings
            .get_mut(&dci)
            .expect("a ring")
            .0
    }

    fn slot_of(&self, target: Target) -> u8 {
        match target {
            Target::Device => self.bench.slot,
            Target::TranslatorHub => self.bench.hub.expect("a hub"),
        }
    }

    fn submit_trb(&mut self, trb: Trb) -> Result<Ticket, SubmitError> {
        let host = &mut self.bench.host;
        let doorbell = controller::command_doorbell(&host.layout);
        host.commands.submit(&mut host.sim, doorbell, trb)
    }
}

impl Wire for StickWire<'_> {
    fn now_ms(&mut self) -> u64 {
        self.bench.host.sim.now_us() / 1000
    }

    fn gone(&mut self) -> bool {
        self.bench
            .host
            .slots
            .get(&self.bench.slot)
            .is_none_or(|s| s.gone)
    }

    fn hold(&mut self, slot: u8, pages: &[u64]) {
        self.bench.held[usize::from(slot)] = pages.to_vec();
    }

    fn write_wrapper(&mut self, cbw: &[u8; 31]) {
        self.bench.page.write_bytes(CBW_AT, cbw);
    }

    fn read_wire(&mut self, at: usize, out: &mut [u8]) {
        self.bench.page.read_bytes(at, out);
    }

    fn bulk(&mut self, pipe: Pipe, buffer: Buffer, length: u32) -> Result<Transfer, PushError> {
        let base = self.bench.page.phys();
        let pages = match buffer {
            Buffer::Wrapper => vec![base + CBW_AT as u64],
            Buffer::Status => vec![base + CSW_AT as u64],
            Buffer::Scratch => vec![base + SCRATCH_AT as u64],
            Buffer::Sense => vec![base + SENSE_AT as u64],
            Buffer::Held(slot) => self.bench.held[usize::from(slot)].clone(),
        };
        let max_packet = self.bench.max_packet;
        let transfer = self.ring(pipe).bulk(&pages, length, max_packet)?;
        let slot = self.bench.slot;
        self.bench.host.doorbell(slot, pipe_dci(pipe));
        Ok(transfer)
    }

    fn bulk_result(&mut self, pipe: Pipe, transfer: Transfer) -> Option<TransferResult> {
        self.ring(pipe).take(transfer)
    }

    fn abandon(&mut self, pipe: Pipe, transfer: Transfer) {
        self.ring(pipe).abandon(transfer);
    }

    fn control(&mut self, target: Target, setup: Setup) -> Result<Transfer, PushError> {
        let slot = self.slot_of(target);
        crate::bus::Host::control(&mut self.bench.host, slot, setup)
    }

    fn control_result(&mut self, target: Target, transfer: Transfer) -> Option<TransferResult> {
        let slot = self.slot_of(target);
        crate::bus::Host::transfer_result(&mut self.bench.host, slot, 1, transfer)
    }

    fn abandon_control(&mut self, target: Target, transfer: Transfer) {
        let slot = self.slot_of(target);
        crate::bus::Host::abandon_transfer(&mut self.bench.host, slot, 1, transfer);
    }

    fn command(&mut self, command: PipeCommand) -> Result<Ticket, SubmitError> {
        if self.bench.busy_commands > 0 {
            self.bench.busy_commands -= 1;
            return Err(SubmitError::Busy);
        }
        let slot = self.bench.slot;
        match command {
            PipeCommand::Reset(pipe) => {
                self.submit_trb(Trb::reset_endpoint(slot, pipe_dci(pipe), false))
            }
            PipeCommand::Stop(pipe) => {
                self.submit_trb(Trb::stop_endpoint(slot, pipe_dci(pipe), false))
            }
            PipeCommand::Reconfigure(mask) if self.bench.skip_toggle_reset => {
                let mut ticket = Err(SubmitError::Busy);
                for pipe in [Pipe::In, Pipe::Out] {
                    if mask & pipe.bit() != 0 {
                        let (dequeue, cycle) = self.ring(pipe).recovery_dequeue();
                        let trb = Trb::set_tr_dequeue(dequeue, cycle, slot, pipe_dci(pipe));
                        ticket = self.submit_trb(trb);
                    }
                }
                ticket
            }
            PipeCommand::Reconfigure(mask) => {
                let mut endpoints = Vec::new();
                for pipe in [Pipe::In, Pipe::Out] {
                    if mask & pipe.bit() != 0 {
                        let (dequeue, cycle) = self.ring(pipe).recovery_dequeue();
                        endpoints.push((pipe_dci(pipe), dequeue, cycle));
                    }
                }
                let contexts = self.bench.host.contexts;
                let s = self.bench.host.slots.get_mut(&slot).expect("a slot");
                write_readded(&s.output, &mut s.input, contexts, &endpoints);
                let input = s.input.phys();
                self.submit_trb(Trb::configure_endpoint(input, slot, false))
            }
        }
    }

    fn command_result(&mut self, ticket: Ticket) -> Option<CommandResult> {
        self.bench.host.commands.take(ticket)
    }

    fn abandon_command(&mut self, ticket: Ticket) {
        self.bench.host.commands.abandon(ticket);
    }

    fn stuck(&mut self) {
        self.bench.stuck += 1;
        crate::bus::Host::stuck(&mut self.bench.host);
    }

    fn recovered(&mut self, pipe: Pipe) {
        self.ring(pipe).recovered();
    }

    fn escalate(&mut self) {
        self.bench.escalated += 1;
    }
}

#[test]
fn a_disk_answers_the_probe_commands() {
    let mut b = Bench::qemu();
    b.disk().luns[0].not_ready = 2;
    let inquiry = b.direct(Command::inquiry());
    assert_eq!((inquiry.outcome, inquiry.moved), (Outcome::Ok, 36));
    let mut bytes = [0u8; 36];
    b.page.read_bytes(SCRATCH_AT, &mut bytes);
    assert!(scsi::Inquiry::parse(&bytes).unwrap().is_disk());
    for _ in 0..2 {
        let ready = b.direct(Command::test_unit_ready());
        assert_eq!(ready.outcome, Outcome::Retry);
        assert_eq!(ready.sense.map(|s| s.key), Some(sense_key::NOT_READY));
    }
    assert_eq!(b.direct(Command::test_unit_ready()).outcome, Outcome::Ok);
    let capacity = b.direct(Command::read_capacity_10());
    assert_eq!((capacity.outcome, capacity.moved), (Outcome::Ok, 8));
    let mut bytes = [0u8; 8];
    b.page.read_bytes(SCRATCH_AT, &mut bytes);
    assert_eq!(
        scsi::Capacity::parse_10(&bytes),
        Some(scsi::CapacityAnswer::Known(scsi::Capacity {
            blocks: 4096,
            block_size: 512
        }))
    );
    b.disk().luns[0].write_protected = true;
    let mode = b.direct(Command::mode_sense());
    assert_eq!(
        (mode.outcome, mode.moved),
        (Outcome::Ok, 4),
        "a short data stage"
    );
    let mut header = [0u8; 4];
    b.page.read_bytes(SCRATCH_AT, &mut header);
    assert_eq!(scsi::write_protected(&header), Some(true));
    assert_eq!(b.transport.counts, Counts::default());
    no_violations(&b.host);
}

#[test]
fn whole_requests_reach_the_media_and_come_back() {
    let mut b = Bench::qemu();
    b.fill(0, 30, 0x5a);
    assert_eq!(b.transfer(true, 8, 240), Outcome::Ok);
    let written = b.read_page(29);
    assert_eq!(
        &b.disk().luns[0].media[8 * 512 + 29 * PAGE_SIZE..8 * 512 + 30 * PAGE_SIZE],
        written.as_slice()
    );
    b.fill(0, 30, 0);
    assert_eq!(b.transfer(false, 8, 240), Outcome::Ok);
    assert_eq!(b.read_page(29), written);
    assert_eq!(b.transport.counts, Counts::default());
    no_violations(&b.host);
}

#[test]
fn a_request_past_the_media_fails_without_recovery() {
    let mut b = Bench::qemu();
    assert_eq!(b.transfer(false, 4095, 8), Outcome::Fail);
    assert_eq!(
        (b.transport.counts.clears, b.transport.counts.resets),
        (1, 0)
    );
    assert_eq!(b.disk().commands.last(), Some(&0x03), "its sense was read");
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    no_violations(&b.host);
}

#[test]
fn a_recovered_error_that_moved_nothing_is_retried() {
    let mut b = Bench::qemu();
    b.disk().faults.fail_with = Some((1, sense_key::RECOVERED_ERROR));
    assert_eq!(b.transfer(false, 0, 16), Outcome::Retry);
    assert_eq!(b.transfer(false, 0, 16), Outcome::Ok);
    no_violations(&b.host);
}

#[test]
fn a_write_to_a_protected_disk_fails_and_a_unit_attention_retries() {
    let mut b = Bench::qemu();
    b.disk().luns[0].write_protected = true;
    assert_eq!(b.transfer(true, 0, 8), Outcome::Fail);
    b.disk().faults.fail_with = Some((1, sense_key::UNIT_ATTENTION));
    assert_eq!(b.transfer(false, 0, 8), Outcome::Retry);
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    no_violations(&b.host);
}

#[test]
fn a_stalled_data_stage_is_cleared_and_its_status_read() {
    for write in [false, true] {
        let mut b = Bench::qemu();
        b.disk().faults.stall_data = 1;
        b.disk().faults.stall_sense = sense_key::MEDIUM_ERROR;
        assert_eq!(b.transfer(write, 0, 8), Outcome::Fail);
        assert_eq!(b.transport.counts.clears, 1);
        assert_eq!(b.transport.counts.resets, 0);
        let address = if write { OUT } else { IN };
        assert_eq!(b.clear_halts(address), 1);
        assert_eq!(b.transfer(write, 0, 8), Outcome::Ok, "toggles still agree");
        no_violations(&b.host);
    }
}

#[test]
fn a_stalled_status_is_read_again_once() {
    let mut b = Bench::qemu();
    b.disk().faults.stall_csw = 1;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    assert_eq!(b.transport.counts.clears, 1);
    assert_eq!(b.clear_halts(IN), 1);
    b.disk().faults.zero_length_csw = 1;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    assert_eq!(b.transport.counts.resets, 0);
    b.disk().faults.stall_csw = 2;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    assert_eq!(b.transport.counts.resets, 1, "a second stall resets it");
    no_violations(&b.host);
}

#[test]
fn a_phase_error_takes_reset_recovery_and_the_command_again() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 1;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    assert_eq!(b.transport.counts.resets, 1);
    let reads = b.disk().commands.iter().filter(|&&c| c == 0x28).count();
    assert_eq!(reads, 2, "the READ went again");
    let requests = &b.device().requests;
    let at = |setup: Setup| requests.iter().rposition(|&r| r == setup);
    let reset = at(Setup::mass_storage_reset(0));
    let (clear_in, clear_out) = (at(Setup::clear_halt(IN)), at(Setup::clear_halt(OUT)));
    assert!(
        reset.is_some() && reset < clear_in && clear_in < clear_out,
        "reset, then bulk-in, then bulk-out (BOT §5.3.4)"
    );

    b.disk().faults.phase_error = 1;
    b.fill(0, 2, 9);
    assert_eq!(b.transfer(true, 0, 16), Outcome::Ok);
    assert_eq!(b.mass_storage_resets(), 2);
    let written = b.read_page(0);
    assert_eq!(b.disk().luns[0].media[..PAGE_SIZE], written[..]);
    no_violations(&b.host);
}

#[test]
fn a_command_that_fails_again_after_its_reissue_fails() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 2;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Fail);
    assert_eq!(b.transport.counts.resets, 2);
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    no_violations(&b.host);
}

#[test]
fn a_status_that_is_not_valid_takes_reset_recovery() {
    let mut b = Bench::qemu();
    b.disk().faults.invalid_csw = 1;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    assert_eq!(b.transport.counts.resets, 1);
    no_violations(&b.host);
}

#[test]
fn a_stalled_wrapper_takes_reset_recovery_and_a_probe_command_is_not_sent_again() {
    let mut b = Bench::qemu();
    b.disk().faults.stall_cbw = 1;
    assert_eq!(b.direct(Command::test_unit_ready()).outcome, Outcome::Retry);
    assert_eq!(b.direct(Command::test_unit_ready()).outcome, Outcome::Ok);
    assert_eq!(b.transport.counts.resets, 1);
    assert_eq!(b.disk().resets, 1);
    no_violations(&b.host);
}

#[test]
fn without_the_controllers_toggle_reset_the_reissue_is_lost() {
    let mut b = Bench::qemu();
    b.skip_toggle_reset = true;
    b.disk().faults.invalid_csw = 1;
    let _ = b.transfer(false, 0, 8);
    assert!(
        b.host.sim.violations.contains(&"a data toggle out of step"),
        "{:?}",
        b.host.sim.violations
    );
}

#[test]
fn a_device_that_never_answers_is_reset_after_its_deadline() {
    let mut b = Bench::qemu();
    b.disk().faults.silent = 1;
    let tag = b.submit(false, 0, 8, 0);
    let early = b.run(COMMAND_MS - 100, |b| b.transport.pop().is_some());
    assert!(!early, "nothing finishes before the deadline");
    assert_eq!(b.finish(&[tag]), [Outcome::Ok]);
    assert_eq!(b.transport.counts.resets, 1);
    assert!(b.host.sim.now_us() / 1000 < COMMAND_MS + RECOVERY_MS);
    no_violations(&b.host);
}

#[test]
fn a_slow_command_holds_the_one_queued_behind_it() {
    let mut b = Bench::qemu();
    let busy = b.host.sim.now_us() + 3_000_000;
    b.disk().faults.busy_until_us = busy;
    b.fill(0, 1, 1);
    let first = b.submit(true, 0, 8, 0);
    let second = b.submit(false, 0, 8, 1);
    let early = b.run(2000, |b| b.transport.pop().is_some());
    assert!(!early);
    assert_eq!(b.finish(&[first, second]), [Outcome::Ok, Outcome::Ok]);
    assert!(b.host.sim.now_us() >= busy);
    assert_eq!(b.read_page(1), b.read_page(0), "in order");
    assert_eq!(b.transport.counts, Counts::default());
    no_violations(&b.host);
}

#[test]
fn queued_commands_reach_the_device_in_the_order_submitted() {
    let mut b = Bench::qemu();
    b.disk().faults.busy_until_us = b.host.sim.now_us() + 1_000_000;
    let read = b.submit(false, 0, 8, 0);
    let mut direct = Vec::new();
    for command in [
        Command::inquiry(),
        Command::test_unit_ready(),
        Command::read_capacity_10(),
    ] {
        direct.push(
            b.with_wire(|t, w| t.submit(w, 0, command, None, true))
                .expect("room"),
        );
    }
    assert_eq!(b.finish(&[read]), [Outcome::Ok]);
    assert!(b.run(TAG_MS, |b| b.transport.is_idle()));
    for tag in direct {
        assert_eq!(b.transport.take(tag).map(|d| d.outcome), Some(Outcome::Ok));
    }
    let sent = b.disk().commands.clone();
    assert_eq!(sent[sent.len() - 4..], [0x28, 0x12, 0x00, 0x25]);
}

#[test]
fn sense_never_lands_on_a_direct_commands_data() {
    let mut b = Bench::qemu();
    let alone = b.direct(Command::mode_sense());
    let mut want = vec![0u8; alone.moved as usize];
    b.page.read_bytes(SCRATCH_AT, &mut want);
    let tag = b
        .with_wire(|t, w| t.submit(w, 0, Command::mode_sense(), None, true))
        .expect("room");
    let past = b.submit(false, 4095, 8, 0);
    assert_eq!(b.finish(&[past]), [Outcome::Fail]);
    let done = b.transport.take(tag).expect("finished before the read");
    let mut got = vec![0u8; done.moved as usize];
    b.page.read_bytes(SCRATCH_AT, &mut got);
    assert_eq!(got, want);
}

#[test]
fn a_reset_the_device_refuses_escalates_and_fails_every_command() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 1;
    b.disk().faults.stall_reset = true;
    let first = b.submit(false, 0, 8, 0);
    let second = b.submit(false, 8, 8, 1);
    assert_eq!(b.finish(&[first, second]), [Outcome::Fail, Outcome::Fail]);
    assert_eq!(b.escalated, 1);
    assert!(b.transport.is_broken());
    assert_eq!(b.transfer(false, 0, 8), Outcome::Fail);
}

#[test]
fn a_full_speed_stick_behind_a_high_speed_hub_has_its_translator_cleared() {
    let mut b = Bench::new(Config::qemu(), Speed::Full, true);
    b.disk().faults.phase_error = 1;
    assert_eq!(b.direct(Command::test_unit_ready()).outcome, Outcome::Retry);
    let hub = b.host.sim.device_at(3, &[]).unwrap();
    let clears = &hub.hub.as_ref().unwrap().tt_clears;
    let address = u16::from(b.device().address) << 4;
    assert_eq!(
        clears.as_slice(),
        &[
            (1 | address | 2 << 11 | 1 << 15, 1),
            (2 | address | 2 << 11, 1)
        ]
    );
    no_violations(&b.host);
}

#[test]
fn a_controller_reporting_both_ends_of_a_short_td_reads_the_same() {
    let mut b = Bench::new(Config::intel(), Speed::High, false);
    let mode = b.direct(Command::mode_sense());
    assert_eq!((mode.outcome, mode.moved), (Outcome::Ok, 4));
    assert_eq!(b.direct(Command::test_unit_ready()).outcome, Outcome::Ok);
    no_violations(&b.host);
}

#[test]
fn a_read_cut_short_inside_its_td_is_retried_on_either_event_shape() {
    for config in [Config::qemu(), Config::intel()] {
        let mut b = Bench::new(config, Speed::High, false);
        b.disk().faults.short_read = Some(PAGE_SIZE + 1000);
        assert_eq!(b.transfer(false, 0, 24), Outcome::Retry);
        assert_eq!(b.transfer(false, 0, 24), Outcome::Ok);
        assert_eq!(b.transport.counts, Counts::default());
        no_violations(&b.host);
    }
}

#[test]
fn a_disk_without_a_cache_is_flushed_once() {
    let mut b = Bench::qemu();
    b.disk().luns[0].no_cache = true;
    assert_eq!(b.direct(Command::synchronize_cache()).outcome, Outcome::Ok);
    assert!(!b.transport.has_cache(0));
    let sent = b.disk().commands.len();
    assert_eq!(b.direct(Command::synchronize_cache()).outcome, Outcome::Ok);
    assert_eq!(b.disk().commands.len(), sent, "not asked again");
    let mut b = Bench::qemu();
    assert_eq!(b.direct(Command::synchronize_cache()).outcome, Outcome::Ok);
    assert_eq!(b.disk().luns[0].syncs, 1);
    assert!(b.transport.has_cache(0));
}

#[test]
fn a_recovery_step_waits_for_room_on_the_command_ring() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 1;
    b.busy_commands = 50;
    assert_eq!(b.transfer(false, 0, 8), Outcome::Ok);
    assert_eq!((b.busy_commands, b.escalated), (0, 0));
    no_violations(&b.host);
}

#[test]
fn a_recovery_step_that_finds_no_room_within_its_time_escalates() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 1;
    b.busy_commands = u32::MAX;
    let tag = b.submit(false, 0, 8, 0);
    let started = b.host.sim.now_us() / 1000;
    assert_eq!(b.finish(&[tag]), [Outcome::Fail]);
    assert_eq!(b.escalated, 1);
    assert!(b.host.sim.now_us() / 1000 - started < COMMAND_MS + RECOVERY_MS);
}

#[test]
fn a_probe_command_that_hung_fails_and_one_recovered_at_once_retries() {
    let mut b = Bench::qemu();
    b.disk().faults.silent = 1;
    assert_eq!(b.direct(Command::inquiry()).outcome, Outcome::Fail);
    b.disk().faults.phase_error = 1;
    assert_eq!(b.direct(Command::inquiry()).outcome, Outcome::Retry);
    assert_eq!(b.direct(Command::inquiry()).outcome, Outcome::Ok);
}

#[test]
fn a_stick_pulled_during_recovery_fails_what_it_held_as_gone() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 1;
    let tag = b.submit(false, 0, 8, 0);
    assert!(b.run(TAG_MS, |b| b.transport.counts.resets == 1));
    let root = b.root;
    let _ = b.host.sim.detach(root);
    assert_eq!(b.finish(&[tag]), [Outcome::Gone]);
}

#[test]
fn a_transport_left_mid_recovery_strands_no_command() {
    let mut b = Bench::qemu();
    b.disk().faults.phase_error = 1;
    let tag = b.submit(false, 0, 8, 0);
    assert!(b.run(TAG_MS, |b| b.host.commands.outstanding() > 0));
    b.with_wire(|t, w| t.leave(w));
    assert_eq!(b.transport.pop(), Some((tag, Outcome::Gone)));
    assert!(b.run(1000, |b| b.host.commands.outstanding() == 0));
    assert!(b.transport.is_broken());
}

#[test]
fn a_stick_pulled_mid_command_fails_everything_at_once() {
    let mut b = Bench::qemu();
    b.disk().faults.silent = 1;
    let first = b.submit(false, 0, 8, 0);
    let second = b.submit(false, 8, 8, 1);
    b.run(5, |_| false);
    let root = b.root;
    let _ = b.host.sim.detach(root);
    let gone = b.finish(&[first, second]);
    assert_eq!(gone, [Outcome::Gone, Outcome::Gone]);
    assert!(
        b.host.sim.now_us() / 1000 < 1000,
        "at once, not at a deadline"
    );
    assert_eq!(b.transfer(false, 0, 8), Outcome::Gone);
}

#[test]
fn a_full_queue_takes_nothing_more() {
    let mut b = Bench::qemu();
    b.disk().faults.silent = 1;
    for i in 0..QUEUE {
        b.submit(false, 0, 1, i);
    }
    let command = Command::transfer(false, 0, 1, 512).unwrap();
    let pages = b.pages(0, 1);
    assert_eq!(
        b.with_wire(|t, w| t.submit(w, 0, command, Some(&pages), false)),
        None
    );
}

#[test]
fn a_direct_command_let_go_keeps_no_completion() {
    let mut b = Bench::qemu();
    for _ in 0..2 * QUEUE {
        let tag = b
            .with_wire(|t, w| t.submit(w, 0, Command::test_unit_ready(), None, true))
            .expect("room");
        b.transport.retire(tag);
        assert!(b.run(TAG_MS, |b| b.transport.is_idle()));
        assert_eq!(b.transport.take(tag), None);
    }
    assert_eq!(b.direct(Command::test_unit_ready()).outcome, Outcome::Ok);
}

#[test]
fn recovery_once_forbidden_fails_the_command_instead() {
    let mut b = Bench::qemu();
    b.transport.forbid_recovery();
    b.disk().faults.phase_error = 1;
    assert_eq!(
        b.direct(Command::synchronize_cache()).outcome,
        Outcome::Fail
    );
    assert_eq!(b.escalated, 0);
    assert_eq!(b.transport.counts.resets, 0);
    assert!(b.transport.is_idle());
}
