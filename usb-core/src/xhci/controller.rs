//! The controller's sequences: taking it from the firmware, halting and
//! resetting it, giving it its memory, running it, and draining its events.

use super::bus::{Error, RegisterBus, Result, Wait, poll};
use super::ext_cap::legacy;
use super::memory::DmaPage;
use super::regs::*;
use super::ring::{EventRing, RING_TRBS};
use super::trb::Event;

/// The firmware has this long to let go of the controller (§4.22.1).
pub const BIOS_RELEASE_MS: u32 = 1000;
/// HCHalted follows Run/Stop within 16 microframes (§5.4.1.1); a margin
/// over that.
pub const HALT_MS: u32 = 32;
/// Some controllers take seconds to leave reset or become ready after one,
/// which a probe waits out; a shutdown, whose power goes regardless, waits
/// less.
pub const PROBE_RESET_MS: u32 = 10_000;
pub const SHUTDOWN_RESET_MS: u32 = 250;
/// Some Intel controllers hang the machine if a register is touched sooner
/// after HCRST is set.
pub const AFTER_RESET_US: u32 = 1000;

/// How the firmware let go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handoff {
    /// The controller has no USB Legacy Support capability.
    NoLegacySupport,
    /// The firmware cleared BIOS Owned when asked.
    Released,
    /// It did not within [`BIOS_RELEASE_MS`], so BIOS Owned was cleared here.
    Forced,
}

/// Ask the firmware for the controller through USBLEGSUP at `legacy`, then
/// turn its SMIs off and acknowledge any it raised: some firmware reports a
/// clean handoff and leaves them armed.
pub fn take_ownership<B: RegisterBus>(bus: &mut B, legacy: Option<usize>) -> Handoff {
    let Some(at) = legacy else {
        return Handoff::NoLegacySupport;
    };
    let mut handoff = Handoff::Released;
    if bus.read32(at) & legacy::BIOS_OWNED != 0 {
        bus.write8(at + legacy::OS_OWNED_BYTE, 1);
        let released = poll(bus, Wait::BiosRelease, 1000, BIOS_RELEASE_MS, |bus| {
            let value = bus.read32(at);
            (value != u32::MAX).then_some(value & legacy::BIOS_OWNED == 0)
        });
        if released.is_err() {
            bus.write8(at + legacy::BIOS_OWNED_BYTE, 0);
            handoff = Handoff::Forced;
        }
    }
    bus.write8(at + legacy::OS_OWNED_BYTE, 1);
    let control = bus.read32(at + legacy::CONTROL);
    bus.write32(
        at + legacy::CONTROL,
        (control & legacy::CONTROL_PRESERVE) | legacy::SMI_EVENTS,
    );
    handoff
}

fn status<B: RegisterBus>(bus: &mut B, layout: &Layout) -> Option<u32> {
    let sts = bus.read32(layout.op(USBSTS));
    (sts != u32::MAX).then_some(sts)
}

/// Clear Run/Stop and wait for the controller to say it halted.
pub fn halt<B: RegisterBus>(bus: &mut B, layout: &Layout) -> Result<()> {
    let sts = status(bus, layout).ok_or(Error::Absent)?;
    if sts & STS_HALTED == 0 {
        let cmd = bus.read32(layout.op(USBCMD));
        bus.write32(layout.op(USBCMD), cmd & !(CMD_RUN | CMD_INTE | CMD_HSEE));
    }
    poll(bus, Wait::Halt, 100, HALT_MS, |bus| {
        status(bus, layout).map(|sts| sts & STS_HALTED != 0)
    })
}

fn wait_ready<B: RegisterBus>(bus: &mut B, layout: &Layout, bound_ms: u32) -> Result<()> {
    poll(bus, Wait::Ready, 1000, bound_ms, |bus| {
        status(bus, layout).map(|sts| sts & STS_NOT_READY == 0)
    })
}

/// Halt the controller, stop it mastering the bus, and reset it. Bus
/// mastering goes only once it has halted, since a firmware schedule may
/// need DMA to stop; the reset comes after, so nothing it was given is
/// read again. Each wait for the controller is bounded by `bound_ms`.
pub fn halt_and_reset<B: RegisterBus>(bus: &mut B, layout: &Layout, bound_ms: u32) -> Result<()> {
    wait_ready(bus, layout, bound_ms)?;
    halt(bus, layout)?;
    bus.bus_master(false);
    bus.write32(layout.op(USBCMD), CMD_RESET);
    bus.delay_us(AFTER_RESET_US);
    poll(bus, Wait::Reset, 1000, bound_ms, |bus| {
        let cmd = bus.read32(layout.op(USBCMD));
        (cmd != u32::MAX).then_some(cmd & CMD_RESET == 0)
    })?;
    wait_ready(bus, layout, bound_ms)
}

/// A dead controller is left halted and off the bus; it is not reset.
pub fn quiesce<B: RegisterBus>(bus: &mut B, layout: &Layout) -> Result<()> {
    let halted = halt(bus, layout);
    bus.bus_master(false);
    halted
}

/// Where a reset controller finds its memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub slots: u8,
    pub dcbaa: u64,
    pub crcr: u64,
    pub segment_table: u64,
    pub event_ring: u64,
}

