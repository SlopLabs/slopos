//! The tree's [`Host`] over one controller: its registers, its command
//! ring, and the devices in its slots.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::klog_info;
use slopos_usb_core::bus::{Candidate, Command, Failure, Host, HubUnused, Node, Path, Report};
use slopos_usb_core::device::Speed;
use slopos_usb_core::device::request::Setup;
use slopos_usb_core::hub::PortStatus;
use slopos_usb_core::xhci::Trb;
use slopos_usb_core::xhci::ring::{CommandResult, SubmitError, Ticket};
use slopos_usb_core::xhci::transfer::{PushError, Transfer, TransferError, TransferResult};

use super::controller::Controller;

/// Device serials, so a snapshot of a function names one device even after
/// its slot is reused.
static SERIALS: AtomicU64 = AtomicU64::new(1);

#[cfg(feature = "test-hooks")]
static CLEARS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// `CLEAR_FEATURE(ENDPOINT_HALT)` requests the tree has sent since boot.
#[cfg(feature = "test-hooks")]
pub fn clears_sent() -> u32 {
    CLEARS.load(Ordering::Acquire)
}

pub(super) struct ControllerHost<'a> {
    controller: &'a Controller,
}

impl<'a> ControllerHost<'a> {
    pub(super) fn new(controller: &'a Controller) -> Self {
        Self { controller }
    }
}

struct Why(Failure);

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Failure::Debounce => f.write_str("its connection never held still"),
            Failure::Reset => f.write_str("its port did not reset"),
            Failure::Command(code) => write!(f, "a command failed with code {}", code.0),
            Failure::CommandLost => f.write_str("a command was lost"),
            Failure::Transfer(error) => write!(f, "a request failed: {:?}", error),
            Failure::TransferTimeout => f.write_str("a request was never answered"),
            Failure::Descriptor(malformed) => write!(f, "a descriptor is {:?}", malformed),
            Failure::MaxPacket(raw) => write!(f, "EP0 packet size {} is not allowed", raw),
            Failure::NoConfiguration => f.write_str("it has no configuration"),
            Failure::ConfigurationTooLarge(length) => {
                write!(f, "its configuration is {} bytes, more than a page", length)
            }
            Failure::NoMemory => f.write_str("out of memory"),
            Failure::Hub => f.write_str("too many of its hub requests failed"),
            Failure::Halted => f.write_str("an endpoint it needs stayed halted"),
            Failure::Recovery => f.write_str("its driver could not recover it"),
            Failure::BadSlot(slot) => write!(f, "the controller gave out slot {}", slot),
        }
    }
}

