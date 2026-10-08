//! The tree over the simulated controller: a host that keeps each slot's
//! memory as the kernel does, and devices that stall, babble, misstate
//! lengths, leave mid-enumeration, sit behind hubs of both generations, and
//! a controller that dies under them.

use super::*;
use crate::device::descriptor::tests::{configuration, endpoint, interface};
use crate::device::descriptor::{Function, kind};
use crate::device::request::Setup;
use crate::hub::change;
use crate::xhci::RegisterBus;
use crate::xhci::context::{
    ContextLayout, EndpointContext, InputControlContext, endpoint_state, read_context, write_input,
};
use crate::xhci::controller;
use crate::xhci::ext_cap::{self, Protocols};
use crate::xhci::memory::{DmaPage, set_device_context, write_segment_table};
use crate::xhci::regs::{Capabilities, Layout, PortSc};
use crate::xhci::ring::{CommandCompletion, CommandRing, EventRing, RING_TRBS};
use crate::xhci::sim::{BAR_LEN, Config, Faults, Memory, SimController, SimDevice, SimPage, Stuck};
use crate::xhci::transfer::TransferRing;
use crate::xhci::trb::{Event, Trb};
use std::collections::{BTreeMap, BTreeSet};
use std::vec;
use std::vec::Vec;

pub(crate) struct Slot {
    pub(crate) output: SimPage,
    pub(crate) input: SimPage,
    pub(crate) ep0: TransferRing<SimPage>,
    pub(crate) control: SimPage,
    pub(crate) store: Vec<u8>,
    pub(crate) rings: BTreeMap<u8, (TransferRing<SimPage>, SimPage)>,
    pub(crate) gone: bool,
}

impl Slot {
    fn ring(&mut self, dci: u8) -> Option<&mut TransferRing<SimPage>> {
        if dci == 1 {
            Some(&mut self.ep0)
        } else {
            self.rings.get_mut(&dci).map(|(ring, _)| ring)
        }
    }
}

pub(crate) struct SimHost {
    pub(crate) sim: SimController,
    pub(crate) mem: Memory,
    pub(crate) layout: Layout,
    pub(crate) protocols: Protocols,
    pub(crate) contexts: ContextLayout,
    pub(crate) commands: CommandRing<SimPage>,
    pub(crate) events: EventRing<SimPage>,
    pub(crate) dcbaa: SimPage,
    _table: SimPage,
    pub(crate) slots: BTreeMap<u8, Slot>,
    pub(crate) changes: BTreeSet<u8>,
    pub(crate) reports: Vec<Report>,
    pub(crate) wants: fn(&Candidate) -> bool,
    pub(crate) offered: BTreeSet<u8>,
    pub(crate) ever_offered: Vec<u8>,
    pub(crate) unbound: Vec<u8>,
    pub(crate) destroyed: Vec<u8>,
    /// Passes the drivers take to bind or unbind a device.
    pub(crate) bind_passes: u32,
    /// How often each slot had been disabled when its memory was made.
    pub(crate) disables_at_create: BTreeMap<u8, usize>,
    pub(crate) binding: BTreeMap<u8, u32>,
    pub(crate) unbinding: BTreeMap<u8, u32>,
    /// A command after this many would be issued to a dead controller.
    pub(crate) commands_when_dead: Option<usize>,
    pub(crate) stuck: usize,
    /// Set TR Dequeue commands refused as if the ring were full.
    pub(crate) busy_dequeues: u32,
    /// Events drained since the tree's last step, which the kernel's work
    /// stamp counts against a settled reading.
    pub(crate) fresh_events: bool,
    /// What the host gets wrong, to show the simulator catches it.
    pub(crate) sabotage: Sabotage,
    /// DCI bits of endpoints a driver recovers itself.
    pub(crate) owned: u32,
    /// Slots whose driver's recovery failed.
    pub(crate) escalations: BTreeSet<u8>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Sabotage {
    #[default]
    None,
    /// Address Device names no transaction translator.
    NoTt,
    /// Configure Endpoint never marks a hub's slot a hub.
    NoHubFlag,
}

fn every(_: &Candidate) -> bool {
    true
}

fn count_down(pending: &mut BTreeMap<u8, u32>, slot: u8) -> bool {
    match pending.get_mut(&slot) {
        Some(passes) if *passes > 0 => {
            *passes -= 1;
            false
        }
        _ => true,
    }
}

impl SimHost {
    pub(crate) fn new(config: Config) -> Self {
        let mut sim = SimController::new(config);
        let caps = Capabilities::read(&mut sim, BAR_LEN).unwrap();
        let layout = caps.layout();
        let found = ext_cap::find(&mut sim, &layout, BAR_LEN);
        controller::take_ownership(&mut sim, found.legacy);
        controller::halt_and_reset(&mut sim, &layout, controller::PROBE_RESET_MS).unwrap();
        let mem = sim.mem.clone();
        let dcbaa = mem.page();
        let commands = CommandRing::new(mem.page());
        let events_page = mem.page();
        let mut table = mem.page();
        write_segment_table(&mut table, events_page.phys(), RING_TRBS);
        let setup = controller::Setup {
            slots: caps.max_slots,
            dcbaa: dcbaa.phys(),
            crcr: commands.crcr(),
            segment_table: table.phys(),
            event_ring: events_page.phys(),
        };
        controller::configure(&mut sim, &layout, &setup);
        controller::start(&mut sim, &layout).unwrap();
        for port in 1..=caps.max_ports {
            controller::power_port(&mut sim, &layout, port);
        }
        let mut host = Self {
            sim,
            mem,
            layout,
            protocols: found.protocols,
            contexts: ContextLayout::new(caps.context_64),
            commands,
            events: EventRing::new(events_page),
            dcbaa,
            _table: table,
            slots: BTreeMap::new(),
            changes: BTreeSet::new(),
            reports: Vec::new(),
            wants: every,
            offered: BTreeSet::new(),
            ever_offered: Vec::new(),
            unbound: Vec::new(),
            destroyed: Vec::new(),
            bind_passes: 0,
            disables_at_create: BTreeMap::new(),
            binding: BTreeMap::new(),
            unbinding: BTreeMap::new(),
            commands_when_dead: None,
            stuck: 0,
            busy_dequeues: 0,
            fresh_events: false,
            sabotage: Sabotage::None,
            owned: 0,
            escalations: BTreeSet::new(),
        };
        host.drain();
        host.changes.clear();
        host
    }

    pub(crate) fn drain(&mut self) {
        let mut events = Vec::new();
        controller::drain(
            &mut self.sim,
            &self.layout,
            &mut self.events,
            controller::DRAIN_BUDGET,
            |e| events.push(e),
        );
        self.fresh_events |= !events.is_empty();
        for event in events {
            match event {
                Event::CommandCompletion {
                    trb,
                    code,
                    parameter,
                    slot,
                } => {
                    let done = CommandCompletion {
                        code,
                        parameter,
                        slot,
                    };
                    self.commands.complete(trb, done);
                }
                Event::Transfer {
                    trb,
                    residual,
                    code,
                    slot,
                    dci,
                    ..
                } => {
                    if let Some(ring) = self.slots.get_mut(&slot).and_then(|s| s.ring(dci)) {
                        ring.complete(trb, code, residual);
                    }
                }
                Event::PortStatusChange { port } => {
                    self.changes.insert(port);
                }
                _ => {}
            }
        }
    }

    pub(crate) fn doorbell(&mut self, slot: u8, dci: u8) {
        let at = self.layout.doorbell(slot);
        self.sim.write32(at, u32::from(dci));
    }

