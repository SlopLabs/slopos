//! xHCI host controllers on PCI: the probe that takes one from the firmware,
//! resets it and runs it, and the registry the USB thread serves.

mod controller;
mod page;

pub use controller::Controller;

use core::fmt;

use slopos_ostd::sync::OnceLock;
use slopos_ostd::{KArc, klog_info};
use slopos_usb_core::knob::Mode;
use slopos_usb_core::xhci::controller::Handoff;
use slopos_usb_core::xhci::{Capabilities, Decline, Found, Trb, ext_cap};

use crate::driver_core::msi::{self as core_msi, IrqMechanism};
use crate::driver_core::shutdown::{self, DeviceShutdown};
use crate::pci::{
    BoundDevice, PciMatch, PciProbeError, ProbeOutcome, enable_memory_space, pci_config_read16,
    pci_config_write16, set_power_d0,
};
use crate::pci_defs::{PCI_COMMAND_OFFSET, PciDeviceInfo};
use page::Bus;

const CLASS_SERIAL_BUS: u8 = 0x0c;
const SUBCLASS_USB: u8 = 0x03;
const PROG_IF_XHCI: u8 = 0x30;
pub const MAX_CONTROLLERS: usize = 8;
/// How long a freshly run controller has to answer its first command by
/// interrupt, and then when polled.
const PROBE_INTERRUPT_MS: u32 = 200;
const PROBE_COMMAND_MS: u32 = 500;
/// The MSI-X table and its PBA each take a dynamic MMIO range, without
/// which a controller falls back to MSI.
const MSIX_RANGES: usize = 2;

static CONTROLLERS: [OnceLock<KArc<Controller>>; MAX_CONTROLLERS] =
    [const { OnceLock::new() }; MAX_CONTROLLERS];

/// Controller `number`, counted from 1 in probe order, as `1-3` names root
/// port 3 of the first.
pub fn controller(number: u8) -> Option<KArc<Controller>> {
    let slot = CONTROLLERS.get(usize::from(number).checked_sub(1)?)?;
    slot.get().map(KArc::clone)
}

fn published() -> impl Iterator<Item = &'static KArc<Controller>> {
    CONTROLLERS.iter().filter_map(OnceLock::get)
}

pub(super) fn serve_all() {
    for controller in published() {
        controller.serve();
    }
}

/// Why a controller taken from the firmware was not brought into service.
#[derive(Clone, Copy)]
enum Failure {
    Sequence(slopos_usb_core::xhci::Error),
    NoMemory,
    NoInterrupt,
    NoAnswer,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Sequence(slopos_usb_core::xhci::Error::Timeout(wait)) => {
                write!(f, "timed out waiting for {:?}", wait)
            }
            Failure::Sequence(slopos_usb_core::xhci::Error::Absent) => {
                f.write_str("its registers read all ones")
            }
            Failure::NoMemory => f.write_str("out of memory"),
            Failure::NoInterrupt => f.write_str("no MSI-X or MSI vector"),
            Failure::NoAnswer => f.write_str("it answered no command"),
        }
    }
}

impl From<Failure> for PciProbeError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::NoMemory => PciProbeError::OutOfMemory,
            Failure::NoInterrupt => PciProbeError::Unsupported,
            Failure::Sequence(_) | Failure::NoAnswer => PciProbeError::DeviceFault,
        }
    }
}

struct Bdf<'a>(&'a PciDeviceInfo);

impl fmt::Display for Bdf<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let info = self.0;
        write!(
            f,
            "{:02x}:{:02x}.{} ({:04x}:{:04x})",
            info.bus, info.device, info.function, info.vendor_id, info.device_id
        )
    }
}

/// The capabilities, protocols and ports of a controller, in one line.
struct Summary<'a> {
    caps: &'a Capabilities,
    found: &'a Found,
}

impl fmt::Display for Summary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let caps = self.caps;
        let (major, minor) = caps.version_parts();
        write!(
            f,
            "xHCI {}.{}, {} slots, {} interrupters, {}-byte contexts, {}, {} scratchpads, {}",
            major,
            minor,
            caps.max_slots,
            caps.max_interrupters,
            caps.context_bytes(),
            if caps.ac64 { "64-bit" } else { "32-bit" },
            caps.scratchpads,
            if self.found.legacy.is_some() {
                "legacy support"
            } else {
                "no legacy support"
            },
        )?;
        let mut separator = ";";
        for protocol in self.found.protocols.iter() {
            let (major, minor) = protocol.revision();
            write!(
                f,
                "{} USB {}.{} ports {}-{}",
                separator,
                major,
                minor,
                protocol.first_port,
                protocol.last_port()
            )?;
            separator = ",";
        }
        Ok(())
    }
}

