//! A device in a slot: enumeration, endpoint recovery and removal. Removal
//! marks the device gone, stops its endpoints, ends its transfers, removes a
//! hub's children, unbinds its drivers and disables its slot, and only then
//! releases its memory; a dead controller is sent no command.

use super::{
    COMMAND_MS, CONTROL_MS, Candidate, Command, Failure, Host, HubUnused, MAX_CONFIGURATION,
    MAX_CONFIGURATIONS, Path, PortId, Report, STORE_CONFIGURATION, STORE_DEVICE, Tree, port,
};
use crate::device::Speed;
use crate::device::descriptor::{
    Configuration, ConfigurationHeader, DeviceDescriptor, Functions, TransferType, class,
    ep0_max_packet, kind,
};
use crate::device::request::{STATUS_SELF_POWERED, Setup};
use crate::hub::{self as hub_class, HubDescriptor, MAX_DEPTH, MAX_HUB_PORTS};
use crate::xhci::context::{SlotContext, dci};
use crate::xhci::ring::{CommandResult, SubmitError, Ticket};
use crate::xhci::transfer::{Transfer, TransferError, TransferResult};
use crate::xhci::trb::CompletionCode;

/// After `SET_ADDRESS` a device has 2 ms before it must answer (USB 2.0
/// §9.2.6.3); more is given, as devices are slow to it.
pub const ADDRESS_MS: u64 = 10;
/// Failed recovery steps before an endpoint is left halted.
pub const MAX_RECOVERIES: u8 = 3;
/// A device that ran this long before its driver gave it up starts its
/// port's failure count afresh: only a device that fails again soon after
/// enumerating spends the port's tries.
pub const SERVED_MS: u64 = 60_000;

const EP0: u32 = 1 << 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    Reset,
    /// Reset Endpoint found the endpoint running, so it is stopped first.
    Stop,
    Dequeue,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Free,
    Addressing(Ticket),
    /// The recovery interval after `SET_ADDRESS`.
    Settling,
    /// The device descriptor's first eight bytes, for a full-speed EP0.
    MaxPacket(Transfer),
    Evaluating(Ticket),
    Device(Transfer),
    ConfigHeader {
        index: u8,
        transfer: Transfer,
    },
    ConfigBody {
        index: u8,
        transfer: Transfer,
    },
    Configuring(Ticket),
    SetConfiguration(Transfer),
    HubDescriptor(Transfer),
    HubStatus(Transfer),
    SetHubDepth(Transfer),
    /// The slot context made a hub's.
    ConfiguringHub(Ticket),
    Running,
    Recovering {
        dci: u8,
        step: Recovery,
        ticket: Ticket,
    },
    /// Resets the device's data toggle as Reset Endpoint reset the host's.
    ClearingHalt {
        dci: u8,
        transfer: Transfer,
    },
    /// Removal: Stop Endpoint on each running endpoint, one at a time.
    Stopping {
        remaining: u32,
        current: Option<(u8, Ticket)>,
    },
    /// Removal: waiting for a hub's children to be removed.
    Orphaning,
    Unbinding,
    Disabling(Ticket),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub stage: Stage,
    /// When the stage's command, transfer or interval falls due.
    pub deadline: u64,
    pub port: PortId,
    pub path: Path,
    pub speed: Speed,
    /// The high-speed hub whose transaction translator serves a low- or
    /// full-speed device below it, and the port the device is reached by.
    pub tt: Option<(u8, u8)>,
    /// What the device's port offers: one unit load on a bus-powered hub.
    pub budget_ma: u32,
    pub max_packet: u16,
    pub vendor: u16,
    pub product: u16,
    pub class: u8,
    pub configurations: u8,
    /// `bConfigurationValue` set, or 0.
    pub configuration: u8,
    pub functions: u8,
    /// Configured as a hub whose ports are driven.
    pub hub: bool,
    fallback: Option<u8>,
    rereading: bool,
    /// The least power any configuration asked for.
    least_ma: u32,
    /// The slot has memory.
    created: bool,
    /// By DCI, since the endpoint last ran.
    failures: [u8; 32],
    /// DCI bits of endpoints left halted.
    given_up: u32,
    /// DCI bits of endpoints whose ring stays halted until EP0 has carried
    /// their `CLEAR_FEATURE(ENDPOINT_HALT)`.
    owed_clears: u32,
    running_since: u64,
}

impl Node {
    pub(super) fn is_recovered<H: Host>(&self, host: &mut H, slot: u8) -> bool {
        self.owed_clears == 0 && host.halted(slot) & !self.given_up == 0
    }