impl Host for ControllerHost<'_> {
    fn now_ms(&mut self) -> u64 {
        slopos_kernel_services::clock::uptime_ms()
    }

    fn root_change(&mut self) -> Option<u8> {
        self.controller.take_port_change()
    }

    fn root_status(&mut self, port: u8) -> Option<PortStatus> {
        self.controller.read_port(port)
    }

    fn root_reset(&mut self, port: u8) {
        self.controller.write_port(port, |portsc| portsc.reset());
    }

    fn root_disable(&mut self, port: u8) {
        self.controller.write_port(port, |portsc| portsc.disable());
    }

    fn root_protocol(&mut self, port: u8) -> (bool, u8) {
        self.controller
            .protocols()
            .of_port(port)
            .map_or((false, 0), |p| (p.major == 3, p.slot_type))
    }

    fn command(&mut self, command: Command) -> Result<Ticket, SubmitError> {
        let c = self.controller;
        let device = |slot| c.device(slot).ok_or(SubmitError::Busy);
        let trb = match command {
            Command::EnableSlot { slot_type } => Trb::enable_slot(slot_type),
            Command::AddressDevice {
                slot,
                context,
                max_packet,
            } => Trb::address_device(
                device(slot)?.address_input(&context, max_packet),
                slot,
                false,
            ),
            Command::EvaluateMaxPacket { slot, max_packet } => {
                Trb::evaluate_context(device(slot)?.evaluate_input(max_packet), slot)
            }
            Command::Configure {
                slot,
                context,
                speed,
            } => {
                let input = device(slot)?
                    .configure_input(context, speed)
                    .map_err(|_| SubmitError::Busy)?;
                Trb::configure_endpoint(input, slot, false)
            }
            Command::ConfigureHub { slot, context } => {
                Trb::configure_endpoint(device(slot)?.hub_input(context), slot, false)
            }
            Command::ResetEndpoint { slot, dci } => Trb::reset_endpoint(slot, dci, false),
            Command::StopEndpoint { slot, dci } => Trb::stop_endpoint(slot, dci, false),
            Command::SetDequeue { slot, dci } => {
                let (dequeue, cycle) = device(slot)?
                    .recovery_dequeue(dci)
                    .ok_or(SubmitError::Busy)?;
                Trb::set_tr_dequeue(dequeue, cycle, slot, dci)
            }
            Command::DisableSlot { slot } => Trb::disable_slot(slot),
        };
        c.submit(trb)
    }

    fn command_result(&mut self, ticket: Ticket) -> Option<CommandResult> {
        self.controller.take(ticket)
    }

    fn abandon_command(&mut self, ticket: Ticket) {
        self.controller.abandon(ticket);
    }

    fn stuck(&mut self) {
        self.controller.kill("a command never completed");
    }

    fn create(&mut self, slot: u8) -> bool {
        let serial = SERIALS.fetch_add(1, Ordering::Relaxed);
        self.controller.create_device(slot, serial)
    }

    fn destroy(&mut self, slot: u8) {
        self.controller.destroy_device(slot);
    }

    fn control(&mut self, slot: u8, setup: Setup) -> Result<Transfer, PushError> {
        let transfer = self
            .controller
            .device(slot)
            .ok_or(PushError::Halted)?
            .control(setup)?;
        #[cfg(feature = "test-hooks")]
        if setup == Setup::clear_halt(setup.index as u8) {
            CLEARS.fetch_add(1, Ordering::AcqRel);
        }
        Ok(transfer)
    }

    fn interrupt_in(&mut self, slot: u8, dci: u8, length: u16) -> Result<Transfer, PushError> {
        self.controller
            .device(slot)
            .ok_or(PushError::Halted)?
            .interrupt_in(dci, length)
    }

    fn transfer_result(&mut self, slot: u8, dci: u8, transfer: Transfer) -> Option<TransferResult> {
        self.controller.device(slot)?.take(dci, transfer)
    }

    fn abandon_transfer(&mut self, slot: u8, dci: u8, transfer: Transfer) {
        if let Some(device) = self.controller.device(slot) {
            device.abandon(dci, transfer);
        }
    }

    fn read(&mut self, slot: u8, dci: u8, out: &mut [u8]) {
        match self.controller.device(slot) {
            Some(device) => device.read(dci, out),
            None => out.fill(0),
        }
    }

    fn keep(&mut self, slot: u8, at: usize, length: usize) {
        if let Some(device) = self.controller.device(slot) {
            device.keep(at, length);
        }
    }

    fn stored<R>(&mut self, slot: u8, read: impl FnOnce(&[u8]) -> R) -> Option<R> {
        Some(self.controller.device(slot)?.stored(read))
    }

    fn halted(&mut self, slot: u8) -> u32 {
        self.controller.device(slot).map_or(0, |d| d.halted())
    }

    fn recovered(&mut self, slot: u8, dci: u8) {
        if let Some(device) = self.controller.device(slot) {
            device.recovered(dci);
        }
        crate::usb::TRANSFERS.wake_all();
    }

    fn escalated(&mut self, slot: u8) -> bool {
        self.controller
            .device(slot)
            .is_some_and(|d| d.take_escalation())
    }

    fn running_endpoints(&mut self, slot: u8) -> u32 {
        self.controller
            .device(slot)
            .map_or(0, |d| d.running_endpoints())
    }

    fn gone(&mut self, slot: u8) {
        if let Some(device) = self.controller.device(slot) {
            device.mark_gone();
        }
    }

    fn fail_transfers(&mut self, slot: u8, error: TransferError) {
        if let Some(device) = self.controller.device(slot) {
            device.fail_all(error);
        }
        crate::usb::TRANSFERS.wake_all();
    }

    fn wanted(&mut self, candidate: &Candidate) -> bool {
        crate::usb::bus::wanted(candidate)
    }

    fn offer(&mut self, slot: u8, node: &Node) {
        if let Some(device) = self.controller.device(slot) {
            crate::usb::bus::offer(&device, node);
        }
    }

    fn offered(&mut self, slot: u8) -> bool {
        self.controller.device(slot).is_none_or(|d| d.resolved())
    }

    fn unbind(&mut self, slot: u8) {
        if let Some(device) = self.controller.device(slot) {
            crate::usb::bus::unbind(&device);
        }
    }

    fn unbound(&mut self, slot: u8) -> bool {
        self.controller.device(slot).is_none_or(|d| d.is_unbound())
    }

    fn report(&mut self, report: Report) {
        let c = self.controller.number();
        if let Report::Enumerated { slot, node, .. } | Report::Unpowered { slot, node, .. } = report
            && let Some(device) = self.controller.device(slot)
        {
            device.set_node(node);
        }
        log(c, report);
    }
}