/// Let a reset controller master the bus and program it with `setup`,
/// interrupter 0 last: writing ERSTBA enables its event ring (§4.9.4), and
/// the controller may read the segment table as soon as it is written.
pub fn configure<B: RegisterBus>(bus: &mut B, layout: &Layout, setup: &Setup) {
    bus.bus_master(true);
    bus.write32(layout.op(CONFIG), u32::from(setup.slots));
    bus.write64(layout.op(DCBAAP), setup.dcbaa);
    bus.write64(layout.op(CRCR), setup.crcr);
    let interrupter = layout.interrupter(0);
    bus.write32(interrupter + IMOD, IMOD_INTERVAL);
    bus.write32(interrupter + ERSTSZ, 1);
    bus.write64(interrupter + ERDP, setup.event_ring);
    bus.write64(interrupter + ERSTBA, setup.segment_table);
    bus.write32(interrupter + IMAN, IMAN_PENDING | IMAN_ENABLE);
}

/// Run a configured controller.
pub fn start<B: RegisterBus>(bus: &mut B, layout: &Layout) -> Result<()> {
    bus.write32(layout.op(USBCMD), CMD_RUN | CMD_INTE | CMD_HSEE);
    poll(bus, Wait::Run, 100, HALT_MS, |bus| {
        status(bus, layout).map(|sts| sts & STS_HALTED == 0)
    })
}

/// A root port as [`acknowledge_port`] left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acknowledged {
    /// The port as last read, with every change bit seen on the way.
    pub status: PortSc,
    /// It kept changing until the reads ran out, so its last change bits
    /// are still set and its next change raises no event.
    pub unsettled: bool,
}

/// Read root port `port`'s status and clear its change bits, so a later
/// change raises a Port Status Change Event of its own. A change that lands
/// between the read and the clear raises none, since its bit was already
/// set, so the port is read again until a read finds nothing changed.
pub fn acknowledge_port<B: RegisterBus>(
    bus: &mut B,
    layout: &Layout,
    port: u8,
) -> Option<Acknowledged> {
    let mut seen = 0;
    for _ in 0..ACKNOWLEDGE_READS {
        let portsc = PortSc(bus.read32(layout.port(port)));
        if portsc.0 == u32::MAX {
            return None;
        }
        seen |= portsc.changes();
        if portsc.changes() == 0 {
            return Some(Acknowledged {
                status: PortSc(portsc.0 | seen),
                unsettled: false,
            });
        }
        bus.write32(layout.port(port), portsc.acknowledge(portsc.changes()));
    }
    let portsc = PortSc(bus.read32(layout.port(port)));
    (portsc.0 != u32::MAX).then_some(Acknowledged {
        status: PortSc(portsc.0 | seen),
        unsettled: portsc.changes() != 0,
    })
}

const ACKNOWLEDGE_READS: usize = 4;

/// Power root port `port` if it is off. A controller that does not switch
/// port power reports every port powered.
pub fn power_port<B: RegisterBus>(bus: &mut B, layout: &Layout, port: u8) {
    let portsc = PortSc(bus.read32(layout.port(port)));
    if portsc.0 != u32::MAX && !portsc.powered() {
        bus.write32(layout.port(port), portsc.power_on());
    }
}

/// Whether the controller still works, as USBSTS says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Health {
    Running,
    /// Host System Error: the controller stopped on a PCI error.
    HostSystemError,
    /// Host Controller Error: an internal error the controller cannot
    /// recover from without a reset.
    ControllerError,
    /// The registers read all ones.
    Absent,
}

impl Health {
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0 => Self::Running,
            1 => Self::HostSystemError,
            2 => Self::ControllerError,
            _ => Self::Absent,
        }
    }
}

pub fn health<B: RegisterBus>(bus: &mut B, layout: &Layout) -> Health {
    match status(bus, layout) {
        None => Health::Absent,
        Some(sts) if sts & STS_HOST_SYSTEM_ERROR != 0 => Health::HostSystemError,
        Some(sts) if sts & STS_CONTROLLER_ERROR != 0 => Health::ControllerError,
        Some(_) => Health::Running,
    }
}

/// The most events one drain hands on: a whole segment, which bounds the
/// drain even against a controller that writes past ERDP.
pub const DRAIN_BUDGET: usize = RING_TRBS as usize;

/// What one drain found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drained {
    pub events: usize,
    pub health: Health,
}

/// Acknowledge interrupter 0, hand each event the ring holds to `handle`,
/// at most `budget`, and move ERDP past them, clearing Event Handler Busy;
/// the controller interrupts again for any left. A controller that is not
/// [`Health::Running`] is not drained.
pub fn drain<B: RegisterBus, P: DmaPage>(
    bus: &mut B,
    layout: &Layout,
    ring: &mut EventRing<P>,
    budget: usize,
    mut handle: impl FnMut(Event),
) -> Drained {
    let health = health(bus, layout);
    if health != Health::Running {
        return Drained { events: 0, health };
    }
    let interrupter = layout.interrupter(0);
    bus.write32(layout.op(USBSTS), STS_EVENT_INTERRUPT);
    bus.write32(interrupter + IMAN, IMAN_PENDING | IMAN_ENABLE);
    let mut events = 0;
    while events < budget {
        let Some(trb) = ring.pop() else { break };
        events += 1;
        handle(Event::decode(trb));
    }
    bus.write64(interrupter + ERDP, ring.dequeue_pointer() | ERDP_BUSY);
    Drained { events, health }
}

/// Doorbell 0's offset, through which the command ring is rung.
pub fn command_doorbell(layout: &Layout) -> usize {
    layout.doorbell(0)
}