    pub fn deadline(&self) -> Option<u64> {
        match self.stage {
            Stage::Free | Stage::Running | Stage::Orphaning | Stage::Unbinding => None,
            _ => Some(self.deadline),
        }
    }

    pub fn is_enumerating(&self) -> bool {
        !matches!(
            self.stage,
            Stage::Free
                | Stage::Running
                | Stage::Recovering { .. }
                | Stage::ClearingHalt { .. }
                | Stage::Stopping { .. }
                | Stage::Orphaning
                | Stage::Unbinding
                | Stage::Disabling(_)
        )
    }

    pub fn is_removing(&self) -> bool {
        matches!(
            self.stage,
            Stage::Stopping { .. } | Stage::Orphaning | Stage::Unbinding | Stage::Disabling(_)
        )
    }
}

enum Poll<T> {
    Pending,
    Done(T),
    TimedOut,
}

fn command<H: Host>(host: &mut H, ticket: Ticket, deadline: u64, now: u64) -> Poll<CommandResult> {
    match host.command_result(ticket) {
        Some(result) => Poll::Done(result),
        None if now >= deadline => {
            host.abandon_command(ticket);
            Poll::TimedOut
        }
        None => Poll::Pending,
    }
}

fn control<H: Host>(
    host: &mut H,
    slot: u8,
    transfer: Transfer,
    deadline: u64,
    now: u64,
) -> Poll<TransferResult> {
    match host.transfer_result(slot, 1, transfer) {
        Some(result) => Poll::Done(result),
        None if now >= deadline => {
            host.abandon_transfer(slot, 1, transfer);
            Poll::TimedOut
        }
        None => Poll::Pending,
    }
}