/// One line per report, each in a frame of its own: a single match over
/// every line's format state would hold them all at once.
fn log(c: u8, report: Report) {
    match report {
        Report::Connected { path, speed } => log_connected(c, path, speed),
        Report::Disconnected { path } => klog_info!("USB: {}-{} detached", c, path),
        Report::OverCurrent { path, over } => log_over_current(c, path, over),
        Report::Enumerated {
            node,
            hub_ports: Some(ports),
            ..
        } => log_hub(c, &node, ports),
        Report::Enumerated { node, .. } if node.functions == 0 => log_bare(c, &node),
        Report::Enumerated { .. } => {}
        Report::Unpowered {
            node,
            needs_ma,
            offers_ma,
            ..
        } => log_unpowered(c, &node, needs_ma, offers_ma),
        Report::HubUnused { node, why, .. } => log_hub_unused(c, &node, why),
        Report::Failed {
            path,
            failure,
            tries,
        } => log_failed(c, path, failure, tries),
        Report::GivenUp { path } => klog_info!("USB: {}-{} disabled until unplugged", c, path),
        Report::Removed { slot, path } => log_removed(c, path, slot),
    }
}

#[inline(never)]
fn log_connected(c: u8, path: Path, speed: Option<Speed>) {
    match speed {
        Some(speed) => klog_info!("USB: {}-{} attached, {}", c, path, speed.name()),
        None => klog_info!("USB: {}-{} attached", c, path),
    }
}

#[inline(never)]
fn log_over_current(c: u8, path: Path, over: bool) {
    let over = if over { "over" } else { "back under" };
    klog_info!("USB: {}-{} {} current", c, path, over);
}

#[inline(never)]
fn log_hub(c: u8, node: &Node, ports: u8) {
    klog_info!(
        "USB: {}-{} {:04x}:{:04x} hub, {} ports, {}",
        c,
        node.path,
        node.vendor,
        node.product,
        ports,
        node.speed.name()
    );
}

#[inline(never)]
fn log_bare(c: u8, node: &Node) {
    klog_info!(
        "USB: {}-{} {:04x}:{:04x} no functions",
        c,
        node.path,
        node.vendor,
        node.product
    );
}

#[inline(never)]
fn log_unpowered(c: u8, node: &Node, needs_ma: u32, offers_ma: u32) {
    klog_info!(
        "USB: {}-{} {:04x}:{:04x} not configured: it needs {} mA, its port offers {}",
        c,
        node.path,
        node.vendor,
        node.product,
        needs_ma,
        offers_ma
    );
}

#[inline(never)]
fn log_hub_unused(c: u8, node: &Node, why: HubUnused) {
    let why = match why {
        HubUnused::TooDeep => "five tiers of hubs above it",
        HubUnused::NoRoom => "no room for another hub",
    };
    klog_info!(
        "USB: {}-{} {:04x}:{:04x} hub's ports not driven: {}",
        c,
        node.path,
        node.vendor,
        node.product,
        why
    );
}

#[inline(never)]
fn log_failed(c: u8, path: Path, failure: Failure, tries: u8) {
    klog_info!(
        "USB: {}-{} enumeration failed, try {} of {}: {}",
        c,
        path,
        tries,
        slopos_usb_core::bus::port::MAX_FAILURES,
        Why(failure)
    );
}

#[inline(never)]
fn log_removed(c: u8, path: Path, slot: u8) {
    klog_info!("USB: {}-{} removed from slot {}", c, path, slot);
}