    fn removed(&self) -> Vec<u8> {
        self.reports
            .iter()
            .filter_map(|r| match r {
                Report::Removed { slot, .. } => Some(*slot),
                _ => None,
            })
            .collect()
    }

    fn enumerated(&self) -> BTreeSet<u8> {
        self.reports
            .iter()
            .filter_map(|r| match r {
                Report::Enumerated { slot, .. } => Some(*slot),
                _ => None,
            })
            .collect()
    }

    fn failures(&self) -> Vec<Failure> {
        self.reports
            .iter()
            .filter_map(|r| match r {
                Report::Failed { failure, .. } => Some(*failure),
                _ => None,
            })
            .collect()
    }

    fn given_up(&self) -> usize {
        self.reports
            .iter()
            .filter(|r| matches!(r, Report::GivenUp { .. }))
            .count()
    }
}

impl Host for SimHost {
    fn now_ms(&mut self) -> u64 {
        self.sim.now_us() / 1000
    }

    fn root_change(&mut self) -> Option<u8> {
        self.changes.pop_first()
    }

    fn root_status(&mut self, port: u8) -> Option<PortStatus> {
        let read = controller::acknowledge_port(&mut self.sim, &self.layout, port)?;
        if read.unsettled {
            self.changes.insert(port);
        }
        let speed = self
            .protocols
            .speed(port, read.status.speed())
            .map(|s| Speed::from_bits_per_second(s.bits_per_second));
        Some(read.status.status(speed))
    }

    fn root_reset(&mut self, port: u8) {
        let portsc = PortSc(self.sim.read32(self.layout.port(port)));
        self.sim.write32(self.layout.port(port), portsc.reset());
    }

    fn root_disable(&mut self, port: u8) {
        let portsc = PortSc(self.sim.read32(self.layout.port(port)));
        self.sim.write32(self.layout.port(port), portsc.disable());
    }

    fn root_protocol(&mut self, port: u8) -> (bool, u8) {
        let usb3 = self.protocols.of_port(port).is_some_and(|p| p.major == 3);
        (usb3, 0)
    }

    fn command(&mut self, command: Command) -> Result<Ticket, SubmitError> {
        if matches!(command, Command::SetDequeue { .. }) && self.busy_dequeues > 0 {
            self.busy_dequeues -= 1;
            return Err(SubmitError::Busy);
        }
        if self.sim.halted() {
            self.commands_when_dead = Some(self.sim.commands_seen.len());
        }
        let contexts = self.contexts;
        let trb = match command {
            Command::EnableSlot { slot_type } => Trb::enable_slot(slot_type),
            Command::AddressDevice {
                slot,
                mut context,
                max_packet,
            } => {
                if self.sabotage == Sabotage::NoTt {
                    context.tt_hub_slot = 0;
                    context.tt_port = 0;
                }
                let s = self.slots.get_mut(&slot).expect("created");
                let (dequeue, cycle) = s.ep0.dequeue();
                let ep0 = EndpointContext::control(max_packet, dequeue, cycle);
                let control = InputControlContext {
                    add: 0b11,
                    ..InputControlContext::default()
                };
                write_input(
                    &mut s.input,
                    contexts,
                    &control,
                    Some(&context),
                    &[(1, ep0)],
                );
                Trb::address_device(s.input.phys(), slot, false)
            }
            Command::EvaluateMaxPacket { slot, max_packet } => {
                let s = self.slots.get_mut(&slot).expect("created");
                let current = read_context(&s.output, contexts.device_endpoint(1));
                let mut ep0 = EndpointContext::decode(&current);
                ep0.max_packet_size = max_packet;
                let control = InputControlContext {
                    add: 0b10,
                    ..InputControlContext::default()
                };
                write_input(&mut s.input, contexts, &control, None, &[(1, ep0)]);
                Trb::evaluate_context(s.input.phys(), slot)
            }
            Command::Configure {
                slot,
                mut context,
                speed,
            } => {
                let mem = self.mem.clone();
                let s = self.slots.get_mut(&slot).expect("created");
                let mut endpoints = Vec::new();
                if let Ok(config) = Configuration::parse(&s.store[STORE_CONFIGURATION..]) {
                    for_each_configured_endpoint(&config, |dci, e| endpoints.push((dci, e)));
                }
                let mut contexts_added = Vec::new();
                let mut add = 1;
                for &(dci, endpoint) in &endpoints {
                    let ring = TransferRing::new(mem.page());
                    let (dequeue, cycle) = ring.dequeue();
                    contexts_added.push((
                        dci,
                        EndpointContext::for_endpoint(&endpoint, speed, dequeue, cycle),
                    ));
                    s.rings.insert(dci, (ring, mem.page()));
                    add |= 1 << dci;
                }
                context.context_entries = endpoints.iter().map(|&(d, _)| d).max().unwrap_or(1);
                if self.sabotage == Sabotage::NoHubFlag {
                    context.hub = false;
                }
                let control = InputControlContext {
                    add,
                    ..InputControlContext::default()
                };
                write_input(
                    &mut s.input,
                    contexts,
                    &control,
                    Some(&context),
                    &contexts_added,
                );
                Trb::configure_endpoint(s.input.phys(), slot, false)
            }
            Command::ConfigureHub { slot, mut context } => {
                if self.sabotage == Sabotage::NoHubFlag {
                    context.hub = false;
                }
                let s = self.slots.get_mut(&slot).expect("created");
                let current = SlotContext::decode(&read_context(&s.output, 0));
                context.context_entries = current.context_entries;
                let control = InputControlContext {
                    add: 1,
                    ..InputControlContext::default()
                };
                write_input(&mut s.input, contexts, &control, Some(&context), &[]);
                Trb::configure_endpoint(s.input.phys(), slot, false)
            }
            Command::ResetEndpoint { slot, dci } => Trb::reset_endpoint(slot, dci, false),
            Command::StopEndpoint { slot, dci } => Trb::stop_endpoint(slot, dci, false),
            Command::SetDequeue { slot, dci } => {
                let s = self.slots.get_mut(&slot).expect("created");
                let (dequeue, cycle) = s.ring(dci).expect("a ring").recovery_dequeue();
                Trb::set_tr_dequeue(dequeue, cycle, slot, dci)
            }
            Command::DisableSlot { slot } => Trb::disable_slot(slot),
        };
        let doorbell = controller::command_doorbell(&self.layout);
        self.commands.submit(&mut self.sim, doorbell, trb)
    }

    fn command_result(&mut self, ticket: Ticket) -> Option<CommandResult> {
        self.commands.take(ticket)
    }

    fn abandon_command(&mut self, ticket: Ticket) {
        self.commands.abandon(ticket);
    }

    fn stuck(&mut self) {
        self.stuck += 1;
        let _ = controller::quiesce(&mut self.sim, &self.layout);
        self.commands.fail_all();
    }

    fn create(&mut self, slot: u8) -> bool {
        let disables = self.sim.disabled.iter().filter(|&&s| s == slot).count();
        self.disables_at_create.insert(slot, disables);
        let mem = &self.mem;
        let output = mem.page();
        set_device_context(&mut self.dcbaa, slot, output.phys());
        self.slots.insert(
            slot,
            Slot {
                output,
                input: mem.page(),
                ep0: TransferRing::new(mem.page()),
                control: mem.page(),
                store: vec![0; STORE_LEN],
                rings: BTreeMap::new(),
                gone: false,
            },
        );
        true
    }

