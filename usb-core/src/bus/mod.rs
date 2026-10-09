//! One controller's tree of devices, enumerated and removed by state machines
//! that never wait: a pass acts on what completed and returns when something
//! next falls due. [`Host`] is the kernel's rings and locks, or the simulator.
//!
//! Devices are indexed by slot, and by port until they have one. One port per
//! controller holds the default-state turn, from its reset to Address Device.

pub mod hub;
pub mod node;
pub mod path;
pub mod port;

pub use hub::Hub;
pub use node::{Node, Stage};
pub use path::Path;
pub use port::Port;

use crate::device::Speed;
use crate::device::descriptor::{Configuration, Endpoint, Function, Item, Malformed};
use crate::hub::PortStatus;
use crate::xhci::context::{SlotContext, dci};
use crate::xhci::ring::{CommandResult, SubmitError, Ticket};
use crate::xhci::transfer::{PushError, Transfer, TransferError, TransferResult};
use crate::xhci::trb::CompletionCode;

/// USB 2.0 §9.2.6.4 allows a request five seconds.
pub const COMMAND_MS: u64 = 5000;
pub const CONTROL_MS: u64 = 5000;
/// A root port's power-on-to-power-good time, which xHCI does not report.
pub const ROOT_POWER_MS: u64 = 20;
/// How long a powered port's device may take to signal attach.
pub const ATTACH_MS: u64 = 100;

/// Offsets into a device's descriptor store.
pub const STORE_DEVICE: usize = 0;
pub const STORE_CONFIGURATION: usize = 64;
pub const STORE_LEN: usize = 4096;
pub const MAX_CONFIGURATION: usize = STORE_LEN - STORE_CONFIGURATION;

pub const MAX_CONFIGURATIONS: u8 = 8;

/// A root port (`hub` 0) or a port of the hub in slot `hub`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortId {
    pub hub: u8,
    pub port: u8,
}

impl PortId {
    pub fn root(port: u8) -> Self {
        Self { hub: 0, port }
    }

    pub fn is_root(&self) -> bool {
        self.hub == 0
    }
}

