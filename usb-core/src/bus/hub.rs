//! A hub's ports, driven one control request at a time: powered, read when
//! the status-change endpoint names them, cleared, then reset or disabled.

use super::port::{self, Port};
use super::{ATTACH_MS, CONTROL_MS, Failure, Host, PortId, Report, Stage, Tree};
use crate::hub::{self as hub_class, MAX_HUB_PORTS, PortStatus, change, change_features, feature};
use crate::xhci::transfer::{Transfer, TransferResult};

/// `wHubChange`: local power (bit 0) and over-current (bit 1).
const HUB_OVER_CURRENT: u16 = 1 << 1;
const HUB_CHANGES: u16 = 0b11;

/// Failed requests since a port was last given its status, after which the
/// hub is removed and its own port tried again.
pub const MAX_ERRORS: u8 = 8;
/// A failed status-change transfer is posted again after this, times the
/// failures in a row.
pub const STATUS_RETRY_MS: u64 = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HubStage {
    #[default]
    Free,
    Enumerating,
    /// `SET_FEATURE(PORT_POWER)` on each port in turn.
    Powering {
        next: u8,
    },
    /// Power-good time plus the time a device takes to signal attach.
    Waiting {
        until: u64,
    },
    Running,
    Closing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Power(u8),
    Read(u8),
    /// Before the port machine sees `status`.
    Clear {
        port: u8,
        status: PortStatus,
        remaining: u16,
    },
    Reset(u8),
    Disable(u8),
    /// What the status-change report's bit 0 names.
    HubStatus,
    ClearHub {
        remaining: u16,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hub {
    /// 0 when the record is free.
    pub slot: u8,
    pub ports: [Port; MAX_HUB_PORTS as usize],
    pub count: u8,
    pub super_speed: bool,
    pub bus_powered: bool,
    /// In the slot context's encoding.
    pub think_time: u8,
    pub power_good_ms: u32,
    pub stage: HubStage,
    pub powered_at: u64,
    pub status_dci: u8,
    pub status_length: u16,
    op: Option<(Op, Transfer, u64)>,
    /// Port bits.
    read: u16,
    reset: u16,
    disable: u16,
    /// Read again after the clear, which may have cleared a change unseen.
    reread: u16,
    hub_changed: bool,
    status: Option<Transfer>,
    retry_at: u64,
    errors: u8,
}

impl Hub {
    pub(super) fn reserve(slot: u8, super_speed: bool) -> Self {
        Self {
            slot,
            super_speed,
            stage: HubStage::Enumerating,
            ..Self::default()
        }
    }

    pub(super) fn power(&mut self) {
        self.stage = HubStage::Powering { next: 1 };
    }

    pub fn is_running(&self) -> bool {
        self.stage == HubStage::Running
    }

    pub(super) fn queue(&mut self, port: u8, action: port::Action) {
        let bit = 1u16 << port;
        match action {
            port::Action::Read => self.read |= bit,
            port::Action::Reset => self.reset |= bit,
            port::Action::Disable if !self.super_speed => self.disable |= bit,
            _ => {}
        }
    }

    fn live_ports(&self) -> &[Port] {
        &self.ports[..usize::from(self.count)]
    }

    pub(super) fn is_slotting(&self) -> bool {
        self.live_ports()
            .iter()
            .any(|p| matches!(p.state, port::State::Slotting { .. }))
    }

    /// Running, powered long enough, listening for changes, nothing waiting,
    /// every port quiet.
    pub(super) fn settled(&self, now: u64) -> bool {
        match self.stage {
            HubStage::Running => {
                now >= self.powered_at + u64::from(self.power_good_ms) + ATTACH_MS
                    && self.status.is_some()
                    && self.op.is_none()
                    && self.read | self.reset | self.disable == 0
                    && !self.hub_changed
                    && self
                        .live_ports()
                        .iter()
                        .all(|p| p.is_quiet() && now >= p.changed_at + port::DEBOUNCE_MS)
            }
            _ => false,
        }
    }

    pub fn deadline(&self) -> Option<u64> {
        if self.slot == 0 {
            return None;
        }
        let stage = match self.stage {
            HubStage::Waiting { until } => Some(until),
            HubStage::Running if self.status.is_none() => Some(self.retry_at),
            _ => None,
        };
        let op = self.op.map(|(_, _, deadline)| deadline);
        let ports = self.live_ports().iter().filter_map(Port::deadline);
        stage.into_iter().chain(op).chain(ports).min()
    }

    /// Drops the hub's requests and hands its ports' devices back for removal.
    pub(super) fn close<H: Host>(
        &mut self,
        host: &mut H,
        slot: u8,
    ) -> [Option<u8>; MAX_HUB_PORTS as usize] {
        if let Some((_, transfer, _)) = self.op.take() {
            host.abandon_transfer(slot, 1, transfer);
        }
        if let Some(transfer) = self.status.take() {
            host.abandon_transfer(slot, self.status_dci, transfer);
        }
        self.stage = HubStage::Closing;
        self.read = 0;
        self.reset = 0;
        self.disable = 0;
        self.reread = 0;
        self.hub_changed = false;
        let mut children = [None; MAX_HUB_PORTS as usize];
        for (port, child) in self.ports.iter_mut().zip(children.iter_mut()) {
            port.state = match port.state {
                port::State::Attached { slot } => {
                    *child = Some(slot);
                    port::State::Idle
                }
                port::State::Slotting {
                    ticket,
                    deadline,
                    speed,
                    ..
                } => port::State::Slotting {
                    ticket,
                    deadline,
                    speed,
                    pulled: true,
                },
                _ => port::State::Idle,
            };
        }
        children
    }
}

impl Tree<'_> {
    fn hub_index(&self, slot: u8) -> Option<usize> {
        self.hubs.iter().position(|h| h.slot == slot)
    }

    pub(super) fn step_hub<H: Host>(&mut self, host: &mut H, now: u64, index: usize) {
        let slot = self.hubs[index].slot;
        if slot == 0 || self.nodes[usize::from(slot)].stage != Stage::Running {
            return;
        }
        if let Some((op, transfer, deadline)) = self.hubs[index].op {
            match host.transfer_result(slot, 1, transfer) {
                Some(result) => {
                    self.hubs[index].op = None;
                    self.op_done(host, now, slot, op, result);
                }
                None if now >= deadline => {
                    host.abandon_transfer(slot, 1, transfer);
                    self.hubs[index].op = None;
                    self.op_done(host, now, slot, op, Err(crate::xhci::TransferError::Lost));
                }
                None => {}
            }
        }
        let Some(index) = self.hub_index(slot) else {
            return;
        };
        if let HubStage::Waiting { until } = self.hubs[index].stage
            && now >= until
        {
            let hub = &mut self.hubs[index];
            hub.stage = HubStage::Running;
            hub.read = ((1u32 << (hub.count + 1)) - 2) as u16;
        }
        if self.hubs[index].op.is_none() {
            self.next_op(host, now, index);
        }
        if self.hubs[index].stage != HubStage::Running {
            return;
        }
        self.status_change(host, now, index);
        for number in 1..=self.hubs[index].count {
            let action = self.hubs[index].ports[usize::from(number) - 1].on_timer(now);
            self.act(
                host,
                now,
                PortId {
                    hub: slot,
                    port: number,
                },
                action,
            );
            if self.hub_index(slot) != Some(index) {
                return;
            }
        }
    }

    fn next_op<H: Host>(&mut self, host: &mut H, now: u64, index: usize) {
        let hub = &mut self.hubs[index];
        let lowest = |bits: u16| (bits != 0).then(|| bits.trailing_zeros() as u8);
        let (op, setup) = match hub.stage {
            HubStage::Powering { next } if next <= hub.count => (
                Op::Power(next),
                hub_class::set_port_feature(next, feature::PORT_POWER),
            ),
            HubStage::Powering { .. } => {
                hub.powered_at = now;
                hub.stage = HubStage::Waiting {
                    until: now + u64::from(hub.power_good_ms) + ATTACH_MS,
                };
                return;
            }
            HubStage::Running => {
                if let Some(port) = lowest(hub.disable) {
                    (
                        Op::Disable(port),
                        hub_class::clear_port_feature(port, feature::PORT_ENABLE),
                    )
                } else if let Some(port) = lowest(hub.reset) {
                    (
                        Op::Reset(port),
                        hub_class::set_port_feature(port, feature::PORT_RESET),
                    )
                } else if let Some(port) = lowest(hub.read) {
                    (Op::Read(port), hub_class::get_port_status(port))
                } else if hub.hub_changed {
                    (Op::HubStatus, hub_class::get_hub_status())
                } else {
                    return;
                }
            }
            _ => return,
        };
        self.submit(host, now, index, op, setup);
    }

    fn submit<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        index: usize,
        op: Op,
        setup: crate::device::request::Setup,
    ) {
        let hub = &mut self.hubs[index];
        let Ok(transfer) = host.control(hub.slot, setup) else {
            return;
        };
        let bit = |port: u8| !(1u16 << port);
        match op {
            Op::Read(port) => hub.read &= bit(port),
            Op::Reset(port) => hub.reset &= bit(port),
            Op::Disable(port) => hub.disable &= bit(port),
            Op::HubStatus => hub.hub_changed = false,
            Op::Power(_) | Op::Clear { .. } | Op::ClearHub { .. } => {}
        }
        hub.op = Some((op, transfer, now + CONTROL_MS));
    }

    fn op_done<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        slot: u8,
        op: Op,
        result: TransferResult,
    ) {
        let Some(index) = self.hub_index(slot) else {
            return;
        };
        let failed = match result {
            Err(error) if error.is_final() => return,
            Err(_) => true,
            Ok(length) => matches!(op, Op::Read(_) | Op::HubStatus) && length != 4,
        };
        if failed && self.hub_error(host, now, index) {
            return;
        }
        let hub = &mut self.hubs[index];
        match op {
            Op::Power(port) if !failed => {
                hub.stage = HubStage::Powering { next: port + 1 };
            }
            Op::Power(_) => {}
            Op::Read(port) | Op::Clear { port, .. } if failed => hub.read |= 1 << port,
            Op::Read(port) => {
                let super_speed = hub.super_speed;
                let bytes = self.scratch(host, slot, 1, 4);
                let status =
                    PortStatus::from_hub([bytes[0], bytes[1], bytes[2], bytes[3]], super_speed);
                self.clear_next(host, now, index, port, status, status.changes);
            }
            Op::Clear {
                port,
                status,
                remaining,
            } => {
                let rest = remaining & remaining.wrapping_sub(1);
                self.clear_next(host, now, index, port, status, rest);
            }
            Op::Reset(port) if failed => {
                let id = PortId { hub: slot, port };
                if let Some(state) = self.port_mut(id)
                    && matches!(state.state, port::State::Resetting { .. })
                {
                    self.port_failed(host, now, id, Failure::Reset);
                }
            }
            Op::Reset(_) | Op::Disable(_) => {}
            Op::HubStatus | Op::ClearHub { .. } if failed => hub.hub_changed = true,
            Op::HubStatus => {
                let b = self.scratch(host, slot, 1, 4);
                let bytes = [b[0], b[1], b[2], b[3]];
                let changes = u16::from_le_bytes([bytes[2], bytes[3]]) & HUB_CHANGES;
                if changes & HUB_OVER_CURRENT != 0 {
                    let path = self.nodes[usize::from(slot)].path;
                    let over = u16::from_le_bytes([bytes[0], bytes[1]]) & HUB_OVER_CURRENT != 0;
                    host.report(Report::OverCurrent { path, over });
                }
                self.clear_hub(host, now, index, changes);
            }
            Op::ClearHub { remaining } => {
                self.clear_hub(host, now, index, remaining & remaining.wrapping_sub(1));
            }
        }
    }

    /// Past [`MAX_ERRORS`] the hub is removed; returns whether it was.
    fn hub_error<H: Host>(&mut self, host: &mut H, now: u64, index: usize) -> bool {
        let hub = &mut self.hubs[index];
        hub.errors = hub.errors.saturating_add(1);
        if hub.errors < MAX_ERRORS {
            return false;
        }
        let slot = hub.slot;
        self.failed(host, now, slot, Failure::Hub);
        true
    }

    fn clear_hub<H: Host>(&mut self, host: &mut H, now: u64, index: usize, remaining: u16) {
        let lowest = remaining & remaining.wrapping_neg();
        if lowest == 0 {
            self.hubs[index].errors = 0;
            return;
        }
        let feature = if lowest == HUB_OVER_CURRENT {
            feature::C_HUB_OVER_CURRENT
        } else {
            feature::C_HUB_LOCAL_POWER
        };
        self.submit(
            host,
            now,
            index,
            Op::ClearHub { remaining },
            hub_class::clear_hub_feature(feature),
        );
        if self.hubs[index].op.is_none() {
            self.hubs[index].hub_changed = true;
        }
    }

    /// With no bit left in `remaining`, hands `status` to the port.
    fn clear_next<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        index: usize,
        port: u8,
        status: PortStatus,
        remaining: u16,
    ) {
        let super_speed = self.hubs[index].super_speed;
        let lowest = remaining & remaining.wrapping_neg();
        if let Some(feature) = change_features(lowest, super_speed).next() {
            let setup = hub_class::clear_port_feature(port, feature);
            let op = Op::Clear {
                port,
                status,
                remaining,
            };
            self.submit(host, now, index, op, setup);
            if self.hubs[index].op.is_none() {
                self.hubs[index].read |= 1 << port;
            }
            return;
        }
        if lowest != 0 {
            return self.clear_next(host, now, index, port, status, remaining & !lowest);
        }
        self.deliver(host, now, index, port, status);
    }

    fn deliver<H: Host>(
        &mut self,
        host: &mut H,
        now: u64,
        index: usize,
        number: u8,
        status: PortStatus,
    ) {
        let slot = self.hubs[index].slot;
        let id = PortId {
            hub: slot,
            port: number,
        };
        let Some(path) = self.path_of(id) else {
            return;
        };
        let hub = &mut self.hubs[index];
        hub.errors = 0;
        let bit = 1u16 << number;
        if status.changes != 0 && hub.reread & bit == 0 {
            hub.reread |= bit;
            hub.read |= bit;
        } else {
            hub.reread &= !bit;
        }
        let port = &mut self.hubs[index].ports[usize::from(number) - 1];
        let was = port.connected;
        if was && (!status.connected || status.changed(change::CONNECT)) {
            host.report(Report::Disconnected { path });
        }
        if status.connected && (!was || status.changed(change::CONNECT)) {
            host.report(Report::Connected {
                path,
                speed: status.speed,
            });
        }
        if status.changed(change::OVER_CURRENT) {
            host.report(Report::OverCurrent {
                path,
                over: status.over_current,
            });
        }
        let action = port.on_status(now, status);
        self.act(host, now, id, action);
    }

    /// Keeps a transfer posted on the status-change endpoint.
    fn status_change<H: Host>(&mut self, host: &mut H, now: u64, index: usize) {
        let hub = &mut self.hubs[index];
        let (slot, dci) = (hub.slot, hub.status_dci);
        if let Some(transfer) = hub.status {
            match host.transfer_result(slot, dci, transfer) {
                None => return,
                Some(Ok(length)) => {
                    hub.status = None;
                    let (count, length) = (hub.count, length.min(u32::from(hub.status_length)));
                    let changed = hub_class::changed(self.scratch(host, slot, dci, length), count);
                    let hub = &mut self.hubs[index];
                    hub.read |= changed & !1;
                    hub.hub_changed |= changed & 1 != 0;
                }
                Some(Err(error)) => {
                    hub.status = None;
                    if error.is_final() || self.status_retry(host, now, index) {
                        return;
                    }
                }
            }
        }
        let hub = &self.hubs[index];
        if hub.status.is_some() || now < hub.retry_at {
            return;
        }
        let posted = if host.halted(slot) & 1 << dci != 0 {
            None
        } else {
            host.interrupt_in(slot, dci, hub.status_length).ok()
        };
        match posted {
            Some(transfer) => self.hubs[index].status = Some(transfer),
            None => {
                self.status_retry(host, now, index);
            }
        }
    }

    /// Returns whether the hub was removed.
    fn status_retry<H: Host>(&mut self, host: &mut H, now: u64, index: usize) -> bool {
        if self.hub_error(host, now, index) {
            return true;
        }
        let hub = &mut self.hubs[index];
        hub.retry_at = now + STATUS_RETRY_MS * u64::from(hub.errors);
        false
    }
}