    fn destroy(&mut self, slot: u8) {
        let disables = self.sim.disabled.iter().filter(|&&s| s == slot).count();
        assert!(
            self.sim.halted() || disables > self.disables_at_create[&slot],
            "a slot's memory released before Disable Slot"
        );
        set_device_context(&mut self.dcbaa, slot, 0);
        assert!(self.slots.remove(&slot).is_some(), "destroyed twice");
        assert!(
            !self.offered.contains(&slot) || self.unbound.contains(&slot),
            "destroyed while bound"
        );
        assert!(
            self.unbinding.get(&slot).is_none_or(|&passes| passes == 0),
            "destroyed while its drivers' removals run"
        );
        self.unbinding.remove(&slot);
        self.binding.remove(&slot);
        self.offered.remove(&slot);
        self.destroyed.push(slot);
    }

    fn control(&mut self, slot: u8, setup: Setup) -> Result<Transfer, PushError> {
        let s = self.slots.get_mut(&slot).ok_or(PushError::Halted)?;
        if s.gone {
            return Err(PushError::Halted);
        }
        let buffer = s.control.phys();
        let transfer = s.ep0.control(setup, buffer)?;
        self.doorbell(slot, 1);
        Ok(transfer)
    }

    fn interrupt_in(&mut self, slot: u8, dci: u8, length: u16) -> Result<Transfer, PushError> {
        let s = self.slots.get_mut(&slot).ok_or(PushError::Halted)?;
        if s.gone {
            return Err(PushError::Halted);
        }
        let (ring, buffer) = s.rings.get_mut(&dci).ok_or(PushError::Halted)?;
        let transfer = ring.normal(buffer.phys(), length.into())?;
        self.doorbell(slot, dci);
        Ok(transfer)
    }

    fn transfer_result(&mut self, slot: u8, dci: u8, transfer: Transfer) -> Option<TransferResult> {
        self.slots.get_mut(&slot)?.ring(dci)?.take(transfer)
    }

    fn abandon_transfer(&mut self, slot: u8, dci: u8, transfer: Transfer) {
        if let Some(ring) = self.slots.get_mut(&slot).and_then(|s| s.ring(dci)) {
            ring.abandon(transfer);
        }
    }

    fn read(&mut self, slot: u8, dci: u8, out: &mut [u8]) {
        let Some(s) = self.slots.get(&slot) else {
            return;
        };
        let page = if dci == 1 {
            &s.control
        } else {
            match s.rings.get(&dci) {
                Some((_, buffer)) => buffer,
                None => return,
            }
        };
        page.read_bytes(0, out);
    }

    fn keep(&mut self, slot: u8, at: usize, length: usize) {
        if let Some(s) = self.slots.get_mut(&slot) {
            let mut bytes = vec![0; length];
            s.control.read_bytes(0, &mut bytes);
            s.store[at..at + length].copy_from_slice(&bytes);
        }
    }

    fn stored<R>(&mut self, slot: u8, read: impl FnOnce(&[u8]) -> R) -> Option<R> {
        self.slots.get(&slot).map(|s| read(&s.store))
    }

    fn halted(&mut self, slot: u8) -> u32 {
        let Some(s) = self.slots.get(&slot).filter(|s| !s.gone) else {
            return 0;
        };
        s.rings
            .iter()
            .filter(|(_, (ring, _))| ring.is_halted())
            .fold(u32::from(s.ep0.is_halted()) << 1, |bits, (&dci, _)| {
                bits | 1 << dci
            })
            & !self.owned
    }

    fn recovered(&mut self, slot: u8, dci: u8) {
        if let Some(ring) = self.slots.get_mut(&slot).and_then(|s| s.ring(dci)) {
            ring.recovered();
        }
    }

    fn escalated(&mut self, slot: u8) -> bool {
        self.escalations.remove(&slot)
    }

    fn running_endpoints(&mut self, slot: u8) -> u32 {
        let contexts = self.contexts;
        let Some(s) = self.slots.get(&slot) else {
            return 0;
        };
        (1..32u8)
            .filter(|&dci| {
                let ep = EndpointContext::decode(&read_context(
                    &s.output,
                    contexts.device_endpoint(dci),
                ));
                ep.state == endpoint_state::RUNNING
            })
            .fold(0, |bits, dci| bits | 1 << dci)
    }

    fn gone(&mut self, slot: u8) {
        if let Some(s) = self.slots.get_mut(&slot) {
            s.gone = true;
        }
    }

    fn fail_transfers(&mut self, slot: u8, error: crate::xhci::TransferError) {
        if let Some(s) = self.slots.get_mut(&slot) {
            s.ep0.fail_all(error);
            for (ring, _) in s.rings.values_mut() {
                ring.fail_all(error);
            }
        }
    }

    fn wanted(&mut self, candidate: &Candidate) -> bool {
        (self.wants)(candidate)
    }

    fn offer(&mut self, slot: u8, _node: &Node) {
        assert!(self.offered.insert(slot), "offered twice");
        self.ever_offered.push(slot);
        self.binding.insert(slot, self.bind_passes);
    }

    fn offered(&mut self, slot: u8) -> bool {
        count_down(&mut self.binding, slot)
    }

    fn unbind(&mut self, slot: u8) {
        self.unbound.push(slot);
        self.unbinding.insert(slot, self.bind_passes);
    }

    fn unbound(&mut self, slot: u8) -> bool {
        count_down(&mut self.unbinding, slot)
    }

    fn report(&mut self, report: Report) {
        self.reports.push(report);
    }
}

pub(crate) struct Tables {
    pub(crate) roots: Vec<Port>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) hubs: Vec<Hub>,
    pub(crate) state: State,
}

impl Tables {
    pub(crate) fn new(host: &SimHost) -> Self {
        Self {
            roots: vec![Port::default(); usize::from(host.layout.max_ports())],
            nodes: vec![Node::default(); host.sim.slots.len()],
            hubs: vec![Hub::default(); 4],
            state: State::default(),
        }
    }

    pub(crate) fn tree(&mut self) -> Tree<'_> {
        Tree {
            roots: &mut self.roots,
            nodes: &mut self.nodes,
            hubs: &mut self.hubs,
            state: &mut self.state,
        }
    }

    fn live(&self) -> usize {
        self.nodes.iter().filter(|n| n.stage != Stage::Free).count()
    }
}

pub(crate) fn run_until(
    host: &mut SimHost,
    tables: &mut Tables,
    limit_ms: u64,
    mut done: impl FnMut(&mut SimHost, &mut Tables) -> bool,
) -> bool {
    for _ in 0..limit_ms {
        host.drain();
        tables.tree().step(host);
        host.fresh_events = false;
        host.drain();
        if done(host, tables) {
            return true;
        }
        host.sim.delay_us(1000);
    }
    false
}

pub(crate) fn settle(host: &mut SimHost, tables: &mut Tables) {
    let settled = run_until(host, tables, 20_000, |host, tables| {
        !host.fresh_events && tables.tree().settled(host)
    });
    assert!(settled, "never settled: {:?}", host.reports);
}

pub(crate) fn no_violations(host: &SimHost) {
    assert!(host.sim.violations.is_empty(), "{:?}", host.sim.violations);
}

/// A stick on SuperSpeed port 1, one on USB 2 port 3, and a full-speed
/// keyboard whose EP0 is 64 bytes on port 4.
fn three_speeds() -> SimHost {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(1, SimDevice::storage(Speed::Super));
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut keyboard = SimDevice::keyboard(Speed::Full);
    keyboard.descriptor[7] = 64;
    host.sim.plug(4, keyboard);
    host
}