/// A command the tree needs; the host writes whatever context it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    EnableSlot {
        slot_type: u8,
    },
    /// Address Device with Block Set Address clear: the controller sends
    /// `SET_ADDRESS` itself (xHCI 1.2 §4.3.4).
    AddressDevice {
        slot: u8,
        context: SlotContext,
        max_packet: u16,
    },
    EvaluateMaxPacket {
        slot: u8,
        max_packet: u16,
    },
    /// Adds what [`for_each_configured_endpoint`] visits; the host sets
    /// Context Entries.
    Configure {
        slot: u8,
        context: SlotContext,
        speed: Speed,
    },
    /// Configure Endpoint of the slot context alone: a hub's ports and TT.
    ConfigureHub {
        slot: u8,
        context: SlotContext,
    },
    ResetEndpoint {
        slot: u8,
        dci: u8,
    },
    StopEndpoint {
        slot: u8,
        dci: u8,
    },
    /// Set TR Dequeue Pointer past everything on the endpoint's ring.
    SetDequeue {
        slot: u8,
        dci: u8,
    },
    DisableSlot {
        slot: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub vendor: u16,
    pub product: u16,
    pub function: Function,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The connection never held still.
    Debounce,
    /// The port did not enable, or its reset never completed.
    Reset,
    Command(CompletionCode),
    CommandLost,
    Transfer(TransferError),
    TransferTimeout,
    Descriptor(Malformed),
    /// `bMaxPacketSize0` is not one the device's speed allows.
    MaxPacket(u8),
    NoConfiguration,
    ConfigurationTooLarge(u16),
    NoMemory,
    /// Too many of the hub's requests failed.
    Hub,
    /// An endpoint a hub or EP0 needs stayed halted through its recoveries.
    Halted,
    /// Its driver could not recover it.
    Recovery,
    /// The controller handed out a slot the tree does not have.
    BadSlot(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
    /// On a hub port; root ports are the host's to report.
    Connected {
        path: Path,
        speed: Option<Speed>,
    },
    Disconnected {
        path: Path,
    },
    OverCurrent {
        path: Path,
        over: bool,
    },
    Enumerated {
        slot: u8,
        node: Node,
        hub_ports: Option<u8>,
    },
    /// No configuration fits the power its port offers, so none is set.
    Unpowered {
        slot: u8,
        node: Node,
        needs_ma: u32,
        offers_ma: u32,
    },
    /// Enumerated, its ports not driven.
    HubUnused {
        slot: u8,
        node: Node,
        why: HubUnused,
    },
    Failed {
        path: Path,
        failure: Failure,
        tries: u8,
    },
    /// Until its device is unplugged.
    GivenUp {
        path: Path,
    },
    Removed {
        slot: u8,
        path: Path,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HubUnused {
    /// A hub on the fifth tier: a route string has no room for its ports.
    TooDeep,
    NoRoom,
}

pub trait Host {
    fn now_ms(&mut self) -> u64;

    /// Each root port an event named, once per naming.
    fn root_change(&mut self) -> Option<u8>;
    /// Clears the port's changes; `None` if it reads all ones.
    fn root_status(&mut self, port: u8) -> Option<PortStatus>;
    fn root_reset(&mut self, port: u8);
    fn root_disable(&mut self, port: u8);
    /// Whether the port is USB 3, and its protocol's slot type.
    fn root_protocol(&mut self, port: u8) -> (bool, u8);

    fn command(&mut self, command: Command) -> Result<Ticket, SubmitError>;
    fn command_result(&mut self, ticket: Ticket) -> Option<CommandResult>;
    fn abandon_command(&mut self, ticket: Ticket);
    /// A command never completed: halt the controller and take it off the bus.
    fn stuck(&mut self);

    /// The slot's memory and store, and its DCBAA entry.
    fn create(&mut self, slot: u8) -> bool;
    /// Clears the DCBAA entry before releasing the slot's memory.
    fn destroy(&mut self, slot: u8);

    /// An IN data stage lands in the slot's control buffer.
    fn control(
        &mut self,
        slot: u8,
        setup: crate::device::request::Setup,
    ) -> Result<Transfer, PushError>;
    /// Into the endpoint's buffer.
    fn interrupt_in(&mut self, slot: u8, dci: u8, length: u16) -> Result<Transfer, PushError>;
    fn transfer_result(&mut self, slot: u8, dci: u8, transfer: Transfer) -> Option<TransferResult>;
    fn abandon_transfer(&mut self, slot: u8, dci: u8, transfer: Transfer);
    fn read(&mut self, slot: u8, dci: u8, out: &mut [u8]);
    /// Copies the control buffer into the store.
    fn keep(&mut self, slot: u8, at: usize, length: usize);
    fn stored<R>(&mut self, slot: u8, read: impl FnOnce(&[u8]) -> R) -> Option<R>;

    /// DCI bits of the endpoints transfers halted.
    fn halted(&mut self, slot: u8) -> u32;
    /// Set TR Dequeue Pointer moved `dci`'s ring past what it held.
    fn recovered(&mut self, slot: u8, dci: u8);
    /// A driver's recovery failed since the last asking: reset the port.
    fn escalated(&mut self, slot: u8) -> bool;
    /// DCI bits of the endpoints whose contexts say running.
    fn running_endpoints(&mut self, slot: u8) -> u32;
    /// Refuse every submission to the slot and ring none of its doorbells.
    fn gone(&mut self, slot: u8);
    fn fail_transfers(&mut self, slot: u8, error: TransferError);

    fn wanted(&mut self, candidate: &Candidate) -> bool;
    fn offer(&mut self, slot: u8, node: &Node);
    /// Every function offered is bound, declined or unmatched.
    fn offered(&mut self, slot: u8) -> bool;
    /// Run the bound drivers' removals.
    fn unbind(&mut self, slot: u8);
    fn unbound(&mut self, slot: u8) -> bool;

    fn report(&mut self, report: Report);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    /// The port holding the default-state turn.
    pub default: Option<PortId>,
    pub started_at: Option<u64>,
    pub dead: bool,
    scratch: [u8; 16],
}

/// Over the host's tables: root ports by number less one, devices by slot.
pub struct Tree<'a> {
    pub roots: &'a mut [Port],
    pub nodes: &'a mut [Node],
    pub hubs: &'a mut [Hub],
    pub state: &'a mut State,
}

impl Tree<'_> {
    /// One pass; returns when something next falls due.
    pub fn step<H: Host>(&mut self, host: &mut H) -> Option<u64> {
        let now = host.now_ms();
        if self.state.started_at.is_none() {
            self.state.started_at = Some(now);
            for port in 1..=self.roots.len() as u8 {
                self.read_root(host, now, port);
            }
        }
        let mut changed = [0u64; 4];
        for _ in 0..self.roots.len() {
            let Some(port) = host.root_change() else {
                break;
            };
            changed[usize::from(port / 64)] |= 1 << (port % 64);
        }
        for port in 1..=self.roots.len() as u8 {
            if changed[usize::from(port / 64)] & 1 << (port % 64) != 0 {
                self.read_root(host, now, port);
            }
        }
        if !self.state.dead {
            for index in 0..self.roots.len() {
                let id = PortId::root(index as u8 + 1);
                let action = self.roots[index].on_timer(now);
                self.act(host, now, id, action);
            }
            self.poll_slotting(host, now);
        }
        for slot in self.slots() {
            self.step_node(host, now, slot);
        }
        for index in 0..self.hubs.len() {
            self.step_hub(host, now, index);
        }
        self.grant(host, now);
        self.deadline()
    }

    fn slots(&self) -> impl Iterator<Item = u8> + use<> {
        (1..self.nodes.len().min(256)).map(|slot| slot as u8)
    }

    /// Every device goes, with no command issued.
    pub fn die<H: Host>(&mut self, host: &mut H) {
        self.state.dead = true;
        self.state.default = None;
        let now = host.now_ms();
        let hub_ports = self.hubs.iter_mut().flat_map(|h| h.ports.iter_mut());
        for port in self.roots.iter_mut().chain(hub_ports) {
            if let port::State::Slotting { ticket, .. } = port.state {
                host.abandon_command(ticket);
            }
            port.state = port::State::Idle;
        }
        for slot in self.slots() {
            self.abandon_removal_command(host, now, slot);
            self.remove(host, now, slot);
        }
    }

    pub(crate) fn stuck<H: Host>(&mut self, host: &mut H) {
        if !self.state.dead {
            host.stuck();
            self.die(host);
        }
    }

    pub(crate) fn port_mut(&mut self, id: PortId) -> Option<&mut Port> {
        if id.is_root() {
            return self.roots.get_mut(usize::from(id.port).checked_sub(1)?);
        }
        let hub = self.hubs.iter_mut().find(|h| h.slot == id.hub)?;
        hub.ports
            .get_mut(usize::from(id.port).checked_sub(1)?)
            .filter(|_| id.port <= hub.count)
    }

    pub(crate) fn path_of(&self, id: PortId) -> Option<Path> {
        if id.is_root() {
            return Some(Path::root(id.port));
        }
        self.nodes.get(usize::from(id.hub))?.path.child(id.port)
    }

    /// What the last transfer on `dci` read, up to the scratch's length.
    pub(crate) fn scratch<H: Host>(
        &mut self,
        host: &mut H,
        slot: u8,
        dci: u8,
        length: u32,
    ) -> &[u8] {
        let length = (length as usize).min(self.state.scratch.len());
        host.read(slot, dci, &mut self.state.scratch[..length]);
        &self.state.scratch[..length]
    }

    fn read_root<H: Host>(&mut self, host: &mut H, now: u64, port: u8) {
        let Some(status) = host.root_status(port) else {
            return;
        };
        let id = PortId::root(port);
        let Some(state) = self.port_mut(id) else {
            return;
        };
        let action = state.on_status(now, status);
        self.act(host, now, id, action);
    }

    pub(crate) fn act<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        id: PortId,
        action: port::Action,
    ) {
        use port::Action;
        match action {
            Action::None => {}
            Action::Read if id.is_root() => self.read_root(host, now, id.port),
            Action::Reset if id.is_root() => host.root_reset(id.port),
            Action::Disable if id.is_root() => host.root_disable(id.port),
            Action::Read | Action::Reset | Action::Disable => {
                if let Some(hub) = self.hubs.iter_mut().find(|h| h.slot == id.hub) {
                    hub.queue(id.port, action);
                }
            }
            Action::EnableSlot(speed) => self.enable_slot(host, now, id, speed),
            Action::Failed(failure) => self.port_failed(host, now, id, failure),
            Action::Detach(slot) => self.remove(host, now, slot),
            Action::GiveUp => {
                if let Some(path) = self.path_of(id) {
                    host.report(Report::GivenUp { path });
                }
                self.act(host, now, id, Action::Disable);
            }
        }
    }

    fn slot_type<H: Host>(&self, host: &mut H, id: PortId) -> u8 {
        let root = if id.is_root() {
            id.port
        } else {
            self.nodes
                .get(usize::from(id.hub))
                .map_or(0, |hub| hub.path.root_port())
        };
        host.root_protocol(root).1
    }

    fn enable_slot<H: Host>(&mut self, host: &mut H, now: u64, id: PortId, speed: Speed) {
        let slot_type = self.slot_type(host, id);
        let submitted = host.command(Command::EnableSlot { slot_type });
        let Some(port) = self.port_mut(id) else {
            if let Ok(ticket) = submitted {
                host.abandon_command(ticket);
            }
            return;
        };
        port.state = match submitted {
            Ok(ticket) => port::State::Slotting {
                ticket,
                deadline: now + COMMAND_MS,
                speed,
                pulled: false,
            },
            Err(SubmitError::Busy) => port::State::Recovering {
                until: now + port::RECOVERY_MS,
                speed,
            },
            Err(SubmitError::Dead) => port::State::Idle,
        };
    }

    fn poll_slotting<H: Host>(&mut self, host: &mut H, now: u64) {
        for index in 0..self.roots.len() {
            self.slotted(host, now, PortId::root(index as u8 + 1));
        }
        for h in 0..self.hubs.len() {
            let hub = self.hubs[h].slot;
            if hub == 0 {
                continue;
            }
            for port in 1..=self.hubs[h].count {
                self.slotted(host, now, PortId { hub, port });
            }
        }
    }

    fn slotted<H: Host>(&mut self, host: &mut H, now: u64, id: PortId) {
        let Some(port) = self.port_mut(id) else {
            return;
        };
        let port::State::Slotting {
            ticket,
            deadline,
            speed,
            pulled,
        } = port.state
        else {
            return;
        };
        let failure = match host.command_result(ticket) {
            None if now < deadline => return,
            None => {
                host.abandon_command(ticket);
                return self.stuck(host);
            }
            Some(Err(_)) => Failure::CommandLost,
            Some(Ok(done)) if !done.code.is_success() => Failure::Command(done.code),
            Some(Ok(done)) => {
                let slot = done.slot;
                if let Some(Stage::Disabling(_)) =
                    self.nodes.get(usize::from(slot)).map(|n| n.stage)
                {
                    self.step_node(host, now, slot);
                }
                let free = self
                    .nodes
                    .get(usize::from(slot))
                    .is_some_and(|n| n.stage == Stage::Free)
                    && slot != 0;
                if !free {
                    Failure::BadSlot(slot)
                } else if pulled {
                    if let Some(port) = self.port_mut(id) {
                        port.state = port::State::Idle;
                    }
                    self.disable_unused(host, now, slot);
                    let action = self.read_after(id);
                    self.act(host, now, id, action);
                    return;
                } else {
                    self.address(host, now, id, slot, speed);
                    return;
                }
            }
        };
        self.port_failed(host, now, id, failure);
    }

    /// A port that lost its device mid-command may hold a replacement.
    fn read_after(&mut self, id: PortId) -> port::Action {
        match self.port_mut(id) {
            Some(port) => {
                port.state = port::State::Empty;
                port::Action::Read
            }
            None => port::Action::None,
        }
    }

    /// A try failed before the device had a slot.
    pub(crate) fn port_failed<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        id: PortId,
        failure: Failure,
    ) {
        let Some(port) = self.port_mut(id) else {
            return;
        };
        let action = port.fail();
        let tries = port.failures;
        if let Some(path) = self.path_of(id) {
            host.report(Report::Failed {
                path,
                failure,
                tries,
            });
        }
        self.act(host, now, id, action);
    }

    fn grant<H: Host>(&mut self, host: &mut H, now: u64) {
        if self.state.dead {
            return;
        }
        let mut next = None;
        if let Some(holder) = self.state.default {
            let port = self.port_mut(holder).map(|p| (p.holds_default(), p.state));
            let node_holds = self
                .nodes
                .iter()
                .any(|n| n.port == holder && matches!(n.stage, Stage::Addressing(_)));
            match port {
                Some((true, _)) => return,
                _ if node_holds => return,
                Some((_, port::State::Ready)) => next = Some(holder),
                _ => self.state.default = None,
            }
        }
        let Some(id) = next.or_else(|| self.first_ready()) else {
            return;
        };
        let (usb3, speed) = if id.is_root() {
            let usb3 = host.root_protocol(id.port).0;
            (usb3, self.port_mut(id).and_then(|p| p.speed))
        } else {
            let super_speed = self
                .hubs
                .iter()
                .find(|h| h.slot == id.hub)
                .is_some_and(|h| h.super_speed);
            (super_speed, Some(Speed::Super).filter(|_| super_speed))
        };
        let Some(port) = self.port_mut(id) else {
            return;
        };
        let action = port.take_turn(now, !usb3, speed);
        self.state.default = Some(id);
        self.act(host, now, id, action);
    }

    fn first_ready(&self) -> Option<PortId> {
        let ready = |p: &Port| p.state == port::State::Ready;
        if let Some(index) = self.roots.iter().position(ready) {
            return Some(PortId::root(index as u8 + 1));
        }
        self.hubs
            .iter()
            .filter(|h| h.slot != 0 && h.is_running())
            .find_map(|h| {
                let index = h.ports[..usize::from(h.count)].iter().position(ready)?;
                Some(PortId {
                    hub: h.slot,
                    port: index as u8 + 1,
                })
            })
    }

    fn deadline(&self) -> Option<u64> {
        let roots = self.roots.iter().filter_map(Port::deadline);
        let nodes = self.nodes.iter().filter_map(Node::deadline);
        let hubs = self.hubs.iter().filter_map(Hub::deadline);
        roots.chain(nodes).chain(hubs).min()
    }

    /// Every port powered long enough to signal attach and quiet for a
    /// debounce interval since, and every device enumerated to an end.
    pub fn settled<H: Host>(&mut self, host: &mut H) -> bool {
        let now = host.now_ms();
        let Some(started) = self.state.started_at else {
            return false;
        };
        if now < started + ROOT_POWER_MS + ATTACH_MS || self.state.default.is_some() {
            return false;
        }
        let quiet = |p: &Port| p.is_quiet() && now >= p.changed_at + port::DEBOUNCE_MS;
        if !self.roots.iter().all(quiet) {
            return false;
        }
        if !self.hubs.iter().all(|h| h.slot == 0 || h.settled(now)) {
            return false;
        }
        for slot in self.slots() {
            let node = &self.nodes[usize::from(slot)];
            let done = match node.stage {
                Stage::Free => true,
                Stage::Running => {
                    node.is_recovered(host, slot)
                        && (node.hub || node.functions == 0 || host.offered(slot))
                }
                _ => false,
            };
            if !done {
                return false;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.iter().all(|n| n.stage == Stage::Free)
    }
}

/// DCIs 2 to 31.
pub const MAX_ENDPOINTS: usize = 30;

/// Each endpoint of every alternate setting 0, with its DCI; the first claim
/// on a DCI wins.
pub fn for_each_configured_endpoint(
    config: &Configuration<'_>,
    mut visit: impl FnMut(u8, Endpoint),
) {
    let mut seen = 0u32;
    let mut in_setting_0 = false;
    for item in config.items() {
        match item {
            Item::Interface(interface) => in_setting_0 = interface.alternate == 0,
            Item::Association(_) => in_setting_0 = false,
            Item::Endpoint(endpoint) if in_setting_0 && endpoint.is_usable() => {
                let index = dci(endpoint.number(), endpoint.is_in());
                if seen & 1 << index == 0 {
                    seen |= 1 << index;
                    visit(index, endpoint);
                }
            }
            _ => {}
        }
    }
}

/// What a Configure Endpoint moving one interface to another alternate
/// setting drops and adds (§4.6.6.1), by DCI bit; the added endpoints are
/// the new setting's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlternateChange {
    pub drop: u32,
    pub add: u32,
}

/// Moving `interface` from alternate `from` to `to`, while the DCIs in
/// `held` have rings. `None` when `to` is no setting of `interface`, or one
/// of its endpoints would take a DCI another interface holds or one its own
/// setting names twice.
pub fn alternate_change(
    config: &Configuration<'_>,
    interface: u8,
    from: u8,
    to: u8,
    held: u32,
) -> Option<AlternateChange> {
    if !config
        .interfaces()
        .any(|i| i.number == interface && i.alternate == to)
    {
        return None;
    }
    let drop = config
        .endpoints(interface, from)
        .fold(0, |bits, e| bits | 1 << dci(e.number(), e.is_in()))
        & held;
    let mut add = 0u32;
    for endpoint in config.endpoints(interface, to) {
        let index = dci(endpoint.number(), endpoint.is_in());
        if (held & !drop | add) & 1 << index != 0 {
            return None;
        }
        add |= 1 << index;
    }
    Some(AlternateChange { drop, add })
}

#[cfg(test)]
pub(crate) mod tests;