fn decline_reason(decline: Decline) -> &'static str {
    match decline {
        Decline::No64BitAddressing => "no 64-bit addressing",
        Decline::TooManyScratchpads(_) => "more scratchpad buffers than a page lists",
        Decline::No4KPages => "no 4 KiB page size",
    }
}

fn probe(bound: &mut BoundDevice<'_>) -> Result<ProbeOutcome, PciProbeError> {
    let info = *bound.info();
    if info.prog_if != PROG_IF_XHCI || super::mode() == Mode::Off {
        return Ok(ProbeOutcome::Declined);
    }
    if !info.has_msix() && !info.has_msi() {
        klog_info!(
            "USB: xhci at {}: neither MSI-X nor MSI; left to the firmware",
            Bdf(&info)
        );
        return Ok(ProbeOutcome::Declined);
    }
    let bar = info.bars[0];
    if bar.base == 0 || bar.is_io != 0 || bar.size == 0 {
        klog_info!(
            "USB: xhci at {}: no memory BAR0; left to the firmware",
            Bdf(&info)
        );
        return Ok(ProbeOutcome::Declined);
    }
    let msix_table = if info.has_msi() { 0 } else { MSIX_RANGES };
    if slopos_ostd::mm::io_mem_ranges_free() < 1 + msix_table {
        klog_info!(
            "USB: xhci at {}: no MMIO range left to map it; left to the firmware",
            Bdf(&info)
        );
        return Ok(ProbeOutcome::Declined);
    }
    let count = published().count();
    if count == MAX_CONTROLLERS {
        klog_info!(
            "USB: xhci at {}: no room for another controller",
            Bdf(&info)
        );
        return Ok(ProbeOutcome::Declined);
    }
    take(bound, &info, count as u8 + 1)
}

/// Read the controller's capabilities and, unless it is declined or only
/// reported, bring it into service as controller `number`.
#[inline(never)]
fn take(
    bound: &mut BoundDevice<'_>,
    info: &PciDeviceInfo,
    number: u8,
) -> Result<ProbeOutcome, PciProbeError> {
    let bar = info.bars[0];
    let command = pci_config_read16(info.bus, info.device, info.function, PCI_COMMAND_OFFSET);
    let restore = || {
        pci_config_write16(
            info.bus,
            info.device,
            info.function,
            PCI_COMMAND_OFFSET,
            command,
        );
    };
    set_power_d0(info);
    enable_memory_space(info);
    let Ok(regs) = bound.map_bar(0, 0, bar.size as usize).cloned() else {
        restore();
        return Err(PciProbeError::OutOfMemory);
    };
    let bar_len = regs.size();
    let mut bus = Bus { regs: &regs, info };
    let caps = match Capabilities::read(&mut bus, bar_len) {
        Ok(caps) => caps,
        Err(malformed) => {
            klog_info!(
                "USB: xhci at {}: {:?}; left to the firmware",
                Bdf(info),
                malformed
            );
            restore();
            return Ok(ProbeOutcome::Declined);
        }
    };
    let found = ext_cap::find(&mut bus, &caps.layout(), bar_len);
    let summary = Summary {
        caps: &caps,
        found: &found,
    };
    if let Some(decline) = caps.decline() {
        klog_info!(
            "USB: xhci at {}: {}; declined: {}",
            Bdf(info),
            summary,
            decline_reason(decline)
        );
        restore();
        return Ok(ProbeOutcome::Declined);
    }
    if super::mode() == Mode::Report {
        klog_info!(
            "USB: xhci at {}: {}; left to the firmware (usb=report)",
            Bdf(info),
            summary
        );
        restore();
        return Ok(ProbeOutcome::Declined);
    }
    klog_info!("USB: xhci {} at {}: {}", number, Bdf(info), summary);
    let Ok(controller) = Controller::new(number, info, regs, caps, &found.protocols) else {
        klog_info!("USB: xhci {} left to the firmware: out of memory", number);
        restore();
        return Err(PciProbeError::OutOfMemory);
    };
    match bring_up(bound, controller, found.legacy) {
        Ok(()) => Ok(ProbeOutcome::Bound),
        Err(failure) => {
            klog_info!("USB: xhci {} left unused: {}", number, failure);
            Err(failure.into())
        }
    }
}