#[test]
fn enumerates_a_superspeed_a_high_speed_and_a_full_speed_device() {
    let mut host = three_speeds();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 3, "{:?}", host.reports);
    assert_eq!(host.offered.len(), 3);
    assert_eq!(host.ever_offered.len(), 3);
    for port in [1, 3, 4] {
        let device = host.sim.device_at(port, &[]).unwrap();
        assert_ne!(device.address, 0);
        assert_eq!(device.configuration, 1);
    }
    assert_eq!(host.sim.port_resets, 2, "only the USB 2 ports are reset");
    let keyboard = host.sim.device_at(4, &[]).unwrap().slot;
    assert_eq!(
        host.sim.slots[usize::from(keyboard)].endpoints[1].max_packet,
        64
    );
    assert!(
        host.sim
            .commands_seen
            .contains(&crate::xhci::trb::kind::EVALUATE_CONTEXT)
    );
    let stick = host.sim.device_at(1, &[]).unwrap().slot;
    let ep = host.sim.slots[usize::from(stick)].endpoints;
    assert_eq!(
        (ep[3].state, ep[4].state),
        (endpoint_state::RUNNING, endpoint_state::RUNNING)
    );
    assert_eq!(ep[2].state, endpoint_state::DISABLED);
    assert_eq!(ep[3].max_packet, 1024);
    let node = tables.nodes[usize::from(stick)];
    assert_eq!(
        (node.vendor, node.product, node.functions),
        (0x46f4, 0x0001, 1)
    );
    assert_eq!(node.path, Path::root(1));
}

#[test]
fn one_device_holds_the_default_state_at_a_time() {
    let mut host = SimHost::new(Config::intel());
    for port in 1..=4 {
        host.sim.plug(port, SimDevice::storage(Speed::High));
    }
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 4);
    assert!(tables.state.default.is_none());
}

#[test]
fn a_controller_with_64_byte_contexts_and_switched_power_enumerates() {
    let mut host = SimHost::new(Config::intel());
    host.sim.plug(5, SimDevice::storage(Speed::Super));
    host.sim.plug(2, SimDevice::keyboard(Speed::Low));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 2, "{:?}", host.reports);
}

/// A high-speed hub on USB 2 port 3 with a full-speed keyboard, a
/// low-speed one, a high-speed stick and a full-speed hub behind it, and a
/// stick behind that.
fn high_speed_tree() -> SimHost {
    let mut host = SimHost::new(Config::qemu());
    let mut hub = SimDevice::hub(Speed::High, 4);
    hub.plug(1, SimDevice::keyboard(Speed::Full));
    hub.plug(2, SimDevice::keyboard(Speed::Low));
    hub.plug(3, SimDevice::storage(Speed::High));
    let mut inner = SimDevice::hub(Speed::Full, 4);
    inner.plug(2, SimDevice::storage(Speed::Full));
    hub.plug(4, inner);
    host.sim.plug(3, hub);
    host
}

#[test]
fn a_high_speed_hub_translates_for_the_slow_devices_behind_it() {
    let mut host = high_speed_tree();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 6, "{:?}", host.reports);
    let hub = host.sim.device_at(3, &[]).unwrap().slot;
    let low = host.sim.device_at(3, &[2]).unwrap().slot;
    let deep = host.sim.device_at(3, &[4, 2]).unwrap().slot;
    let fast = host.sim.device_at(3, &[3]).unwrap().slot;
    let context = |slot: u8| host.sim.slots[usize::from(slot)].context;
    assert_eq!((context(low).tt_hub_slot, context(low).tt_port), (hub, 2));
    assert_eq!((context(deep).tt_hub_slot, context(deep).tt_port), (hub, 4));
    assert_eq!(context(deep).route_string, 0x24);
    assert_eq!((context(fast).tt_hub_slot, context(fast).tt_port), (0, 0));
    assert_eq!(host.sim.slots[usize::from(hub)].hub_ports, Some(4));
    assert_eq!(host.sim.slots[usize::from(hub)].context.tt_think_time, 1);
    assert!(
        !host.offered.contains(&hub),
        "a hub is the tree's, never offered"
    );
    let paths: BTreeSet<std::string::String> = tables
        .nodes
        .iter()
        .filter(|n| n.stage == Stage::Running)
        .map(|n| std::format!("{}", n.path))
        .collect();
    let want: BTreeSet<std::string::String> = ["3", "3.1", "3.2", "3.3", "3.4", "3.4.2"]
        .iter()
        .map(|s| std::string::String::from(*s))
        .collect();
    assert_eq!(paths, want);
}

#[test]
fn superspeed_hubs_are_given_their_depth_and_route_their_children() {
    let mut host = SimHost::new(Config::qemu());
    let mut outer = SimDevice::hub(Speed::Super, 4);
    let mut inner = SimDevice::hub(Speed::Super, 4);
    inner.plug(1, SimDevice::storage(Speed::Super));
    outer.plug(2, SimDevice::storage(Speed::Super));
    outer.plug(3, inner);
    host.sim.plug(1, outer);
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 4, "{:?}", host.reports);
    let depth = |hubs: &[u8]| {
        host.sim
            .device_at(1, hubs)
            .unwrap()
            .hub
            .as_ref()
            .unwrap()
            .depth
    };
    assert_eq!(depth(&[]), Some(0));
    assert_eq!(depth(&[3]), Some(1));
    let deep = host.sim.device_at(1, &[3, 1]).unwrap().slot;
    assert_eq!(host.sim.slots[usize::from(deep)].context.route_string, 0x13);
    assert_eq!(host.sim.port_resets, 0, "a SuperSpeed port enables itself");
}

#[test]
fn qemus_full_speed_hub_carries_two_devices() {
    let mut host = SimHost::new(Config::qemu());
    let mut hub = SimDevice::hub(Speed::Full, 8);
    if let Some(h) = hub.hub.as_mut() {
        h.power_switching = false;
    }
    hub.plug(1, SimDevice::keyboard(Speed::Full));
    hub.plug(2, SimDevice::storage(Speed::Full));
    host.sim.plug(4, hub);
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 3);
    let stick = host.sim.device_at(4, &[2]).unwrap().slot;
    assert_eq!(host.sim.slots[usize::from(stick)].context.tt_hub_slot, 0);
}

fn pull_tree(host: &mut SimHost, tables: &mut Tables) {
    host.sim.detach(3);
    let gone = run_until(host, tables, 10_000, |_, tables| tables.live() == 0);
    assert!(gone, "{:?}", host.reports);
}

#[test]
fn a_pulled_hub_takes_its_children_first_and_leaks_nothing() {
    let mut host = high_speed_tree();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let hub = host.sim.device_at(3, &[]).unwrap().slot;
    let inner = host.sim.device_at(3, &[4]).unwrap().slot;
    let deep = host.sim.device_at(3, &[4, 2]).unwrap().slot;
    pull_tree(&mut host, &mut tables);
    no_violations(&host);
    let removed = host.removed();
    assert_eq!(removed.len(), 6);
    let at = |slot| removed.iter().position(|&s| s == slot).unwrap();
    assert!(at(deep) < at(inner) && at(inner) < at(hub));
    assert!(host.slots.is_empty(), "every slot's memory released");
    assert_eq!(host.sim.enabled_slots(), 0, "every slot disabled");
    assert!(tables.hubs.iter().all(|h| h.slot == 0));
    let mut unbound = host.unbound.clone();
    unbound.sort_unstable();
    for slot in host.ever_offered.clone() {
        assert!(unbound.contains(&slot), "slot {slot} was never unbound");
    }
    let stops = host
        .sim
        .commands_seen
        .iter()
        .filter(|&&k| k == crate::xhci::trb::kind::STOP_ENDPOINT)
        .count();
    assert!(stops >= 6, "every running endpoint stopped: {stops}");
}