impl Tree<'_> {
    fn node(&mut self, slot: u8) -> &mut Node {
        &mut self.nodes[usize::from(slot)]
    }

    /// Enable Slot gave the port's device `slot`.
    pub(super) fn address<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        id: PortId,
        slot: u8,
        speed: Speed,
    ) {
        let Some(path) = self.path_of(id) else {
            return;
        };
        if !host.create(slot) {
            self.disable_unused(host, now, slot);
            self.port_failed(host, now, id, Failure::NoMemory);
            return;
        }
        let (tt, budget_ma) = self.parent_terms(id, speed);
        let max_packet = speed.initial_max_packet();
        *self.node(slot) = Node {
            port: id,
            path,
            speed,
            tt,
            budget_ma,
            max_packet,
            created: true,
            least_ma: u32::MAX,
            ..Node::default()
        };
        if let Some(port) = self.port_mut(id) {
            port.state = port::State::Attached { slot };
        }
        let context = self.slot_context(slot);
        let submitted = host.command(Command::AddressDevice {
            slot,
            context,
            max_packet,
        });
        self.await_command(host, now, slot, submitted, Stage::Addressing);
    }

    /// The transaction translator a device at `speed` on `id` goes
    /// through, and the current its port offers.
    fn parent_terms(&self, id: PortId, speed: Speed) -> (Option<(u8, u8)>, u32) {
        if id.is_root() {
            return (None, u32::MAX);
        }
        let parent = &self.nodes[usize::from(id.hub)];
        let tt = if speed >= Speed::High {
            None
        } else if parent.speed == Speed::High {
            Some((id.hub, id.port))
        } else {
            parent.tt
        };
        let bus_powered = self
            .hubs
            .iter()
            .find(|h| h.slot == id.hub)
            .is_some_and(|h| h.bus_powered);
        let budget = if bus_powered {
            speed.unit_load_ma()
        } else {
            u32::MAX
        };
        (tt, budget)
    }

    fn slot_context(&self, slot: u8) -> SlotContext {
        let node = &self.nodes[usize::from(slot)];
        let hub = self.hubs.iter().find(|h| h.slot == slot && node.hub);
        let (tt_hub_slot, tt_port) = node.tt.unwrap_or((0, 0));
        SlotContext {
            route_string: node.path.route_string(),
            speed: node.speed.default_psiv(),
            context_entries: 1,
            root_hub_port: node.path.root_port(),
            tt_hub_slot,
            tt_port,
            hub: hub.is_some(),
            ports: hub.map_or(0, |h| h.count),
            tt_think_time: match hub {
                Some(h) if node.speed == Speed::High => h.think_time,
                _ => 0,
            },
            ..SlotContext::default()
        }
    }

    fn await_command<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        submitted: Result<Ticket, SubmitError>,
        stage: fn(Ticket) -> Stage,
    ) {
        match submitted {
            Ok(ticket) => {
                let node = self.node(slot);
                node.stage = stage(ticket);
                node.deadline = now + COMMAND_MS;
            }
            Err(_) => self.failed(host, now, slot, Failure::CommandLost),
        }
    }

    fn request<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        setup: Setup,
        stage: impl FnOnce(Transfer) -> Stage,
    ) {
        match host.control(slot, setup) {
            Ok(transfer) => {
                let node = self.node(slot);
                node.stage = stage(transfer);
                node.deadline = now + CONTROL_MS;
            }
            Err(_) => self.failed(host, now, slot, Failure::Transfer(TransferError::Cancelled)),
        }
    }

    /// The device goes, and its port tries again unless this was the last try.
    pub(super) fn failed<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, failure: Failure) {
        let node = *self.node(slot);
        let mut tries = 1;
        let mut action = port::Action::None;
        if let Some(port) = self.port_mut(node.port)
            && port.state == (port::State::Attached { slot })
        {
            if failure == Failure::Recovery && now.saturating_sub(node.running_since) >= SERVED_MS {
                port.failures = 0;
            }
            action = port.fail();
            tries = port.failures;
        }
        host.report(Report::Failed {
            path: node.path,
            failure,
            tries,
        });
        self.remove(host, now, slot);
        self.act(host, now, node.port, action);
    }

    /// A slot whose device left before it had any memory: only Disable Slot.
    pub(super) fn disable_unused<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        *self.node(slot) = Node {
            stage: Stage::Orphaning,
            deadline: now,
            ..Node::default()
        };
        self.step_node(host, now, slot);
    }

    pub(super) fn step_node<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let node = *self.node(slot);
        if self.state.dead && node.is_enumerating() {
            self.remove(host, now, slot);
            return;
        }
        match node.stage {
            Stage::Free => {}
            Stage::Addressing(ticket) => match command(host, ticket, node.deadline, now) {
                Poll::Pending => {}
                Poll::TimedOut => self.stuck(host),
                Poll::Done(result) => match command_code(result) {
                    Ok(()) => {
                        let node = self.node(slot);
                        node.stage = Stage::Settling;
                        node.deadline = now + ADDRESS_MS;
                    }
                    Err(failure) => self.failed(host, now, slot, failure),
                },
            },
            Stage::Settling if now >= node.deadline => {
                if node.speed == Speed::Full {
                    let setup = Setup::get_descriptor(kind::DEVICE, 0, 0, 8);
                    self.request(host, now, slot, setup, Stage::MaxPacket);
                } else {
                    self.read_device(host, now, slot);
                }
            }
            Stage::Settling => {}
            Stage::MaxPacket(transfer) => {
                if let Some(length) = self.transferred(host, now, slot, transfer) {
                    self.max_packet_read(host, now, slot, length);
                }
            }
            Stage::Evaluating(ticket) => match command(host, ticket, node.deadline, now) {
                Poll::Pending => {}
                Poll::TimedOut => self.stuck(host),
                Poll::Done(result) => match command_code(result) {
                    Ok(()) => self.read_device(host, now, slot),
                    Err(failure) => self.failed(host, now, slot, failure),
                },
            },
            Stage::Device(transfer) => {
                if let Some(length) = self.transferred(host, now, slot, transfer) {
                    self.device_read(host, now, slot, length);
                }
            }
            Stage::ConfigHeader { index, transfer } => {
                if let Some(length) = self.transferred(host, now, slot, transfer) {
                    self.header_read(host, now, slot, index, length);
                }
            }
            Stage::ConfigBody { index, transfer } => {
                if let Some(length) = self.transferred(host, now, slot, transfer) {
                    self.configuration_read(host, now, slot, index, length);
                }
            }
            Stage::Configuring(ticket) => match command(host, ticket, node.deadline, now) {
                Poll::Pending => {}
                Poll::TimedOut => self.stuck(host),
                Poll::Done(result) => match command_code(result) {
                    Ok(()) => {
                        let setup = Setup::set_configuration(node.configuration);
                        self.request(host, now, slot, setup, Stage::SetConfiguration);
                    }
                    Err(failure) => self.failed(host, now, slot, failure),
                },
            },
            Stage::SetConfiguration(transfer) => {
                if self.transferred(host, now, slot, transfer).is_some() {
                    if self.hubs.iter().any(|h| h.slot == slot) {
                        let setup = hub_class::get_descriptor(node.speed.is_super());
                        self.request(host, now, slot, setup, Stage::HubDescriptor);
                    } else {
                        self.running(host, now, slot);
                    }
                }
            }
            Stage::HubDescriptor(transfer) => {
                if let Some(length) = self.transferred(host, now, slot, transfer) {
                    self.hub_descriptor_read(host, now, slot, length);
                }
            }
            Stage::HubStatus(transfer) => {
                if let Some(length) = self.transferred(host, now, slot, transfer) {
                    let status = self.scratch(host, slot, 1, length.min(2));
                    let status = u16::from(status.first().copied().unwrap_or(0));
                    let self_powered = status & STATUS_SELF_POWERED != 0;
                    if let Some(hub) = self.hubs.iter_mut().find(|h| h.slot == slot) {
                        hub.bus_powered = !self_powered;
                    }
                    if node.speed.is_super() {
                        let setup = hub_class::set_hub_depth(node.path.depth());
                        self.request(host, now, slot, setup, Stage::SetHubDepth);
                    } else {
                        self.configure_hub(host, now, slot);
                    }
                }
            }
            Stage::SetHubDepth(transfer) => {
                if self.transferred(host, now, slot, transfer).is_some() {
                    self.configure_hub(host, now, slot);
                }
            }
            Stage::ConfiguringHub(ticket) => match command(host, ticket, node.deadline, now) {
                Poll::Pending => {}
                Poll::TimedOut => self.stuck(host),
                Poll::Done(result) => match command_code(result) {
                    Ok(()) => self.running(host, now, slot),
                    Err(failure) => self.failed(host, now, slot, failure),
                },
            },
            Stage::Running => self.watch(host, now, slot),
            Stage::Recovering { dci, step, ticket } => {
                self.recover(host, now, slot, dci, step, ticket)
            }
            Stage::ClearingHalt { dci, transfer } => self.clearing(host, now, slot, dci, transfer),
            Stage::Stopping { remaining, current } => {
                self.stop_endpoints(host, now, slot, remaining, current)
            }
            Stage::Orphaning => self.orphaned(host, now, slot),
            Stage::Unbinding => {
                if !node.created || host.unbound(slot) {
                    self.disable(host, now, slot);
                }
            }
            Stage::Disabling(ticket) => match command(host, ticket, node.deadline, now) {
                Poll::Pending => {}
                Poll::TimedOut => self.stuck(host),
                Poll::Done(_) => self.release(host, now, slot),
            },
        }
    }

    /// A failure or a timeout fails the try.
    fn transferred<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        transfer: Transfer,
    ) -> Option<u32> {
        let deadline = self.node(slot).deadline;
        match control(host, slot, transfer, deadline, now) {
            Poll::Pending => None,
            Poll::TimedOut => {
                self.failed(host, now, slot, Failure::TransferTimeout);
                None
            }
            Poll::Done(Ok(length)) => Some(length),
            Poll::Done(Err(error)) => {
                self.failed(host, now, slot, Failure::Transfer(error));
                None
            }
        }
    }

    fn read_device<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let setup = Setup::get_descriptor(kind::DEVICE, 0, 0, 18);
        self.request(host, now, slot, setup, Stage::Device);
    }

    fn max_packet_read<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, length: u32) {
        let prefix = self.scratch(host, slot, 1, length.min(8));
        let raw = match DeviceDescriptor::max_packet0_of(prefix) {
            Ok(raw) => raw,
            Err(malformed) => return self.failed(host, now, slot, Failure::Descriptor(malformed)),
        };
        let Some(max_packet) = ep0_max_packet(Speed::Full, raw) else {
            return self.failed(host, now, slot, Failure::MaxPacket(raw));
        };
        if max_packet == self.node(slot).max_packet {
            return self.read_device(host, now, slot);
        }
        self.node(slot).max_packet = max_packet;
        let submitted = host.command(Command::EvaluateMaxPacket { slot, max_packet });
        self.await_command(host, now, slot, submitted, Stage::Evaluating);
    }

    fn device_read<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, length: u32) {
        let length = (length as usize).min(18);
        host.keep(slot, STORE_DEVICE, length);
        let parsed = host
            .stored(slot, |b| {
                DeviceDescriptor::parse(&b[STORE_DEVICE..STORE_DEVICE + length])
            })
            .unwrap_or(Err(crate::device::descriptor::Malformed::Short));
        let device = match parsed {
            Ok(device) => device,
            Err(malformed) => return self.failed(host, now, slot, Failure::Descriptor(malformed)),
        };
        let speed = self.node(slot).speed;
        if speed == Speed::Low && ep0_max_packet(speed, device.max_packet0).is_none() {
            return self.failed(host, now, slot, Failure::MaxPacket(device.max_packet0));
        }
        if device.configurations == 0 {
            return self.failed(host, now, slot, Failure::NoConfiguration);
        }
        let node = self.node(slot);
        node.vendor = device.vendor;
        node.product = device.product;
        node.class = device.class;
        node.configurations = device.configurations.min(MAX_CONFIGURATIONS);
        self.read_header(host, now, slot, 0);
    }

    fn read_header<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, index: u8) {
        let setup = Setup::get_descriptor(kind::CONFIGURATION, index, 0, 9);
        self.request(host, now, slot, setup, |transfer| Stage::ConfigHeader {
            index,
            transfer,
        });
    }

    fn header_read<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, index: u8, length: u32) {
        let header = self.scratch(host, slot, 1, length.min(9));
        let header = match ConfigurationHeader::parse(header) {
            Ok(header) => header,
            Err(malformed) => return self.failed(host, now, slot, Failure::Descriptor(malformed)),
        };
        if usize::from(header.total_length) > MAX_CONFIGURATION {
            let failure = Failure::ConfigurationTooLarge(header.total_length);
            return self.failed(host, now, slot, failure);
        }
        let setup = Setup::get_descriptor(kind::CONFIGURATION, index, 0, header.total_length);
        self.request(host, now, slot, setup, |transfer| Stage::ConfigBody {
            index,
            transfer,
        });
    }

    /// Choose the first configuration in which a driver wants a function
    /// and whose power the port can give, else the first whose power it
    /// can; a hub is always wanted, by the tree.
    fn configuration_read<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        index: u8,
        length: u32,
    ) {
        let length = (length as usize).min(MAX_CONFIGURATION);
        host.keep(slot, STORE_CONFIGURATION, length);
        let parsed = host
            .stored(slot, |b| {
                Configuration::parse(&b[STORE_CONFIGURATION..STORE_CONFIGURATION + length])
                    .map(|c| (c.header, c.functions(), c.has_interface_class(class::HUB)))
            })
            .unwrap_or(Err(crate::device::descriptor::Malformed::Short));
        let (header, functions, hub_interface) = match parsed {
            Ok(parsed) => parsed,
            Err(malformed) => return self.failed(host, now, slot, Failure::Descriptor(malformed)),
        };
        let node = *self.node(slot);
        let needs = header.max_power_ma(node.speed);
        let fits = needs <= node.budget_ma;
        if node.rereading && !fits {
            return self.failed(host, now, slot, Failure::NoConfiguration);
        }
        let hub = node.class == class::HUB && hub_interface;
        let wanted = node.rereading || hub || self.wanted(host, &node, &functions);
        if fits && wanted {
            return self.chosen(host, now, slot, header, &functions, hub);
        }
        let node = self.node(slot);
        node.least_ma = node.least_ma.min(needs);
        if fits && node.fallback.is_none() {
            node.fallback = Some(index);
        }
        let next = index + 1;
        if next < node.configurations {
            return self.read_header(host, now, slot, next);
        }
        match node.fallback {
            Some(fallback) if fallback == index => {
                self.chosen(host, now, slot, header, &functions, hub)
            }
            Some(fallback) => {
                node.rereading = true;
                self.read_header(host, now, slot, fallback);
            }
            None => {
                let (needs_ma, offers_ma) = (node.least_ma, node.budget_ma);
                node.stage = Stage::Running;
                let node = *node;
                host.report(Report::Unpowered {
                    slot,
                    node,
                    needs_ma,
                    offers_ma,
                });
            }
        }
    }

    fn wanted<H: Host>(&self, host: &mut H, node: &Node, functions: &Functions) -> bool {
        functions.as_slice().iter().any(|&function| {
            host.wanted(&Candidate {
                vendor: node.vendor,
                product: node.product,
                function,
            })
        })
    }

    fn chosen<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        header: ConfigurationHeader,
        functions: &Functions,
        hub: bool,
    ) {
        let node = self.node(slot);
        node.configuration = header.value;
        node.functions = functions.as_slice().len() as u8;
        if !hub {
            return self.configure(host, now, slot);
        }
        node.functions = 0;
        let (depth, super_speed) = (node.path.depth(), node.speed.is_super());
        let unused = if depth >= MAX_DEPTH {
            Some(HubUnused::TooDeep)
        } else if let Some(record) = self.hubs.iter_mut().find(|h| h.slot == 0) {
            *record = super::Hub::reserve(slot, super_speed);
            None
        } else {
            Some(HubUnused::NoRoom)
        };
        if let Some(why) = unused {
            let node = *self.node(slot);
            host.report(Report::HubUnused { slot, node, why });
        }
        self.configure(host, now, slot);
    }

    fn hub_descriptor_read<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, length: u32) {
        let super_speed = self.node(slot).speed.is_super();
        let bytes = self.scratch(host, slot, 1, length);
        let descriptor = match HubDescriptor::parse(bytes, super_speed) {
            Ok(descriptor) => descriptor,
            Err(malformed) => return self.failed(host, now, slot, Failure::Descriptor(malformed)),
        };
        let status = host
            .stored(slot, |b| {
                let config = Configuration::parse(&b[STORE_CONFIGURATION..]).ok()?;
                config
                    .interfaces()
                    .filter(|i| i.alternate == 0 && i.class == class::HUB)
                    .flat_map(|i| config.endpoints(i.number, 0))
                    .find(|e| e.is_in() && e.transfer_type() == TransferType::Interrupt)
            })
            .flatten();
        let Some(status) = status else {
            let failure = Failure::Descriptor(crate::device::descriptor::Malformed::Chain);
            return self.failed(host, now, slot, failure);
        };
        if let Some(hub) = self.hubs.iter_mut().find(|h| h.slot == slot) {
            hub.count = descriptor.ports.min(MAX_HUB_PORTS);
            hub.think_time = descriptor.think_time();
            hub.power_good_ms = descriptor.power_good_ms;
            hub.status_dci = dci(status.number(), true);
            let report = hub_class::report_length(hub.count);
            hub.status_length = status.max_packet_size().clamp(report, 16);
        }
        self.node(slot).hub = true;
        self.request(host, now, slot, Setup::get_status(), Stage::HubStatus);
    }

    fn configure<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let context = self.slot_context(slot);
        let speed = self.node(slot).speed;
        let submitted = host.command(Command::Configure {
            slot,
            context,
            speed,
        });
        self.await_command(host, now, slot, submitted, Stage::Configuring);
    }

    fn configure_hub<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let context = self.slot_context(slot);
        let submitted = host.command(Command::ConfigureHub { slot, context });
        self.await_command(host, now, slot, submitted, Stage::ConfiguringHub);
    }

    fn running<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let node = self.node(slot);
        node.stage = Stage::Running;
        node.running_since = now;
        let (hub, functions) = (node.hub, node.functions);
        let hub_ports = self
            .hubs
            .iter_mut()
            .find(|h| h.slot == slot && hub)
            .map(|h| {
                h.power();
                h.count
            });
        let node = *self.node(slot);
        if hub_ports.is_none() && functions > 0 {
            host.offer(slot, &node);
        }
        host.report(Report::Enumerated {
            slot,
            node,
            hub_ports,
        });
    }

    /// EP0 is recovered first, then owed clears are sent, then any other
    /// endpoint a transfer halted is recovered.
    fn watch<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        if host.escalated(slot) {
            return self.failed(host, now, slot, Failure::Recovery);
        }
        let node = *self.node(slot);
        let halted = host.halted(slot) & !node.given_up;
        if halted & EP0 != 0 {
            self.recovery_command(host, now, slot, 1, Recovery::Reset);
        } else if node.owed_clears != 0 {
            self.clear_halt(host, now, slot);
        } else if halted != 0 {
            let dci = halted.trailing_zeros() as u8;
            self.recovery_command(host, now, slot, dci, Recovery::Reset);
        }
    }

    fn recovery_command<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        dci: u8,
        step: Recovery,
    ) {
        let command = match step {
            Recovery::Reset => Command::ResetEndpoint { slot, dci },
            Recovery::Stop => Command::StopEndpoint { slot, dci },
            Recovery::Dequeue => Command::SetDequeue { slot, dci },
        };
        match host.command(command) {
            Ok(ticket) => {
                let node = self.node(slot);
                node.stage = Stage::Recovering { dci, step, ticket };
                node.deadline = now + COMMAND_MS;
            }
            Err(_) => self.node(slot).stage = Stage::Running,
        }
    }

    /// A non-control endpoint Reset Endpoint found halted is halted on the
    /// device too, which only `CLEAR_FEATURE(ENDPOINT_HALT)` lifts, so its
    /// ring stays shut until that is sent; EP0's halt ends with the next
    /// SETUP (USB 2.0 §8.5.3.4).
    fn recover<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        dci: u8,
        step: Recovery,
        ticket: Ticket,
    ) {
        let deadline = self.node(slot).deadline;
        let code = match command(host, ticket, deadline, now) {
            Poll::Pending => return,
            Poll::TimedOut => return self.stuck(host),
            Poll::Done(result) => result.ok().map(|c| c.code),
        };
        let next = match (step, code) {
            (Recovery::Reset, Some(CompletionCode::SUCCESS)) if dci != 1 => {
                let node = self.node(slot);
                node.stage = Stage::Running;
                node.owed_clears |= 1 << dci;
                return self.watch(host, now, slot);
            }
            (Recovery::Reset, Some(CompletionCode::SUCCESS)) => Some(Recovery::Dequeue),
            (Recovery::Reset, Some(CompletionCode::CONTEXT_STATE)) => Some(Recovery::Stop),
            (Recovery::Stop, Some(_)) => Some(Recovery::Dequeue),
            (Recovery::Dequeue, Some(CompletionCode::SUCCESS)) => {
                let node = self.node(slot);
                node.stage = Stage::Running;
                node.failures[usize::from(dci)] = 0;
                return host.recovered(slot, dci);
            }
            _ => None,
        };
        match next {
            Some(step) => self.recovery_command(host, now, slot, dci, step),
            None => self.recovery_failed(host, now, slot, dci),
        }
    }

    /// The step is tried again on a later pass, until [`MAX_RECOVERIES`]
    /// leave the endpoint halted; a hub, or a device whose EP0 that would
    /// be, is removed instead.
    fn recovery_failed<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, dci: u8) {
        let node = self.node(slot);
        node.stage = Stage::Running;
        let failures = &mut node.failures[usize::from(dci)];
        *failures = failures.saturating_add(1);
        if *failures < MAX_RECOVERIES {
            return;
        }
        node.owed_clears &= !(1 << dci);
        if dci == 1 || node.hub {
            return self.failed(host, now, slot, Failure::Halted);
        }
        node.given_up |= 1 << dci;
    }

    /// One EP0 cannot take now is sent on a later pass.
    fn clear_halt<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let dci = self.node(slot).owed_clears.trailing_zeros() as u8;
        let address = dci / 2 | if dci % 2 == 1 { 0x80 } else { 0 };
        if let Ok(transfer) = host.control(slot, Setup::clear_halt(address)) {
            let node = self.node(slot);
            node.stage = Stage::ClearingHalt { dci, transfer };
            node.deadline = now + CONTROL_MS;
        }
    }

    /// A clear another request's halt kept from running is sent again.
    fn clearing<H: Host>(&mut self, host: &mut H, now: u64, slot: u8, dci: u8, transfer: Transfer) {
        let deadline = self.node(slot).deadline;
        let done = match control(host, slot, transfer, deadline, now) {
            Poll::Done(result) => result.is_ok(),
            Poll::TimedOut => false,
            Poll::Pending if host.halted(slot) & EP0 != 0 => {
                host.abandon_transfer(slot, 1, transfer);
                self.node(slot).stage = Stage::Running;
                return;
            }
            Poll::Pending => return,
        };
        if !done {
            return self.recovery_failed(host, now, slot, dci);
        }
        self.node(slot).owed_clears &= !(1 << dci);
        self.recovery_command(host, now, slot, dci, Recovery::Dequeue);
    }

    /// Whatever the device waited for is abandoned, and it is marked gone.
    pub(crate) fn remove<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let Some(node) = self.nodes.get(usize::from(slot)).copied() else {
            return;
        };
        if node.stage == Stage::Free || node.is_removing() {
            return;
        }
        match node.stage {
            Stage::Addressing(t)
            | Stage::Evaluating(t)
            | Stage::Configuring(t)
            | Stage::ConfiguringHub(t)
            | Stage::Recovering { ticket: t, .. } => host.abandon_command(t),
            Stage::MaxPacket(t)
            | Stage::Device(t)
            | Stage::ConfigHeader { transfer: t, .. }
            | Stage::ConfigBody { transfer: t, .. }
            | Stage::HubDescriptor(t)
            | Stage::HubStatus(t)
            | Stage::SetConfiguration(t)
            | Stage::SetHubDepth(t)
            | Stage::ClearingHalt { transfer: t, .. } => host.abandon_transfer(slot, 1, t),
            _ => {}
        }
        if node.created {
            host.gone(slot);
        }
        let remaining = if node.created && !self.state.dead {
            host.running_endpoints(slot)
        } else {
            0
        };
        let entry = self.node(slot);
        entry.stage = Stage::Stopping {
            remaining,
            current: None,
        };
        entry.deadline = now;
        self.orphan_children(host, now, slot);
    }

    /// A hub being removed lets go of its ports: each child is removed, and
    /// a port still waiting on Enable Slot disables the slot it gets.
    fn orphan_children<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let Some(index) = self.hubs.iter().position(|h| h.slot == slot) else {
            return;
        };
        let children = self.hubs[index].close(host, slot);
        for child in children.into_iter().flatten() {
            self.remove(host, now, child);
        }
    }

    fn stop_endpoints<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        mut remaining: u32,
        current: Option<(u8, Ticket)>,
    ) {
        if let Some((_, ticket)) = current {
            let deadline = self.node(slot).deadline;
            match command(host, ticket, deadline, now) {
                Poll::Pending => return,
                Poll::TimedOut => return self.stuck(host),
                Poll::Done(_) => {}
            }
        }
        if self.state.dead {
            remaining = 0;
        }
        if remaining != 0 {
            let dci = remaining.trailing_zeros() as u8;
            let node = self.node(slot);
            match host.command(Command::StopEndpoint { slot, dci }) {
                Ok(ticket) => {
                    node.stage = Stage::Stopping {
                        remaining: remaining & !(1 << dci),
                        current: Some((dci, ticket)),
                    };
                    node.deadline = now + COMMAND_MS;
                }
                Err(SubmitError::Busy) => {
                    node.stage = Stage::Stopping {
                        remaining,
                        current: None,
                    };
                    node.deadline = now + port::RECOVERY_MS;
                }
                Err(SubmitError::Dead) => {
                    node.stage = Stage::Stopping {
                        remaining: 0,
                        current: None,
                    };
                }
            }
            return;
        }
        if self.node(slot).created {
            let error = if self.state.dead {
                TransferError::Dead
            } else {
                TransferError::Gone
            };
            host.fail_transfers(slot, error);
        }
        self.node(slot).stage = Stage::Orphaning;
        self.orphaned(host, now, slot);
    }

    fn orphaned<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let child_left = self
            .nodes
            .iter()
            .any(|n| n.port.hub == slot && n.stage != Stage::Free);
        if let Some(index) = self.hubs.iter().position(|h| h.slot == slot) {
            if child_left || self.hubs[index].is_slotting() {
                return;
            }
            self.hubs[index] = super::Hub::default();
        } else if child_left {
            return;
        }
        let node = self.node(slot);
        node.stage = Stage::Unbinding;
        if node.created {
            host.unbind(slot);
        }
        self.step_node(host, now, slot);
    }

    fn disable<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        if self.state.dead {
            return self.release(host, now, slot);
        }
        let node = self.node(slot);
        match host.command(Command::DisableSlot { slot }) {
            Ok(ticket) => {
                node.stage = Stage::Disabling(ticket);
                node.deadline = now + COMMAND_MS;
            }
            Err(SubmitError::Busy) => node.stage = Stage::Unbinding,
            Err(SubmitError::Dead) => self.release(host, now, slot),
        }
    }

    /// Disable Slot completed, or the controller is dead.
    fn release<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let Node {
            created,
            path,
            port,
            ..
        } = *self.node(slot);
        *self.node(slot) = Node::default();
        if created {
            host.destroy(slot);
            host.report(Report::Removed { slot, path });
        }
        if !port.is_root() && self.node(port.hub).stage == Stage::Orphaning {
            self.orphaned(host, now, port.hub);
        }
    }

    /// A dead controller answers no command.
    pub(super) fn abandon_removal_command<H: Host>(&mut self, host: &mut H, now: u64, slot: u8) {
        let node = self.node(slot);
        match node.stage {
            Stage::Stopping {
                current: Some((_, ticket)),
                ..
            } => {
                host.abandon_command(ticket);
                node.stage = Stage::Stopping {
                    remaining: 0,
                    current: None,
                };
            }
            Stage::Disabling(ticket) => {
                host.abandon_command(ticket);
                self.release(host, now, slot);
            }
            _ => {}
        }
    }
}

fn command_code(result: CommandResult) -> Result<(), Failure> {
    match result {
        Ok(done) if done.code.is_success() => Ok(()),
        Ok(done) => Err(Failure::Command(done.code)),
        Err(_) => Err(Failure::CommandLost),
    }
}