/// Take the controller from the firmware, reset it, run it and publish it.
/// What can fail without touching it came before the handoff, which cannot
/// be undone; past it a failure takes the controller off the bus and resets
/// it, or logs that it would not reset.
#[inline(never)]
fn bring_up(
    bound: &mut BoundDevice<'_>,
    controller: KArc<Controller>,
    legacy: Option<usize>,
) -> Result<(), Failure> {
    let number = controller.number();
    match controller.take_ownership(legacy) {
        Handoff::NoLegacySupport => {}
        Handoff::Released => klog_info!("USB: xhci {} taken from the firmware", number),
        Handoff::Forced => klog_info!("USB: xhci {} taken from firmware that never let go", number),
    }
    controller.reset().map_err(Failure::Sequence)?;
    if let Err(failure) = run(bound, &controller) {
        controller.stop();
        return Err(failure);
    }
    publish(controller);
    Ok(())
}

#[inline(never)]
fn run(bound: &mut BoundDevice<'_>, controller: &KArc<Controller>) -> Result<(), Failure> {
    controller.configure();
    controller.set_interrupt(interrupts(bound, controller)?);
    controller.run().map_err(Failure::Sequence)?;
    controller.scan_ports();
    controller.drain();
    if answers(controller) {
        Ok(())
    } else {
        Err(Failure::NoAnswer)
    }
}

/// MSI-X entry 0 or the MSI vector, both for interrupter 0, the only one.
#[inline(never)]
fn interrupts(
    bound: &mut BoundDevice<'_>,
    controller: &KArc<Controller>,
) -> Result<&'static str, Failure> {
    let info = *bound.info();
    let handler = KArc::clone(controller);
    let mut vectors = [0u8; 1];
    let irq = core_msi::setup_interrupts(bound, 1, &mut vectors, move |_entry: u8| {
        handler.handle_irq();
    })
    .ok_or(Failure::NoInterrupt)?;
    let kind = match &irq {
        IrqMechanism::Msix { cap, .. } => {
            crate::msix::msix_enable(info.bus, info.device, info.function, cap);
            "MSI-X"
        }
        IrqMechanism::Msi { .. } => "MSI",
    };
    bound.attach(irq).map_err(|_| Failure::NoMemory)?;
    Ok(kind)
}

/// Send a No Op command and wait for its completion: proof the rings and
/// the doorbell work. The interrupt has the first chance to deliver it; a
/// controller that only answers when polled is logged and kept.
#[inline(never)]
fn answers(controller: &Controller) -> bool {
    let Ok(ticket) = controller.submit(Trb::no_op_command()) else {
        return false;
    };
    let mut result = None;
    crate::hpet::spin_until(
        &mut || {
            result = controller.take(ticket);
            result.is_some()
        },
        PROBE_INTERRUPT_MS,
    );
    if result.is_none() {
        crate::hpet::spin_until(
            &mut || {
                controller.drain();
                result = controller.take(ticket);
                result.is_some()
            },
            PROBE_COMMAND_MS,
        );
        if result.is_some() {
            klog_info!(
                "USB: xhci {} raised no interrupt; served by polling",
                controller.number()
            );
        }
    }
    if result.is_none() {
        controller.abandon(ticket);
    }
    matches!(result, Some(Ok(c)) if c.code.is_success())
}

#[inline(never)]
fn publish(controller: KArc<Controller>) {
    klog_info!(
        "USB: xhci {} running on {}",
        controller.number(),
        controller.interrupt()
    );
    let hook: KArc<dyn DeviceShutdown> = controller.clone();
    if !shutdown::register(hook) {
        klog_info!(
            "USB: xhci {} will not be reset at poweroff",
            controller.number()
        );
    }
    if let Some(slot) = CONTROLLERS.get(usize::from(controller.number()) - 1) {
        slot.call_once(|| controller);
    }
    super::start_thread();
    super::wake();
}

crate::pci_driver! {
    pub static XHCI_DRIVER = {
        name: "xhci",
        match_table: &[PciMatch::ClassSubclass {
            class: CLASS_SERIAL_BUS,
            subclass: SUBCLASS_USB,
        }],
        probe: probe,
    };
}