#[test]
fn devices_survive_being_pulled_and_plugged_again_and_again() {
    let mut host = high_speed_tree();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    for round in 0..3 {
        let tree = host.sim.detach(3).unwrap();
        let gone = run_until(&mut host, &mut tables, 10_000, |_, t| t.live() == 0);
        assert!(gone, "round {round}");
        let fresh = high_speed_tree().sim.roots[2].take().unwrap();
        let _ = tree;
        host.sim.plug(3, fresh);
        settle(&mut host, &mut tables);
        assert_eq!(tables.live(), 6, "round {round}: {:?}", host.reports);
        no_violations(&host);
    }
    assert_eq!(host.slots.len(), 6);
    assert_eq!(host.sim.enabled_slots(), 6);
}

#[test]
fn a_device_pulled_from_a_hub_port_goes_alone() {
    let mut host = high_speed_tree();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let stick = host.sim.device_at(3, &[3]).unwrap().slot;
    host.sim.unplug_at(3, &[], 3).unwrap();
    let gone = run_until(&mut host, &mut tables, 5_000, |host, _| {
        host.removed() == [stick]
    });
    assert!(gone, "{:?}", host.reports);
    assert!(host.reports.iter().any(
        |r| matches!(r, Report::Disconnected { path } if *path == Path::root(3).child(3).unwrap())
    ));
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 5);
    host.sim.plug_at(3, &[], 3, SimDevice::storage(Speed::High));
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 6);
    no_violations(&host);
}

fn faulty(faults: Faults) -> (SimHost, Tables) {
    let mut host = SimHost::new(Config::qemu());
    let mut stick = SimDevice::storage(Speed::High);
    stick.faults = faults;
    host.sim.plug(3, stick);
    let tables = Tables::new(&host);
    (host, tables)
}

fn assert_given_up(host: &mut SimHost, tables: &mut Tables) {
    settle(host, tables);
    assert_eq!(host.given_up(), 1, "{:?}", host.reports);
    assert_eq!(host.failures().len(), 3);
    assert_eq!(tables.live(), 0);
    assert!(host.slots.is_empty());
    assert_eq!(host.sim.enabled_slots(), 0);
    assert_eq!(
        host.sim.portsc(3) & crate::xhci::regs::PORT_ENABLED,
        0,
        "port disabled"
    );
}

#[test]
fn a_device_that_always_stalls_is_given_up_after_three_tries() {
    let (mut host, mut tables) = faulty(Faults {
        stall_descriptor: Some(kind::CONFIGURATION),
        ..Faults::default()
    });
    assert_given_up(&mut host, &mut tables);
    assert!(
        host.failures()
            .iter()
            .all(|f| *f == Failure::Transfer(crate::xhci::TransferError::Stall))
    );
    host.sim.detach(3);
    host.sim.plug(3, SimDevice::storage(Speed::High));
    settle(&mut host, &mut tables);
    assert_eq!(
        tables.live(),
        1,
        "a fresh device on the port is tried afresh"
    );
}

#[test]
fn a_device_that_stalls_once_enumerates_on_its_second_try() {
    let (mut host, mut tables) = faulty(Faults {
        stall_descriptor: Some(kind::DEVICE),
        stalls: 1,
        ..Faults::default()
    });
    settle(&mut host, &mut tables);
    assert_eq!(host.failures().len(), 1);
    assert_eq!(host.enumerated().len(), 1);
    assert_eq!(host.removed().len(), 1, "the failed try's slot was removed");
    no_violations(&host);
}

#[test]
fn a_babbling_device_is_given_up() {
    let (mut host, mut tables) = faulty(Faults {
        babble_descriptor: Some(kind::DEVICE),
        ..Faults::default()
    });
    assert_given_up(&mut host, &mut tables);
    assert_eq!(
        host.failures()[0],
        Failure::Transfer(crate::xhci::TransferError::Babble)
    );
}

#[test]
fn misstated_lengths_fail_their_try() {
    for (kind_, len) in [(kind::CONFIGURATION, 20), (kind::DEVICE, 12)] {
        let (mut host, mut tables) = faulty(Faults {
            truncate: Some((kind_, len)),
            ..Faults::default()
        });
        assert_given_up(&mut host, &mut tables);
        assert!(
            matches!(host.failures()[0], Failure::Descriptor(_)),
            "{:?}",
            host.failures()
        );
    }
}

#[test]
fn descriptors_out_of_bounds_fail_their_try() {
    let mut host = SimHost::new(Config::qemu());
    let mut odd = SimDevice::keyboard(Speed::Full);
    odd.descriptor[7] = 7;
    host.sim.plug(3, odd);
    let mut huge = SimDevice::storage(Speed::High);
    huge.configurations[0][2..4].copy_from_slice(&5000u16.to_le_bytes());
    host.sim.plug(4, huge);
    let mut none = SimDevice::storage(Speed::Super);
    none.descriptor[17] = 0;
    host.sim.plug(1, none);
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let failures = host.failures();
    assert!(failures.contains(&Failure::MaxPacket(7)), "{failures:?}");
    assert!(failures.contains(&Failure::ConfigurationTooLarge(5000)));
    assert!(failures.contains(&Failure::NoConfiguration));
    assert!(
        !failures.iter().any(|f| matches!(f, Failure::BadSlot(_))),
        "a slot freed by one try is handed to the next: {failures:?}"
    );
    assert_eq!(host.given_up(), 3);
    assert!(host.slots.is_empty());
}

#[test]
fn a_device_pulled_mid_enumeration_leaves_nothing_behind() {
    let (mut host, mut tables) = faulty(Faults::default());
    settle(&mut host, &mut tables);
    let requests = host.sim.device_at(3, &[]).unwrap().requests.len() as u32;
    for at in 1..=requests {
        let (mut host, mut tables) = faulty(Faults {
            pull_at: Some(at),
            ..Faults::default()
        });
        settle(&mut host, &mut tables);
        assert!(host.sim.roots[2].is_none(), "pulled at request {at}");
        assert_eq!(
            tables.live(),
            0,
            "pulled at request {at}: {:?}",
            host.reports
        );
        assert!(host.slots.is_empty());
        assert_eq!(host.sim.enabled_slots(), 0);
        assert_eq!(
            host.given_up(),
            0,
            "a device that left is not held against its port"
        );
        host.sim.plug(3, SimDevice::storage(Speed::High));
        settle(&mut host, &mut tables);
        assert_eq!(tables.live(), 1, "pulled at request {at}");
        no_violations(&host);
    }
}

#[test]
fn a_device_behind_a_hub_pulled_mid_enumeration_leaves_nothing_behind() {
    let mut host = SimHost::new(Config::qemu());
    let mut hub = SimDevice::hub(Speed::High, 4);
    let mut stick = SimDevice::storage(Speed::High);
    stick.faults.pull_at = Some(3);
    hub.plug(2, stick);
    host.sim.plug(3, hub);
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 1, "the hub alone: {:?}", host.reports);
    assert_eq!(host.slots.len(), 1);
    host.sim.plug_at(3, &[], 2, SimDevice::storage(Speed::High));
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 2);
    no_violations(&host);
}

#[test]
fn a_controller_that_dies_mid_transfer_takes_every_device_with_no_command() {
    let mut host = high_speed_tree();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let hub = host.sim.device_at(3, &[]).unwrap().slot;
    let pending = host
        .control(hub, crate::hub::get_port_status(1))
        .expect("submitted");
    host.sim.host_system_error();
    let issued = host.sim.commands_seen.len();
    host.commands.fail_all();
    tables.tree().die(&mut host);
    let gone = run_until(&mut host, &mut tables, 1000, |_, t| t.live() == 0);
    assert!(gone);
    assert_eq!(host.sim.commands_seen.len(), issued);
    assert_eq!(
        host.commands_when_dead, None,
        "no command was even submitted"
    );
    assert!(host.slots.is_empty());
    assert_eq!(host.removed().len(), 6);
    assert!(host.slots.is_empty());
    let _ = pending;
}

#[test]
fn a_bus_powered_hub_offers_one_unit_load() {
    let mut host = SimHost::new(Config::qemu());
    let mut hub = SimDevice::hub(Speed::High, 4).bus_powered();
    let mut greedy = SimDevice::storage(Speed::High);
    greedy.configurations[0][8] = 250;
    hub.plug(1, greedy);
    hub.plug(2, SimDevice::keyboard(Speed::Full));
    host.sim.plug(3, hub);
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    let greedy = host.sim.device_at(3, &[1]).unwrap();
    assert_eq!(greedy.configuration, 0, "its configuration is not set");
    assert!(host.reports.iter().any(|r| matches!(
        r,
        Report::Unpowered { slot, needs_ma: 500, offers_ma: 100, .. } if *slot == greedy.slot
    )));
    assert_eq!(host.sim.device_at(3, &[2]).unwrap().configuration, 1);
}

#[test]
fn the_first_configuration_a_driver_wants_is_chosen() {
    let vendor = configuration(
        1,
        50,
        &[
            &interface(0, 0, 1, [0xff, 0, 0]),
            &endpoint(0x81, 2, 512, 0),
        ],
    );
    let storage = configuration(
        2,
        50,
        &[
            &interface(0, 0, 2, [8, 6, 0x50]),
            &endpoint(0x81, 2, 512, 0),
            &endpoint(0x02, 2, 512, 0),
        ],
    );
    let device = |configs: Vec<Vec<u8>>| {
        let mut d = SimDevice::storage(Speed::High);
        d.descriptor[17] = configs.len() as u8;
        d.configurations = configs;
        d
    };
    fn storage_only(candidate: &Candidate) -> bool {
        candidate.function.class == 8
    }
    let mut host = SimHost::new(Config::qemu());
    host.wants = storage_only;
    host.sim
        .plug(3, device(vec![vendor.clone(), storage.clone()]));
    host.sim
        .plug(4, device(vec![vendor.clone(), vendor.clone()]));
    host.sim
        .plug(5, device(vec![storage.clone(), vendor.clone()]));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.sim.device_at(3, &[]).unwrap().configuration, 2);
    assert_eq!(
        host.sim.device_at(4, &[]).unwrap().configuration,
        1,
        "else the first"
    );
    assert_eq!(host.sim.device_at(5, &[]).unwrap().configuration, 2);
    let second = host.sim.device_at(3, &[]).unwrap().slot;
    let rings = &host.slots[&second].rings;
    assert_eq!(rings.keys().copied().collect::<Vec<_>>(), [3, 4]);
}

#[test]
fn a_stalled_request_halts_ep0_until_the_tree_recovers_it() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(3, &[]).unwrap().slot;
    let stalled = host
        .control(slot, Setup::get_descriptor(0x42, 0, 0, 8))
        .unwrap();
    host.drain();
    assert_eq!(
        host.transfer_result(slot, 1, stalled),
        Some(Err(crate::xhci::TransferError::Stall))
    );
    assert_eq!(
        host.control(slot, Setup::get_status()),
        Err(PushError::Halted)
    );
    let recovered = run_until(&mut host, &mut tables, 100, |host, _| {
        host.halted(slot) == 0
    });
    assert!(recovered);
    let next = host.control(slot, Setup::get_status()).unwrap();
    host.drain();
    assert_eq!(host.transfer_result(slot, 1, next), Some(Ok(2)));
    no_violations(&host);
}

#[test]
fn settle_waits_out_the_debounce_and_every_enumeration() {
    let mut host = three_speeds();
    let mut tables = Tables::new(&host);
    host.drain();
    tables.tree().step(&mut host);
    assert!(!tables.tree().settled(&mut host));
    settle(&mut host, &mut tables);
    let mut empty = SimHost::new(Config::qemu());
    let mut tables = Tables::new(&empty);
    tables.tree().step(&mut empty);
    assert!(
        !tables.tree().settled(&mut empty),
        "not before a device could signal attach"
    );
    empty.sim.delay_us(200_000);
    assert!(tables.tree().settled(&mut empty));
}

#[test]
fn a_connection_that_never_holds_still_is_given_up() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    for _ in 0..100 {
        host.sim.detach(3);
        host.sim.plug(3, SimDevice::storage(Speed::High));
        host.drain();
        tables.tree().step(&mut host);
        host.sim.delay_us(30_000);
    }
    assert_eq!(host.failures()[0], Failure::Debounce, "{:?}", host.reports);
    assert!(host.slots.len() <= 1);
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 1, "once it holds still it enumerates");
}

#[test]
fn a_full_slot_table_fails_enable_slot() {
    let mut host = SimHost::new(Config {
        max_slots: 2,
        ..Config::qemu()
    });
    for port in [1, 2, 3] {
        host.sim.plug(
            port,
            SimDevice::storage(if port < 3 { Speed::Super } else { Speed::High }),
        );
    }
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 2);
    assert!(host.failures().contains(&Failure::Command(
        crate::xhci::trb::CompletionCode::NO_SLOTS
    )));
}

#[test]
fn a_port_whose_changes_never_clear_is_read_once_a_pass() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    host.sim.flapping = Some(3);
    let mut tables = Tables::new(&host);
    for _ in 0..50 {
        host.drain();
        tables.tree().step(&mut host);
        host.sim.delay_us(10_000);
    }
    host.sim.flapping = None;
    settle(&mut host, &mut tables);
}

#[test]
fn a_controller_with_255_slots_enumerates() {
    let mut host = SimHost::new(Config {
        max_slots: 255,
        ..Config::qemu()
    });
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    assert_eq!(tables.nodes.len(), 256);
    assert_eq!(host.enumerated().len(), 1, "{:?}", host.reports);
}

/// Holds the controller's commands until `release`.
fn ring_command_doorbell(host: &mut SimHost) {
    let at = controller::command_doorbell(&host.layout);
    host.sim.write32(at, 0);
}

#[test]
fn a_device_that_refuses_its_address_keeps_the_turn_until_it_is_reset_again() {
    let mut host = SimHost::new(Config::qemu());
    let mut refuser = SimDevice::storage(Speed::High);
    refuser.faults.refuse_address = 2;
    host.sim.plug(4, refuser);
    let mut tables = Tables::new(&host);
    host.sim.stuck = Some(Stuck::Commands);
    let slotting = run_until(&mut host, &mut tables, 1000, |_, t| {
        matches!(t.roots[3].state, port::State::Slotting { .. })
    });
    assert!(slotting);
    host.sim.plug(3, SimDevice::storage(Speed::High));
    run_until(&mut host, &mut tables, 300, |_, t| {
        t.roots[2].state == port::State::Ready
    });
    assert_eq!(
        tables.roots[2].state,
        port::State::Ready,
        "waiting its turn"
    );
    host.sim.stuck = None;
    ring_command_doorbell(&mut host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.enumerated().len(), 2, "{:?}", host.reports);
    assert_eq!(host.failures().len(), 2);
}

#[test]
fn a_device_that_never_takes_an_address_is_given_up_and_disabled() {
    let mut host = SimHost::new(Config::qemu());
    let mut refuser = SimDevice::storage(Speed::High);
    refuser.faults.refuse_address = u32::MAX;
    host.sim.plug(3, refuser);
    host.sim.plug(4, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    no_violations(&host);
    assert_eq!(host.given_up(), 1, "{:?}", host.reports);
    assert_eq!(host.enumerated().len(), 1);
    assert_eq!(host.sim.portsc(3) & crate::xhci::regs::PORT_ENABLED, 0);
}

#[test]
fn a_hub_that_stalls_every_clear_is_removed_and_its_port_given_up() {
    let mut host = SimHost::new(Config::qemu());
    let mut hub = SimDevice::hub(Speed::High, 4);
    hub.faults.stall_port_clear = true;
    hub.plug(1, SimDevice::keyboard(Speed::Full));
    host.sim.plug(3, hub);
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    assert_eq!(host.given_up(), 1, "{:?}", host.reports);
    assert_eq!(host.failures(), [Failure::Hub; 3]);
    assert_eq!(tables.live(), 0);
    assert!(host.slots.is_empty());
    no_violations(&host);
}

#[test]
fn a_command_that_never_completes_kills_the_controller() {
    let mut host = three_speeds();
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    host.sim.stuck = Some(Stuck::Commands);
    host.sim.detach(3);
    let dead = run_until(&mut host, &mut tables, COMMAND_MS + 1000, |_, t| {
        t.state.dead
    });
    assert!(dead, "{:?}", host.reports);
    assert_eq!(host.stuck, 1);
    let empty = run_until(&mut host, &mut tables, 1000, |host, t| {
        t.live() == 0 && t.tree().settled(host)
    });
    assert!(empty);
    assert!(host.slots.is_empty(), "every slot's memory released");
    assert_eq!(host.removed().len(), 3);
}

#[test]
fn a_controller_that_dies_while_a_hub_port_waits_for_a_slot_leaves_nothing() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::hub(Speed::High, 4));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    host.sim.stuck = Some(Stuck::Commands);
    host.sim.plug_at(3, &[], 1, SimDevice::storage(Speed::High));
    let slotting = run_until(&mut host, &mut tables, 2000, |_, t| {
        t.hubs.iter().any(|h| h.is_slotting())
    });
    assert!(slotting);
    host.sim.host_system_error();
    host.commands.fail_all();
    tables.tree().die(&mut host);
    let gone = run_until(&mut host, &mut tables, 1000, |_, t| t.live() == 0);
    assert!(gone);
    assert!(host.slots.is_empty());
}

#[test]
fn a_stalled_endpoint_is_cleared_on_the_device_too() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(4, SimDevice::keyboard(Speed::Full));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(4, &[]).unwrap().slot;
    host.sim.device_mut(4, 0).unwrap().halted_endpoints = 1 << 3;
    let report = host.interrupt_in(slot, 3, 8).unwrap();
    host.drain();
    assert_eq!(
        host.transfer_result(slot, 3, report),
        Some(Err(crate::xhci::TransferError::Stall))
    );
    let recovered = run_until(&mut host, &mut tables, 100, |host, _| {
        host.halted(slot) == 0
    });
    assert!(recovered);
    let keyboard = host.sim.device_at(4, &[]).unwrap();
    assert_eq!(keyboard.halted_endpoints, 0, "the device's halt is cleared");
    assert!(keyboard.requests.contains(&Setup::clear_halt(0x81)));
    let next = host.interrupt_in(slot, 3, 8).unwrap();
    host.drain();
    assert_eq!(
        host.transfer_result(slot, 3, next),
        None,
        "a NAK, not a STALL"
    );
    no_violations(&host);
}

#[test]
fn a_clear_owed_while_ep0_is_halted_waits_for_ep0() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(4, SimDevice::keyboard(Speed::Full));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(4, &[]).unwrap().slot;
    host.sim.device_mut(4, 0).unwrap().halted_endpoints = 1 << 3;
    let report = host.interrupt_in(slot, 3, 8).unwrap();
    let dequeuing = run_until(&mut host, &mut tables, 100, |_, t| {
        matches!(
            t.nodes[usize::from(slot)].stage,
            Stage::Recovering {
                step: node::Recovery::Reset,
                ..
            }
        )
    });
    assert!(dequeuing);
    let _ = host.transfer_result(slot, 3, report);
    let stalled = host
        .control(slot, Setup::get_descriptor(0x42, 0, 0, 8))
        .unwrap();
    host.drain();
    tables.tree().step(&mut host);
    assert!(
        host.slots[&slot].rings[&3].0.is_halted(),
        "the endpoint stays shut while its clear waits for EP0"
    );
    let _ = host.transfer_result(slot, 1, stalled);
    let recovered = run_until(&mut host, &mut tables, 200, |host, _| {
        host.halted(slot) == 0
    });
    assert!(recovered);
    let keyboard = host.sim.device_at(4, &[]).unwrap();
    assert_eq!(keyboard.halted_endpoints, 0);
    assert!(keyboard.requests.contains(&Setup::clear_halt(0x81)));
    no_violations(&host);
}

fn clears_sent(host: &SimHost, address: u8) -> usize {
    let stick = host.sim.device_at(3, &[]).unwrap();
    stick
        .requests
        .iter()
        .filter(|&&r| r == Setup::clear_halt(address))
        .count()
}

#[test]
fn an_endpoint_whose_clear_stalls_is_left_halted_alone() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(3, &[]).unwrap().slot;
    let stick = host.sim.device_mut(3, 0).unwrap();
    stick.halted_endpoints = 1 << 3;
    stick.faults.stall_clear_halt = 1 << 3;
    let read = host.interrupt_in(slot, 3, 8).unwrap();
    host.drain();
    let _ = host.transfer_result(slot, 3, read);
    run_until(&mut host, &mut tables, 30_000, |_, _| false);
    assert_eq!(clears_sent(&host, 0x81), usize::from(node::MAX_RECOVERIES));
    assert_eq!(host.interrupt_in(slot, 3, 8), Err(PushError::Halted));
    assert!(tables.tree().settled(&mut host));

    host.sim.device_mut(3, 0).unwrap().halted_endpoints = 1 << 4;
    let write = host.interrupt_in(slot, 4, 8).unwrap();
    host.drain();
    let _ = host.transfer_result(slot, 4, write);
    let recovered = run_until(&mut host, &mut tables, 200, |host, _| {
        host.halted(slot) == 1 << 3
    });
    assert!(recovered, "another endpoint still recovers");
    assert_eq!(clears_sent(&host, 0x02), 1);

    let stalled = host
        .control(slot, Setup::get_descriptor(0x42, 0, 0, 8))
        .unwrap();
    host.drain();
    let _ = host.transfer_result(slot, 1, stalled);
    let recovered = run_until(&mut host, &mut tables, 200, |host, _| {
        host.halted(slot) == 1 << 3
    });
    assert!(recovered, "and so does EP0");
    let status = host.control(slot, Setup::get_status()).unwrap();
    host.drain();
    assert_eq!(host.transfer_result(slot, 1, status), Some(Ok(2)));
    no_violations(&host);
}

#[test]
fn a_device_whose_driver_could_not_recover_it_is_enumerated_again() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(3, &[]).unwrap().slot;
    let resets = host.sim.port_resets;
    host.escalations.insert(slot);
    settle(&mut host, &mut tables);
    assert_eq!(host.removed(), [slot]);
    assert!(host.failures().contains(&Failure::Recovery));
    assert!(host.sim.port_resets > resets, "its port is reset");
    assert_eq!(host.enumerated().len(), 1);
    assert_eq!(tables.live(), 1, "and it is back");
    no_violations(&host);
}

#[test]
fn escalations_spend_the_ports_tries_only_soon_after_enumeration() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let escalate = |host: &mut SimHost, tables: &mut Tables| {
        let slot = host.sim.device_at(3, &[]).unwrap().slot;
        host.escalations.insert(slot);
        settle(host, tables);
    };
    for _ in 0..2 * port::MAX_FAILURES {
        host.sim.advance_us(node::SERVED_MS * 1000);
        escalate(&mut host, &mut tables);
        assert_eq!(tables.live(), 1, "a device that served keeps its port");
    }
    for left in (0..port::MAX_FAILURES - 1).rev() {
        escalate(&mut host, &mut tables);
        assert_eq!(
            tables.live(),
            usize::from(left > 0),
            "one that fails again at once spends a try"
        );
    }
    no_violations(&host);
}

#[test]
fn refused_set_dequeues_keep_the_ring_shut_until_the_device_is_cleared() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(3, &[]).unwrap().slot;
    host.sim.device_mut(3, 0).unwrap().halted_endpoints = 1 << 3;
    host.busy_dequeues = u32::from(node::MAX_RECOVERIES);
    let read = host.interrupt_in(slot, 3, 8).unwrap();
    host.drain();
    let _ = host.transfer_result(slot, 3, read);
    let recovered = run_until(&mut host, &mut tables, 200, |host, _| {
        host.halted(slot) == 0
    });
    assert!(recovered);
    assert_eq!(host.busy_dequeues, 0);
    assert_eq!(host.sim.device_at(3, &[]).unwrap().halted_endpoints, 0);
    assert_eq!(clears_sent(&host, 0x81), 1);
    no_violations(&host);
}

#[test]
fn a_hub_whose_status_endpoint_halted_settles_once_it_listens_again() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(3, SimDevice::hub(Speed::High, 4));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    host.sim.device_mut(3, 0).unwrap().halted_endpoints = 1 << 3;
    host.sim.plug_at(3, &[], 1, SimDevice::storage(Speed::High));
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 2, "{:?}", host.reports);
    no_violations(&host);
}

#[test]
fn a_device_pulled_while_its_drivers_bind_waits_for_their_removal() {
    let mut host = SimHost::new(Config::qemu());
    host.bind_passes = 40;
    host.sim.plug(3, SimDevice::storage(Speed::High));
    let mut tables = Tables::new(&host);
    let offered = run_until(&mut host, &mut tables, 2000, |host, _| {
        !host.ever_offered.is_empty()
    });
    assert!(offered);
    assert!(!tables.tree().settled(&mut host), "not while a probe runs");
    host.sim.detach(3);
    settle(&mut host, &mut tables);
    assert_eq!(tables.live(), 0);
    assert!(host.slots.is_empty());
    assert_eq!(host.unbound.len(), 1);
    no_violations(&host);
}

#[test]
fn an_abandoned_transfer_is_moved_past_before_its_ring_runs_again() {
    let mut host = SimHost::new(Config::qemu());
    host.sim.plug(4, SimDevice::keyboard(Speed::Full));
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    let slot = host.sim.device_at(4, &[]).unwrap().slot;
    let report = host.interrupt_in(slot, 3, 8).unwrap();
    host.drain();
    assert_eq!(host.transfer_result(slot, 3, report), None, "a NAK");
    host.abandon_transfer(slot, 3, report);
    assert_eq!(host.interrupt_in(slot, 3, 8), Err(PushError::Halted));
    let recovered = run_until(&mut host, &mut tables, 100, |host, _| {
        host.halted(slot) == 0
    });
    assert!(recovered);
    use crate::xhci::trb::kind::{SET_TR_DEQUEUE, STOP_ENDPOINT};
    assert!(host.sim.commands_seen.contains(&STOP_ENDPOINT));
    assert!(host.sim.commands_seen.contains(&SET_TR_DEQUEUE));
    let keyboard = host.sim.device_at(4, &[]).unwrap();
    assert!(
        !keyboard.requests.contains(&Setup::clear_halt(0x81)),
        "a stopped endpoint keeps both toggles"
    );
    assert!(host.interrupt_in(slot, 3, 8).is_ok());
    no_violations(&host);
}

#[test]
fn devices_with_mutated_descriptors_never_panic_or_leak() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let templates = [
        SimDevice::storage(Speed::High),
        SimDevice::keyboard(Speed::Full),
        SimDevice::hub(Speed::High, 2),
        SimDevice::storage(Speed::Super),
    ];
    for round in 0..120 {
        let mut device = templates[round % templates.len()].clone();
        let r = next();
        if r & 1 == 0 {
            let at = (r >> 8) as usize % device.descriptor.len();
            device.descriptor[at] = (r >> 32) as u8;
        } else {
            let config = &mut device.configurations[0];
            let at = (r >> 8) as usize % config.len();
            config[at] = (r >> 32) as u8;
        }
        let port = if device.speed.is_super() { 1 } else { 3 };
        let mut host = SimHost::new(Config::qemu());
        if device.hub.is_some() {
            device.plug(1, SimDevice::keyboard(Speed::Low));
        }
        host.sim.plug(port, device);
        let mut tables = Tables::new(&host);
        run_until(&mut host, &mut tables, 3000, |host, tables| {
            tables.tree().settled(host)
        });
        host.sim.detach(port);
        let gone = run_until(&mut host, &mut tables, 10_000, |host, tables| {
            tables.live() == 0 && tables.tree().settled(host)
        });
        assert!(gone, "round {round}: {:?}", host.reports);
        assert!(host.slots.is_empty(), "round {round}");
    }
}

#[test]
fn every_function_of_a_configuration_is_offered_by_its_class() {
    let config = configuration(
        1,
        0,
        &[
            &interface(0, 0, 1, [3, 1, 1]),
            &endpoint(0x81, 3, 8, 10),
            &interface(1, 0, 1, [3, 0, 2]),
            &endpoint(0x82, 3, 8, 10),
        ],
    );
    let parsed = Configuration::parse(&config).unwrap();
    let functions = parsed.functions();
    assert_eq!(
        functions.as_slice(),
        &[
            Function {
                first_interface: 0,
                interfaces: 1,
                class: 3,
                subclass: 1,
                protocol: 1
            },
            Function {
                first_interface: 1,
                interfaces: 1,
                class: 3,
                subclass: 0,
                protocol: 2
            },
        ]
    );
    let mut dcis = Vec::new();
    for_each_configured_endpoint(&parsed, |dci, _| dcis.push(dci));
    assert_eq!(dcis, [3, 5]);
    let _ = change::CONNECT;
}

#[test]
fn the_simulated_controller_catches_what_it_checks() {
    let mut host = high_speed_tree();
    host.sabotage = Sabotage::NoTt;
    let mut tables = Tables::new(&host);
    settle(&mut host, &mut tables);
    assert!(
        host.sim
            .violations
            .contains(&"slot context TT fields wrong")
    );

    let mut host = high_speed_tree();
    host.sabotage = Sabotage::NoHubFlag;
    let mut tables = Tables::new(&host);
    run_until(&mut host, &mut tables, 3000, |host, tables| {
        tables.tree().settled(host)
    });
    assert!(
        host.sim
            .violations
            .contains(&"a child addressed before its hub's slot was marked a hub")
    );

    let mut host = SimHost::new(Config::qemu());
    let mut hub = SimDevice::hub(Speed::Super, 2);
    hub.plug(1, SimDevice::storage(Speed::Super));
    host.sim.plug(1, hub);
    let mut tables = Tables::new(&host);
    for _ in 0..2000 {
        host.drain();
        tables.tree().step(&mut host);
        if let Some(hub) = host.sim.roots[0].as_mut().and_then(|d| d.hub.as_mut()) {
            hub.depth = None;
        }
        host.sim.delay_us(1000);
    }
    assert!(
        host.sim
            .violations
            .contains(&"a SuperSpeed hub's child addressed before SET_HUB_DEPTH")
    );
}
